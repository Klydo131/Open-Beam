//! HTTP surface: a read-only OData v4 service over the engine's datasets.

use crate::config::Credentials;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HOST, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use beam_engine::{ColumnType, Engine, Param};
use beam_odata::{EdmType, EntitySet, Literal, Property};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use subtle::ConstantTimeEq;

const JSON: &str = "application/json;odata.metadata=minimal;charset=utf-8";

pub struct AppState {
    pub engine: Engine,
    pub sets: Vec<EntitySet>,
    pub max_page_size: u64,
    pub public_url: Option<String>,
    pub fallback_host: String,
    pub credentials: Option<Credentials>,
}

impl AppState {
    pub fn new(
        engine: Engine,
        max_page_size: u64,
        public_url: Option<String>,
        fallback_host: String,
        credentials: Option<Credentials>,
    ) -> Self {
        let sets = engine
            .datasets()
            .iter()
            .map(|d| EntitySet {
                name: d.name.clone(),
                key: d.key.clone(),
                properties: d
                    .columns
                    .iter()
                    .map(|c| Property {
                        name: c.name.clone(),
                        edm: match c.ty {
                            ColumnType::Boolean => EdmType::Boolean,
                            ColumnType::Int32 => EdmType::Int32,
                            ColumnType::Int64 => EdmType::Int64,
                            ColumnType::Float64 => EdmType::Double,
                            ColumnType::Decimal { precision, scale } => EdmType::Decimal { precision, scale },
                            ColumnType::Text => EdmType::String,
                            ColumnType::Date => EdmType::Date,
                            ColumnType::Timestamp => EdmType::DateTimeOffset,
                        },
                    })
                    .collect(),
            })
            .collect();
        Self {
            engine,
            sets,
            max_page_size,
            public_url,
            fallback_host,
            credentials,
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/odata", get(service_document))
        .route("/odata/", get(service_document))
        .route("/odata/$metadata", get(metadata))
        .route("/odata/{set}", get(collection))
        .route("/odata/{set}/$count", get(count))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

/// Enforces Basic authentication when configured and stamps the headers
/// every response carries.
async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let mut response = match &state.credentials {
        Some(expected) if !authorised(request.headers(), expected) => {
            let mut denied = error(StatusCode::UNAUTHORIZED, "Authentication required");
            denied
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Open Beam\""));
            denied
        }
        _ => next.run(request).await,
    };
    let headers = response.headers_mut();
    headers.insert("OData-Version", HeaderValue::from_static("4.0"));
    headers.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn authorised(headers: &HeaderMap, expected: &Credentials) -> bool {
    let Some(encoded) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Some(split) = decoded.iter().position(|b| *b == b':') else {
        return false;
    };
    let (username, password) = (&decoded[..split], &decoded[split + 1..]);
    // Both comparisons always run, so timing does not reveal which one failed.
    let user_ok = username.ct_eq(expected.username.as_bytes());
    let pass_ok = password.ct_eq(expected.password.as_bytes());
    bool::from(user_ok & pass_ok)
}

fn error(status: StatusCode, message: &str) -> Response {
    let code = status.canonical_reason().unwrap_or("Error").replace(' ', "");
    let body = json!({ "error": { "code": code, "message": message } });
    (status, [(CONTENT_TYPE, JSON)], body.to_string()).into_response()
}

fn odata_base(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = &state.public_url {
        return format!("{url}/odata");
    }
    // The Host header is echoed into links, so accept only host[:port] characters.
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)))
        .unwrap_or(&state.fallback_host);
    format!("http://{host}/odata")
}

async fn service_document(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let document = beam_odata::service_document(&odata_base(&state, &headers), &state.sets);
    ([(CONTENT_TYPE, JSON)], document.to_string()).into_response()
}

async fn metadata(State(state): State<Arc<AppState>>) -> Response {
    (
        [(CONTENT_TYPE, "application/xml;charset=utf-8")],
        beam_odata::metadata_xml(&state.sets),
    )
        .into_response()
}

fn find_set<'a>(state: &'a AppState, name: &str) -> Option<&'a EntitySet> {
    state.sets.iter().find(|s| s.name == name)
}

fn unknown_set() -> Response {
    error(StatusCode::NOT_FOUND, "Unknown entity set")
}

