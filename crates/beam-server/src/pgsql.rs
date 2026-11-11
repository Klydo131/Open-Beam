//! Deciding what a PostgreSQL client may run, and presenting results the
//! way PostgreSQL clients expect.
//!
//! A statement is parsed by the engine's own parser into a syntax tree. The
//! tree is then walked and every part of it must be on a short list of
//! things known to be safe: plain `SELECT` queries, the datasets this
//! account can see, a fixed list of functions and operators, and a few
//! catalog tables rebuilt from the account's own datasets. Anything not
//! recognised is refused. The checked tree, not the client's text, is what
//! runs.

use crate::app::AppState;
use crate::catalog;
use crate::postgres::PgError;
use beam_accounts::Account;
use beam_engine::{EngineError, Param};
use serde_json::{json, Value};
use std::collections::HashSet;

/// Reported to clients as the server version. Clients use it to decide
/// which features to rely on; Open Beam answers in PostgreSQL's dialect.
pub const SERVER_VERSION: &str = "16.0";

pub const TEXT_OID: i32 = 25;
const BOOL_OID: i32 = 16;
const INT2_OID: i32 = 21;
const INT4_OID: i32 = 23;
const INT8_OID: i32 = 20;
const FLOAT4_OID: i32 = 700;
const FLOAT8_OID: i32 = 701;
const NUMERIC_OID: i32 = 1700;
const DATE_OID: i32 = 1082;
const TIME_OID: i32 = 1083;
const TIMESTAMP_OID: i32 = 1114;
const TIMESTAMPTZ_OID: i32 = 1184;
const VARCHAR_OID: i32 = 1043;

/// One result column as a PostgreSQL client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub oid: i32,
    /// Size in bytes of the binary form, or -1 when it varies.
    pub size: i16,
}

impl Column {
    pub fn text(name: &str) -> Self {
        Self {
            name: name.to_string(),
            oid: TEXT_OID,
            size: -1,
        }
    }

    /// The PostgreSQL type for an engine type name such as `DECIMAL(18,3)`.
    fn from_engine(name: &str, engine_type: &str) -> Self {
        let upper = engine_type.to_ascii_uppercase();
        let (oid, size) = match upper.as_str() {
            "BOOLEAN" => (BOOL_OID, 1),
            "TINYINT" | "SMALLINT" | "UTINYINT" => (INT2_OID, 2),
            "INTEGER" | "USMALLINT" => (INT4_OID, 4),
            "BIGINT" | "UINTEGER" => (INT8_OID, 8),
            "FLOAT" => (FLOAT4_OID, 4),
            "DOUBLE" => (FLOAT8_OID, 8),
            "DATE" => (DATE_OID, 4),
            "TIME" => (TIME_OID, 8),
            "TIMESTAMP" => (TIMESTAMP_OID, 8),
            "TIMESTAMP WITH TIME ZONE" => (TIMESTAMPTZ_OID, 8),
            "UBIGINT" | "HUGEINT" | "UHUGEINT" => (NUMERIC_OID, -1),
            other if other.starts_with("DECIMAL") => (NUMERIC_OID, -1),
            // Text, and anything else, shown as the engine writes it.
            _ => (TEXT_OID, -1),
        };
        Self {
            name: name.to_string(),
            oid,
            size,
        }
    }
}

/// A statement that passed every check, ready to run.
#[derive(Debug, Clone)]
pub struct Checked {
    pub sql: String,
    pub columns: Vec<Column>,
    pub parameters: usize,
}

/// Session commands clients send while setting up, answered without doing
/// anything. Returns the command tag to report.
pub fn ignored_command(sql: &str) -> Option<&'static str> {
    let lower = sql.to_ascii_lowercase();
    let first = lower.split_whitespace().next()?;
    Some(match first {
        "set" => "SET",
        "reset" => "RESET",
        "begin" => "BEGIN",
        "start" if lower.starts_with("start transaction") => "START TRANSACTION",
        "commit" | "end" => "COMMIT",
        "rollback" | "abort" => "ROLLBACK",
        "discard" => "DISCARD ALL",
        "deallocate" => "DEALLOCATE",
        _ => return None,
    })
}

/// The setting named by `SHOW name`, if that is what `sql` is.
pub fn show_name(sql: &str) -> Option<String> {
    let mut words = sql.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("show") {
        return None;
    }
    let name = words.next()?.trim_matches('"').to_ascii_lowercase();
    words.next().is_none().then_some(name)
}

