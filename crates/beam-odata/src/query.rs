//! Turns OData system query options into one parameterised SQL statement.

use crate::filter::{self, FilterError};
use crate::{quote_ident, EdmType, EntitySet, Literal, Property};

const UNSUPPORTED: &[&str] = &["$expand", "$apply", "$search", "$compute", "$skiptoken", "$deltatoken"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueryError {
    #[error(transparent)]
    Filter(#[from] FilterError),
    #[error("query option '{0}' is not supported")]
    Unsupported(String),
    #[error("unknown query option '{0}'")]
    UnknownOption(String),
    #[error("invalid value for '{0}'")]
    InvalidValue(&'static str),
    #[error("unknown property '{0}'")]
    UnknownProperty(String),
}

impl QueryError {
    /// `true` when the request was well formed but asks for a feature the
    /// service does not implement (HTTP 501 rather than 400).
    pub fn is_not_implemented(&self) -> bool {
        matches!(self, QueryError::Unsupported(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Selected properties, in output column order.
    pub columns: Vec<Property>,
    /// Returns one page of rows. Fetches `page_size + 1` rows so the caller
    /// can tell whether another page exists without a second query.
    pub rows_sql: String,
    pub rows_params: Vec<Literal>,
    /// Counts rows matching `$filter`, ignoring paging. Present when the
    /// client sent `$count=true`.
    pub count_sql: Option<String>,
    pub count_params: Vec<Literal>,
    /// Maximum rows to return in this response.
    pub page_size: u64,
    pub skip: u64,
    /// Client-requested `$top`, if any.
    pub top: Option<u64>,
}

/// Plans a collection request. `options` are the decoded query string pairs.
pub fn plan(set: &EntitySet, options: &[(String, String)], max_page_size: u64) -> Result<Plan, QueryError> {
    let mut select = None;
    let mut filter_text = None;
    let mut orderby = None;
    let mut top = None;
    let mut skip = 0;
    let mut count = false;

    for (name, value) in options {
        match name.as_str() {
            "$select" => select = Some(value.as_str()),
            "$filter" => filter_text = Some(value.as_str()),
            "$orderby" => orderby = Some(value.as_str()),
            "$top" => top = Some(value.parse::<u64>().map_err(|_| QueryError::InvalidValue("$top"))?),
            "$skip" => skip = value.parse::<u64>().map_err(|_| QueryError::InvalidValue("$skip"))?,
            "$count" => {
                count = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(QueryError::InvalidValue("$count")),
                }
            }
            "$format" if value.eq_ignore_ascii_case("json") || value.starts_with("application/json") => {}
            "$format" => return Err(QueryError::Unsupported("$format".into())),
            other if UNSUPPORTED.contains(&other) => return Err(QueryError::Unsupported(other.into())),
            other if other.starts_with('$') => return Err(QueryError::UnknownOption(other.into())),
            // Custom (non-`$`) options are ignored, as the OData spec requires.
            _ => {}
        }
    }

    let columns = match select.map(str::trim) {
        None | Some("*") => set.properties.clone(),
        Some(list) => {
            let mut chosen: Vec<Property> = Vec::new();
            for name in list.split(',').map(str::trim) {
                let property = lookup(set, name)?;
                if !chosen.contains(property) {
                    chosen.push(property.clone());
                }
            }
            chosen
        }
    };

    let mut where_params = Vec::new();
    let where_sql = match filter_text {
        Some(text) => format!(
            " WHERE {}",
            filter::to_sql(&filter::parse(text)?, set, &mut where_params)?
        ),
        None => String::new(),
    };

    // The key is always the final sort term so that paging is deterministic.
    let mut order_terms = Vec::new();
    for term in orderby.into_iter().flat_map(|o| o.split(',')) {
        let mut words = term.split_whitespace();
        let name = words.next().ok_or(QueryError::InvalidValue("$orderby"))?;
        let direction = match (words.next(), words.next()) {
            (None, _) | (Some("asc"), None) => "ASC",
            (Some("desc"), None) => "DESC",
            _ => return Err(QueryError::InvalidValue("$orderby")),
        };
        order_terms.push(format!("{} {direction}", quote_ident(&lookup(set, name)?.name)));
    }
    order_terms.push(format!("{} ASC", quote_ident(&set.key)));

    let page_size = top.map_or(max_page_size, |t| t.min(max_page_size));
    let select_list: Vec<String> = columns.iter().map(output_expr).collect();
    let from = quote_ident(&set.name);
    let rows_sql = format!(
        "SELECT {} FROM {from}{where_sql} ORDER BY {} LIMIT {} OFFSET {skip}",
        select_list.join(", "),
        order_terms.join(", "),
        page_size.saturating_add(1),
    );

    Ok(Plan {
        columns,
        rows_sql,
        rows_params: where_params.clone(),
        count_sql: count.then(|| format!("SELECT count(*) FROM {from}{where_sql}")),
        count_params: where_params,
        page_size,
        skip,
        top,
    })
}

fn lookup<'a>(set: &'a EntitySet, name: &str) -> Result<&'a Property, QueryError> {
    set.property(name)
        .ok_or_else(|| QueryError::UnknownProperty(name.to_string()))
}

/// Dates and timestamps are formatted by the engine so the JSON payload
/// carries the ISO 8601 text OData requires. Timestamps are treated as UTC.
fn output_expr(property: &Property) -> String {
    let ident = quote_ident(&property.name);
    match property.edm {
        EdmType::Date => format!("strftime({ident}, '%Y-%m-%d') AS {ident}"),
        EdmType::DateTimeOffset => format!("strftime({ident}, '%Y-%m-%dT%H:%M:%S.%fZ') AS {ident}"),
        _ => ident,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                p("sold_on", EdmType::Date),
            ],
        }
    }

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn default_plan_selects_everything_ordered_by_key() {
        let plan = plan(&set(), &[], 100).unwrap();
        assert_eq!(
            plan.rows_sql,
            "SELECT \"id\", \"region\", strftime(\"sold_on\", '%Y-%m-%d') AS \"sold_on\" \
             FROM \"sales\" ORDER BY \"id\" ASC LIMIT 101 OFFSET 0"
        );
        assert_eq!(plan.count_sql, None);
    }

    #[test]
    fn all_options_together() {
        let plan = plan(
            &set(),
            &opts(&[
                ("$select", "region, id"),
                ("$filter", "region eq 'EU'"),
                ("$orderby", "region desc,id"),
                ("$top", "5"),
                ("$skip", "10"),
                ("$count", "true"),
            ]),
            100,
        )
        .unwrap();
        assert_eq!(
            plan.rows_sql,
            "SELECT \"region\", \"id\" FROM \"sales\" WHERE (\"region\" = ?) \
             ORDER BY \"region\" DESC, \"id\" ASC, \"id\" ASC LIMIT 6 OFFSET 10"
        );
        assert_eq!(plan.rows_params, vec![Literal::Text("EU".into())]);
        assert_eq!(
            plan.count_sql.as_deref(),
            Some("SELECT count(*) FROM \"sales\" WHERE (\"region\" = ?)")
        );
        assert_eq!((plan.page_size, plan.skip, plan.top), (5, 10, Some(5)));
    }

    #[test]
    fn top_is_capped_by_the_server_page_size() {
        let plan = plan(&set(), &opts(&[("$top", "1000000")]), 100).unwrap();
        assert_eq!(plan.page_size, 100);
        assert!(plan.rows_sql.ends_with("LIMIT 101 OFFSET 0"));
    }

    #[test]
    fn rejects_bad_options() {
        let fail = |pairs: &[(&str, &str)]| plan(&set(), &opts(pairs), 100).unwrap_err();
        assert_eq!(
            fail(&[("$select", "id,password")]),
            QueryError::UnknownProperty("password".into())
        );
        assert_eq!(
            fail(&[("$orderby", "id; drop table sales")]),
            QueryError::InvalidValue("$orderby")
        );
        assert_eq!(
            fail(&[("$orderby", "nope")]),
            QueryError::UnknownProperty("nope".into())
        );
        assert_eq!(fail(&[("$top", "-1")]), QueryError::InvalidValue("$top"));
        assert_eq!(fail(&[("$skip", "1 OR 1=1")]), QueryError::InvalidValue("$skip"));
        assert_eq!(fail(&[("$bogus", "1")]), QueryError::UnknownOption("$bogus".into()));
        assert!(fail(&[("$expand", "x")]).is_not_implemented());
        assert!(fail(&[("$format", "xml")]).is_not_implemented());
    }
}
