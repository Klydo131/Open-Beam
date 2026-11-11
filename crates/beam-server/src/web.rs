//! The web interface: server-rendered pages with no client-side script.
//!
//! Every page is plain HTML produced by `maud`, which escapes all
//! interpolated values. State changes are form posts that carry the session's
//! anti-forgery token.

// Handlers return the page to show as the `Err` side of small helpers.
#![allow(clippy::result_large_err)]

use crate::app::AppState;
use crate::auth::{self, Signed};
use crate::catalog;
use crate::feed;
use axum::extract::{Form, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_accounts::{Account, AccountError, Mode, Role};
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

type App = State<Arc<AppState>>;

// ---------------------------------------------------------------- responses

pub fn page(status: StatusCode, markup: Markup) -> Response {
    (
        status,
        [(CONTENT_TYPE, "text/html; charset=utf-8")],
        markup.into_string(),
    )
        .into_response()
}

pub fn redirect(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(LOCATION, to.to_string())]).into_response()
}

pub fn layout(title: &str, signed: Option<&Signed>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Open Beam" }
                link rel="stylesheet" href="/static/app.css";
                link rel="icon" href="/static/logo.svg" type="image/svg+xml";
            }
            body {
                header.topbar {
                    a.brand href="/" {
                        img src="/static/logo.svg" alt="" width="28" height="28";
                        span { "Open Beam" }
                    }
                    @if let Some(signed) = signed {
                        nav aria-label="Main" {
                            a href="/" { "Datasets" }
                            a href="/data/new" { "Add data" }
                            a href="/people" { "People" }
                            @if signed.session.account.role == Role::Admin {
                                a href="/admin/accounts" { "Accounts" }
                            }
                            a href="/account" { "Settings" }
                        }
                        form.signout method="post" action="/logout" {
                            input type="hidden" name="csrf" value=(signed.session.csrf);
                            span.who {
                                (signed.session.account.name())
                                span.badge { (role_label(signed.session.account.role)) }
                                a.badge.mode href="/account#interface" title="Change in Settings" {
                                    (if signed.session.account.mode == Mode::Advanced { "Advanced" } else { "Basic" })
                                }
                            }
                            button.quiet type="submit" { "Sign out" }
                        }
                    }
                }
                main { (body) }
            }
        }
    }
}

/// A small standalone page for errors and refusals.
pub fn message_page(status: StatusCode, heading: &str, detail: &str) -> Response {
    page(
        status,
        layout(
            heading,
            None,
            html! {
                section.panel.narrow {
                    h1 { (heading) }
                    p { (detail) }
                    p { a href="/" { "Back to Open Beam" } }
                }
            },
        ),
    )
}

pub async fn not_found() -> Response {
    message_page(
        StatusCode::NOT_FOUND,
        "Page not found",
        "There is nothing at this address.",
    )
}

pub async fn stylesheet() -> Response {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../static/app.css"),
    )
        .into_response()
}

pub async fn logo() -> Response {
    (
        [(CONTENT_TYPE, "image/svg+xml"), (CACHE_CONTROL, "public, max-age=3600")],
        include_str!("../../../assets/logo.svg"),
    )
        .into_response()
}

pub fn role_label(role: Role) -> &'static str {
    match role {
        Role::Admin => "Administrator",
        Role::User => "User",
    }
}

// ------------------------------------------------------------------- access

