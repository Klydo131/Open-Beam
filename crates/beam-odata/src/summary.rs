//! Grouped summaries, used to draw charts.
//!
//! This is not part of OData. It lives here because it shares the schema
//! model, the `$filter` parser and the same rule as every other planner in
//! this crate: names are checked against the entity set before they are
//! quoted, and client values are only ever bound as parameters.

use crate::filter;
use crate::query::QueryError;
use crate::{quote_ident, EdmType, EntitySet, Literal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// Number of rows in each group. Needs no measure.
    Count,
    Sum,
    Average,
    Min,
    Max,
}

/// How date and timestamp columns are grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Day,
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Returns `(label TEXT, value DOUBLE)` rows, one more than the limit so
    /// the caller can tell whether groups were left out.
    pub sql: String,
    pub params: Vec<Literal>,
    /// `true` when groups have a natural order (numbers and dates) and are
    /// returned in it; otherwise they are returned largest value first.
    pub ordered: bool,
}

pub fn plan_summary(
    set: &EntitySet,
    group_by: &str,
    measure: Option<&str>,
    aggregate: Aggregate,
    bucket: Bucket,
    filter_text: Option<&str>,
    limit: u64,
) -> Result<Summary, QueryError> {
    let unknown = |name: &str| QueryError::UnknownProperty(name.to_string());
    let group = set.property(group_by).ok_or_else(|| unknown(group_by))?;
    let group_ident = quote_ident(&group.name);

    let value = value_sql(set, measure, aggregate)?;

    let label = match (group.edm, bucket) {
        (EdmType::String, _) => group_ident.clone(),
        (EdmType::Date | EdmType::DateTimeOffset, Bucket::Day) => format!("strftime({group_ident}, '%Y-%m-%d')"),
        (EdmType::Date | EdmType::DateTimeOffset, Bucket::Month) => format!("strftime({group_ident}, '%Y-%m')"),
        (EdmType::Date | EdmType::DateTimeOffset, Bucket::Year) => format!("strftime({group_ident}, '%Y')"),
        (EdmType::Date | EdmType::DateTimeOffset, Bucket::Quarter) => {
            format!("strftime({group_ident}, '%Y') || '-Q' || quarter({group_ident})")
        }
        _ => format!("CAST({group_ident} AS VARCHAR)"),
    };
    let ordered = !matches!(group.edm, EdmType::String | EdmType::Boolean);
    let order = if ordered {
        format!("min({group_ident}) ASC NULLS LAST")
    } else {
        "2 DESC NULLS LAST, 1 ASC".to_string()
    };

    let mut params = Vec::new();
    let where_sql = where_sql(set, filter_text, &mut params)?;
    let sql = format!(
        "SELECT coalesce({label}, '(empty)') AS \"label\", CAST({value} AS DOUBLE) AS \"value\" \
         FROM {}{where_sql} GROUP BY 1 ORDER BY {order} LIMIT {}",
        quote_ident(&set.name),
        limit.saturating_add(1),
    );
    Ok(Summary { sql, params, ordered })
}

/// The aggregate expression, after checking the measure is a number column.
fn value_sql(set: &EntitySet, measure: Option<&str>, aggregate: Aggregate) -> Result<String, QueryError> {
    let unknown = |name: &str| QueryError::UnknownProperty(name.to_string());
    Ok(match (aggregate, measure) {
        (Aggregate::Count, _) => "count(*)".to_string(),
        (_, None) => return Err(QueryError::InvalidValue("measure")),
        (_, Some(name)) => {
            let property = set.property(name).ok_or_else(|| unknown(name))?;
            let numeric = matches!(
                property.edm,
                EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
            );
            if !numeric {
                return Err(QueryError::InvalidValue("measure"));
            }
            let function = match aggregate {
                Aggregate::Sum => "sum",
                Aggregate::Average => "avg",
                Aggregate::Min => "min",
                _ => "max",
            };
            format!("{function}({})", quote_ident(&property.name))
        }
    })
}

fn where_sql(set: &EntitySet, filter_text: Option<&str>, params: &mut Vec<Literal>) -> Result<String, QueryError> {
    Ok(match filter_text {
        Some(text) => format!(" WHERE {}", filter::to_sql(&filter::parse(text)?, set, params)?),
        None => String::new(),
    })
}

