//! The read-only JSON API, version 1.
//!
//! Everything here is also what the AI-assistant connection (`mcp.rs`)
//! uses, so both always give the same answers. Datasets are found through
//! `catalog`, so an account sees exactly what it sees in the browser.
//! Sign in with a browser session, HTTP Basic, or `Authorization: Bearer`
//! with a personal API token.

use crate::app::AppState;
use crate::auth::Viewer;
use crate::catalog::{self, Entry};
use crate::feed::{self, QueryFailure};
use axum::extract::{Extension, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_accounts::Account;
use beam_odata::{Aggregate, Bucket, EdmType};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Most rows one Parquet download may hold.
pub const MAX_PARQUET_ROWS: u64 = 5_000_000;

type App = State<Arc<AppState>>;

/// A failed request, as an HTTP status and a message for people.
#[derive(Debug, PartialEq)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "There is no dataset with that name.")
    }

    pub fn into_response(self) -> Response {
        feed::error(self.status, &self.message)
    }
}

impl From<QueryFailure> for ApiError {
    fn from(failure: QueryFailure) -> Self {
        match failure {
            QueryFailure::TimedOut => Self::new(StatusCode::SERVICE_UNAVAILABLE, feed::TOO_LONG),
            QueryFailure::Rejected => Self::new(
                StatusCode::BAD_REQUEST,
                "The query could not be run. Check the filter values.",
            ),
            QueryFailure::Internal => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
        }
    }
}

impl From<beam_odata::QueryError> for ApiError {
    fn from(error: beam_odata::QueryError) -> Self {
        let status = if error.is_not_implemented() {
            StatusCode::NOT_IMPLEMENTED
        } else {
            StatusCode::BAD_REQUEST
        };
        Self::new(status, error.to_string())
    }
}

fn json_response(value: Value) -> Response {
    ([(CONTENT_TYPE, "application/json; charset=utf-8")], value.to_string()).into_response()
}

pub fn type_name(edm: EdmType) -> &'static str {
    match edm {
        EdmType::Boolean => "boolean",
        EdmType::Int32 | EdmType::Int64 => "integer",
        EdmType::Double => "number",
        EdmType::Decimal { .. } => "decimal",
        EdmType::String => "text",
        EdmType::Date => "date",
        EdmType::DateTimeOffset => "datetime",
    }
}

fn describe(state: &AppState, entry: &Entry) -> Value {
    let owner = entry
        .upload
        .as_ref()
        .and_then(|u| state.accounts.get(u.owner_id).ok())
        .map(|a| a.username);
    json!({
        "name": entry.set.name,
        "key": entry.set.key,
        "source": if entry.upload.is_some() { "uploaded" } else { "built-in" },
        "owner": owner,
        "columns": entry.set.properties.iter().map(|p| json!({ "name": p.name, "type": type_name(p.edm) })).collect::<Vec<_>>(),
    })
}

// ------------------------------------------------- shared with the AI connection

pub fn list_datasets(state: &AppState, account: &Account) -> Value {
    Value::Array(
        catalog::visible(state, account)
            .iter()
            .map(|entry| describe(state, entry))
            .collect(),
    )
}

pub fn dataset(state: &AppState, account: &Account, name: &str) -> Result<Value, ApiError> {
    catalog::find(state, account, name)
        .map(|entry| describe(state, &entry))
        .ok_or_else(ApiError::not_found)
}

/// Translates API option names (`filter`, `select`, …) to the planner's
/// OData names. Unknown options are refused rather than ignored, so a typo
/// cannot silently return unfiltered data.
fn options(pairs: &[(String, String)], allowed: &[&str]) -> Result<Vec<(String, String)>, ApiError> {
    pairs
        .iter()
        .map(|(key, value)| {
            if allowed.contains(&key.as_str()) {
                Ok((format!("${key}"), value.clone()))
            } else {
                Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("Unknown option '{key}'. Allowed: {}.", allowed.join(", ")),
                ))
            }
        })
        .collect()
}