/// The signed-in visitor, or the redirect that sends them to sign in (or to
/// first-time set-up when no account exists yet).
pub fn require_signed_in(state: &AppState, headers: &HeaderMap) -> Result<Signed, Response> {
    if let Some(signed) = auth::signed_in(state, headers) {
        return Ok(signed);
    }
    let fresh = state.accounts.count().is_ok_and(|count| count == 0);
    Err(redirect(if fresh { "/setup" } else { "/login" }))
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<Signed, Response> {
    let signed = require_signed_in(state, headers)?;
    if signed.session.account.role == Role::Admin {
        Ok(signed)
    } else {
        Err(message_page(
            StatusCode::FORBIDDEN,
            "Administrators only",
            "Your account does not have permission to manage accounts.",
        ))
    }
}

pub fn check_csrf(signed: &Signed, submitted: &str) -> Result<(), Response> {
    if bool::from(submitted.as_bytes().ct_eq(signed.session.csrf.as_bytes())) {
        Ok(())
    } else {
        Err(message_page(
            StatusCode::FORBIDDEN,
            "Request refused",
            "This form has expired. Go back, reload the page and try again.",
        ))
    }
}

fn account_error_code(error: &AccountError) -> &'static str {
    match error {
        AccountError::InvalidUsername => "username",
        AccountError::InvalidPassword => "password",
        AccountError::UsernameTaken => "taken",
        AccountError::LastAdministrator => "last-admin",
        AccountError::NotFound => "not-found",
        AccountError::InvalidProfile => "profile",
        _ => "failed",
    }
}

pub fn notice(query: &HashMap<String, String>) -> Markup {
    let ok = match query.get("ok").map(String::as_str) {
        Some("created") => Some("Account created."),
        Some("role") => Some("Role updated."),
        Some("enabled") => Some("Account enabled."),
        Some("disabled") => Some("Account disabled. It has been signed out everywhere."),
        Some("reset") => Some("Password reset. That account has been signed out everywhere."),
        Some("password") => Some("Your password has been changed. Other devices have been signed out."),
        Some("profile") => Some("Profile saved."),
        Some("mode") => Some("Interface updated."),
        Some("shared") => Some("Shared."),
        Some("unshared") => Some("No longer shared with that person."),
        Some("deleted") => Some("Dataset deleted."),
        Some("contact-added") => Some("Added to your contacts."),
        Some("contact-removed") => Some("Removed from your contacts."),
        Some("discarded") => Some("Draft discarded."),
        Some("token-revoked") => Some("Token revoked. Anything using it can no longer connect."),
        _ => None,
    };
    let error = match query.get("error").map(String::as_str) {
        Some("username") => Some(AccountError::InvalidUsername.to_string()),
        Some("password") => Some(AccountError::InvalidPassword.to_string()),
        Some("taken") => Some(AccountError::UsernameTaken.to_string()),
        Some("last-admin") => Some(AccountError::LastAdministrator.to_string()),
        Some("not-found") => Some(AccountError::NotFound.to_string()),
        Some("mismatch") => Some("The two passwords do not match.".to_string()),
        Some("current") => Some("Your current password is not correct.".to_string()),
        Some("role") => Some("Choose a role.".to_string()),
        Some("profile") => Some(AccountError::InvalidProfile.to_string()),
        Some("confirm") => Some("Tick the box to confirm.".to_string()),
        Some("tokens") => Some(AccountError::TooManyTokens.to_string()),
        Some("name-taken") => Some("That dataset name is already in use. Choose another.".to_string()),
        Some(_) => Some("That did not work. Please try again.".to_string()),
        None => None,
    };
    html! {
        @if let Some(text) = ok { p.notice.ok role="status" { (text) } }
        @if let Some(text) = error { p.notice.error role="alert" { (text) } }
    }
}

/// Runs an account-store call that hashes a password on a blocking thread.
async fn blocking<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&AppState) -> Result<T, AccountError> + Send + 'static,
) -> Result<T, AccountError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || work(&state))
        .await
        .unwrap_or(Err(AccountError::Hashing))
}

fn start_session(state: &AppState, account: &Account, to: &str) -> Response {
    match state.accounts.start_session(account.id) {
        Ok(session) => {
            let mut response = redirect(to);
            response
                .headers_mut()
                .insert(SET_COOKIE, auth::session_cookie(state, &session.token));
            response
        }
        Err(_) => message_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "Could not sign you in.",
        ),
    }
}

// ----------------------------------------------------------- set-up, sign-in

#[derive(Deserialize)]
pub struct SetupForm {
    username: String,
    password: String,
    confirm: String,
}

