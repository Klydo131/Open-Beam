//! Who is asking: browser sessions, feed credentials, and their limits.

use crate::app::AppState;
use crate::feed;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine as _;
use beam_accounts::{Account, Session};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SESSION_COOKIE: &str = "beam_session";
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const FEED_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_TRACKED: usize = 10_000;

/// In-memory limits and caches. Nothing here survives a restart.
#[derive(Default)]
pub struct Guard {
    /// Recent failed sign-ins per username.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    /// Feed clients send their password with every request. Verifying a
    /// password is deliberately slow, so a successful check is remembered
    /// briefly, keyed by a digest of the credentials.
    verified: Mutex<HashMap<[u8; 32], (i64, Instant)>>,
}

impl Guard {
    fn key(username: &str) -> String {
        username.trim().to_ascii_lowercase().chars().take(64).collect()
    }

    pub fn is_locked(&self, username: &str) -> bool {
        let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        failures
            .get(&Self::key(username))
            .is_some_and(|(count, since)| *count >= MAX_FAILURES && since.elapsed() < FAILURE_WINDOW)
    }

    pub fn record_failure(&self, username: &str) {
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        if failures.len() >= MAX_TRACKED {
            failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
        }
        if failures.len() >= MAX_TRACKED {
            failures.clear();
        }
        let entry = failures.entry(Self::key(username)).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= FAILURE_WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    pub fn record_success(&self, username: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&Self::key(username));
    }

    /// Called whenever any account changes, so a disabled account or an old
    /// password stops working on the feed at once.
    pub fn forget_verified(&self) {
        self.verified.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// The signed-in browser, if the request carries a valid session cookie.
pub struct Signed {
    pub token: String,
    pub session: Session,
}

pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, token)| token)
}

pub fn signed_in(state: &AppState, headers: &HeaderMap) -> Option<Signed> {
    let token = session_token(headers)?;
    let session = state.accounts.session(token).ok()??;
    Some(Signed {
        token: token.to_string(),
        session,
    })
}

pub fn session_cookie(state: &AppState, token: &str) -> HeaderValue {
    let secure = if state.secure_cookies() { "; Secure" } else { "" };
    let cookie = format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200{secure}");
    HeaderValue::from_str(&cookie).unwrap_or(HeaderValue::from_static(""))
}

pub fn expired_cookie() -> HeaderValue {
    HeaderValue::from_static("beam_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Checks a username and password off the async runtime, applying the
/// failed-attempt limit. `Err` means too many recent failures.
pub async fn check_password(state: &Arc<AppState>, username: &str, password: &str) -> Result<Option<Account>, ()> {
    if state.guard.is_locked(username) {
        return Err(());
    }
    let (worker, name, secret) = (state.clone(), username.to_string(), password.to_string());
    let verified = tokio::task::spawn_blocking(move || worker.accounts.verify(&name, &secret)).await;
    match verified {
        Ok(Ok(Some(account))) => {
            state.guard.record_success(username);
            Ok(Some(account))
        }
        Ok(Ok(None)) => {
            state.guard.record_failure(username);
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers.get(AUTHORIZATION)?.to_str().ok()?.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, password) = text.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// Admits a feed request that carries either a browser session or HTTP Basic
/// credentials for an active account.
pub async fn feed_guard(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let mut response = match admit(&state, request.headers()).await {
        Ok(Some(account)) => {
            request.extensions_mut().insert(Viewer(account));
            next.run(request).await
        }
        Ok(None) => {
            let mut denied = feed::error(
                StatusCode::UNAUTHORIZED,
                "Sign in with your Open Beam username and password",
            );
            denied
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Open Beam\""));
            denied
        }
        Err(()) => feed::error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed sign-in attempts. Try again in a few minutes.",
        ),
    };
    response
        .headers_mut()
        .insert("OData-Version", HeaderValue::from_static("4.0"));
    response
}

/// The account behind a feed request, for deciding which datasets it sees.
#[derive(Clone)]
pub struct Viewer(pub Account);

async fn admit(state: &Arc<AppState>, headers: &HeaderMap) -> Result<Option<Account>, ()> {
    if let Some(signed) = signed_in(state, headers) {
        return Ok(Some(signed.session.account));
    }
    let Some((username, password)) = basic_credentials(headers) else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    hasher.update(username.as_bytes());
    hasher.update([0]);
    hasher.update(password.as_bytes());
    let key: [u8; 32] = hasher.finalize().into();

    {
        let mut verified = state.guard.verified.lock().unwrap_or_else(|e| e.into_inner());
        verified.retain(|_, (_, at)| at.elapsed() < FEED_CACHE_TTL);
        if let Some((id, _)) = verified.get(&key) {
            // Re-read the account so a role change is seen at once; a
            // disabled account is refused even while remembered.
            return Ok(state.accounts.get(*id).ok().filter(|account| account.active));
        }
    }
    let Some(account) = check_password(state, &username, &password).await? else {
        return Ok(None);
    };
    let mut verified = state.guard.verified.lock().unwrap_or_else(|e| e.into_inner());
    if verified.len() < MAX_TRACKED {
        verified.insert(key, (account.id, Instant::now()));
    }
    Ok(Some(account))
}

/// Browsers label where a request came from. A form posted from another site
/// is refused before it reaches a handler; the per-session token in each form
/// is the second line of defence.
pub async fn same_site_posts_only(request: Request, next: Next) -> Response {
    let cross_site = request.method() == Method::POST
        && request
            .headers()
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|site| site != "same-origin" && site != "none");
    if cross_site {
        return crate::web::message_page(
            StatusCode::FORBIDDEN,
            "Request refused",
            "This form was submitted from another site.",
        );
    }
    next.run(request).await
}