/// The few settings clients ask about.
pub fn setting(name: &str, username: &str) -> Option<String> {
    Some(
        match name {
            "server_version" => SERVER_VERSION,
            "server_version_num" => "160000",
            "server_encoding" | "client_encoding" => "UTF8",
            "datestyle" => "ISO, MDY",
            "intervalstyle" => "postgres",
            "timezone" => "UTC",
            "standard_conforming_strings" | "integer_datetimes" => "on",
            "transaction_isolation" | "default_transaction_isolation" => "read committed",
            "transaction_read_only" | "default_transaction_read_only" => "on",
            "search_path" => "public",
            "max_identifier_length" => "63",
            "is_superuser" => "off",
            "session_authorization" => return Some(username.to_string()),
            _ => return None,
        }
        .to_string(),
    )
}

/// An engine failure as the client should see it. Engine messages can name
/// files and internals, so only parser messages and mistakes in the query
/// itself (an unknown column, a value of the wrong type) are passed on, and
/// only when they name no file.
pub fn engine_error(error: EngineError) -> PgError {
    match error {
        EngineError::Syntax(message) => PgError::new("42601", message),
        EngineError::TimedOut => PgError::new("57014", crate::feed::TOO_LONG),
        other => {
            let text = other.to_string();
            let first = text.lines().next().unwrap_or_default();
            let about_the_query = [("Binder Error: ", "42703"), ("Conversion Error: ", "22P02")]
                .iter()
                .find(|(prefix, _)| first.starts_with(prefix));
            match about_the_query {
                Some((prefix, code)) if !first.contains(['/', '\\']) => {
                    PgError::new(code, first.trim_start_matches(prefix).to_string())
                }
                _ => {
                    eprintln!("postgres query failed: {text}");
                    PgError::new("22000", "the query could not be run; check its values and column types")
                }
            }
        }
    }
}

/// A parameter sent by the client. Text arrives as text; a client that
/// declared a number or a flag may send it in binary.
pub fn parameter(declared: i32, format: i16, bytes: &[u8]) -> Result<Param, PgError> {
    let bad = || PgError::new("22P03", "a parameter could not be read");
    if format == 0 || matches!(declared, 0 | TEXT_OID | VARCHAR_OID) {
        let text = std::str::from_utf8(bytes).map_err(|_| PgError::new("22021", "parameter is not valid UTF-8"))?;
        return Ok(Param::Text(text.to_string()));
    }
    Ok(match (declared, bytes.len()) {
        (BOOL_OID, 1) => Param::Bool(bytes[0] != 0),
        (INT2_OID, 2) => Param::Int(i16::from_be_bytes([bytes[0], bytes[1]]).into()),
        (INT4_OID, 4) => Param::Int(i32::from_be_bytes(bytes.try_into().map_err(|_| bad())?).into()),
        (INT8_OID, 8) => Param::Int(i64::from_be_bytes(bytes.try_into().map_err(|_| bad())?)),
        (FLOAT4_OID, 4) => Param::Float(f32::from_be_bytes(bytes.try_into().map_err(|_| bad())?).into()),
        (FLOAT8_OID, 8) => Param::Float(f64::from_be_bytes(bytes.try_into().map_err(|_| bad())?)),
        _ => {
            return Err(PgError::new(
                "22P03",
                "send this parameter as text; only numbers and true/false may be sent in binary",
            ))
        }
    })
}

/// A value in PostgreSQL's text form. The engine writes most values the
/// same way PostgreSQL does; flags and special numbers differ.
pub fn text(column: &Column, value: &str) -> String {
    match (column.oid, value) {
        (BOOL_OID, "true") => "t".into(),
        (BOOL_OID, "false") => "f".into(),
        (FLOAT4_OID | FLOAT8_OID, "inf") => "Infinity".into(),
        (FLOAT4_OID | FLOAT8_OID, "-inf") => "-Infinity".into(),
        (FLOAT4_OID | FLOAT8_OID, "nan") => "NaN".into(),
        _ => value.to_string(),
    }
}

