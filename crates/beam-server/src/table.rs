//! The Table view of a dataset, and the choices it shares with exports.
//!
//! A view is described entirely by its address (`?sort=…&column=…`), so it can
//! be bookmarked, shared with a colleague, downloaded, or turned into the
//! feed address that Excel and Power BI use, and all four agree.

use crate::app::AppState;
use crate::auth::Signed;
use crate::catalog::{self, Entry};
use crate::feed;
use crate::web::{layout, message_page, page, require_signed_in};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_accounts::{Mode, Role};
use beam_odata::{EdmType, EntitySet, Property};
use maud::{html, Markup};
use serde_json::Value;
use std::sync::Arc;

const PAGE_ROWS: u64 = 50;

/// Comparisons offered in the filter form: code, label, shown in Basic mode.
const OPERATORS: &[(&str, &str, bool)] = &[
    ("eq", "equals", true),
    ("contains", "contains", true),
    ("ne", "does not equal", true),
    ("gt", "is greater than", true),
    ("ge", "is at least", true),
    ("lt", "is less than", true),
    ("le", "is at most", true),
    ("startswith", "starts with", false),
    ("endswith", "ends with", false),
    ("empty", "is empty", false),
    ("notempty", "is not empty", false),
];

/// Comparisons that need no value.
pub fn needs_value(op: &str) -> bool {
    !matches!(op, "empty" | "notempty")
}

/// What a person chose to look at. Built from the query string by hand
/// because a column list repeats the same key (`cols=a&cols=b`).
#[derive(Default, Clone)]
pub struct TableQuery {
    pub page: Option<u64>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub column: Option<String>,
    pub op: Option<String>,
    pub value: Option<String>,
    pub cols: Vec<String>,
}

impl TableQuery {
    pub fn from_pairs(pairs: &[(String, String)]) -> Self {
        let mut query = TableQuery::default();
        for (key, value) in pairs {
            match key.as_str() {
                "page" => query.page = value.parse().ok(),
                "sort" => query.sort = Some(value.clone()),
                "dir" => query.dir = Some(value.clone()),
                "column" => query.column = Some(value.clone()),
                "op" => query.op = Some(value.clone()),
                "value" => query.value = Some(value.clone()),
                "cols" if query.cols.len() < 512 => query.cols.push(value.clone()),
                _ => {}
            }
        }
        query
    }
}

/// A `TableQuery` checked against a dataset's columns.
pub struct View<'a> {
    pub sort: Option<&'a str>,
    pub descending: bool,
    /// The OData `$filter` expression, when a usable filter was chosen.
    pub filter: Option<String>,
    pub filter_column: Option<&'a Property>,
    pub op: &'a str,
    pub value: &'a str,
    /// Columns to show, in dataset order. All of them unless some were chosen.
    pub columns: Vec<&'a Property>,
    pub problem: Option<&'static str>,
}

impl View<'_> {
    pub fn chosen_subset(&self, set: &EntitySet) -> bool {
        self.columns.len() < set.properties.len()
    }

    /// Query options for the planner, without paging.
    pub fn options(&self, set: &EntitySet) -> Vec<(String, String)> {
        let mut options = Vec::new();
        if let Some(column) = self.sort {
            let direction = if self.descending { "desc" } else { "asc" };
            options.push(("$orderby".into(), format!("{column} {direction}")));
        }
        if let Some(filter) = &self.filter {
            options.push(("$filter".into(), filter.clone()));
        }
        if self.chosen_subset(set) {
            let names: Vec<&str> = self.columns.iter().map(|p| p.name.as_str()).collect();
            options.push(("$select".into(), names.join(",")));
        }
        options
    }
}

pub fn view<'a>(set: &'a EntitySet, query: &'a TableQuery) -> View<'a> {
    let sort = query.sort.as_deref().filter(|column| set.property(column).is_some());
    let op = query.op.as_deref().unwrap_or("");
    let value = query.value.as_deref().unwrap_or("").trim();
    let filter_column = query.column.as_deref().and_then(|column| set.property(column));
    let (mut filter, mut problem) = (None, None);
    if let Some(property) = filter_column {
        if !value.is_empty() || !needs_value(op) {
            match filter_expression(property, op, value) {
                Ok(expression) => filter = Some(expression),
                Err(message) => problem = Some(message),
            }
        }
    }
    let chosen: Vec<&Property> = set.properties.iter().filter(|p| query.cols.contains(&p.name)).collect();
    View {
        sort,
        descending: query.dir.as_deref() == Some("desc"),
        filter,
        filter_column,
        op,
        value,
        columns: if chosen.is_empty() {
            set.properties.iter().collect()
        } else {
            chosen
        },
        problem,
    }
}