fn setup_page(status: StatusCode, username: &str, error: Option<&str>) -> Response {
    page(
        status,
        layout(
            "Set up",
            None,
            html! {
                section.panel.narrow {
                    h1 { "Welcome to Open Beam" }
                    p { "Create the administrator account. The administrator adds accounts for everyone else." }
                    @if let Some(error) = error { p.notice.error role="alert" { (error) } }
                    form method="post" action="/setup" {
                        label for="username" { "Username" }
                        input #username name="username" value=(username) required autofocus autocomplete="username"
                            minlength="3" maxlength="32";
                        label for="password" { "Password" }
                        input #password name="password" type="password" required autocomplete="new-password"
                            minlength=(beam_accounts::MIN_PASSWORD_CHARS);
                        p.hint { "At least " (beam_accounts::MIN_PASSWORD_CHARS) " characters. A few unrelated words work well." }
                        label for="confirm" { "Repeat password" }
                        input #confirm name="confirm" type="password" required autocomplete="new-password";
                        button type="submit" { "Create administrator" }
                    }
                }
            },
        ),
    )
}

pub async fn setup_form(State(state): App) -> Response {
    match state.accounts.count() {
        Ok(0) => setup_page(StatusCode::OK, "", None),
        _ => redirect("/login"),
    }
}

