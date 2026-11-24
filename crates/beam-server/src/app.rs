//! Shared state and the route table.

use crate::{api, auth, chart, curate, export, feed, mcp, people, review, system, table, uploads, web};
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use beam_accounts::Store;
use beam_engine::Engine;
use std::sync::Arc;

/// The figures of a dataset's profile, as the profile query returns them.
pub type Profile = Arc<Vec<Option<f64>>>;

pub struct AppState {
    pub engine: Engine,
    pub max_page_size: u64,
    /// Largest file accepted on the Add data page, in bytes.
    pub max_upload_bytes: usize,
    pub public_url: Option<String>,
    pub fallback_host: String,
    pub accounts: Store,
    pub guard: auth::Guard,
    /// Open connections, for the limits in [`crate::net`].
    pub connections: Arc<crate::net::Connections>,
    /// Review-page previews, worked out once per draft and kept while the
    /// draft waits. Bounded; anything evicted is simply worked out again.
    previews: std::sync::Mutex<std::collections::HashMap<String, Arc<beam_engine::Preview>>>,
    /// Dataset profiles, each with the engine generation it was worked out
    /// at, so it is reused until the datasets change. Bounded.
    profiles: std::sync::Mutex<std::collections::HashMap<String, (u64, Profile)>>,
    /// The model from the configuration, if one was written. Replaced as a
    /// whole when an administrator applies a change.
    pub model: std::sync::RwLock<Option<Arc<crate::model::Model>>>,
    /// The file the model is read from and saved to, when there is one.
    pub model_path: Option<std::path::PathBuf>,
    /// Held while a change to the model is applied, so two cannot overlap.
    pub model_edit: tokio::sync::Mutex<()>,
    /// The model's checks against the data and when they were made. An async
    /// lock, held while they run, so concurrent first views wait for one run.
    pub model_checks: tokio::sync::Mutex<Option<(std::time::Instant, Arc<crate::model::Checks>)>>,
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
            connections: Arc::default(),
            previews: std::sync::Mutex::default(),
            profiles: std::sync::Mutex::default(),
            model: std::sync::RwLock::new(None),
            model_path: None,
            model_edit: tokio::sync::Mutex::const_new(()),
            model_checks: tokio::sync::Mutex::const_new(None),
        }
    }

    pub fn remember_preview(&self, file: &str, preview: beam_engine::Preview) {
        const KEPT: usize = 64;
        let mut previews = self.previews.lock().unwrap_or_else(|e| e.into_inner());
        if previews.len() >= KEPT {
            previews.clear();
        }
        previews.insert(file.to_string(), Arc::new(preview));
    }

    pub fn remembered_preview(&self, file: &str) -> Option<Arc<beam_engine::Preview>> {
        self.previews
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(file)
            .cloned()
    }

    /// The model in force now.
    pub fn model(&self) -> Option<Arc<crate::model::Model>> {
        self.model.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The profile of `dataset` if it was worked out since the datasets last
    /// changed.
    pub fn remembered_profile(&self, dataset: &str) -> Option<Profile> {
        let generation = self.engine.generation();
        self.profiles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(dataset)
            .filter(|(at, _)| *at == generation)
            .map(|(_, figures)| figures.clone())
    }

    /// Keeps a profile worked out at engine generation `generation`.
    pub fn remember_profile(&self, dataset: &str, generation: u64, figures: Profile) {
        const KEPT: usize = 256;
        let mut profiles = self.profiles.lock().unwrap_or_else(|e| e.into_inner());
        if profiles.len() >= KEPT {
            profiles.clear();
        }
        profiles.insert(dataset.to_string(), (generation, figures));
    }

    pub fn forget_preview(&self, file: &str) {
        self.previews.lock().unwrap_or_else(|e| e.into_inner()).remove(file);
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
        .route("/api/v1", get(api::index))
        .route("/api/v1/openapi.json", get(api::openapi))
        .route("/api/v1/datasets", get(api::datasets))
        .route("/api/v1/datasets/{name}", get(api::one))
        .route("/api/v1/datasets/{name}/rows", get(api::rows_route))
        .route("/api/v1/datasets/{name}/rows.parquet", get(api::parquet))
        .route("/api/v1/datasets/{name}/summary", get(api::summary_route))
        .route("/api/v1/datasets/{name}/questions/{id}", get(api::question_route))
        .route("/api/v1/watches", get(api::watches_route))
        .route("/mcp", get(mcp::get).post(mcp::post))
        .route("/metrics", get(system::metrics_route))
        .layer(middleware::from_fn_with_state(state.clone(), auth::feed_guard));

    let pages = Router::new()
        .route("/", get(web::home))
        .route("/setup", get(web::setup_form).post(web::setup))
        .route("/login", get(web::login_form).post(web::login))
        .route("/logout", post(web::logout))
        .route("/datasets/{name}", get(table::dataset))
        .route("/datasets/{name}/chart", get(chart::chart))
        .route("/datasets/{name}/about", get(curate::about).post(curate::save_notes))
        .route("/datasets/{name}/questions", post(curate::add_question))
        .route("/datasets/{name}/questions/{id}/remove", post(curate::remove_question))
        .route("/datasets/{name}/watches", post(curate::add_watch))
        .route("/datasets/{name}/watches/{id}/run", post(curate::run_watch_now))
        .route("/datasets/{name}/watches/{id}/remove", post(curate::remove_watch))
        .route("/watches", get(curate::overview))
        .route("/admin/system", get(system::page_route))
        .route("/model", get(crate::model::page_route))
        .route("/model/edit", get(crate::model_edit::edit_route))
        .route("/model/plan", post(crate::model_edit::plan_route))
        .route("/model/apply", post(crate::model_edit::apply_route))
        .route("/datasets/{name}/export.csv", get(export::csv))
        .route("/datasets/{name}/export.xlsx", get(export::excel))
        .route("/datasets/{name}/share", get(uploads::sharing).post(uploads::share))
        .route("/datasets/{name}/share/{account}/remove", post(uploads::unshare))
        .route("/datasets/{name}/delete", post(uploads::delete))
        .route("/data/new", get(uploads::upload_form))
        .route("/data/drafts/{id}", get(review::review))
        .route("/data/drafts/{id}/save", post(review::save))
        .route("/data/drafts/{id}/discard", post(review::discard))
        .route(
            "/data",
            post(uploads::upload).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/people", get(people::people))
        .route("/people/{id}/contact", post(people::contact))
        .route("/account/mode", post(web::set_mode))
        .route("/account/tokens", post(web::create_token))
        .route("/account/tokens/{id}/revoke", post(web::revoke_token))
        .route("/account/profile", post(web::set_profile))
        .route("/account", get(web::account))
        .route("/account/password", post(web::change_password))
        .route("/admin/accounts", get(web::accounts).post(web::create_account))
        .route("/admin/accounts/{id}/role", post(web::set_role))
        .route("/admin/accounts/{id}/active", post(web::set_active))
        .route("/admin/accounts/{id}/password", post(web::reset_password))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::same_site_posts_only,
        ));

    Router::new()
        .merge(feed)
        .merge(pages)
        .route("/static/app.css", get(web::stylesheet))
        .route("/static/logo.svg", get(web::logo))
        .route("/healthz", get(|| async { "ok" }))
        .route("/robots.txt", get(robots))
        .route("/.well-known/security.txt", get(security_txt))
        .fallback(web::not_found)
        .layer(middleware::from_fn(crate::net::request_time_limit))
        .layer(middleware::from_fn_with_state(state.clone(), security_headers))
        .with_state(state)
}