/// One page of rows, with the total when `count=true` and the offset of the
/// next page when there is one.
pub async fn rows(
    state: &Arc<AppState>,
    account: &Account,
    name: &str,
    pairs: &[(String, String)],
) -> Result<Value, ApiError> {
    let entry = catalog::find(state, account, name).ok_or_else(ApiError::not_found)?;
    let options = options(pairs, &["filter", "select", "orderby", "top", "skip", "count"])?;
    let plan = beam_odata::plan(&entry.set, &options, state.max_page_size)?;
    let mut found = feed::run(state, plan.rows_sql.clone(), &plan.rows_params, plan.columns.len()).await?;
    let more = found.len() as u64 > plan.page_size;
    found.truncate(usize::try_from(plan.page_size).unwrap_or(usize::MAX));
    let returned = found.len() as u64;
    let rows: Vec<Value> = found
        .into_iter()
        .map(|row| {
            Value::Object(
                plan.columns
                    .iter()
                    .map(|c| c.name.clone())
                    .zip(row)
                    .collect::<Map<_, _>>(),
            )
        })
        .collect();
    let mut body = Map::new();
    if let Some(sql) = &plan.count_sql {
        let counted = feed::run(state, sql.clone(), &plan.count_params, 1).await?;
        body.insert("count".into(), counted[0][0].clone());
    }
    let wanted_more = plan.top.is_none_or(|top| top > plan.page_size);
    body.insert(
        "next_skip".into(),
        if more && wanted_more {
            json!(plan.skip + returned)
        } else {
            Value::Null
        },
    );
    body.insert("rows".into(), Value::Array(rows));
    Ok(Value::Object(body))
}

/// Grouped totals, as drawn by the Chart view.
pub async fn summary(
    state: &Arc<AppState>,
    account: &Account,
    name: &str,
    pairs: &[(String, String)],
) -> Result<Value, ApiError> {
    let entry = catalog::find(state, account, name).ok_or_else(ApiError::not_found)?;
    let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    if let Some((key, _)) = pairs
        .iter()
        .find(|(k, _)| !["group", "measure", "aggregate", "bucket", "filter"].contains(&k.as_str()))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Unknown option '{key}'."),
        ));
    }
    let group = get("group")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Choose a column to group by with 'group'."))?;
    let aggregate = match get("aggregate").unwrap_or("count") {
        "count" => Aggregate::Count,
        "sum" => Aggregate::Sum,
        "avg" => Aggregate::Average,
        "min" => Aggregate::Min,
        "max" => Aggregate::Max,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "'aggregate' is one of count, sum, avg, min, max.",
            ))
        }
    };
    let bucket = match get("bucket").unwrap_or("month") {
        "day" => Bucket::Day,
        "month" => Bucket::Month,
        "quarter" => Bucket::Quarter,
        "year" => Bucket::Year,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "'bucket' is one of day, month, quarter, year.",
            ))
        }
    };
    const LIMIT: u64 = 1_000;
    let planned = beam_odata::plan_summary(
        &entry.set,
        group,
        get("measure"),
        aggregate,
        bucket,
        get("filter"),
        LIMIT,
    )?;
    let mut found = feed::run(state, planned.sql, &planned.params, 2).await?;
    let truncated = found.len() as u64 > LIMIT;
    found.truncate(LIMIT as usize);
    let groups: Vec<Value> = found
        .into_iter()
        .map(|row| json!({ "label": row[0], "value": row[1] }))
        .collect();
    Ok(json!({ "groups": groups, "truncated": truncated }))
}

// ------------------------------------------------------------------ routes

pub async fn index() -> Response {
    json_response(json!({
        "name": "Open Beam API",
        "version": "1",
        "openapi": "/api/v1/openapi.json",
        "datasets": "/api/v1/datasets",
    }))
}

pub async fn openapi() -> Response {
    json_response(openapi_document())
}

pub async fn datasets(State(state): App, Extension(viewer): Extension<Viewer>) -> Response {
    json_response(list_datasets(&state, &viewer.0))
}

pub async fn one(State(state): App, Extension(viewer): Extension<Viewer>, Path(name): Path<String>) -> Response {
    match dataset(&state, &viewer.0, &name) {
        Ok(value) => json_response(value),
        Err(error) => error.into_response(),
    }
}

pub async fn rows_route(
    State(state): App,
    Extension(viewer): Extension<Viewer>,
    Path(name): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    match rows(&state, &viewer.0, &name, &pairs).await {
        Ok(value) => json_response(value),
        Err(error) => error.into_response(),
    }
}