pub async fn setup(State(state): App, Form(form): Form<SetupForm>) -> Response {
    if form.password != form.confirm {
        return setup_page(
            StatusCode::BAD_REQUEST,
            &form.username,
            Some("The two passwords do not match."),
        );
    }
    let (username, password) = (form.username.clone(), form.password);
    match blocking(&state, move |s| s.accounts.create_first_admin(&username, &password)).await {
        Ok(account) => start_session(&state, &account, "/"),
        Err(AccountError::AlreadySetUp) => redirect("/login"),
        Err(e) => setup_page(StatusCode::BAD_REQUEST, &form.username, Some(&e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

fn login_page(status: StatusCode, username: &str, error: Option<&str>) -> Response {
    page(
        status,
        layout(
            "Sign in",
            None,
            html! {
                section.panel.narrow {
                    h1 { "Sign in" }
                    @if let Some(error) = error { p.notice.error role="alert" { (error) } }
                    form method="post" action="/login" {
                        label for="username" { "Username" }
                        input #username name="username" value=(username) required autofocus autocomplete="username";
                        label for="password" { "Password" }
                        input #password name="password" type="password" required autocomplete="current-password";
                        button type="submit" { "Sign in" }
                    }
                    p.hint { "No account? Ask your Open Beam administrator to create one." }
                }
            },
        ),
    )
}

pub async fn login_form(State(state): App, headers: HeaderMap) -> Response {
    if auth::signed_in(&state, &headers).is_some() {
        return redirect("/");
    }
    match state.accounts.count() {
        Ok(0) => redirect("/setup"),
        _ => login_page(StatusCode::OK, "", None),
    }
}

pub async fn login(State(state): App, Form(form): Form<LoginForm>) -> Response {
    match auth::check_password(&state, &form.username, &form.password).await {
        Ok(Some(account)) => start_session(&state, &account, "/"),
        // One message for every failure, so the page does not reveal which usernames exist.
        Ok(None) => login_page(
            StatusCode::UNAUTHORIZED,
            &form.username,
            Some("Incorrect username or password."),
        ),
        Err(refused) => login_page(StatusCode::TOO_MANY_REQUESTS, &form.username, Some(refused.message())),
    }
}

#[derive(Deserialize)]
pub struct CsrfOnly {
    csrf: String,
}

pub async fn logout(State(state): App, headers: HeaderMap, Form(form): Form<CsrfOnly>) -> Response {
    if let Some(signed) = auth::signed_in(&state, &headers) {
        if let Err(response) = check_csrf(&signed, &form.csrf) {
            return response;
        }
        let _ = state.accounts.end_session(&signed.token);
    }
    let mut response = redirect("/login");
    response.headers_mut().insert(SET_COOKIE, auth::expired_cookie());
    response
}

// ----------------------------------------------------------------- datasets

pub async fn home(State(state): App, Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let me = &signed.session.account;
    let entries = catalog::visible(&state, me);
    let drafts = state.accounts.drafts_of(me.id).unwrap_or_default();
    let people = state.accounts.list().unwrap_or_default();
    let owner_name = |id: i64| {
        people
            .iter()
            .find(|a| a.id == id)
            .map_or("someone".to_string(), |a| a.name().to_string())
    };
    let mine: Vec<&catalog::Entry> = entries
        .iter()
        .filter(|e| e.upload.as_ref().is_some_and(|u| u.owner_id == me.id))
        .collect();
    let others: Vec<&catalog::Entry> = entries
        .iter()
        .filter(|e| e.upload.as_ref().is_some_and(|u| u.owner_id != me.id))
        .collect();
    let built_in: Vec<&catalog::Entry> = entries.iter().filter(|e| e.upload.is_none()).collect();
    let feed_url = format!("{}/odata/", feed::base_url(&state, &headers));
    let card = |entry: &catalog::Entry, note: String| {
        html! {
            li {
                a.card href={ "/datasets/" (entry.set.name) } {
                    strong { (entry.set.name) }
                    span { (entry.set.properties.len()) " columns · " (note) }
                }
            }
        }
    };
    let others_heading = if me.role == Role::Admin {
        "Added by others"
    } else {
        "Shared with you"
    };

    let body = html! {
        div.titlebar {
            h1 { "Datasets" }
            div.actions { a.button href="/data/new" { "Add data" } }
        }
        (notice(&query))
        @if !drafts.is_empty() {
            section.panel.attention {
                h2 { "Waiting for your review" }
                p.hint { "Only you can see these. Review each one and press Save to turn it into a dataset." }
                ul.people {
                    @for draft in &drafts {
                        li {
                            span { strong { (draft.original_name) } span.muted { " · added " (draft.created_at.get(..16).unwrap_or("")) " UTC" } }
                            a.button href={ "/data/drafts/" (draft.id) } { "Review" }
                        }
                    }
                }
            }
        }
        @if entries.is_empty() && drafts.is_empty() {
            section.panel.narrow {
                h2 { "Start with your own data" }
                p { "Add a CSV, Excel or Parquet file. You can explore it here, chart it, share it with colleagues, and open it in Excel or Power BI." }
                a.button href="/data/new" { "Add data" }
            }
        }
        @if !mine.is_empty() {
            h2.section { "Yours" }
            ul.cards { @for entry in &mine { (card(entry, format!("added {}", entry.upload.as_ref().map_or("", |u| u.created_at.get(..10).unwrap_or(""))))) } }
        }
        @if !others.is_empty() {
            h2.section { (others_heading) }
            ul.cards { @for entry in &others { (card(entry, format!("from {}", entry.upload.as_ref().map_or("someone".to_string(), |u| owner_name(u.owner_id))))) } }
        }
        @if !built_in.is_empty() {
            h2.section { "Shared with everyone" }
            ul.cards { @for entry in &built_in { (card(entry, "set up by your administrator".to_string())) } }
        }
        section.panel {
            h2 { "Use this data in Excel or Power BI" }
            ol {
                li { "In Excel choose Data → Get Data → From Other Sources → From OData Feed. In Power BI Desktop choose Get Data → OData feed." }
                li { "Enter this address: " code { (feed_url) } }
                li { "When asked how to sign in, choose " strong { "Basic" } " and enter your Open Beam username and password." }
            }
            p.hint { "You will see the same datasets there as here. To download one view instead, use " strong { "Download Excel" } " on its page." }
        }
    };
    page(StatusCode::OK, layout("Datasets", Some(&signed), body))
}

// --------------------------------------------------------------- my account

pub async fn account(State(state): App, Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let me = &signed.session.account;
    let csrf = &signed.session.csrf;
    let advanced = me.mode == Mode::Advanced;
    let body = html! {
        h1 { "Settings" }
        (notice(&query))
        section.panel.narrow #interface {
            h2 { "Interface" }
            form method="post" action="/account/mode" {
                input type="hidden" name="csrf" value=(csrf);
                label.choice {
                    input type="radio" name="mode" value="basic" checked[!advanced];
                    span { strong { "Basic" } " — the essentials, kept simple: browse, filter, chart, share and download." }
                }
                label.choice {
                    input type="radio" name="mode" value="advanced" checked[advanced];
                    span { strong { "Advanced" } " — adds column choice, more filters, naming uploads, and ready-made connections for Excel and Power BI." }
                }
                button type="submit" { "Save" }
            }
            p.hint { "This only changes what you see. It never changes what you are allowed to do." }
        }
        section.panel.narrow {
            h2 { "Profile" }
            p.hint { "How colleagues see you on the People page and when you share data." }
            form method="post" action="/account/profile" {
                input type="hidden" name="csrf" value=(csrf);
                label for="display_name" { "Name" }
                input #display_name name="display_name" value=(me.display_name.as_deref().unwrap_or("")) maxlength=(beam_accounts::MAX_PROFILE_CHARS) autocomplete="name";
                label for="team" { "Team or role" }
                input #team name="team" value=(me.team.as_deref().unwrap_or("")) maxlength=(beam_accounts::MAX_PROFILE_CHARS) autocomplete="organization-title";
                button type="submit" { "Save profile" }
            }
        }
        (tokens_section(&state, &signed, None))
        section.panel.narrow {
            h2 { "Change password" }
            form method="post" action="/account/password" {
                input type="hidden" name="csrf" value=(csrf);
                label for="current" { "Current password" }
                input #current name="current" type="password" required autocomplete="current-password";
                label for="password" { "New password" }
                input #password name="password" type="password" required autocomplete="new-password"
                    minlength=(beam_accounts::MIN_PASSWORD_CHARS);
                label for="confirm" { "Repeat new password" }
                input #confirm name="confirm" type="password" required autocomplete="new-password";
                button type="submit" { "Change password" }
            }
        }
        section.panel.narrow {
            h2 { "Account" }
            dl {
                dt { "Username" } dd { (me.username) }
                dt { "Role" } dd { (role_label(me.role)) }
                dt { "Created" } dd { (me.created_at) " UTC" }
            }
        }
    };
    page(StatusCode::OK, layout("Settings", Some(&signed), body))
}

/// The API tokens part of Settings. `new_secret` is shown only on the page
/// that created it.
fn tokens_section(state: &AppState, signed: &Signed, new_secret: Option<&str>) -> Markup {
    let tokens = state.accounts.tokens(signed.session.account.id).unwrap_or_default();
    let csrf = &signed.session.csrf;
    html! {
        section.panel.narrow #tokens {
            h2 { "API tokens" }
            p.hint { "For tools that connect on your behalf: the API, AI assistants, scripts. A token can read what you can read, nothing more. Send it as " code { "Authorization: Bearer <token>" } "." }
            @if let Some(secret) = new_secret {
                div.notice.ok role="status" {
                    p { strong { "Copy your new token now." } " It will not be shown again." }
                    input.copy readonly value=(secret) aria-label="Your new API token";
                }
            }
            @if !tokens.is_empty() {
                ul.people {
                    @for token in &tokens {
                        li {
                            span {
                                strong { (token.name) }
                                span.muted { " · created " (token.created_at.get(..10).unwrap_or("")) " · " (token.last_used.as_deref().map_or("never used".to_string(), |t| format!("last used {}", t.get(..16).unwrap_or(t)))) }
                            }
                            form.row method="post" action={ "/account/tokens/" (token.id) "/revoke" } {
                                input type="hidden" name="csrf" value=(csrf);
                                button.quiet type="submit" { "Revoke" }
                            }
                        }
                    }
                }
            }
            form method="post" action="/account/tokens" {
                input type="hidden" name="csrf" value=(csrf);
                label for="token_name" { "New token name" }
                input #token_name name="name" required maxlength=(beam_accounts::MAX_PROFILE_CHARS) placeholder="for example: Claude on my laptop";
                button type="submit" { "Create token" }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct TokenForm {
    csrf: String,
    name: String,
}

/// Creates a token and shows its secret once, on this response only.
pub async fn create_token(State(state): App, headers: HeaderMap, Form(form): Form<TokenForm>) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    match state.accounts.create_token(signed.session.account.id, &form.name) {
        Ok((_, secret)) => {
            let body = html! {
                h1 { "Settings" }
                (tokens_section(&state, &signed, Some(&secret)))
                p { a href="/account" { "Back to all settings" } }
            };
            page(StatusCode::OK, layout("API token created", Some(&signed), body))
        }
        Err(AccountError::TooManyTokens) => redirect("/account?error=tokens#tokens"),
        Err(e) => redirect(&format!("/account?error={}#tokens", account_error_code(&e))),
    }
}

pub async fn revoke_token(
    State(state): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfOnly>,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    match state.accounts.revoke_token(signed.session.account.id, id) {
        Ok(()) => redirect("/account?ok=token-revoked#tokens"),
        Err(_) => redirect("/account?error=not-found#tokens"),
    }
}

#[derive(Deserialize)]
pub struct ModeForm {
    csrf: String,
    mode: String,
}

pub async fn set_mode(State(state): App, headers: HeaderMap, Form(form): Form<ModeForm>) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let Some(mode) = Mode::parse(&form.mode) else {
        return redirect("/account?error=failed");
    };
    match state.accounts.set_mode(signed.session.account.id, mode) {
        Ok(()) => redirect("/account?ok=mode"),
        Err(e) => redirect(&format!("/account?error={}", account_error_code(&e))),
    }
}

#[derive(Deserialize)]
pub struct ProfileForm {
    csrf: String,
    display_name: String,
    team: String,
}

pub async fn set_profile(State(state): App, headers: HeaderMap, Form(form): Form<ProfileForm>) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    match state
        .accounts
        .set_profile(signed.session.account.id, &form.display_name, &form.team)
    {
        Ok(()) => redirect("/account?ok=profile"),
        Err(e) => redirect(&format!("/account?error={}", account_error_code(&e))),
    }
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    csrf: String,
    current: String,
    password: String,
    confirm: String,
}

pub async fn change_password(State(state): App, headers: HeaderMap, Form(form): Form<ChangePasswordForm>) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    if form.password != form.confirm {
        return redirect("/account?error=mismatch");
    }
    let me = signed.session.account.clone();
    match auth::check_password(&state, &me.username, &form.current).await {
        Ok(Some(_)) => {}
        _ => return redirect("/account?error=current"),
    }
    let (id, password) = (me.id, form.password);
    match blocking(&state, move |s| s.accounts.set_password(id, &password)).await {
        Ok(()) => {
            state.guard.forget_verified();
            // Changing the password ended every session, including this one.
            start_session(&state, &me, "/account?ok=password")
        }
        Err(e) => redirect(&format!("/account?error={}", account_error_code(&e))),
    }
}

// ------------------------------------------------------- account management

pub async fn accounts(State(state): App, Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let signed = match require_admin(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let list = state.accounts.list().unwrap_or_default();
    let csrf = &signed.session.csrf;
    let body = html! {
        h1 { "Accounts" }
        (notice(&query))
        section.panel {
            h2 { "Add an account" }
            form.inline method="post" action="/admin/accounts" {
                input type="hidden" name="csrf" value=(csrf);
                div { label for="new-username" { "Username" }
                    input #new-username name="username" required minlength="3" maxlength="32" autocomplete="off"; }
                div { label for="new-password" { "Temporary password" }
                    input #new-password name="password" type="password" required autocomplete="new-password"
                        minlength=(beam_accounts::MIN_PASSWORD_CHARS); }
                div { label for="new-role" { "Role" }
                    select #new-role name="role" {
                        option value="user" { "User — can read datasets" }
                        option value="admin" { "Administrator — can also manage accounts" }
                    } }
                button type="submit" { "Add account" }
            }
            p.hint { "Give the person their username and temporary password privately, and ask them to change it under My account." }
        }
        div.tablewrap {
            table.accounts {
                thead { tr { th { "Username" } th { "Role" } th { "Status" } th { "Last sign-in (UTC)" } th { "Reset password" } } }
                tbody {
                    @for account in &list {
                        tr {
                            td { (account.username) @if account.id == signed.session.account.id { span.badge { "you" } } }
                            td {
                                form.row method="post" action={ "/admin/accounts/" (account.id) "/role" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    select name="role" aria-label={ "Role for " (account.username) } {
                                        option value="user" selected[account.role == Role::User] { "User" }
                                        option value="admin" selected[account.role == Role::Admin] { "Administrator" }
                                    }
                                    button.quiet type="submit" { "Save" }
                                }
                            }
                            td {
                                form.row method="post" action={ "/admin/accounts/" (account.id) "/active" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    input type="hidden" name="active" value=(if account.active { "false" } else { "true" });
                                    span class=(if account.active { "status on" } else { "status off" }) { (if account.active { "Active" } else { "Disabled" }) }
                                    button.quiet type="submit" { (if account.active { "Disable" } else { "Enable" }) }
                                }
                            }
                            td { (account.last_sign_in.as_deref().unwrap_or("Never")) }
                            td {
                                form.row method="post" action={ "/admin/accounts/" (account.id) "/password" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    input name="password" type="password" required autocomplete="new-password"
                                        minlength=(beam_accounts::MIN_PASSWORD_CHARS)
                                        aria-label={ "New password for " (account.username) } placeholder="new password";
                                    button.quiet type="submit" { "Reset" }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    page(StatusCode::OK, layout("Accounts", Some(&signed), body))
}

/// Shared tail of every account-management action.
fn after_change(state: &AppState, result: Result<(), AccountError>, ok: &str) -> Response {
    state.guard.forget_verified();
    match result {
        Ok(()) => redirect(&format!("/admin/accounts?ok={ok}")),
        Err(e) => redirect(&format!("/admin/accounts?error={}", account_error_code(&e))),
    }
}

fn admin_post(state: &AppState, headers: &HeaderMap, csrf: &str) -> Result<Signed, Response> {
    let signed = require_admin(state, headers)?;
    check_csrf(&signed, csrf)?;
    Ok(signed)
}

#[derive(Deserialize)]
pub struct CreateAccountForm {
    csrf: String,
    username: String,
    password: String,
    role: String,
}

pub async fn create_account(State(state): App, headers: HeaderMap, Form(form): Form<CreateAccountForm>) -> Response {
    if let Err(response) = admin_post(&state, &headers, &form.csrf) {
        return response;
    }
    let Some(role) = Role::parse(&form.role) else {
        return redirect("/admin/accounts?error=role");
    };
    let result = blocking(&state, move |s| {
        s.accounts.create(&form.username, &form.password, role).map(|_| ())
    })
    .await;
    after_change(&state, result, "created")
}

#[derive(Deserialize)]
pub struct RoleForm {
    csrf: String,
    role: String,
}

pub async fn set_role(
    State(state): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<RoleForm>,
) -> Response {
    if let Err(response) = admin_post(&state, &headers, &form.csrf) {
        return response;
    }
    let Some(role) = Role::parse(&form.role) else {
        return redirect("/admin/accounts?error=role");
    };
    after_change(&state, state.accounts.set_role(id, role), "role")
}

#[derive(Deserialize)]
pub struct ActiveForm {
    csrf: String,
    active: bool,
}

pub async fn set_active(
    State(state): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<ActiveForm>,
) -> Response {
    if let Err(response) = admin_post(&state, &headers, &form.csrf) {
        return response;
    }
    let ok = if form.active { "enabled" } else { "disabled" };
    after_change(&state, state.accounts.set_active(id, form.active), ok)
}

#[derive(Deserialize)]
pub struct ResetForm {
    csrf: String,
    password: String,
}

pub async fn reset_password(
    State(state): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<ResetForm>,
) -> Response {
    let signed = match admin_post(&state, &headers, &form.csrf) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let result = blocking(&state, move |s| s.accounts.set_password(id, &form.password)).await;
    if result.is_ok() && id == signed.session.account.id {
        // An administrator who resets their own password stays signed in.
        state.guard.forget_verified();
        return start_session(&state, &signed.session.account, "/admin/accounts?ok=reset");
    }
    after_change(&state, result, "reset")
}