/// Turns the filter form into an OData `$filter` expression. Values are
/// written as OData literals; the planner then binds them as parameters.
pub fn filter_expression(property: &Property, op: &str, value: &str) -> Result<String, &'static str> {
    let value = value.trim();
    let name = &property.name;
    let text = || format!("'{}'", value.to_lowercase().replace('\'', "''"));
    match op {
        "empty" => return Ok(format!("{name} eq null")),
        "notempty" => return Ok(format!("{name} ne null")),
        "contains" | "startswith" | "endswith" => {
            if property.edm != EdmType::String {
                return Err("That comparison works on text columns. Choose another one.");
            }
            return Ok(format!("{op}(tolower({name}), {})", text()));
        }
        _ => {}
    }
    if !OPERATORS.iter().any(|(code, _, _)| *code == op) {
        return Err("Choose a comparison.");
    }
    let literal = match property.edm {
        EdmType::String => format!("'{}'", value.replace('\'', "''")),
        EdmType::Boolean => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" => "true".to_string(),
            "false" | "no" => "false".to_string(),
            _ => return Err("Enter true or false."),
        },
        EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. } => {
            if let Ok(whole) = value.parse::<i64>() {
                whole.to_string()
            } else {
                match value.parse::<f64>() {
                    Ok(number) if number.is_finite() => number.to_string(),
                    _ => return Err("Enter a number."),
                }
            }
        }
        EdmType::Date | EdmType::DateTimeOffset => {
            let shaped = value.len() >= 10 && value.bytes().all(|b| b.is_ascii_digit() || b"-:T.Z+".contains(&b));
            if !shaped {
                return Err("Enter a date as YYYY-MM-DD.");
            }
            value.to_string()
        }
    };
    Ok(format!("{name} {op} {literal}"))
}

/// The query string that reproduces a view, optionally with a different
/// page or sort. Empty when nothing was chosen.
pub fn view_query(query: &TableQuery, page: u64, sort: Option<(&str, &str)>) -> String {
    let mut link = form_urlencoded::Serializer::new(String::new());
    let sort = sort.or(query.sort.as_deref().zip(query.dir.as_deref().or(Some("asc"))));
    if let Some((column, dir)) = sort {
        link.append_pair("sort", column).append_pair("dir", dir);
    }
    if let (Some(column), Some(op)) = (&query.column, &query.op) {
        let value = query.value.as_deref().unwrap_or("");
        if !value.trim().is_empty() || !needs_value(op) {
            link.append_pair("column", column)
                .append_pair("op", op)
                .append_pair("value", value);
        }
    }
    for column in &query.cols {
        link.append_pair("cols", column);
    }
    if page > 1 {
        link.append_pair("page", &page.to_string());
    }
    link.finish()
}

