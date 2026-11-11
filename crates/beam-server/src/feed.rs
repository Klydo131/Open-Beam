//! The read-only OData v4 data feed.

use crate::app::AppState;
use crate::auth::Viewer;
use crate::catalog;
use axum::extract::{Extension, Path, Query, State};
use axum::http::header::{CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_engine::Param;
use beam_odata::{EntitySet, Literal};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const JSON: &str = "application/json;odata.metadata=minimal;charset=utf-8";

pub fn error(status: StatusCode, message: &str) -> Response {
    let code = status.canonical_reason().unwrap_or("Error").replace(' ', "");
    let body = json!({ "error": { "code": code, "message": message } });
    (status, [(CONTENT_TYPE, JSON)], body.to_string()).into_response()
}

pub fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = &state.public_url {
        return url.clone();
    }
    // The Host header is echoed into links, so accept only host[:port] characters.
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)))
        .unwrap_or(&state.fallback_host);
    format!("http://{host}")
}

fn visible_sets(state: &AppState, viewer: &Viewer) -> Vec<EntitySet> {
    catalog::visible(state, &viewer.0)
        .into_iter()
        .map(|entry| entry.set)
        .collect()
}

pub async fn service_document(
    State(state): State<Arc<AppState>>,
    Extension(viewer): Extension<Viewer>,
    headers: HeaderMap,
) -> Response {
    let sets = visible_sets(&state, &viewer);
    let document = beam_odata::service_document(&format!("{}/odata", base_url(&state, &headers)), &sets);
    ([(CONTENT_TYPE, JSON)], document.to_string()).into_response()
}

pub async fn metadata(State(state): State<Arc<AppState>>, Extension(viewer): Extension<Viewer>) -> Response {
    (
        [(CONTENT_TYPE, "application/xml;charset=utf-8")],
        beam_odata::metadata_xml(&visible_sets(&state, &viewer)),
    )
        .into_response()
}

fn unknown_set() -> Response {
    error(StatusCode::NOT_FOUND, "Unknown entity set")
}

pub fn to_params(literals: &[Literal]) -> Vec<Param> {
    literals
        .iter()
        .map(|l| match l {
            Literal::Null => Param::Null,
            Literal::Bool(b) => Param::Bool(*b),
            Literal::Int(i) => Param::Int(*i),
            Literal::Float(f) => Param::Float(*f),
            Literal::Text(s) => Param::Text(s.clone()),
        })
        .collect()
}

/// Why a planned statement returned nothing. Deliberately tiny: each caller
/// decides how to present it, and an error value should not be as large as a
/// whole HTTP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryFailure {
    /// The engine refused the statement, which at this point means one of the
    /// client's values was unusable (for example an impossible date).
    Rejected,
    /// The query ran past the time limit and was cancelled.
    TimedOut,
    Internal,
}

impl QueryFailure {
    fn response(self) -> Response {
        match self {
            QueryFailure::Rejected => error(StatusCode::BAD_REQUEST, "The query could not be executed"),
            QueryFailure::TimedOut => error(StatusCode::SERVICE_UNAVAILABLE, TOO_LONG),
            QueryFailure::Internal => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
        }
    }
}

/// Runs a planned statement off the async runtime. The engine's own error
/// text goes to the server log only, never to the client.
/// Shown wherever a query is cancelled for running too long.
pub const TOO_LONG: &str = "That query took too long. Narrow it with a filter, choose fewer columns, or ask an administrator to raise the time limit.";

pub async fn run(
    state: &Arc<AppState>,
    sql: String,
    literals: &[Literal],
    width: usize,
) -> Result<Vec<Vec<Value>>, QueryFailure> {
    let (state, params) = (state.clone(), to_params(literals));
    let outcome = tokio::task::spawn_blocking(move || state.engine.query(&sql, &params, width)).await;
    match outcome {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(beam_engine::EngineError::TimedOut)) => Err(QueryFailure::TimedOut),
        Ok(Err(e)) => {
            eprintln!("query failed: {e}");
            Err(QueryFailure::Rejected)
        }
        Err(e) => {
            eprintln!("query task failed: {e}");
            Err(QueryFailure::Internal)
        }
    }
}

