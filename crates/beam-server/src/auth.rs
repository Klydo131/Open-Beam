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
pub const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const FEED_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_TRACKED: usize = 10_000;

/// Password checks that may run at once. Each Argon2 check holds about
/// 19 MiB, so this bounds memory under a flood of sign-in attempts.
const CONCURRENT_PASSWORD_CHECKS: usize = 4;

/// Sign-in attempts that may wait for a free check. Beyond this the server
/// answers at once that it is busy, so a flood of guesses cannot queue up
/// in front of real people and delay their sign-in for many seconds.
pub const MAX_WAITING_CHECKS: usize = 32;

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
    waiting: std::sync::atomic::AtomicUsize,
}

/// Why a sign-in attempt was refused before the password was looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The username used up its attempts for now.
    Locked,
    /// Too many sign-in attempts are already waiting.
    Busy,
}

impl Refused {
    pub fn message(self) -> &'static str {
        match self {
            Refused::Locked => "Too many failed attempts. Wait a few minutes and try again.",
            Refused::Busy => "Sign-in is busy right now. Try again in a moment.",
        }
    }
}

/// One reserved attempt. It is released when dropped, so a request that is
/// abandoned part-way (the client disconnects) still gives its place back.
/// An attempt that never reported a result counts as a failure.
struct Reservation<'a> {
    guard: &'a Guard,
    key: String,
    succeeded: Option<bool>,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.guard.finish(&self.key, self.succeeded.unwrap_or(false));
    }
}

/// Counts a caller waiting for a password check; undone when dropped.
struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            attempts: Mutex::default(),
            verified: Mutex::default(),
            checks: tokio::sync::Semaphore::new(CONCURRENT_PASSWORD_CHECKS),
            waiting: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Guard {
    fn key(username: &str) -> String {
        username.trim().to_ascii_lowercase().chars().take(64).collect()
    }

    /// Reserves a sign-in attempt, or refuses when the username has used up
    /// its attempts. Reserving and checking happen under one lock.
    fn begin(&self, username: &str) -> Option<Reservation<'_>> {
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
        // A new window forgets old failures. Attempts still in flight keep
        // their places, so they cannot be overtaken by a fresh burst.
        if entry.since.elapsed() >= FAILURE_WINDOW {
            entry.failed = 0;
            entry.since = Instant::now();
        }
        if entry.failed + entry.pending >= MAX_FAILURES {
            return None;
        }
        entry.pending += 1;
        Some(Reservation {
            guard: self,
            key: Self::key(username),
            succeeded: None,
        })
    }

    fn finish(&self, key: &str, succeeded: bool) {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = attempts.get_mut(key) {
            entry.pending = entry.pending.saturating_sub(1);
            if succeeded {
                entry.failed = 0;
            } else {
                entry.failed += 1;
            }
        }
    }

    /// Attempts for `username` still marked as in flight.
    #[cfg(test)]
    pub fn pending(&self, username: &str) -> u32 {
        let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.get(&Self::key(username)).map_or(0, |a| a.pending)
    }

    /// Moves every window into the past, as if `by` had elapsed.
    #[cfg(test)]
    pub fn age_attempts(&self, by: Duration) {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        for entry in attempts.values_mut() {
            entry.since = entry.since.checked_sub(by).unwrap_or(entry.since);
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
/// failed-attempt limit and the bound on waiting attempts.
pub async fn check_password(state: &Arc<AppState>, username: &str, password: &str) -> Result<Option<Account>, Refused> {
    use std::sync::atomic::Ordering;
    // Counted before anything else, and undone when this attempt ends or
    // is abandoned.
    let waiting = Waiting(&state.guard.waiting);
    if state.guard.waiting.fetch_add(1, Ordering::SeqCst) >= MAX_WAITING_CHECKS {
        return Err(Refused::Busy);
    }
    let mut reservation = state.guard.begin(username).ok_or(Refused::Locked)?;
    let verified = match state.guard.checks.acquire().await {
        Ok(_permit) => {
            drop(waiting);
            let (worker, name, secret) = (state.clone(), username.to_string(), password.to_string());
            tokio::task::spawn_blocking(move || worker.accounts.verify(&name, &secret)).await
        }
        Err(_) => Ok(Ok(None)),
    };
    let account = match verified {
        Ok(Ok(account)) => account,
        _ => None,
    };
    reservation.succeeded = Some(account.is_some());
    Ok(account)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
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
        Err(refused) => feed::error(StatusCode::TOO_MANY_REQUESTS, refused.message()),
    };
    response
        .headers_mut()
        .insert("OData-Version", HeaderValue::from_static("4.0"));
    response
}

/// The account behind a feed request, for deciding which datasets it sees.
#[derive(Clone)]
pub struct Viewer(pub Account);

async fn admit(state: &Arc<AppState>, headers: &HeaderMap) -> Result<Option<Account>, Refused> {
    if let Some(signed) = signed_in(state, headers) {
        return Ok(Some(signed.session.account));
    }
    // A personal API token: a long random secret looked up by its digest.
    if let Some(secret) = bearer_token(headers) {
        return Ok(state.accounts.token_account(secret).ok().flatten());
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
