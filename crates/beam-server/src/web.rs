//! The web interface: server-rendered pages with no client-side script.
//!
//! Every page is plain HTML produced by `maud`, which escapes all
//! interpolated values. State changes are form posts that carry the session's
//! anti-forgery token.

// Handlers return the page to show as the `Err` side of small helpers.
#![allow(clippy::result_large_err)]

use crate::app::AppState;
use crate::auth::{self, Signed};
use crate::feed;
use axum::extract::{Form, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_accounts::{Account, AccountError, Role};
use beam_odata::{EdmType, EntitySet, Property};
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

const PAGE_ROWS: u64 = 50;
const OPERATORS: &[(&str, &str)] = &[
    ("eq", "equals"),
    ("contains", "contains"),
    ("ne", "does not equal"),
    ("gt", "is greater than"),
    ("ge", "is at least"),
    ("lt", "is less than"),
    ("le", "is at most"),
];

type App = State<Arc<AppState>>;

// ---------------------------------------------------------------- responses

fn page(status: StatusCode, markup: Markup) -> Response {
    (
        status,
        [(CONTENT_TYPE, "text/html; charset=utf-8")],
        markup.into_string(),
    )
        .into_response()
}

fn redirect(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(LOCATION, to.to_string())]).into_response()
}

fn layout(title: &str, signed: Option<&Signed>, body: Markup) -> Markup {
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
                            @if signed.session.account.role == Role::Admin {
                                a href="/admin/accounts" { "Accounts" }
                            }
                            a href="/account" { "My account" }
                        }
                        form.signout method="post" action="/logout" {
                            input type="hidden" name="csrf" value=(signed.session.csrf);
                            span.who {
                                (signed.session.account.username)
                                span.badge { (role_label(signed.session.account.role)) }
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

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Admin => "Administrator",
        Role::User => "User",
    }
}

// ------------------------------------------------------------------- access

/// The signed-in visitor, or the redirect that sends them to sign in (or to
/// first-time set-up when no account exists yet).
fn require_signed_in(state: &AppState, headers: &HeaderMap) -> Result<Signed, Response> {
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

fn check_csrf(signed: &Signed, submitted: &str) -> Result<(), Response> {
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
        _ => "failed",
    }
}

fn notice(query: &HashMap<String, String>) -> Markup {
    let ok = match query.get("ok").map(String::as_str) {
        Some("created") => Some("Account created."),
        Some("role") => Some("Role updated."),
        Some("enabled") => Some("Account enabled."),
        Some("disabled") => Some("Account disabled. It has been signed out everywhere."),
        Some("reset") => Some("Password reset. That account has been signed out everywhere."),
        Some("password") => Some("Your password has been changed. Other devices have been signed out."),
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
        Err(()) => login_page(
            StatusCode::TOO_MANY_REQUESTS,
            &form.username,
            Some("Too many failed attempts. Wait a few minutes and try again."),
        ),
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

pub async fn home(State(state): App, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let feed_url = format!("{}/odata/", feed::base_url(&state, &headers));
    let body = html! {
        h1 { "Datasets" }
        @if state.sets.is_empty() {
            p { "No datasets are configured yet." }
        }
        ul.cards {
            @for set in &state.sets {
                li {
                    a.card href={ "/datasets/" (set.name) } {
                        strong { (set.name) }
                        span { (set.properties.len()) " columns" }
                    }
                }
            }
        }
        section.panel {
            h2 { "Use this data in Excel or Power BI" }
            ol {
                li { "In Excel choose Data → Get Data → From Other Sources → From OData Feed. In Power BI Desktop choose Get Data → OData feed." }
                li { "Enter this address: " code { (feed_url) } }
                li { "When asked how to sign in, choose " strong { "Basic" } " and enter your Open Beam username and password." }
            }
        }
    };
    page(StatusCode::OK, layout("Datasets", Some(&signed), body))
}

#[derive(Deserialize, Default)]
pub struct TableQuery {
    #[serde(default)]
    page: Option<u64>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    column: Option<String>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

/// Turns the filter form into an OData `$filter` expression. Values are
/// written as OData literals; the planner then binds them as parameters.
fn filter_expression(property: &Property, op: &str, value: &str) -> Result<String, &'static str> {
    let value = value.trim();
    let name = &property.name;
    if op == "contains" {
        if property.edm != EdmType::String {
            return Err("“contains” works on text columns. Choose another comparison.");
        }
        return Ok(format!(
            "contains(tolower({name}), '{}')",
            value.to_lowercase().replace('\'', "''")
        ));
    }
    if !OPERATORS.iter().any(|(code, _)| *code == op) {
        return Err("Choose a comparison.");
    }
    let literal = match property.edm {
        EdmType::String => format!("'{}'", value.replace('\'', "''")),
        EdmType::Boolean => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" => "true".to_string(),
            "false" | "no" => "false".to_string(),
            _ => return Err("Enter true or false."),
        },
        EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. } => {
            if let Ok(whole) = value.parse::<i64>() {
                whole.to_string()
            } else {
                match value.parse::<f64>() {
                    Ok(number) if number.is_finite() => number.to_string(),
                    _ => return Err("Enter a number."),
                }
            }
        }
        EdmType::Date | EdmType::DateTimeOffset => {
            let shaped = value.len() >= 10 && value.bytes().all(|b| b.is_ascii_digit() || b"-:T.Z+".contains(&b));
            if !shaped {
                return Err("Enter a date as YYYY-MM-DD.");
            }
            value.to_string()
        }
    };
    Ok(format!("{name} {op} {literal}"))
}