/// A value in PostgreSQL's binary form, built from the engine's text.
pub fn binary(column: &Column, value: &str) -> Result<Vec<u8>, PgError> {
    let bad = || {
        PgError::new(
            "22P02",
            format!("a value in column \"{}\" cannot be sent in binary", column.name),
        )
    };
    Ok(match column.oid {
        BOOL_OID => vec![u8::from(value == "true")],
        INT2_OID => value.parse::<i16>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        INT4_OID => value.parse::<i32>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        INT8_OID => value.parse::<i64>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        FLOAT4_OID => float(value)
            .map(|f| (f as f32).to_be_bytes().to_vec())
            .ok_or_else(bad)?,
        FLOAT8_OID => float(value).map(|f| f.to_be_bytes().to_vec()).ok_or_else(bad)?,
        DATE_OID => days_since_2000(value).ok_or_else(bad)?.to_be_bytes().to_vec(),
        TIME_OID => time_micros(value).ok_or_else(bad)?.to_be_bytes().to_vec(),
        TIMESTAMP_OID | TIMESTAMPTZ_OID => timestamp_micros(value).ok_or_else(bad)?.to_be_bytes().to_vec(),
        NUMERIC_OID => numeric(value).ok_or_else(bad)?,
        _ => value.as_bytes().to_vec(),
    })
}

fn float(value: &str) -> Option<f64> {
    match value {
        "inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        "nan" => Some(f64::NAN),
        other => other.parse().ok(),
    }
}

/// Days from 1 January 2000, PostgreSQL's epoch, to a `YYYY-MM-DD` date.
fn days_since_2000(value: &str) -> Option<i32> {
    let mut parts = value.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from 1970-01-01, by Howard Hinnant's civil calendar method.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let unix_days = era * 146_097 + doe - 719_468;
    i32::try_from(unix_days - 10_957).ok()
}

/// Microseconds since midnight for `HH:MM:SS[.ffffff]`.
fn time_micros(value: &str) -> Option<i64> {
    let (clock, fraction) = value.split_once('.').unwrap_or((value, ""));
    let mut parts = clock.splitn(3, ':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let s: i64 = parts.next()?.parse().ok()?;
    if !fraction.bytes().all(|b| b.is_ascii_digit()) || fraction.len() > 6 {
        return None;
    }
    let micros: i64 = format!("{fraction:0<6}").parse().ok()?;
    Some(((h * 60 + m) * 60 + s) * 1_000_000 + micros)
}

/// Microseconds since 2000-01-01 00:00 for `YYYY-MM-DD HH:MM:SS[.ffffff][+00]`.
fn timestamp_micros(value: &str) -> Option<i64> {
    let (date, clock) = value.split_once(' ').unwrap_or((value, "00:00:00"));
    // Times with a zone are always written in UTC here.
    let clock = clock.strip_suffix("+00").unwrap_or(clock);
    let days = i64::from(days_since_2000(date)?);
    Some(days * 86_400_000_000 + time_micros(clock)?)
}

/// PostgreSQL's binary `numeric`: base-10000 digit groups.
fn numeric(value: &str) -> Option<Vec<u8>> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = u16::try_from(fraction.len()).ok()?;
    // Pad both sides to whole groups of four digits around the point.
    let left_pad = (4 - whole.len() % 4) % 4;
    let right_pad = (4 - fraction.len() % 4) % 4;
    let padded = format!("{}{whole}{fraction}{}", "0".repeat(left_pad), "0".repeat(right_pad));
    let mut groups: Vec<i16> = padded
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).ok()?.parse().ok())
        .collect::<Option<_>>()?;
    let mut weight = ((whole.len() + left_pad) / 4) as i16 - 1;
    while groups.first() == Some(&0) && !groups.is_empty() {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend((groups.len() as i16).to_be_bytes());
    out.extend(weight.to_be_bytes());
    out.extend((if negative && !groups.is_empty() { 0x4000u16 } else { 0 }).to_be_bytes());
    out.extend(scale.to_be_bytes());
    for group in groups {
        out.extend(group.to_be_bytes());
    }
    Some(out)
}