/// True for a request another website made the browser send, other than
/// following a link to one of our pages. Such requests are refused before
/// they reach a handler: a hostile page cannot load our data as a script
/// or image, frame it, or time how long our answers take to learn about a
/// visitor (cross-site leaks). Browsers label every request with
/// `Sec-Fetch-Site`; clients that are not browsers (Excel, Power BI,
/// scripts) send no label and are unaffected.
pub fn cross_site_fetch(method: &Method, headers: &HeaderMap) -> bool {
    let label = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    match label("sec-fetch-site") {
        "" | "same-origin" | "same-site" | "none" => false,
        _ => {
            let link = (method == Method::GET || method == Method::HEAD)
                && label("sec-fetch-mode") == "navigate"
                && matches!(label("sec-fetch-dest"), "" | "document");
            !link
        }
    }
}

/// Headers every response carries. Pages load nothing from other origins and
/// run no scripts, and the policy says so.
async fn security_headers(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let mut response = if cross_site_fetch(request.method(), request.headers()) {
        (
            StatusCode::FORBIDDEN,
            "Requests from other websites are not accepted.\n",
        )
            .into_response()
    } else {
        next.run(request).await
    };
    let secure = state.secure_cookies();
    let headers = response.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // "same-origin", not "no-referrer": under "no-referrer" browsers send
    // `Origin: null` on form posts, which the cross-site check must refuse,
    // so every form on the site would stop working. Other sites still learn
    // nothing about the page a visitor came from.
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if secure {
            "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'; upgrade-insecure-requests"
        } else {
            "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'"
        }),
    );
    // Our pages share no window with other sites, and other sites may not
    // embed anything we serve.
    headers.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    headers.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    headers.insert("x-permitted-cross-domain-policies", HeaderValue::from_static("none"));
    // No page here needs a camera, a microphone, the visitor's location or
    // any other device, so none may ask.
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static(
            "accelerometer=(), bluetooth=(), browsing-topics=(), camera=(), display-capture=(), \
             geolocation=(), gyroscope=(), hid=(), magnetometer=(), microphone=(), midi=(), \
             payment=(), serial=(), usb=()",
        ),
    );
    // Private data: search engines and AI crawlers that honour these labels
    // keep nothing. Those that do not still meet the sign-in.
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow, noarchive, nosnippet, noimageindex, noai, noimageai"),
    );
    if secure {
        // Browsers then refuse plain HTTP to this site for a year, so a
        // network attacker cannot downgrade a visit to read the session.
        headers.insert(STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Every crawler, search engine and AI training bot is asked to stay out.
async fn robots() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nDisallow: /\n",
    )
}

/// Where to report a security problem (RFC 9116). The expiry moves with the
/// server's clock, so the file never goes stale.
async fn security_txt() -> impl IntoResponse {
    let expires = std::time::SystemTime::now() + std::time::Duration::from_secs(180 * 24 * 3600);
    let seconds = expires.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!(
            "Contact: https://github.com/Klydo131/Open-Beam/security/advisories/new\n\
             Expires: {}\n\
             Policy: https://github.com/Klydo131/Open-Beam/blob/main/SECURITY.md\n\
             Preferred-Languages: en\n",
            crate::net::rfc3339(seconds)
        ),
    )
}
