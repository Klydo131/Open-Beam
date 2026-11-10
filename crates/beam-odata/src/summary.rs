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

    let value = match (aggregate, measure) {
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
    };

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
    let where_sql = match filter_text {
        Some(text) => format!(" WHERE {}", filter::to_sql(&filter::parse(text)?, set, &mut params)?),
        None => String::new(),
    };
    let sql = format!(
        "SELECT coalesce({label}, '(empty)') AS \"label\", CAST({value} AS DOUBLE) AS \"value\" \
         FROM {}{where_sql} GROUP BY 1 ORDER BY {order} LIMIT {}",
        quote_ident(&set.name),
        limit.saturating_add(1),
    );
    Ok(Summary { sql, params, ordered })
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
}