/// Functions and operators a query may use. Everything here only computes
/// with the values it is given; nothing reads files, settings or the
/// engine's own catalog.
const ALLOWED_FUNCTIONS: &[&str] = &[
    // Operators, as the parser names them.
    "+",
    "-",
    "*",
    "/",
    "//",
    "%",
    "^",
    "**",
    "||",
    "~~",
    "!~~",
    "~~*",
    "!~~*",
    "&",
    "|",
    "<<",
    ">>",
    "~",
    // Aggregates.
    "count",
    "count_star",
    "sum",
    "avg",
    "mean",
    "min",
    "max",
    "median",
    "mode",
    "stddev",
    "stddev_samp",
    "stddev_pop",
    "variance",
    "var_samp",
    "var_pop",
    "string_agg",
    "listagg",
    "bool_and",
    "bool_or",
    "approx_count_distinct",
    "first",
    "last",
    "any_value",
    "arg_min",
    "arg_max",
    "argmin",
    "argmax",
    "quantile_cont",
    "quantile_disc",
    "corr",
    "covar_pop",
    "covar_samp",
    "product",
    "count_if",
    // Window functions.
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    // Text.
    "lower",
    "upper",
    "lcase",
    "ucase",
    "length",
    "char_length",
    "character_length",
    "strlen",
    "concat",
    "concat_ws",
    "substring",
    "substr",
    "trim",
    "ltrim",
    "rtrim",
    "replace",
    "left",
    "right",
    "strpos",
    "position",
    "instr",
    "starts_with",
    "ends_with",
    "prefix",
    "suffix",
    "contains",
    "lpad",
    "rpad",
    "split_part",
    "reverse",
    "repeat",
    "regexp_matches",
    "regexp_replace",
    "regexp_extract",
    "regexp_full_match",
    "ascii",
    "chr",
    "translate",
    "format",
    "printf",
    "md5",
    // Numbers and comparisons.
    "nullif",
    "ifnull",
    "coalesce",
    "greatest",
    "least",
    "abs",
    "round",
    "round_even",
    "ceil",
    "ceiling",
    "floor",
    "trunc",
    "sqrt",
    "cbrt",
    "power",
    "pow",
    "exp",
    "ln",
    "log",
    "log10",
    "log2",
    "sign",
    "mod",
    "pi",
    "even",
    "gcd",
    "lcm",
    "isnan",
    "isinf",
    "isfinite",
    // Dates and times.
    "date_trunc",
    "datetrunc",
    "date_part",
    "datepart",
    "year",
    "month",
    "day",
    "dayofmonth",
    "dayofweek",
    "dayofyear",
    "isodow",
    "quarter",
    "week",
    "weekofyear",
    "yearweek",
    "hour",
    "minute",
    "second",
    "millisecond",
    "epoch",
    "epoch_ms",
    "date_diff",
    "datediff",
    "date_sub",
    "datesub",
    "date_add",
    "age",
    "make_date",
    "make_time",
    "make_timestamp",
    "to_timestamp",
    "strftime",
    "strptime",
    "try_strptime",
    "last_day",
    "monthname",
    "dayname",
    "now",
    "today",
    "current_date",
    "current_timestamp",
    "get_current_timestamp",
    "time_bucket",
    "to_days",
    "to_weeks",
    "to_months",
    "to_years",
    "to_hours",
    "to_minutes",
    "to_seconds",
    "to_milliseconds",
    "to_microseconds",
    // Literals the parser rewrites into function calls.
    "list_value",
    "typeof",
];

/// Expression kinds a query may contain.
const ALLOWED_EXPRESSIONS: &[&str] = &[
    "CONSTANT",
    "COLUMN_REF",
    "COMPARISON",
    "CONJUNCTION",
    "OPERATOR",
    "CASE",
    "CAST",
    "BETWEEN",
    "FUNCTION",
    "WINDOW",
    "SUBQUERY",
    "STAR",
    "PARAMETER",
    "TYPE",
    "COLLATE",
    "POSITIONAL_REFERENCE",
];

/// Query shapes a statement may contain.
const ALLOWED_NODES: &[&str] = &["SELECT_NODE", "SET_OPERATION_NODE", "RECURSIVE_CTE_NODE"];

/// Every kind of table reference the parser produces. Only some are allowed,
/// but all are recognised so none can pass unexamined.
const TABLE_REFS: &[&str] = &[
    "BASE_TABLE",
    "JOIN",
    "SUBQUERY",
    "EMPTY",
    "EXPRESSION_LIST",
    "TABLE_FUNCTION",
    "PIVOT",
    "SHOW_REF",
    "COLUMN_DATA",
    "DELIM_GET",
    "BOUND_TABLE_REF",
    "CTE",
];

/// Functions answered as PostgreSQL would, without asking the engine.
fn substitute(name: &str) -> Option<String> {
    match name {
        "version" => Some(format!(
            "PostgreSQL {SERVER_VERSION} (Open Beam {}, read-only)",
            env!("CARGO_PKG_VERSION")
        )),
        "current_database" => Some("beam".into()),
        "current_schema" => Some("public".into()),
        _ => None,
    }
}

fn refused(what: impl std::fmt::Display) -> PgError {
    PgError::new("42501", format!("{what} is not available through Open Beam"))
}

fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