fn to_params(literals: &[Literal]) -> Vec<Param> {
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

/// Runs a planned statement off the async runtime. Engine errors at this
/// point come from the client's values (for example an impossible date), so
/// they are reported as a bad request; the detail goes to the server log only.
async fn run(
    state: &Arc<AppState>,
    sql: String,
    literals: &[Literal],
    width: usize,
) -> Result<Vec<Vec<Value>>, Response> {
    let (state, params) = (state.clone(), to_params(literals));
    let outcome = tokio::task::spawn_blocking(move || state.engine.query(&sql, &params, width)).await;
    match outcome {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(e)) => {
            eprintln!("query failed: {e}");
            Err(error(StatusCode::BAD_REQUEST, "The query could not be executed"))
        }
        Err(e) => {
            eprintln!("query task failed: {e}");
            Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
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

async fn collection(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(options): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let Some(set) = find_set(&state, &name) else {
        return unknown_set();
    };
    let plan = match beam_odata::plan(set, &options, state.max_page_size) {
        Ok(plan) => plan,
        Err(e) => return plan_error(e),
    };
    let mut rows = match run(&state, plan.rows_sql.clone(), &plan.rows_params, plan.columns.len()).await {
        Ok(rows) => rows,
        Err(response) => return response,
    };

    let base = odata_base(&state, &headers);
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
            Err(response) => return response,
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

async fn count(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(options): Query<Vec<(String, String)>>,
) -> Response {
    let Some(set) = find_set(&state, &name) else {
        return unknown_set();
    };
    let mut options: Vec<_> = options.into_iter().filter(|(k, _)| k == "$filter").collect();
    options.push(("$count".into(), "true".into()));
    let plan = match beam_odata::plan(set, &options, state.max_page_size) {
        Ok(plan) => plan,
        Err(e) => return plan_error(e),
    };
    let sql = plan.count_sql.unwrap_or_default();
    match run(&state, sql, &plan.count_params, 1).await {
        Ok(result) => ([(CONTENT_TYPE, "text/plain;charset=utf-8")], result[0][0].to_string()).into_response(),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use beam_engine::DatasetSpec;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(credentials: Option<Credentials>, max_page_size: u64) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sales.csv");
        std::fs::write(
            &path,
            "order_id,region,amount,sold_on,shipped_at\n\
             1,EU,10.5,2024-01-05,2024-01-06 08:30:00\n\
             2,US,20.0,2024-02-10,2024-02-11 09:00:00\n\
             3,EU,30.25,2024-03-15,2024-03-16 10:15:30\n\
             4,APAC,40.0,2024-04-20,\n",
        )
        .unwrap();
        let spec = DatasetSpec {
            name: "sales".into(),
            source: beam_engine::Source::File { path, format: None },
            key: Some("order_id".into()),
        };
        let engine = Engine::open(&[spec]).unwrap();
        let state = AppState::new(engine, max_page_size, None, "127.0.0.1:8787".into(), credentials);
        (router(Arc::new(state)), dir)
    }

    async fn send(app: &Router, uri: &str, authorization: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let mut request = axum::http::Request::builder().uri(uri).header(HOST, "beam.test:9000");
        if let Some(value) = authorization {
            request = request.header(AUTHORIZATION, value);
        }
        let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (parts.status, parts.headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn get_json(app: &Router, uri: &str) -> Value {
        let (status, _, body) = send(app, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn service_document_and_metadata() {
        let (app, _dir) = app(None, 100);
        let document = get_json(&app, "/odata/").await;
        assert_eq!(document["@odata.context"], "http://beam.test:9000/odata/$metadata");
        assert_eq!(document["value"][0]["name"], "sales");

        let (status, headers, xml) = send(&app, "/odata/$metadata", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["OData-Version"], "4.0");
        assert!(xml.contains("<Property Name=\"order_id\" Type=\"Edm.Int64\" Nullable=\"false\"/>"));
        assert!(xml.contains("<Property Name=\"sold_on\" Type=\"Edm.Date\"/>"));
        assert!(xml.contains("<Property Name=\"shipped_at\" Type=\"Edm.DateTimeOffset\" Precision=\"6\"/>"));
    }

    #[tokio::test]
    async fn collection_returns_typed_rows() {
        let (app, _dir) = app(None, 100);
        let body = get_json(&app, "/odata/sales").await;
        assert_eq!(body["@odata.context"], "http://beam.test:9000/odata/$metadata#sales");
        assert!(body.get("@odata.nextLink").is_none());
        assert_eq!(body["value"].as_array().unwrap().len(), 4);
        assert_eq!(
            body["value"][0],
            json!({
                "order_id": 1, "region": "EU", "amount": 10.5,
                "sold_on": "2024-01-05", "shipped_at": "2024-01-06T08:30:00.000000Z"
            })
        );
        assert_eq!(body["value"][3]["shipped_at"], Value::Null);
    }

    #[tokio::test]
    async fn filter_select_order_and_count() {
        let (app, _dir) = app(None, 100);
        let uri = "/odata/sales?$filter=region%20eq%20'EU'%20and%20sold_on%20ge%202024-02-01\
                   &$select=order_id,amount&$orderby=amount%20desc&$count=true";
        let body = get_json(&app, uri).await;
        assert_eq!(
            body["@odata.context"],
            "http://beam.test:9000/odata/$metadata#sales(order_id,amount)"
        );
        assert_eq!(body["@odata.count"], 1);
        assert_eq!(body["value"], json!([{ "order_id": 3, "amount": 30.25 }]));

        let (status, _, text) = send(&app, "/odata/sales/$count?$filter=amount%20gt%2015", None).await;
        assert_eq!((status, text.as_str()), (StatusCode::OK, "3"));
    }

    #[tokio::test]
    async fn pages_follow_next_links_to_the_end() {
        let (app, _dir) = app(None, 3);
        let first = get_json(&app, "/odata/sales?$orderby=order_id%20desc").await;
        assert_eq!(first["value"].as_array().unwrap().len(), 3);
        let next = first["@odata.nextLink"].as_str().unwrap();
        assert_eq!(
            next,
            "http://beam.test:9000/odata/sales?%24orderby=order_id+desc&%24skip=3"
        );

        let second = get_json(&app, next.strip_prefix("http://beam.test:9000").unwrap()).await;
        assert_eq!(
            second["value"],
            json!([{
                "order_id": 1, "region": "EU", "amount": 10.5,
                "sold_on": "2024-01-05", "shipped_at": "2024-01-06T08:30:00.000000Z"
            }])
        );
        assert!(second.get("@odata.nextLink").is_none());

        // An explicit $top within the page size is honoured without a next link.
        let top = get_json(&app, "/odata/sales?$top=2").await;
        assert_eq!(top["value"].as_array().unwrap().len(), 2);
        assert!(top.get("@odata.nextLink").is_none());
        // A larger $top is paged, and the link carries what remains.
        let big = get_json(&app, "/odata/sales?$top=4").await;
        assert!(big["@odata.nextLink"]
            .as_str()
            .unwrap()
            .ends_with("?%24top=1&%24skip=3"));
    }

    #[tokio::test]
    async fn hostile_input_is_rejected_or_inert() {
        let (app, _dir) = app(None, 100);
        let injection = "/odata/sales?$filter=region%20eq%20'x'';%20DROP%20VIEW%20sales;%20--'";
        assert_eq!(get_json(&app, injection).await["value"], json!([]));
        assert_eq!(
            get_json(&app, "/odata/sales").await["value"].as_array().unwrap().len(),
            4
        );

        for (uri, expected) in [
            ("/odata/sales?$filter=1%20eq%201;%20select%201", StatusCode::BAD_REQUEST),
            ("/odata/sales?$filter=read_csv('x')%20eq%201", StatusCode::BAD_REQUEST),
            ("/odata/sales?$select=nope", StatusCode::BAD_REQUEST),
            (
                "/odata/sales?$filter=sold_on%20eq%202024-99-99",
                StatusCode::BAD_REQUEST,
            ),
            ("/odata/sales?$expand=orders", StatusCode::NOT_IMPLEMENTED),
            ("/odata/information_schema.tables", StatusCode::NOT_FOUND),
            ("/odata/nope", StatusCode::NOT_FOUND),
        ] {
            let (status, _, body) = send(&app, uri, None).await;
            assert_eq!(status, expected, "{uri}: {body}");
            assert!(body.contains("\"error\""), "{uri}");
            assert!(!body.contains("sales.csv"), "error bodies must not reveal file paths");
        }
    }

    #[tokio::test]
    async fn basic_auth_guards_every_odata_route() {
        let credentials = Credentials {
            username: "analyst".into(),
            password: "s3cret:with-colon".into(),
        };
        let (app, _dir) = app(Some(credentials), 100);
        let good = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("analyst:s3cret:with-colon")
        );
        let bad = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("analyst:wrong")
        );

        for uri in [
            "/odata",
            "/odata/",
            "/odata/$metadata",
            "/odata/sales",
            "/odata/sales/$count",
        ] {
            let (status, headers, _) = send(&app, uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
            assert_eq!(headers[WWW_AUTHENTICATE], "Basic realm=\"Open Beam\"");
            assert_eq!(send(&app, uri, Some(&bad)).await.0, StatusCode::UNAUTHORIZED, "{uri}");
            assert_eq!(
                send(&app, uri, Some("Bearer nope")).await.0,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
            assert_eq!(send(&app, uri, Some(&good)).await.0, StatusCode::OK, "{uri}");
        }
        assert_eq!(send(&app, "/healthz", None).await.0, StatusCode::OK);
    }
}