pub async fn summary_route(
    State(state): App,
    Extension(viewer): Extension<Viewer>,
    Path(name): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    match summary(&state, &viewer.0, &name, &pairs).await {
        Ok(value) => json_response(value),
        Err(error) => error.into_response(),
    }
}

/// The same rows as `rows`, all pages, as a Parquet file with stored types.
pub async fn parquet(
    State(state): App,
    Extension(viewer): Extension<Viewer>,
    Path(name): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    _headers: HeaderMap,
) -> Response {
    let result: Result<Vec<u8>, ApiError> = async {
        let entry = catalog::find(&state, &viewer.0, &name).ok_or_else(ApiError::not_found)?;
        let mut options = options(&pairs, &["filter", "select", "orderby", "top", "skip"])?;
        options.push(("$count".into(), "true".into()));
        let plan = beam_odata::plan(&entry.set, &options, MAX_PARQUET_ROWS)?;
        let counted = feed::run(
            &state,
            plan.count_sql.clone().unwrap_or_default(),
            &plan.count_params,
            1,
        )
        .await?;
        let total = counted[0][0].as_u64().unwrap_or(0).saturating_sub(plan.skip);
        if plan.top.unwrap_or(total).min(total) > MAX_PARQUET_ROWS {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "A Parquet download holds up to 5,000,000 rows. Use a filter, or 'top' and 'skip', to split it.",
            ));
        }
        let worker = state.clone();
        let params = feed::to_params(&plan.rows_params);
        let sql = plan.export_sql;
        match tokio::task::spawn_blocking(move || worker.engine.query_to_parquet(&sql, &params)).await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(beam_engine::EngineError::TimedOut)) => Err(QueryFailure::TimedOut.into()),
            Ok(Err(beam_engine::EngineError::UploadsDisabled)) => Err(ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "Parquet downloads need the uploads folder, which is turned off on this server.",
            )),
            Ok(Err(e)) => {
                eprintln!("parquet export failed: {e}");
                Err(QueryFailure::Rejected.into())
            }
            Err(_) => Err(QueryFailure::Internal.into()),
        }
    }
    .await;
    match result {
        Ok(bytes) => {
            // Dataset names are plain identifiers, so they are safe in the header.
            let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}.parquet\""))
                .unwrap_or(HeaderValue::from_static("attachment"));
            (
                [
                    (CONTENT_TYPE, HeaderValue::from_static("application/vnd.apache.parquet")),
                    (CONTENT_DISPOSITION, disposition),
                ],
                bytes,
            )
                .into_response()
        }
        Err(error) => error.into_response(),
    }
}

// ----------------------------------------------------------------- OpenAPI

/// Every path this document lists. A test requests each one, so the
/// document cannot describe an address the server does not answer.
#[cfg_attr(not(test), allow(dead_code))]
pub const DOCUMENTED_PATHS: &[&str] = &[
    "/api/v1",
    "/api/v1/openapi.json",
    "/api/v1/datasets",
    "/api/v1/datasets/{name}",
    "/api/v1/datasets/{name}/rows",
    "/api/v1/datasets/{name}/rows.parquet",
    "/api/v1/datasets/{name}/summary",
];