struct Checker<'a> {
    state: &'a AppState,
    /// Datasets this account may read: lower-case name to real name.
    visible: std::collections::HashMap<String, String>,
    /// Every dataset name in the engine, visible or not, lower case.
    all_names: HashSet<String>,
    /// Names defined by `WITH` anywhere in the statement, lower case.
    ctes: HashSet<String>,
    /// SQL for each catalog table, built from the visible datasets.
    catalog: std::collections::HashMap<&'static str, String>,
}

/// Parses, checks and prepares one statement for `account`.
pub fn check(state: &AppState, account: &Account, sql: &str) -> Result<Checked, PgError> {
    let mut tree = state.engine.parse_select(sql).map_err(|e| match e {
        // The engine's parser only accepts SELECT, so any other statement lands here.
        EngineError::Syntax(message) if message.contains("Only SELECT") => {
            PgError::new("25006", "Open Beam is read-only: only SELECT statements can run")
        }
        other => engine_error(other),
    })?;
    let statements = tree["statements"].as_array().map_or(0, Vec::len);
    if statements != 1 {
        return Err(PgError::new("0A000", "send one statement at a time"));
    }

    let parameters = parameter_count(&tree["statements"][0]["named_param_map"])?;
    let entries = catalog::visible(state, account);
    let mut checker = Checker {
        state,
        visible: entries
            .iter()
            .map(|e| (e.set.name.to_ascii_lowercase(), e.set.name.clone()))
            .collect(),
        all_names: state
            .engine
            .datasets()
            .iter()
            .map(|d| d.name.to_ascii_lowercase())
            .collect(),
        ctes: HashSet::new(),
        catalog: catalog_tables(&entries),
    };
    checker.collect_ctes(&tree)?;
    checker.walk(&mut tree["statements"][0]["node"])?;

    let sql = state.engine.render_select(&tree).map_err(engine_error)?;
    // Describing needs no parameter values, so NULL stands in for each.
    let described = if parameters == 0 {
        state.engine.describe_select(&sql)
    } else {
        let null = state.engine.parse_select("SELECT NULL").map_err(engine_error)?;
        let mut stand_in = tree.clone();
        replace_parameters(&mut stand_in, &null["statements"][0]["node"]["select_list"][0]);
        let stand_in = state.engine.render_select(&stand_in).map_err(engine_error)?;
        state.engine.describe_select(&stand_in)
    }
    .map_err(engine_error)?;
    let columns = described
        .iter()
        .map(|(name, engine_type)| Column::from_engine(name, engine_type))
        .collect();
    Ok(Checked {
        sql,
        columns,
        parameters,
    })
}

/// `$1`, `$2`, ... are allowed; named parameters are not.
fn parameter_count(map: &Value) -> Result<usize, PgError> {
    let mut count = 0;
    for entry in map.as_array().into_iter().flatten() {
        let number: usize = entry["key"]
            .as_str()
            .and_then(|k| k.parse().ok())
            .filter(|n| (1..=1000).contains(n))
            .ok_or_else(|| PgError::new("42P02", "use numbered parameters such as $1"))?;
        count = count.max(number);
    }
    Ok(count)
}

/// Puts `null` in place of every parameter.
fn replace_parameters(value: &mut Value, null: &Value) {
    match value {
        Value::Object(map) if map.get("class").and_then(Value::as_str) == Some("PARAMETER") => {
            *value = null.clone();
        }
        Value::Object(map) => map.values_mut().for_each(|v| replace_parameters(v, null)),
        Value::Array(items) => items.iter_mut().for_each(|v| replace_parameters(v, null)),
        _ => {}
    }
}