/// One number for the whole dataset (or the filtered part of it): returns a
/// single `(value DOUBLE)` row.
pub fn plan_total(
    set: &EntitySet,
    measure: Option<&str>,
    aggregate: Aggregate,
    filter_text: Option<&str>,
) -> Result<Summary, QueryError> {
    let value = value_sql(set, measure, aggregate)?;
    let mut params = Vec::new();
    let where_sql = where_sql(set, filter_text, &mut params)?;
    let sql = format!(
        "SELECT CAST({value} AS DOUBLE) AS \"value\" FROM {}{where_sql}",
        quote_ident(&set.name)
    );
    Ok(Summary {
        sql,
        params,
        ordered: false,
    })
}

/// Number columns profiled at most, to bound the cost of one profile.
pub const MAX_PROFILE_COLUMNS: usize = 12;

/// Figures returned per profiled column, in order: count, sum (the control
/// total), minimum, lower quartile, median, upper quartile, maximum, and how
/// many values lie beyond Tukey's fences (1.5 interquartile ranges outside
/// the quartiles).
pub const PROFILE_WIDTH: usize = 8;

/// Control totals and the five-number summary of number columns: returns one
/// row with the number of rows, then [`PROFILE_WIDTH`] figures per column.
pub fn plan_profile(set: &EntitySet, columns: &[&str]) -> Result<Summary, QueryError> {
    if columns.is_empty() || columns.len() > MAX_PROFILE_COLUMNS {
        return Err(QueryError::InvalidValue("columns"));
    }
    let mut idents = Vec::with_capacity(columns.len());
    for name in columns {
        let property = set
            .property(name)
            .ok_or_else(|| QueryError::UnknownProperty(name.to_string()))?;
        if !matches!(
            property.edm,
            EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
        ) {
            return Err(QueryError::InvalidValue("columns"));
        }
        idents.push(quote_ident(&property.name));
    }
    let quartiles = idents
        .iter()
        .enumerate()
        .map(|(i, c)| format!("quantile_cont({c}, 0.25) AS q1_{i}, quantile_cont({c}, 0.75) AS q3_{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let figures = idents
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let fence = format!("1.5 * (q.q3_{i} - q.q1_{i})");
            format!(
                "CAST(count({c}) AS DOUBLE), CAST(sum({c}) AS DOUBLE), CAST(min({c}) AS DOUBLE), \
                 CAST(any_value(q.q1_{i}) AS DOUBLE), CAST(quantile_cont({c}, 0.5) AS DOUBLE), \
                 CAST(any_value(q.q3_{i}) AS DOUBLE), CAST(max({c}) AS DOUBLE), \
                 CAST(count(*) FILTER (WHERE {c} < q.q1_{i} - {fence} OR {c} > q.q3_{i} + {fence}) AS DOUBLE)"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let table = quote_ident(&set.name);
    Ok(Summary {
        sql: format!(
            "WITH q AS (SELECT {quartiles} FROM {table}) \
             SELECT CAST(count(*) AS DOUBLE), {figures} FROM {table}, q"
        ),
        params: Vec::new(),
        ordered: false,
    })
}

/// Whether `key` identifies each row of `set`: returns one row of
/// `(rows, distinct, empty)`. The key holds when `empty` is 0 and
/// `distinct` equals `rows`.
pub fn plan_key_check(set: &EntitySet, key: &str) -> Result<Summary, QueryError> {
    let key = set
        .property(key)
        .ok_or_else(|| QueryError::UnknownProperty(key.to_string()))?;
    let k = quote_ident(&key.name);
    Ok(Summary {
        sql: format!(
            "SELECT CAST(count(*) AS DOUBLE), CAST(count(DISTINCT {k}) AS DOUBLE), \
             CAST(count(*) FILTER (WHERE {k} IS NULL) AS DOUBLE) FROM {}",
            quote_ident(&set.name)
        ),
        params: Vec::new(),
        ordered: false,
    })
}

/// Rows of `from` whose `column` names no row of `to` by its `key`: returns
/// one row of `(linked, orphans)`, counting only rows where `column` is set.
pub fn plan_link_check(from: &EntitySet, column: &str, to: &EntitySet, key: &str) -> Result<Summary, QueryError> {
    let unknown = |name: &str| QueryError::UnknownProperty(name.to_string());
    let column = from.property(column).ok_or_else(|| unknown(column))?;
    let key = to.property(key).ok_or_else(|| unknown(key))?;
    let (c, k) = (quote_ident(&column.name), quote_ident(&key.name));
    Ok(Summary {
        sql: format!(
            "SELECT CAST(count(*) FILTER (WHERE t.{k} IS NOT NULL) AS DOUBLE), \
             CAST(count(*) FILTER (WHERE t.{k} IS NULL) AS DOUBLE) \
             FROM (SELECT {c} AS v FROM {} WHERE {c} IS NOT NULL) f \
             LEFT JOIN (SELECT DISTINCT {k} FROM {}) t ON f.v = t.{k}",
            quote_ident(&from.name),
            quote_ident(&to.name)
        ),
        params: Vec::new(),
        ordered: false,
    })
}

/// What a validity rule asks of a column: a closed set, after SHACL Core's
/// minCount, minInclusive/maxInclusive, in and uniqueness.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleCheck {
    /// Never empty.
    Present,
    /// Numbers no lower than `min` and no higher than `max`: whole numbers
    /// as [`Literal::Int`], so large ones compare exactly.
    Between { min: Option<Literal>, max: Option<Literal> },
    /// Text, when given, is one of these.
    OneOf(Vec<String>),
    /// No value appears twice.
    Unique,
}

/// Rows of `set` that break `check` on `column`: returns one row of
/// `(rows, breaking)`. Values from the rule are bound, never written in.
pub fn plan_rule(set: &EntitySet, column: &str, check: &RuleCheck) -> Result<Summary, QueryError> {
    let property = set
        .property(column)
        .ok_or_else(|| QueryError::UnknownProperty(column.to_string()))?;
    let c = quote_ident(&property.name);
    let numeric = matches!(
        property.edm,
        EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
    );
    let mut params = Vec::new();
    let breaking = match check {
        RuleCheck::Present => format!("count(*) FILTER (WHERE {c} IS NULL)"),
        RuleCheck::Unique => format!("count({c}) - count(DISTINCT {c})"),
        RuleCheck::Between { min, max } => {
            if !numeric || (min.is_none() && max.is_none()) {
                return Err(QueryError::InvalidValue("between"));
            }
            let mut sides = Vec::new();
            for (bound, side) in [(min, "<"), (max, ">")] {
                match bound {
                    None => {}
                    Some(value @ (Literal::Int(_) | Literal::Float(_))) => {
                        params.push(value.clone());
                        sides.push(format!("{c} {side} ?"));
                    }
                    Some(_) => return Err(QueryError::InvalidValue("between")),
                }
            }
            format!("count(*) FILTER (WHERE {})", sides.join(" OR "))
        }
        RuleCheck::OneOf(values) => {
            if property.edm != EdmType::String || values.is_empty() {
                return Err(QueryError::InvalidValue("one_of"));
            }
            params.extend(values.iter().map(|v| Literal::Text(v.clone())));
            let marks = vec!["?"; values.len()].join(", ");
            format!("count(*) FILTER (WHERE {c} IS NOT NULL AND {c} NOT IN ({marks}))")
        }
    };
    Ok(Summary {
        sql: format!(
            "SELECT CAST(count(*) AS DOUBLE), CAST({breaking} AS DOUBLE) FROM {}",
            quote_ident(&set.name)
        ),
        params,
        ordered: false,
    })
}

/// How often records repeat: returns one row of `(groups, extra)` where
/// `groups` counts combinations of `keys` that appear more than once, and
/// `extra` counts the rows beyond the first of each. Rows where every key is
/// empty are left out, because they identify nothing.
pub fn plan_repeats(set: &EntitySet, keys: &[&str], filter_text: Option<&str>) -> Result<Summary, QueryError> {
    if keys.is_empty() {
        return Err(QueryError::InvalidValue("keys"));
    }
    let mut idents = Vec::with_capacity(keys.len());
    for key in keys {
        let property = set
            .property(key)
            .ok_or_else(|| QueryError::UnknownProperty(key.to_string()))?;
        idents.push(quote_ident(&property.name));
    }
    let mut params = Vec::new();
    let filter = where_sql(set, filter_text, &mut params)?;
    let not_empty = format!(
        "NOT ({})",
        idents
            .iter()
            .map(|i| format!("{i} IS NULL"))
            .collect::<Vec<_>>()
            .join(" AND ")
    );
    let where_all = if filter.is_empty() {
        format!(" WHERE {not_empty}")
    } else {
        format!("{filter} AND {not_empty}")
    };
    let sql = format!(
        "SELECT CAST(count(*) AS DOUBLE) AS \"groups\", CAST(coalesce(sum(n - 1), 0) AS DOUBLE) AS \"extra\" \
         FROM (SELECT count(*) AS n FROM {}{where_all} GROUP BY {} HAVING count(*) > 1)",
        quote_ident(&set.name),
        idents.join(", ")
    );
    Ok(Summary {
        sql,
        params,
        ordered: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Property;

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
                p("units", EdmType::Int32),
                p("sold_on", EdmType::Date),
            ],
        }
    }

    #[test]
    fn counts_rows_by_a_text_column_largest_first() {
        let summary = plan_summary(&set(), "region", None, Aggregate::Count, Bucket::Day, None, 20).unwrap();
        assert_eq!(
            summary.sql,
            "SELECT coalesce(\"region\", '(empty)') AS \"label\", CAST(count(*) AS DOUBLE) AS \"value\" \
             FROM \"sales\" GROUP BY 1 ORDER BY 2 DESC NULLS LAST, 1 ASC LIMIT 21"
        );
        assert!(!summary.ordered);
    }

    #[test]
    fn sums_a_measure_by_month_in_date_order_with_a_filter() {
        let summary = plan_summary(
            &set(),
            "sold_on",
            Some("units"),
            Aggregate::Sum,
            Bucket::Month,
            Some("region eq 'x''; DROP VIEW sales; --'"),
            100,
        )
        .unwrap();
        assert_eq!(
            summary.sql,
            "SELECT coalesce(strftime(\"sold_on\", '%Y-%m'), '(empty)') AS \"label\", \
             CAST(sum(\"units\") AS DOUBLE) AS \"value\" FROM \"sales\" WHERE (\"region\" = ?) \
             GROUP BY 1 ORDER BY min(\"sold_on\") ASC NULLS LAST LIMIT 101"
        );
        assert_eq!(summary.params, vec![Literal::Text("x'; DROP VIEW sales; --".into())]);
        assert!(summary.ordered);
    }

    #[test]
    fn other_buckets_and_number_groups() {
        let sql = |group, bucket| {
            plan_summary(&set(), group, None, Aggregate::Count, bucket, None, 5)
                .unwrap()
                .sql
        };
        assert!(sql("sold_on", Bucket::Quarter).contains("strftime(\"sold_on\", '%Y') || '-Q' || quarter(\"sold_on\")"));
        assert!(sql("sold_on", Bucket::Year).contains("strftime(\"sold_on\", '%Y')"));
        assert!(sql("units", Bucket::Year).contains("coalesce(CAST(\"units\" AS VARCHAR), '(empty)')"));
    }

    #[test]
    fn rejects_unknown_names_and_measures_that_are_not_numbers() {
        let plan = |group, measure, aggregate| {
            plan_summary(&set(), group, measure, aggregate, Bucket::Day, None, 5).unwrap_err()
        };
        assert_eq!(
            plan("nope", None, Aggregate::Count),
            QueryError::UnknownProperty("nope".into())
        );
        assert_eq!(
            plan("region", Some("nope"), Aggregate::Sum),
            QueryError::UnknownProperty("nope".into())
        );
        assert_eq!(
            plan("region", Some("region"), Aggregate::Sum),
            QueryError::InvalidValue("measure")
        );
        assert_eq!(
            plan("region", Some("sold_on"), Aggregate::Average),
            QueryError::InvalidValue("measure")
        );
        assert_eq!(
            plan("region", None, Aggregate::Max),
            QueryError::InvalidValue("measure")
        );
        assert_eq!(
            plan("\"; DROP VIEW sales; --", None, Aggregate::Count),
            QueryError::UnknownProperty("\"; DROP VIEW sales; --".into())
        );
        let bad_filter = plan_summary(
            &set(),
            "region",
            None,
            Aggregate::Count,
            Bucket::Day,
            Some("secret eq 1"),
            5,
        );
        assert!(matches!(bad_filter, Err(QueryError::Filter(_))));
    }

    #[test]
    fn totals_and_repeats_check_names_and_bind_values() {
        let total = plan_total(&set(), Some("units"), Aggregate::Sum, Some("region eq 'EU'")).unwrap();
        assert_eq!(
            total.sql,
            "SELECT CAST(sum(\"units\") AS DOUBLE) AS \"value\" FROM \"sales\" WHERE (\"region\" = ?)"
        );
        assert_eq!(total.params, vec![Literal::Text("EU".into())]);
        assert!(plan_total(&set(), Some("region"), Aggregate::Sum, None).is_err());
        assert!(plan_total(&set(), Some("nope"), Aggregate::Sum, None).is_err());

        let repeats = plan_repeats(&set(), &["region", "sold_on"], None).unwrap();
        assert!(repeats
            .sql
            .contains("WHERE NOT (\"region\" IS NULL AND \"sold_on\" IS NULL)"));
        assert!(repeats
            .sql
            .contains("GROUP BY \"region\", \"sold_on\" HAVING count(*) > 1"));
        assert!(plan_repeats(&set(), &[], None).is_err());
        assert!(plan_repeats(&set(), &["x\" OR 1=1 --"], None).is_err());
    }
}