fn plan_error(e: beam_odata::QueryError) -> Response {
    let status = if e.is_not_implemented() {
        StatusCode::NOT_IMPLEMENTED
    } else {
        StatusCode::BAD_REQUEST
    };
    error(status, &e.to_string())
}

pub async fn collection(
    State(state): State<Arc<AppState>>,
    Extension(viewer): Extension<Viewer>,
    Path(name): Path<String>,
    Query(options): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = catalog::find(&state, &viewer.0, &name) else {
        return unknown_set();
    };
    let set = &entry.set;
    let plan = match beam_odata::plan(set, &options, state.max_page_size) {
        Ok(plan) => plan,
        Err(e) => return plan_error(e),
    };
    let mut rows = match run(&state, plan.rows_sql.clone(), &plan.rows_params, plan.columns.len()).await {
        Ok(rows) => rows,
        Err(failure) => return failure.response(),
    };

    let base = format!("{}/odata", base_url(&state, &headers));
    let page = usize::try_from(plan.page_size).unwrap_or(usize::MAX);
    let truncated = rows.len() > page;
    rows.truncate(page);

    let mut body = Map::new();
    let selected = options.iter().any(|(k, v)| k == "$select" && v.trim() != "*");
    let projection = if selected {
        format!(
            "({})",
            plan.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        )
    } else {
        String::new()
    };
    body.insert(
        "@odata.context".into(),
        format!("{base}/$metadata#{}{projection}", set.name).into(),
    );

    if let Some(count_sql) = &plan.count_sql {
        match run(&state, count_sql.clone(), &plan.count_params, 1).await {
            Ok(result) => body.insert("@odata.count".into(), result[0][0].clone()),
            Err(failure) => return failure.response(),
        };
    }

    // More rows exist and the client did not ask for fewer than a full page.
    if truncated && plan.top.is_none_or(|top| top > plan.page_size) {
        let mut link = form_urlencoded::Serializer::new(String::new());
        for (key, value) in options.iter().filter(|(k, _)| k != "$skip" && k != "$top") {
            link.append_pair(key, value);
        }
        if let Some(top) = plan.top {
            link.append_pair("$top", &(top - plan.page_size).to_string());
        }
        link.append_pair("$skip", &plan.skip.saturating_add(plan.page_size).to_string());
        body.insert(
            "@odata.nextLink".into(),
            format!("{base}/{}?{}", set.name, link.finish()).into(),
        );
    }

    let value: Vec<Value> = rows
        .into_iter()
        .map(|row| Value::Object(plan.columns.iter().map(|c| c.name.clone()).zip(row).collect()))
        .collect();
    body.insert("value".into(), value.into());
    ([(CONTENT_TYPE, JSON)], Value::Object(body).to_string()).into_response()
}

pub async fn count(
    State(state): State<Arc<AppState>>,
    Extension(viewer): Extension<Viewer>,
    Path(name): Path<String>,
    Query(options): Query<Vec<(String, String)>>,
) -> Response {
    let Some(entry) = catalog::find(&state, &viewer.0, &name) else {
        return unknown_set();
    };
    let set = &entry.set;
    let mut options: Vec<_> = options.into_iter().filter(|(k, _)| k == "$filter").collect();
    options.push(("$count".into(), "true".into()));
    let plan = match beam_odata::plan(set, &options, state.max_page_size) {
        Ok(plan) => plan,
        Err(e) => return plan_error(e),
    };
    let sql = plan.count_sql.unwrap_or_default();
    match run(&state, sql, &plan.count_params, 1).await {
        Ok(result) => ([(CONTENT_TYPE, "text/plain;charset=utf-8")], result[0][0].to_string()).into_response(),
        Err(failure) => failure.response(),
    }
}