fn with_query(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

fn table_link(set: &EntitySet, query: &TableQuery, page: u64, sort: Option<(&str, &str)>) -> String {
    with_query(&format!("/datasets/{}", set.name), &view_query(query, page, sort))
}

/// Groups a run of digits in threes: `1234567` becomes `1,234,567`.
pub fn group_digits(digits: &str) -> String {
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn thousands(n: u64) -> String {
    group_digits(&n.to_string())
}

/// Feed timestamps are ISO 8601 with microseconds. On a page, show them the
/// way a person writes them: `2025-01-07 09:15:00`, keeping any fraction.
pub fn readable_timestamp(text: &str) -> String {
    let text = text.strip_suffix('Z').unwrap_or(text).replacen('T', " ", 1);
    match text.split_once('.') {
        Some((whole, fraction)) if fraction.bytes().all(|b| b == b'0') => whole.to_string(),
        _ => text,
    }
}

fn cell(value: &Value, edm: EdmType) -> Markup {
    match value {
        Value::Null => html! { td.null { "—" } },
        Value::String(s) if edm == EdmType::DateTimeOffset => html! { td { (readable_timestamp(s)) } },
        Value::Number(n) => html! { td.num { (n.to_string()) } },
        Value::Bool(b) => html! { td { (if *b { "true" } else { "false" }) } },
        Value::String(s) => html! { td { (s) } },
        other => html! { td { (other.to_string()) } },
    }
}

/// The tabs above every view of a dataset.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Table,
    Chart,
    Share,
}

/// Whether `signed` may share and delete this dataset: its owner, or an administrator.
pub fn can_manage(signed: &Signed, entry: &Entry) -> bool {
    let account = &signed.session.account;
    entry
        .upload
        .as_ref()
        .is_some_and(|upload| upload.owner_id == account.id || account.role == Role::Admin)
}

pub fn dataset_tabs(set: &EntitySet, current: Tab, filter: &[(&str, &str)], manage: bool) -> Markup {
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (name, value) in filter {
        query.append_pair(name, value);
    }
    let query = query.finish();
    let base = format!("/datasets/{}", set.name);
    html! {
        nav.tabs aria-label="Views" {
            a href=(with_query(&base, &query)) aria-current=[(current == Tab::Table).then_some("page")] { "Table" }
            a href=(with_query(&format!("{base}/chart"), &query)) aria-current=[(current == Tab::Chart).then_some("page")] { "Chart" }
            @if manage {
                a href={ (base) "/share" } aria-current=[(current == Tab::Share).then_some("page")] { "Sharing" }
            }
        }
    }
}

/// The formula that loads a feed address in Power Query (Excel and Power BI).
pub fn power_query(url: &str) -> String {
    format!(
        "let\n    Source = OData.Feed(\"{}\", null, [Implementation = \"2.0\"])\nin\n    Source",
        url.replace('"', "\"\"")
    )
}

pub async fn dataset(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let Some(entry) = catalog::find(&state, &signed.session.account, &name) else {
        return message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        );
    };
    let set = &entry.set;
    let query = TableQuery::from_pairs(&pairs);
    let view = view(set, &query);
    let advanced = signed.session.account.mode == Mode::Advanced;

    let page_number = query.page.unwrap_or(1).clamp(1, 1_000_000);
    let mut problem = view.problem;
    let mut options = view.options(set);
    options.push(("$top".into(), PAGE_ROWS.to_string()));
    options.push(("$skip".into(), ((page_number - 1) * PAGE_ROWS).to_string()));
    options.push(("$count".into(), "true".into()));

    let mut rows = Vec::new();
    let mut total = 0;
    let failed = "That filter could not be applied. Check the value and try again.";
    match beam_odata::plan(set, &options, PAGE_ROWS) {
        Ok(plan) => {
            let width = plan.columns.len();
            let counted = match &plan.count_sql {
                Some(sql) => feed::run(&state, sql.clone(), &plan.count_params, 1).await,
                None => Ok(Vec::new()),
            };
            match (
                feed::run(&state, plan.rows_sql, &plan.rows_params, width).await,
                counted,
            ) {
                (Ok(mut fetched), Ok(count)) => {
                    fetched.truncate(PAGE_ROWS as usize);
                    rows = fetched;
                    total = count
                        .first()
                        .and_then(|r| r.first())
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
                _ => problem = Some(failed),
            }
        }
        Err(_) => problem = Some(failed),
    }

    let first = (page_number - 1) * PAGE_ROWS + 1;
    let last = first + rows.len() as u64 - u64::from(!rows.is_empty());
    let has_next = page_number * PAGE_ROWS < total;
    let filtered = view.filter.is_some();
    // The Chart view keeps the filter chosen here.
    let carried: Vec<(&str, &str)> = match (view.filter_column, filtered) {
        (Some(property), true) => vec![
            ("column", property.name.as_str()),
            ("op", view.op),
            ("value", view.value),
        ],
        _ => Vec::new(),
    };
    let here = view_query(&query, 1, None);
    let feed_url = with_query(&format!("{}/odata/{}", feed::base_url(&state, &headers), set.name), &{
        let mut feed_query = form_urlencoded::Serializer::new(String::new());
        for (key, value) in view.options(set) {
            feed_query.append_pair(&key, &value);
        }
        feed_query.finish()
    });
    let direction = |descending: bool| if descending { "desc" } else { "asc" };
    let clear = TableQuery {
        cols: query.cols.clone(),
        ..TableQuery::default()
    };

    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / " (set.name) }
        div.titlebar {
            h1 { (set.name) }
            div.actions {
                a.button.quiet href=(with_query(&format!("/datasets/{}/export.csv", set.name), &here)) download { "Download CSV" }
                a.button href=(with_query(&format!("/datasets/{}/export.xlsx", set.name), &here)) download { "Download Excel" }
            }
        }
        (dataset_tabs(set, Tab::Table, &carried, can_manage(&signed, &entry)))
        form.filter method="get" action={ "/datasets/" (set.name) } {
            @if let Some(column) = view.sort {
                input type="hidden" name="sort" value=(column);
                input type="hidden" name="dir" value=(direction(view.descending));
            }
            @for column in &query.cols { input type="hidden" name="cols" value=(column); }
            label for="column" { "Show rows where" }
            select #column name="column" {
                @for property in &set.properties {
                    // Until someone chooses, start on the first real column rather than the row number.
                    @let chosen = match query.column.as_deref() {
                        Some(column) => column == property.name,
                        None => Some(property) == set.properties.iter().find(|p| p.name != beam_engine::ROW_KEY),
                    };
                    option value=(property.name) selected[chosen] { (property.name) }
                }
            }
            select name="op" aria-label="Comparison" {
                @for (code, label, basic) in OPERATORS {
                    @if *basic || advanced || view.op == *code {
                        option value=(code) selected[view.op == *code] { (label) }
                    }
                }
            }
            input name="value" value=(view.value) aria-label="Value" placeholder="value";
            button type="submit" { "Apply" }
            @if filtered { a.quiet.button href=(table_link(set, &clear, 1, view.sort.map(|c| (c, direction(view.descending))))) { "Clear" } }
        }
        @if advanced {
            details.panel.tools open[view.chosen_subset(set)] {
                summary { "Columns" }
                form.columns method="get" action={ "/datasets/" (set.name) } {
                    @if let Some(column) = view.sort {
                        input type="hidden" name="sort" value=(column);
                        input type="hidden" name="dir" value=(direction(view.descending));
                    }
                    @for (key, value) in &carried { input type="hidden" name=(key) value=(value); }
                    @for property in &set.properties {
                        label.check {
                            input type="checkbox" name="cols" value=(property.name)
                                checked[!view.chosen_subset(set) || query.cols.contains(&property.name)];
                            (property.name)
                        }
                    }
                    button type="submit" { "Show these columns" }
                }
            }
        }
        @if let Some(problem) = problem { p.notice.error role="alert" { (problem) } }
        div.tablewrap {
            table {
                thead {
                    tr {
                        @for property in &view.columns {
                            @let active = view.sort == Some(property.name.as_str());
                            @let next = if active && !view.descending { "desc" } else { "asc" };
                            th aria-sort=[active.then_some(if view.descending { "descending" } else { "ascending" })] {
                                a href=(table_link(set, &query, 1, Some((&property.name, next)))) {
                                    (property.name)
                                    @if active { span.arrow aria-hidden="true" { (if view.descending { " ▼" } else { " ▲" }) } }
                                }
                            }
                        }
                    }
                }
                tbody {
                    @for row in &rows {
                        tr { @for (value, property) in row.iter().zip(&view.columns) { (cell(value, property.edm)) } }
                    }
                }
            }
        }
        @if rows.is_empty() && problem.is_none() { p.empty { "No rows match." } }
        nav.pager aria-label="Pages" {
            @if !rows.is_empty() {
                span { "Rows " (thousands(first)) "–" (thousands(last)) " of " (thousands(total)) }
            }
            @if page_number > 1 { a href=(table_link(set, &query, page_number - 1, None)) { "← Previous" } }
            @if has_next { a href=(table_link(set, &query, page_number + 1, None)) { "Next →" } }
        }
        section.panel.elsewhere {
            h2 { "Use this data somewhere else" }
            ul.destinations {
                li {
                    strong { "Excel" }
                    span { "Select " a href=(with_query(&format!("/datasets/{}/export.xlsx", set.name), &here)) download { "Download Excel" } " and open the file." }
                }
                li {
                    strong { "Google Sheets" }
                    span { "Select " a href=(with_query(&format!("/datasets/{}/export.csv", set.name), &here)) download { "Download CSV" } ", then in Sheets choose File → Import → Upload." }
                }
                li {
                    strong { "Power BI" }
                    span { "Choose Get Data → OData feed, enter " code { (feed::base_url(&state, &headers)) "/odata/" } " and sign in with Basic using your Open Beam username and password. It stays up to date when you refresh." }
                }
            }
            p.hint { "Downloads contain exactly what is shown above: the same filter, sort and columns." }
        }
        @if advanced {
            details.panel.tools {
                summary { "Connect this exact view to Excel or Power BI" }
                p { "This address returns exactly the rows and columns shown above. Use it with " strong { "From OData Feed" } " and sign in with " strong { "Basic" } "." }
                input.copy readonly value=(feed_url) aria-label="Feed address for this view";
                p { "Or paste this into a blank query in Power Query:" }
                textarea.copy readonly rows="5" aria-label="Power Query formula for this view" { (power_query(&feed_url)) }
            }
        }
    };
    page(StatusCode::OK, layout(&set.name, Some(&signed), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_columns_and_presence_filters_survive_the_round_trip() {
        let pairs: Vec<(String, String)> = [
            ("cols", "a"),
            ("cols", "b"),
            ("column", "a"),
            ("op", "empty"),
            ("value", ""),
            ("page", "x"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let query = TableQuery::from_pairs(&pairs);
        assert_eq!((query.cols.len(), query.page), (2, None));
        assert_eq!(
            view_query(&query, 2, None),
            "column=a&op=empty&value=&cols=a&cols=b&page=2"
        );
    }

    #[test]
    fn power_query_formula_quotes_the_address() {
        assert_eq!(
            power_query("http://h/odata/s?$filter=a eq \"x\""),
            "let\n    Source = OData.Feed(\"http://h/odata/s?$filter=a eq \"\"x\"\"\", null, [Implementation = \"2.0\"])\nin\n    Source"
        );
    }
}