impl Checker<'_> {
    /// Records every `WITH` name. A name that is also a dataset or catalog
    /// table is refused, so a reference can never mean one thing to this
    /// check and another to the engine.
    fn collect_ctes(&mut self, value: &Value) -> Result<(), PgError> {
        match value {
            Value::Object(map) => {
                if let Some(entries) = map.get("cte_map").and_then(|c| c["map"].as_array()) {
                    for entry in entries {
                        let name = entry["key"].as_str().unwrap_or_default();
                        let lower = name.to_ascii_lowercase();
                        if !is_plain_name(name)
                            || self.all_names.contains(&lower)
                            || self.catalog.contains_key(lower.as_str())
                        {
                            return Err(PgError::new(
                                "42P07",
                                format!("choose another name for \"{name}\": it is already a table name"),
                            ));
                        }
                        self.ctes.insert(lower);
                    }
                }
                map.values().try_for_each(|v| self.collect_ctes(v))
            }
            Value::Array(items) => items.iter().try_for_each(|v| self.collect_ctes(v)),
            _ => Ok(()),
        }
    }

    /// Walks every part of the tree. Unknown kinds are refused.
    fn walk(&self, value: &mut Value) -> Result<(), PgError> {
        match value {
            Value::Array(items) => items.iter_mut().try_for_each(|v| self.walk(v)),
            Value::Object(map) => {
                if let Some(class) = map.get("class").and_then(Value::as_str).map(str::to_string) {
                    if let Some(replacement) = self.expression(&class, map)? {
                        *value = replacement;
                        return Ok(());
                    }
                } else if let Some(kind) = map.get("type").and_then(Value::as_str).map(str::to_string) {
                    if TABLE_REFS.contains(&kind.as_str()) {
                        if let Some(replacement) = self.table(&kind, map)? {
                            *value = replacement;
                            return Ok(());
                        }
                    } else if kind.ends_with("_NODE")
                        && !kind.ends_with("_MODIFIER")
                        && !ALLOWED_NODES.contains(&kind.as_str())
                    {
                        return Err(refused(format!(
                            "a {} query",
                            kind.trim_end_matches("_NODE").to_lowercase()
                        )));
                    }
                }
                map.values_mut().try_for_each(|v| self.walk(v))
            }
            _ => Ok(()),
        }
    }

    /// Checks one expression; returns a replacement for substituted functions.
    fn expression(&self, class: &str, map: &serde_json::Map<String, Value>) -> Result<Option<Value>, PgError> {
        if !ALLOWED_EXPRESSIONS.contains(&class) {
            return Err(refused(format!("{} expressions", class.to_lowercase())));
        }
        match class {
            "FUNCTION" | "WINDOW" => {
                let name = map
                    .get("function_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let schema = map.get("schema").and_then(Value::as_str).unwrap_or_default();
                let catalog = map.get("catalog").and_then(Value::as_str).unwrap_or_default();
                let path: Vec<&str> = map
                    .get("qualified_name")
                    .and_then(|q| q["path"].as_array())
                    .map(|p| p.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let plain_path = path.is_empty() || path.last().is_some_and(|last| last.eq_ignore_ascii_case(&name));
                let plain_schema = matches!(schema, "" | "main" | "pg_catalog") && catalog.is_empty();
                if !plain_path || !plain_schema || path.len() > 2 {
                    return Err(refused(format!("the function {name}")));
                }
                if let Some(text) = substitute(&name) {
                    let alias = map
                        .get("alias")
                        .and_then(Value::as_str)
                        .filter(|a| !a.is_empty())
                        .unwrap_or(&name);
                    let constant = self.constant(&text, alias)?;
                    return Ok(Some(constant));
                }
                if !ALLOWED_FUNCTIONS.contains(&name.as_str()) {
                    return Err(refused(format!("the function {name}()")));
                }
            }
            "COLUMN_REF" => {
                let names = map.get("column_names").and_then(Value::as_array);
                if names.is_some_and(|n| n.len() > 3) {
                    return Err(refused("that column name"));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// A text constant as the parser would build it, with a column name.
    fn constant(&self, text: &str, alias: &str) -> Result<Value, PgError> {
        let tree = self
            .state
            .engine
            .parse_select(&format!("SELECT {} AS \"{}\"", quote(text), alias.replace('"', "\"\"")))
            .map_err(engine_error)?;
        Ok(tree["statements"][0]["node"]["select_list"][0].clone())
    }

    /// Checks one table reference; returns a replacement for catalog tables
    /// and for datasets, which are always named exactly.
    fn table(&self, kind: &str, map: &serde_json::Map<String, Value>) -> Result<Option<Value>, PgError> {
        match kind {
            "JOIN" | "SUBQUERY" | "EMPTY" | "EXPRESSION_LIST" => Ok(None),
            "BASE_TABLE" => {
                let text = |key: &str| map.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                let (catalog, schema, name) = (
                    text("catalog_name"),
                    text("schema_name").to_ascii_lowercase(),
                    text("table_name"),
                );
                let path: Vec<String> = map
                    .get("qualified_name")
                    .and_then(|q| q["path"].as_array())
                    .map(|p| {
                        p.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_ascii_lowercase)
                            .collect()
                    })
                    .unwrap_or_default();
                let expected: Vec<String> = if schema.is_empty() {
                    vec![name.to_ascii_lowercase()]
                } else {
                    vec![schema.clone(), name.to_ascii_lowercase()]
                };
                if !catalog.is_empty() || path != expected || !map.get("at_clause").is_none_or(Value::is_null) {
                    return Err(refused(format!("the table {name}")));
                }
                let lower = name.to_ascii_lowercase();
                let alias = map
                    .get("alias")
                    .and_then(Value::as_str)
                    .filter(|a| !a.is_empty())
                    .unwrap_or(&name)
                    .to_string();
                match schema.as_str() {
                    "" | "public" | "main" => {
                        if schema.is_empty() && self.ctes.contains(&lower) {
                            return Ok(None);
                        }
                        if let Some(real) = self.visible.get(&lower) {
                            let mut exact = map.clone();
                            exact.insert("catalog_name".into(), json!(""));
                            exact.insert("schema_name".into(), json!("main"));
                            exact.insert("table_name".into(), json!(real));
                            exact.insert("qualified_name".into(), json!({ "path": ["main", real] }));
                            exact.insert("alias".into(), json!(alias));
                            return Ok(Some(Value::Object(exact)));
                        }
                        if schema.is_empty() {
                            if let Some(sql) = self.catalog.get(lower.as_str()) {
                                return self.catalog_table(sql, &alias, map).map(Some);
                            }
                        }
                        // Unknown and not allowed look the same, as in PostgreSQL.
                        Err(PgError::new("42P01", format!("relation \"{name}\" does not exist")))
                    }
                    "information_schema" | "pg_catalog" => {
                        let key = if schema == "pg_catalog" {
                            lower.clone()
                        } else {
                            format!("information_schema.{lower}")
                        };
                        match self.catalog.get(key.as_str()) {
                            Some(sql) => self.catalog_table(sql, &alias, map).map(Some),
                            None => Err(refused(format!("the catalog table {schema}.{name}"))),
                        }
                    }
                    _ => Err(PgError::new(
                        "42P01",
                        format!("relation \"{schema}.{name}\" does not exist"),
                    )),
                }
            }
            other => Err(refused(format!("{} in FROM", other.to_lowercase().replace('_', " ")))),
        }
    }

    /// A catalog table rebuilt from this account's datasets, as a subquery.
    fn catalog_table(
        &self,
        sql: &str,
        alias: &str,
        original: &serde_json::Map<String, Value>,
    ) -> Result<Value, PgError> {
        let tree = self
            .state
            .engine
            .parse_select(&format!("SELECT * FROM ({sql}) AS \"{}\"", alias.replace('"', "\"\"")))
            .map_err(engine_error)?;
        let mut table = tree["statements"][0]["node"]["from_table"].clone();
        if let Some(columns) = original.get("column_name_alias") {
            table["column_name_alias"] = columns.clone();
        }
        Ok(table)
    }
}

/// The catalog tables, each as a `SELECT` over constants describing only
/// the datasets this account can see.
fn catalog_tables(entries: &[catalog::Entry]) -> std::collections::HashMap<&'static str, String> {
    let rows = |columns: &str, types: &[&str], values: Vec<Vec<String>>| {
        let names: Vec<&str> = columns.split(',').collect();
        let select: Vec<String> = names
            .iter()
            .zip(types)
            .enumerate()
            .map(|(i, (name, ty))| format!("CAST(col{} AS {ty}) AS {name}", i))
            .collect();
        let body = if values.is_empty() {
            format!(
                "SELECT {} WHERE false",
                names
                    .iter()
                    .zip(types)
                    .map(|(n, t)| format!("CAST(NULL AS {t}) AS {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            let list: Vec<String> = values.iter().map(|row| format!("({})", row.join(", "))).collect();
            let aliases: Vec<String> = (0..names.len()).map(|i| format!("col{i}")).collect();
            format!(
                "SELECT {} FROM (VALUES {}) AS v({})",
                select.join(", "),
                list.join(", "),
                aliases.join(", ")
            )
        };
        body
    };
    let tables: Vec<Vec<String>> = entries
        .iter()
        .map(|e| vec![quote("beam"), quote("public"), quote(&e.set.name), quote("BASE TABLE")])
        .collect();
    let mut columns = Vec::new();
    for entry in entries {
        for (index, property) in entry.set.properties.iter().enumerate() {
            let data_type = match property.edm {
                beam_odata::EdmType::Boolean => "boolean",
                beam_odata::EdmType::Int32 => "integer",
                beam_odata::EdmType::Int64 => "bigint",
                beam_odata::EdmType::Double => "double precision",
                beam_odata::EdmType::Decimal { .. } => "numeric",
                beam_odata::EdmType::String => "text",
                beam_odata::EdmType::Date => "date",
                beam_odata::EdmType::DateTimeOffset => "timestamp without time zone",
            };
            columns.push(vec![
                quote("beam"),
                quote("public"),
                quote(&entry.set.name),
                quote(&property.name),
                (index + 1).to_string(),
                quote(data_type),
                quote("YES"),
            ]);
        }
    }
    let pg_tables: Vec<Vec<String>> = entries
        .iter()
        .map(|e| vec![quote("public"), quote(&e.set.name), quote("beam")])
        .collect();
    let mut out = std::collections::HashMap::new();
    out.insert(
        "information_schema.schemata",
        rows(
            "catalog_name,schema_name,schema_owner",
            &["VARCHAR", "VARCHAR", "VARCHAR"],
            vec![vec![quote("beam"), quote("public"), quote("beam")]],
        ),
    );
    out.insert(
        "information_schema.tables",
        rows(
            "table_catalog,table_schema,table_name,table_type",
            &["VARCHAR"; 4],
            tables,
        ),
    );
    out.insert(
        "information_schema.columns",
        rows(
            "table_catalog,table_schema,table_name,column_name,ordinal_position,data_type,is_nullable",
            &[
                "VARCHAR", "VARCHAR", "VARCHAR", "VARCHAR", "INTEGER", "VARCHAR", "VARCHAR",
            ],
            columns,
        ),
    );
    out.insert(
        "pg_namespace",
        rows(
            "oid,nspname",
            &["INTEGER", "VARCHAR"],
            vec![vec!["2200".into(), quote("public")]],
        ),
    );
    out.insert(
        "pg_tables",
        rows("schemaname,tablename,tableowner", &["VARCHAR"; 3], pg_tables),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_parts(text: &str) -> (i16, i16, u16, u16, Vec<i16>) {
        let bytes = numeric(text).unwrap();
        let word = |i: usize| i16::from_be_bytes([bytes[i], bytes[i + 1]]);
        let count = word(0) as usize;
        (
            word(0),
            word(2),
            word(4) as u16,
            word(6) as u16,
            (0..count).map(|n| word(8 + n * 2)).collect(),
        )
    }

    #[test]
    fn numbers_use_postgresql_base_10000_groups() {
        // (digit count, weight, sign, scale, digits), as PostgreSQL sends them.
        assert_eq!(numeric_parts("2.50"), (2, 0, 0, 2, vec![2, 5000]));
        assert_eq!(numeric_parts("12345.678"), (3, 1, 0, 3, vec![1, 2345, 6780]));
        assert_eq!(numeric_parts("0.001"), (1, -1, 0, 3, vec![10]));
        assert_eq!(numeric_parts("-5"), (1, 0, 0x4000, 0, vec![5]));
        assert_eq!(numeric_parts("0"), (0, 0, 0, 0, vec![]));
        assert_eq!(numeric_parts("10000"), (1, 1, 0, 0, vec![1]));
        assert!(numeric("1e5").is_none());
        assert!(numeric("").is_none());
    }

    #[test]
    fn dates_and_times_count_from_the_year_2000() {
        assert_eq!(days_since_2000("2000-01-01"), Some(0));
        assert_eq!(days_since_2000("1970-01-01"), Some(-10_957));
        assert_eq!(days_since_2000("2024-01-05"), Some(8_770));
        assert_eq!(days_since_2000("2024-13-05"), None);
        assert_eq!(timestamp_micros("2000-01-01 00:00:01"), Some(1_000_000));
        assert_eq!(timestamp_micros("2000-01-02 00:00:00.5"), Some(86_400_500_000));
        assert_eq!(timestamp_micros("2000-01-01 00:00:00+00"), Some(0));
        assert_eq!(time_micros("01:02:03.000004"), Some(3_723_000_004));
    }

    #[test]
    fn setup_commands_are_recognised_without_running_anything() {
        assert_eq!(ignored_command("SET DateStyle = ISO"), Some("SET"));
        assert_eq!(ignored_command("begin read only"), Some("BEGIN"));
        assert_eq!(ignored_command("SELECT 1"), None);
        assert_eq!(show_name("SHOW search_path"), Some("search_path".into()));
        assert_eq!(show_name("SHOW search_path, x"), None);
        assert_eq!(text(&Column::from_engine("b", "BOOLEAN"), "true"), "t");
        assert_eq!(
            binary(&Column::from_engine("n", "BIGINT"), "7").unwrap(),
            7i64.to_be_bytes()
        );
    }
}
