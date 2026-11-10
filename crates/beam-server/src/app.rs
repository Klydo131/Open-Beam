//! Shared state and the route table.

use crate::{auth, chart, feed, web};
use axum::extract::Request;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use beam_accounts::Store;
use beam_engine::{ColumnType, Engine};
use beam_odata::{EdmType, EntitySet, Property};
use std::sync::Arc;

pub struct AppState {
    pub engine: Engine,
    pub sets: Vec<EntitySet>,
    pub max_page_size: u64,
    pub public_url: Option<String>,
    pub fallback_host: String,
    pub accounts: Store,
    pub guard: auth::Guard,
}

impl AppState {
    pub fn new(
        engine: Engine,
        accounts: Store,
        max_page_size: u64,
        public_url: Option<String>,
        fallback_host: String,
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
            accounts,
            guard: auth::Guard::default(),
        }
    }

    /// Cookies are marked `Secure` when the site is published over HTTPS.
    pub fn secure_cookies(&self) -> bool {
        self.public_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let feed = Router::new()
        .route("/odata", get(feed::service_document))
        .route("/odata/", get(feed::service_document))
        .route("/odata/$metadata", get(feed::metadata))
        .route("/odata/{set}", get(feed::collection))
        .route("/odata/{set}/$count", get(feed::count))
        .layer(middleware::from_fn_with_state(state.clone(), auth::feed_guard));

    let pages = Router::new()
        .route("/", get(web::home))
        .route("/setup", get(web::setup_form).post(web::setup))
        .route("/login", get(web::login_form).post(web::login))
        .route("/logout", post(web::logout))
        .route("/datasets/{name}", get(web::dataset))
        .route("/datasets/{name}/chart", get(chart::chart))
        .route("/account", get(web::account))
        .route("/account/password", post(web::change_password))
        .route("/admin/accounts", get(web::accounts).post(web::create_account))
        .route("/admin/accounts/{id}/role", post(web::set_role))
        .route("/admin/accounts/{id}/active", post(web::set_active))
        .route("/admin/accounts/{id}/password", post(web::reset_password))
        .layer(middleware::from_fn(auth::same_site_posts_only));

    Router::new()
        .merge(feed)
        .merge(pages)
        .route("/static/app.css", get(web::stylesheet))
        .route("/static/logo.svg", get(web::logo))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(web::not_found)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Headers every response carries. Pages load nothing from other origins and
/// run no scripts, and the policy says so.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}
