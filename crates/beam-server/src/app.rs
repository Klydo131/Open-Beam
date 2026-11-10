//! Shared state and the route table.

use crate::{auth, chart, export, feed, people, table, uploads, web};
use axum::extract::DefaultBodyLimit;
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
use beam_engine::Engine;
use std::sync::Arc;

pub struct AppState {
    pub engine: Engine,
    pub max_page_size: u64,
    /// Largest file accepted on the Add data page, in bytes.
    pub max_upload_bytes: usize,
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
        Self {
            engine,
            max_page_size,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_MB * 1024 * 1024,
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

pub const DEFAULT_MAX_UPLOAD_MB: usize = 50;

pub fn router(state: Arc<AppState>) -> Router {
    // Room for the form's other fields around the file itself.
    let upload_limit = state.max_upload_bytes.saturating_add(64 * 1024);
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
        .route("/datasets/{name}", get(table::dataset))
        .route("/datasets/{name}/chart", get(chart::chart))
        .route("/datasets/{name}/export.csv", get(export::csv))
        .route("/datasets/{name}/export.xlsx", get(export::excel))
        .route("/datasets/{name}/share", get(uploads::sharing).post(uploads::share))
        .route("/datasets/{name}/share/{account}/remove", post(uploads::unshare))
        .route("/datasets/{name}/delete", post(uploads::delete))
        .route("/data/new", get(uploads::upload_form))
        .route(
            "/data",
            post(uploads::upload).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/people", get(people::people))
        .route("/people/{id}/contact", post(people::contact))
        .route("/account/mode", post(web::set_mode))
        .route("/account/profile", post(web::set_profile))
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