fn table_link(set: &EntitySet, query: &TableQuery, page: u64, sort: Option<(&str, &str)>) -> String {
    let mut link = form_urlencoded::Serializer::new(String::new());
    let sort = sort.or(query.sort.as_deref().zip(query.dir.as_deref().or(Some("asc"))));
    if let Some((column, dir)) = sort {
        link.append_pair("sort", column).append_pair("dir", dir);
    }
    if let (Some(column), Some(op), Some(value)) = (&query.column, &query.op, &query.value) {
        if !value.trim().is_empty() {
            link.append_pair("column", column)
                .append_pair("op", op)
                .append_pair("value", value);
        }
    }
    if page > 1 {
        link.append_pair("page", &page.to_string());
    }
    let encoded = link.finish();
    let separator = if encoded.is_empty() { "" } else { "?" };
    format!("/datasets/{}{separator}{encoded}", set.name)
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Feed timestamps are ISO 8601 with microseconds. On a page, show them the
/// way a person writes them: `2025-01-07 09:15:00`, keeping any fraction.
fn readable_timestamp(text: &str) -> String {
    let text = text.strip_suffix('Z').unwrap_or(text).replacen('T', " ", 1);
    match text.split_once('.') {
        Some((whole, fraction)) if fraction.bytes().all(|b| b == b'0') => whole.to_string(),
        _ => text,
    }
}

fn cell(value: &Value, edm: EdmType) -> Markup {
    match value {
        Value::Null => html! { td.null { "—" } },
        Value::String(s) if edm == EdmType::DateTimeOffset => html! { td { (readable_timestamp(s)) } },
        Value::Number(n) => html! { td.num { (n.to_string()) } },
        Value::Bool(b) => html! { td { (if *b { "true" } else { "false" }) } },
        Value::String(s) => html! { td { (s) } },
        other => html! { td { (other.to_string()) } },
    }
}

pub async fn dataset(
    State(state): App,
    Path(name): Path<String>,
    Query(query): Query<TableQuery>,
    headers: HeaderMap,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let Some(set) = feed::find_set(&state, &name) else {
        return message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        );
    };

    let page_number = query.page.unwrap_or(1).clamp(1, 1_000_000);
    let descending = query.dir.as_deref() == Some("desc");
    let sort = query.sort.as_deref().filter(|column| set.property(column).is_some());
    let filter_value = query.value.as_deref().unwrap_or("").trim();
    let filter_column = query.column.as_deref().and_then(|column| set.property(column));

    let mut problem = None;
    let mut options = vec![
        ("$top".to_string(), PAGE_ROWS.to_string()),
        ("$skip".to_string(), ((page_number - 1) * PAGE_ROWS).to_string()),
        ("$count".to_string(), "true".to_string()),
    ];
    if let Some(column) = sort {
        options.push((
            "$orderby".into(),
            format!("{column} {}", if descending { "desc" } else { "asc" }),
        ));
    }
    if let (Some(property), false) = (filter_column, filter_value.is_empty()) {
        match filter_expression(property, query.op.as_deref().unwrap_or(""), filter_value) {
            Ok(expression) => options.push(("$filter".into(), expression)),
            Err(message) => problem = Some(message),
        }
    }

    let mut rows = Vec::new();
    let mut total = 0;
    match beam_odata::plan(set, &options, PAGE_ROWS) {
        Ok(plan) => {
            let width = plan.columns.len();
            let counted = match &plan.count_sql {
                Some(sql) => feed::run(&state, sql.clone(), &plan.count_params, 1).await,
                None => Ok(Vec::new()),
            };
            match (
                feed::run(&state, plan.rows_sql, &plan.rows_params, width).await,
                counted,
            ) {
                (Ok(mut fetched), Ok(count)) => {
                    fetched.truncate(PAGE_ROWS as usize);
                    rows = fetched;
                    total = count
                        .first()
                        .and_then(|r| r.first())
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
                _ => problem = Some("That filter could not be applied. Check the value and try again."),
            }
        }
        Err(_) => problem = Some("That filter could not be applied. Check the value and try again."),
    }

    let first = (page_number - 1) * PAGE_ROWS + 1;
    let last = first + rows.len() as u64 - u64::from(!rows.is_empty());
    let has_next = page_number * PAGE_ROWS < total;
    let filtered = filter_column.is_some() && !filter_value.is_empty();

    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / " (set.name) }
        h1 { (set.name) }
        form.filter method="get" action={ "/datasets/" (set.name) } {
            @if let Some(column) = sort {
                input type="hidden" name="sort" value=(column);
                input type="hidden" name="dir" value=(if descending { "desc" } else { "asc" });
            }
            label for="column" { "Show rows where" }
            select #column name="column" {
                @for property in &set.properties {
                    option value=(property.name) selected[query.column.as_deref() == Some(property.name.as_str())] { (property.name) }
                }
            }
            select name="op" aria-label="Comparison" {
                @for (code, label) in OPERATORS {
                    option value=(code) selected[query.op.as_deref() == Some(*code)] { (label) }
                }
            }
            input name="value" value=(filter_value) aria-label="Value" placeholder="value";
            button type="submit" { "Apply" }
            @if filtered { a.quiet href=(table_link(set, &TableQuery::default(), 1, sort.map(|c| (c, if descending { "desc" } else { "asc" })))) { "Clear" } }
        }
        @if let Some(problem) = problem { p.notice.error role="alert" { (problem) } }
        div.tablewrap {
            table {
                thead {
                    tr {
                        @for property in &set.properties {
                            @let active = sort == Some(property.name.as_str());
                            @let next = if active && !descending { "desc" } else { "asc" };
                            th aria-sort=[active.then_some(if descending { "descending" } else { "ascending" })] {
                                a href=(table_link(set, &query, 1, Some((&property.name, next)))) {
                                    (property.name)
                                    @if active { span.arrow aria-hidden="true" { (if descending { " ▼" } else { " ▲" }) } }
                                }
                            }
                        }
                    }
                }
                tbody {
                    @for row in &rows {
                        tr { @for (value, property) in row.iter().zip(&set.properties) { (cell(value, property.edm)) } }
                    }
                }
            }
        }
        @if rows.is_empty() && problem.is_none() { p.empty { "No rows match." } }
        nav.pager aria-label="Pages" {
            @if !rows.is_empty() {
                span { "Rows " (thousands(first)) "–" (thousands(last)) " of " (thousands(total)) }
            }
            @if page_number > 1 { a href=(table_link(set, &query, page_number - 1, None)) { "← Previous" } }
            @if has_next { a href=(table_link(set, &query, page_number + 1, None)) { "Next →" } }
        }
    };
    page(StatusCode::OK, layout(&set.name, Some(&signed), body))
}

// --------------------------------------------------------------- my account

pub async fn account(State(state): App, Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let me = &signed.session.account;
    let body = html! {
        h1 { "My account" }
        (notice(&query))
        section.panel.narrow {
            dl {
                dt { "Username" } dd { (me.username) }
                dt { "Role" } dd { (role_label(me.role)) }
                dt { "Created" } dd { (me.created_at) " UTC" }
            }
        }
        section.panel.narrow {
            h2 { "Change password" }
            form method="post" action="/account/password" {
                input type="hidden" name="csrf" value=(signed.session.csrf);
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
    };
    page(StatusCode::OK, layout("My account", Some(&signed), body))
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
