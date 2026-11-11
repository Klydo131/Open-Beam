//! Who is asking: browser sessions, feed credentials, and their limits.

use crate::app::AppState;
use crate::feed;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN, WWW_AUTHENTICATE};
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

/// Password checks that may run at once. Each Argon2 check holds about
/// 19 MiB, so this bounds memory under a flood of sign-in attempts.
const CONCURRENT_PASSWORD_CHECKS: usize = 4;

/// Sign-in attempts for one username within the current window.
struct Attempts {
    failed: u32,
    /// Checks started but not finished. They count against the limit, so a
    /// burst of simultaneous guesses cannot all slip in before any is recorded.
    pending: u32,
    since: Instant,
}

/// In-memory limits and caches. Nothing here survives a restart.
pub struct Guard {
    attempts: Mutex<HashMap<String, Attempts>>,
    /// Feed clients send their password with every request. Verifying a
    /// password is deliberately slow, so a successful check is remembered
    /// briefly, keyed by a digest of the credentials.
    verified: Mutex<HashMap<[u8; 32], (i64, Instant)>>,
    checks: tokio::sync::Semaphore,
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            attempts: Mutex::default(),
            verified: Mutex::default(),
            checks: tokio::sync::Semaphore::new(CONCURRENT_PASSWORD_CHECKS),
        }
    }
}

impl Guard {
    fn key(username: &str) -> String {
        username.trim().to_ascii_lowercase().chars().take(64).collect()
    }

    /// Reserves a sign-in attempt, or refuses when the username has used up
    /// its attempts. Reserving and checking happen under one lock.
    fn begin(&self, username: &str) -> bool {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        if attempts.len() >= MAX_TRACKED {
            attempts.retain(|_, a| a.pending > 0 || a.since.elapsed() < FAILURE_WINDOW);
        }
        if attempts.len() >= MAX_TRACKED {
            attempts.retain(|_, a| a.pending > 0);
        }
        let entry = attempts.entry(Self::key(username)).or_insert(Attempts {
            failed: 0,
            pending: 0,
            since: Instant::now(),
        });
        if entry.since.elapsed() >= FAILURE_WINDOW && entry.pending == 0 {
            *entry = Attempts {
                failed: 0,
                pending: 0,
                since: Instant::now(),
            };
        }
        if entry.failed + entry.pending >= MAX_FAILURES {
            return false;
        }
        entry.pending += 1;
        true
    }

    fn finish(&self, username: &str, succeeded: bool) {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = attempts.get_mut(&Self::key(username)) {
            entry.pending = entry.pending.saturating_sub(1);
            if succeeded {
                entry.failed = 0;
            } else {
                entry.failed += 1;
            }
        }
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
    if !state.guard.begin(username) {
        return Err(());
    }
    let verified = match state.guard.checks.acquire().await {
        Ok(_permit) => {
            let (worker, name, secret) = (state.clone(), username.to_string(), password.to_string());
            tokio::task::spawn_blocking(move || worker.accounts.verify(&name, &secret)).await
        }
        Err(_) => Ok(Ok(None)),
    };
    let account = match verified {
        Ok(Ok(account)) => account,
        _ => None,
    };
    state.guard.finish(username, account.is_some());
    Ok(account)
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
///
/// Browsers that do not send that label still send `Origin` on a form post;
/// it must name this site. This also covers the sign-in form, which has no
/// session token yet.
pub async fn same_site_posts_only(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let labelled_cross_site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|site| site != "same-origin" && site != "none");
    let foreign_origin = headers.get(ORIGIN).is_some_and(|origin| {
        let origin = origin.to_str().unwrap_or("null");
        let host = headers.get(HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let ours = [format!("http://{host}"), format!("https://{host}")];
        !(ours.iter().any(|o| o == origin) || state.public_url.as_deref() == Some(origin))
    });
    if request.method() == Method::POST && (labelled_cross_site || foreign_origin) {
        return crate::web::message_page(
            StatusCode::FORBIDDEN,
            "Request refused",
            "This form was submitted from another site.",
        );
    }
    next.run(request).await
}