fn openapi_document() -> Value {
    let name = json!({ "name": "name", "in": "path", "required": true, "schema": { "type": "string" }, "description": "Dataset name" });
    let query = |name: &str, description: &str, schema: Value| json!({ "name": name, "in": "query", "required": false, "schema": schema, "description": description });
    let view = vec![
        query(
            "filter",
            "OData $filter expression, for example region eq 'EU' and amount gt 10",
            json!({ "type": "string" }),
        ),
        query("select", "Comma-separated column names", json!({ "type": "string" })),
        query(
            "orderby",
            "Column and direction, for example amount desc",
            json!({ "type": "string" }),
        ),
        query("top", "Most rows to return", json!({ "type": "integer", "minimum": 0 })),
        query("skip", "Rows to skip", json!({ "type": "integer", "minimum": 0 })),
    ];
    let mut rows_parameters = vec![name.clone()];
    rows_parameters.extend(view.clone());
    rows_parameters.push(query(
        "count",
        "Include the total number of matching rows",
        json!({ "type": "boolean" }),
    ));
    let mut parquet_parameters = vec![name.clone()];
    parquet_parameters.extend(view);
    let error = json!({ "description": "Error", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } });
    let ok = |description: &str, schema: Value| json!({ "description": description, "content": { "application/json": { "schema": schema } } });
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Open Beam API",
            "version": "1",
            "description": "Read-only access to the datasets your account can see. Every request needs a browser session, HTTP Basic, or a personal API token sent as Authorization: Bearer <token>.",
            "license": { "name": "MIT", "identifier": "MIT" },
        },
        "security": [{ "token": [] }, { "basic": [] }],
        "paths": {
            "/api/v1": { "get": { "summary": "Where to start", "responses": { "200": ok("Links", json!({ "type": "object" })) } } },
            "/api/v1/openapi.json": { "get": { "summary": "This document", "responses": { "200": ok("OpenAPI 3.1 document", json!({ "type": "object" })) } } },
            "/api/v1/datasets": { "get": { "summary": "Datasets you can see", "responses": { "200": ok("Datasets", json!({ "type": "array", "items": { "$ref": "#/components/schemas/Dataset" } })) } } },
            "/api/v1/datasets/{name}": { "get": { "summary": "One dataset's columns", "parameters": [name], "responses": { "200": ok("Dataset", json!({ "$ref": "#/components/schemas/Dataset" })), "404": error } } },
            "/api/v1/datasets/{name}/rows": { "get": { "summary": "Rows, one page at a time", "parameters": rows_parameters, "responses": { "200": ok("A page of rows", json!({ "$ref": "#/components/schemas/Rows" })), "400": error, "404": error, "503": error } } },
            "/api/v1/datasets/{name}/rows.parquet": { "get": { "summary": "Rows as a Parquet file (up to 5,000,000)", "parameters": parquet_parameters, "responses": { "200": { "description": "Parquet file", "content": { "application/vnd.apache.parquet": {} } }, "400": error, "404": error, "413": error, "503": error } } },
            "/api/v1/datasets/{name}/summary": { "get": { "summary": "Grouped totals, as in the Chart view", "parameters": [
                name,
                query("group", "Column to group by", json!({ "type": "string" })),
                query("aggregate", "count, sum, avg, min or max", json!({ "enum": ["count", "sum", "avg", "min", "max"] })),
                query("measure", "Number column, for every aggregate except count", json!({ "type": "string" })),
                query("bucket", "For date columns: day, month, quarter or year", json!({ "enum": ["day", "month", "quarter", "year"] })),
                query("filter", "OData $filter expression", json!({ "type": "string" })),
            ], "responses": { "200": ok("Groups", json!({ "$ref": "#/components/schemas/Summary" })), "400": error, "404": error, "503": error } } },
        },
        "components": {
            "securitySchemes": {
                "token": { "type": "http", "scheme": "bearer", "description": "A personal API token from Settings" },
                "basic": { "type": "http", "scheme": "basic" },
            },
            "schemas": {
                "Error": { "type": "object", "properties": { "error": { "type": "object", "properties": { "code": { "type": "string" }, "message": { "type": "string" } } } } },
                "Dataset": { "type": "object", "properties": {
                    "name": { "type": "string" }, "key": { "type": "string" },
                    "source": { "enum": ["built-in", "uploaded"] }, "owner": { "type": ["string", "null"] },
                    "columns": { "type": "array", "items": { "type": "object", "properties": { "name": { "type": "string" }, "type": { "enum": ["boolean", "integer", "number", "decimal", "text", "date", "datetime"] } } } },
                } },
                "Rows": { "type": "object", "properties": {
                    "rows": { "type": "array", "items": { "type": "object" } },
                    "count": { "type": "integer", "description": "Present when count=true" },
                    "next_skip": { "type": ["integer", "null"], "description": "Pass as skip for the next page" },
                } },
                "Summary": { "type": "object", "properties": {
                    "groups": { "type": "array", "items": { "type": "object", "properties": { "label": { "type": "string" }, "value": { "type": "number" } } } },
                    "truncated": { "type": "boolean" },
                } },
            },
        },
    })
}

#[cfg(test)]
pub fn documented_paths_in_document() -> Vec<String> {
    openapi_document()["paths"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}
