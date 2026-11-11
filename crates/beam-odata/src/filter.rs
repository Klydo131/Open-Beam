//! `$filter` parsing and SQL generation.
//!
//! Supported: `and`, `or`, `not`, parentheses, the six comparison operators,
//! `null`/boolean/number/string/date/date-time literals, and the functions
//! listed in [`FUNCTIONS`]. Arithmetic, lambdas and `in` are rejected.

use crate::{quote_ident, EntitySet, Literal};

const MAX_DEPTH: usize = 32;
const MAX_FILTER_BYTES: usize = 8 * 1024;
const MAX_IN_VALUES: usize = 1_000;

/// OData function name, DuckDB function name, argument count.
const FUNCTIONS: &[(&str, &str, usize)] = &[
    ("contains", "contains", 2),
    ("startswith", "starts_with", 2),
    ("endswith", "ends_with", 2),
    ("tolower", "lower", 1),
    ("toupper", "upper", 1),
    ("trim", "trim", 1),
    ("length", "length", 1),
    ("year", "year", 1),
    ("month", "month", 1),
    ("day", "day", 1),
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FilterError {
    #[error("$filter is too long")]
    TooLong,
    #[error("$filter is nested too deeply")]
    TooDeep,
    #[error("$filter syntax error at position {0}")]
    Syntax(usize),
    #[error("unknown property '{0}'")]
    UnknownProperty(String),
    #[error("unsupported function '{0}'")]
    UnsupportedFunction(String),
    #[error("function '{0}' takes {1} argument(s)")]
    Arity(&'static str, usize),
    #[error("'in' takes between 1 and {MAX_IN_VALUES} values")]
    InList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Property(String),
    Value(Literal),
    Date(String),
    DateTime(String),
    Call(&'static str, Vec<Expr>),
    Compare(Box<Expr>, CmpOp, Box<Expr>),
    /// `property in (value, value, ...)`.
    In(Box<Expr>, Vec<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Date(String),
    DateTime(String),
    LParen,
    RParen,
    Comma,
}

fn lex(input: &str) -> Result<Vec<(usize, Token)>, FilterError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'(' => {
                tokens.push((start, Token::LParen));
                i += 1;
            }
            b')' => {
                tokens.push((start, Token::RParen));
                i += 1;
            }
            b',' => {
                tokens.push((start, Token::Comma));
                i += 1;
            }
            b'\'' => {
                // OData escapes a quote inside a string by doubling it.
                let mut value = String::new();
                i += 1;
                loop {
                    let rest = input.get(i..).ok_or(FilterError::Syntax(start))?;
                    let end = rest.find('\'').ok_or(FilterError::Syntax(start))?;
                    value.push_str(&rest[..end]);
                    i += end + 1;
                    if bytes.get(i) == Some(&b'\'') {
                        value.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                }
                tokens.push((start, Token::Str(value)));
            }
            b'0'..=b'9' | b'-' => {
                i += 1;
                while i < bytes.len()
                    && matches!(
                        bytes[i],
                        b'0'..=b'9' | b'.' | b':' | b'-' | b'+' | b'T' | b'Z' | b'e' | b'E'
                    )
                {
                    i += 1;
                }
                tokens.push((
                    start,
                    number_or_date(&input[start..i]).ok_or(FilterError::Syntax(start))?,
                ));
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push((start, Token::Ident(input[start..i].to_string())));
            }
            _ => return Err(FilterError::Syntax(start)),
        }
    }
    Ok(tokens)
}

fn number_or_date(text: &str) -> Option<Token> {
    let b = text.as_bytes();
    let is_date = b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
    if is_date {
        return match b.len() {
            10 => Some(Token::Date(text.to_string())),
            _ if b[10] == b'T' => Some(Token::DateTime(text.to_string())),
            _ => None,
        };
    }
    if let Ok(n) = text.parse::<i64>() {
        return Some(Token::Int(n));
    }
    text.parse::<f64>().ok().filter(|f| f.is_finite()).map(Token::Float)
}

struct Parser {
    tokens: Vec<(usize, Token)>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(_, t)| t)
    }

    fn here(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.end, |(at, _)| *at)
    }

    fn syntax<T>(&self) -> Result<T, FilterError> {
        Err(FilterError::Syntax(self.here()))
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        let hit = matches!(self.peek(), Some(Token::Ident(w)) if w == word);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect(&mut self, token: Token) -> Result<(), FilterError> {
        if self.peek() == Some(&token) {
            self.pos += 1;
            Ok(())
        } else {
            self.syntax()
        }
    }

    fn or(&mut self, depth: usize) -> Result<Expr, FilterError> {
        let mut left = self.and(depth)?;
        while self.eat_keyword("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and(depth)?));
        }
        Ok(left)
    }

    fn and(&mut self, depth: usize) -> Result<Expr, FilterError> {
        let mut left = self.not(depth)?;
        while self.eat_keyword("and") {
            left = Expr::And(Box::new(left), Box::new(self.not(depth)?));
        }
        Ok(left)
    }

    fn not(&mut self, depth: usize) -> Result<Expr, FilterError> {
        if depth > MAX_DEPTH {
            return Err(FilterError::TooDeep);
        }
        if self.eat_keyword("not") {
            return Ok(Expr::Not(Box::new(self.not(depth + 1)?)));
        }
        self.comparison(depth)
    }

    fn comparison(&mut self, depth: usize) -> Result<Expr, FilterError> {
        let left = self.primary(depth)?;
        if self.eat_keyword("in") {
            self.expect(Token::LParen)?;
            let mut values = Vec::new();
            loop {
                match self.primary(depth + 1)? {
                    value @ (Expr::Value(_) | Expr::Date(_) | Expr::DateTime(_)) => values.push(value),
                    _ => return self.syntax(),
                }
                if values.len() > MAX_IN_VALUES {
                    return Err(FilterError::InList);
                }
                if self.peek() == Some(&Token::Comma) {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            self.expect(Token::RParen)?;
            return Ok(Expr::In(Box::new(left), values));
        }
        let op = match self.peek() {
            Some(Token::Ident(w)) => match w.as_str() {
                "eq" => CmpOp::Eq,
                "ne" => CmpOp::Ne,
                "gt" => CmpOp::Gt,
                "ge" => CmpOp::Ge,
                "lt" => CmpOp::Lt,
                "le" => CmpOp::Le,
                _ => return Ok(left),
            },
            _ => return Ok(left),
        };
        self.pos += 1;
        let right = self.primary(depth)?;
        Ok(Expr::Compare(Box::new(left), op, Box::new(right)))
    }

    fn primary(&mut self, depth: usize) -> Result<Expr, FilterError> {
        let Some(token) = self.peek().cloned() else {
            return self.syntax();
        };
        self.pos += 1;
        Ok(match token {
            Token::LParen => {
                let inner = self.or(depth + 1)?;
                self.expect(Token::RParen)?;
                inner
            }
            Token::Str(s) => Expr::Value(Literal::Text(s)),
            Token::Int(n) => Expr::Value(Literal::Int(n)),
            Token::Float(f) => Expr::Value(Literal::Float(f)),
            Token::Date(d) => Expr::Date(d),
            Token::DateTime(d) => Expr::DateTime(d),
            Token::Ident(word) => match word.as_str() {
                "true" => Expr::Value(Literal::Bool(true)),
                "false" => Expr::Value(Literal::Bool(false)),
                "null" => Expr::Value(Literal::Null),
                _ if self.peek() == Some(&Token::LParen) => self.call(&word, depth)?,
                _ => Expr::Property(word),
            },
            Token::RParen | Token::Comma => {
                self.pos -= 1;
                return self.syntax();
            }
        })
    }

    fn call(&mut self, name: &str, depth: usize) -> Result<Expr, FilterError> {
        let &(odata_name, _, arity) = FUNCTIONS
            .iter()
            .find(|(n, _, _)| *n == name)
            .ok_or_else(|| FilterError::UnsupportedFunction(name.to_string()))?;
        self.expect(Token::LParen)?;
        let mut args = Vec::new();
        if self.peek() != Some(&Token::RParen) {
            loop {
                args.push(self.or(depth + 1)?);
                if self.peek() == Some(&Token::Comma) {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        self.expect(Token::RParen)?;
        if args.len() != arity {
            return Err(FilterError::Arity(odata_name, arity));
        }
        Ok(Expr::Call(odata_name, args))
    }
}

pub fn parse(input: &str) -> Result<Expr, FilterError> {
    if input.len() > MAX_FILTER_BYTES {
        return Err(FilterError::TooLong);
    }
    let mut parser = Parser {
        tokens: lex(input)?,
        pos: 0,
        end: input.len(),
    };
    let expr = parser.or(0)?;
    if parser.pos != parser.tokens.len() {
        return parser.syntax();
    }
    Ok(expr)
}

/// Renders `expr` as a SQL boolean expression, appending bind values to
/// `params` in placeholder order.
pub fn to_sql(expr: &Expr, set: &EntitySet, params: &mut Vec<Literal>) -> Result<String, FilterError> {
    Ok(match expr {
        Expr::Property(name) => {
            if set.property(name).is_none() {
                return Err(FilterError::UnknownProperty(name.clone()));
            }
            quote_ident(name)
        }
        Expr::Value(Literal::Null) => "NULL".to_string(),
        Expr::Value(value) => {
            params.push(value.clone());
            "?".to_string()
        }
        Expr::Date(text) => {
            params.push(Literal::Text(text.clone()));
            "CAST(? AS DATE)".to_string()
        }
        Expr::DateTime(text) => {
            params.push(Literal::Text(text.clone()));
            "CAST(? AS TIMESTAMP)".to_string()
        }
        Expr::Call(name, args) => {
            let sql_name = FUNCTIONS.iter().find(|(n, _, _)| n == name).map_or(*name, |f| f.1);
            let rendered: Result<Vec<_>, _> = args.iter().map(|a| to_sql(a, set, params)).collect();
            format!("{sql_name}({})", rendered?.join(", "))
        }
        Expr::Compare(left, op, right) => {
            let null = Expr::Value(Literal::Null);
            let (operand, other) = if **right == null { (left, right) } else { (right, left) };
            if **other == null && matches!(op, CmpOp::Eq | CmpOp::Ne) {
                let test = if *op == CmpOp::Eq { "IS NULL" } else { "IS NOT NULL" };
                return Ok(format!("({} {test})", to_sql(operand, set, params)?));
            }
            let symbol = match op {
                CmpOp::Eq => "=",
                CmpOp::Ne => "<>",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
            };
            let l = to_sql(left, set, params)?;
            let r = to_sql(right, set, params)?;
            format!("({l} {symbol} {r})")
        }
        Expr::In(left, values) => {
            let left = to_sql(left, set, params)?;
            let rendered: Result<Vec<String>, FilterError> = values.iter().map(|v| to_sql(v, set, params)).collect();
            format!("({left} IN ({}))", rendered?.join(", "))
        }
        Expr::And(l, r) => format!("({} AND {})", to_sql(l, set, params)?, to_sql(r, set, params)?),
        Expr::Or(l, r) => format!("({} OR {})", to_sql(l, set, params)?, to_sql(r, set, params)?),
        Expr::Not(inner) => format!("(NOT {})", to_sql(inner, set, params)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EdmType, Property};

    fn set() -> EntitySet {
        let p = |name: &str, edm| Property {
            name: name.to_string(),
            edm,
        };
        EntitySet {
            name: "sales".into(),
            key: "id".into(),
            properties: vec![
                p("id", EdmType::Int64),
                p("region", EdmType::String),
                p("amount", EdmType::Double),
                p("sold_on", EdmType::Date),
            ],
        }
    }

    fn sql(filter: &str) -> (String, Vec<Literal>) {
        let mut params = Vec::new();
        let text = to_sql(&parse(filter).unwrap(), &set(), &mut params).unwrap();
        (text, params)
    }

    #[test]
    fn values_become_parameters_never_sql_text() {
        let (text, params) = sql("region eq 'x''; DROP TABLE sales; --'");
        assert_eq!(text, "(\"region\" = ?)");
        assert_eq!(params, vec![Literal::Text("x'; DROP TABLE sales; --".into())]);
    }

    #[test]
    fn precedence_and_grouping() {
        let (text, _) = sql("amount gt 10 or amount lt 2 and not (region eq 'EU')");
        assert_eq!(
            text,
            "((\"amount\" > ?) OR ((\"amount\" < ?) AND (NOT (\"region\" = ?))))"
        );
    }

    #[test]
    fn null_comparisons_use_is_null() {
        assert_eq!(sql("region eq null").0, "(\"region\" IS NULL)");
        assert_eq!(sql("null ne region").0, "(\"region\" IS NOT NULL)");
    }

    #[test]
    fn functions_and_dates() {
        let (text, params) = sql("contains(tolower(region), 'eu') and sold_on ge 2024-01-31");
        assert_eq!(
            text,
            "(contains(lower(\"region\"), ?) AND (\"sold_on\" >= CAST(? AS DATE)))"
        );
        assert_eq!(
            params,
            vec![Literal::Text("eu".into()), Literal::Text("2024-01-31".into())]
        );
        assert_eq!(
            sql("sold_on lt 2024-01-31T00:00:00Z").0,
            "(\"sold_on\" < CAST(? AS TIMESTAMP))"
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(sql("amount eq -1.5e3").1, vec![Literal::Float(-1500.0)]);
        assert_eq!(sql("id eq -7").1, vec![Literal::Int(-7)]);
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        let fail = |f: &str| parse(f).and_then(|e| to_sql(&e, &set(), &mut Vec::new())).unwrap_err();
        assert_eq!(fail("secret eq 1"), FilterError::UnknownProperty("secret".into()));
        assert_eq!(
            fail("system('ls') eq 1"),
            FilterError::UnsupportedFunction("system".into())
        );
        assert_eq!(fail("contains(region) eq true"), FilterError::Arity("contains", 2));
        assert_eq!(fail("region eq 'open"), FilterError::Syntax(10));
        assert_eq!(fail("id eq 1 2"), FilterError::Syntax(8));
        assert_eq!(fail("id eq 1; select 1"), FilterError::Syntax(7));
        assert_eq!(fail("id add 1 eq 2"), FilterError::Syntax(3));
        assert_eq!(fail(""), FilterError::Syntax(0));
    }

    #[test]
    fn bounded_input() {
        let deep = format!("{}id eq 1{}", "(".repeat(100), ")".repeat(100));
        assert_eq!(parse(&deep).unwrap_err(), FilterError::TooDeep);
        let nots = format!("{}id eq 1", "not ".repeat(100));
        assert_eq!(parse(&nots).unwrap_err(), FilterError::TooDeep);
        assert_eq!(parse(&"a".repeat(9000)).unwrap_err(), FilterError::TooLong);
    }

    #[test]
    fn in_lists_bind_every_value_and_are_bounded() {
        let (text, params) = sql("region in ('EU', 'US') and id in (1)");
        assert_eq!(text, "((\"region\" IN (?, ?)) AND (\"id\" IN (?)))");
        assert_eq!(
            params,
            vec![Literal::Text("EU".into()), Literal::Text("US".into()), Literal::Int(1)]
        );
        let fail = |f: &str| parse(f).and_then(|e| to_sql(&e, &set(), &mut Vec::new())).unwrap_err();
        assert_eq!(fail("region in ()"), FilterError::Syntax(11));
        assert_eq!(fail("region in (region)"), FilterError::Syntax(17));
        let many = format!("id in ({})", vec!["1"; 1_001].join(","));
        assert_eq!(fail(&many), FilterError::InList);
    }
}
