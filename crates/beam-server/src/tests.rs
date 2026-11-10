//! End-to-end tests through the router: pages, accounts and the data feed.

use crate::app::{router, AppState};
use crate::config;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, LOCATION, SET_COOKIE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use base64::Engine as _;
use beam_accounts::{Role, Store};
use beam_engine::{DatasetSpec, Engine, Source};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const ADMIN_PASSWORD: &str = "correct horse battery";
const USER_PASSWORD: &str = "another long password";

struct Site {
    app: Router,
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers.get(LOCATION).map_or("", |v| v.to_str().unwrap())
    }

    /// The `beam_session=...` pair from Set-Cookie, ready to send back.
    fn cookie(&self) -> Option<String> {
        let value = self.headers.get(SET_COOKIE)?.to_str().ok()?;
        Some(value.split(';').next()?.to_string())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }
}

#[derive(Default)]
struct Call<'a> {
    cookie: Option<&'a str>,
    basic: Option<(&'a str, &'a str)>,
    form: Option<&'a [(&'a str, &'a str)]>,
    fetch_site: Option<&'a str>,
}

/// A site with two datasets and, unless `fresh`, an administrator `ada` and a user `bob`.
fn site(fresh: bool, max_page_size: u64) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let sales = dir.path().join("sales.csv");
    std::fs::write(
        &sales,
        "order_id,region,amount,sold_on,shipped_at\n\
         1,EU,10.5,2024-01-05,2024-01-06 08:30:00\n\
         2,US,20.0,2024-02-10,2024-02-11 09:00:00\n\
         3,EU,30.25,2024-03-15,2024-03-16 10:15:30\n\
         4,<script>alert(1)</script>,40.0,2024-04-20,\n",
    )
    .unwrap();
    let many = dir.path().join("many.csv");
    let lines: Vec<String> = (1..=120).map(|n| format!("{n},item {n}")).collect();
    std::fs::write(&many, format!("id,label\n{}\n", lines.join("\n"))).unwrap();
    let file = |name: &str, path, key: &str| DatasetSpec {
        name: name.into(),
        source: Source::File { path, format: None },
        key: Some(key.into()),
    };
    let engine = Engine::open(&[file("sales", sales, "order_id"), file("many", many, "id")]).unwrap();
    let accounts = Store::open_in_memory().unwrap();
    if !fresh {
        accounts.create_first_admin("ada", ADMIN_PASSWORD).unwrap();
        accounts.create("bob", USER_PASSWORD, Role::User).unwrap();
    }
    let state = Arc::new(AppState::new(
        engine,
        accounts,
        max_page_size,
        None,
        "127.0.0.1:8787".into(),
    ));
    Site {
        app: router(state.clone()),
        state,
        _dir: dir,
    }
}

async fn call(site: &Site, method: &str, uri: &str, call: Call<'_>) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(HOST, "beam.test:9000");
    if let Some(cookie) = call.cookie {
        request = request.header(COOKIE, cookie);
    }
    if let Some((username, password)) = call.basic {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        request = request.header(AUTHORIZATION, format!("Basic {encoded}"));
    }
    if let Some(site) = call.fetch_site {
        request = request.header("sec-fetch-site", site);
    }
    let body = match call.form {
        Some(fields) => {
            request = request.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
            let mut encoded = form_urlencoded::Serializer::new(String::new());
            for (name, value) in fields {
                encoded.append_pair(name, value);
            }
            Body::from(encoded.finish())
        }
        None => Body::empty(),
    };
    let response = site.app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn get(site: &Site, uri: &str, cookie: Option<&str>) -> Reply {
    call(
        site,
        "GET",
        uri,
        Call {
            cookie,
            ..Call::default()
        },
    )
    .await
}

async fn post(site: &Site, uri: &str, cookie: Option<&str>, form: &[(&str, &str)]) -> Reply {
    call(
        site,
        "POST",
        uri,
        Call {
            cookie,
            form: Some(form),
            ..Call::default()
        },
    )
    .await
}

async fn feed(site: &Site, uri: &str) -> Reply {
    call(
        site,
        "GET",
        uri,
        Call {
            basic: Some(("bob", USER_PASSWORD)),
            ..Call::default()
        },
    )
    .await
}

/// Signs in and returns the session cookie and the anti-forgery token.
async fn sign_in(site: &Site, username: &str, password: &str) -> (String, String) {
    let reply = post(site, "/login", None, &[("username", username), ("password", password)]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let cookie = reply.cookie().expect("a session cookie");
    let page = get(site, "/account", Some(&cookie)).await;
    let marker = "name=\"csrf\" value=\"";
    let start = page.body.find(marker).expect("a csrf field") + marker.len();
    let csrf = page.body[start..start + 64].to_string();
    (cookie, csrf)
}

/// Posts a form with the anti-forgery token added.
async fn submit(site: &Site, cookie: &str, csrf: &str, uri: &str, fields: &[(&str, &str)]) -> Reply {
    let mut all = fields.to_vec();
    all.push(("csrf", csrf));
    post(site, uri, Some(cookie), &all).await
}

fn account_id(site: &Site, username: &str) -> i64 {
    site.state
        .accounts
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.username == username)
        .unwrap()
        .id
}

// ------------------------------------------------------------------ set-up

#[tokio::test]
async fn a_fresh_site_offers_set_up_exactly_once() {
    let site = site(true, 100);
    assert_eq!(get(&site, "/", None).await.location(), "/setup");
    assert_eq!(get(&site, "/login", None).await.location(), "/setup");
    assert_eq!(get(&site, "/odata/", None).await.status, StatusCode::UNAUTHORIZED);
    assert!(get(&site, "/setup", None)
        .await
        .body
        .contains("Create the administrator account"));

    let mismatch = [
        ("username", "ada"),
        ("password", ADMIN_PASSWORD),
        ("confirm", "something else!"),
    ];
    assert_eq!(
        post(&site, "/setup", None, &mismatch).await.status,
        StatusCode::BAD_REQUEST
    );
    let weak = [("username", "ada"), ("password", "short"), ("confirm", "short")];
    assert_eq!(post(&site, "/setup", None, &weak).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(site.state.accounts.count().unwrap(), 0);

    let good = [
        ("username", "Ada"),
        ("password", ADMIN_PASSWORD),
        ("confirm", ADMIN_PASSWORD),
    ];
    let created = post(&site, "/setup", None, &good).await;
    assert_eq!((created.status, created.location()), (StatusCode::SEE_OTHER, "/"));
    let home = get(&site, "/", created.cookie().as_deref()).await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("Datasets") && home.body.contains("ada") && home.body.contains("Administrator"));

    // Set-up is closed for good once an account exists.
    assert_eq!(get(&site, "/setup", None).await.location(), "/login");
    let second = [
        ("username", "eve"),
        ("password", ADMIN_PASSWORD),
        ("confirm", ADMIN_PASSWORD),
    ];
    let refused = post(&site, "/setup", None, &second).await;
    assert_eq!(refused.location(), "/login");
    assert!(refused.cookie().is_none());
    assert_eq!(site.state.accounts.count().unwrap(), 1);
}

#[test]
fn refuses_to_be_reachable_before_an_administrator_exists() {
    let load = |text: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("beam.toml");
        std::fs::write(&path, format!("{text}\n[[dataset]]\nname = \"a\"\npath = \"a.csv\"\n")).unwrap();
        config::Config::load(&path).unwrap()
    };
    let local = load("");
    let network = load("bind = \"0.0.0.0:8787\"");
    let proxied = load("public_url = \"https://beam.example.org/\"");
    assert_eq!(proxied.public_url.as_deref(), Some("https://beam.example.org"));
    assert!(local.accounts.ends_with("beam-accounts.sqlite"));

    assert!(config::check_exposure(&local, 0).is_ok());
    assert!(config::check_exposure(&network, 0)
        .unwrap_err()
        .contains("create-admin"));
    assert!(config::check_exposure(&proxied, 0)
        .unwrap_err()
        .contains("create-admin"));
    assert!(config::check_exposure(&network, 1).is_ok());
    assert!(config::check_exposure(&proxied, 1).is_ok());
}

// ----------------------------------------------------------------- sign-in

#[tokio::test]
async fn signing_in_and_out() {
    let site = site(false, 100);
    assert_eq!(get(&site, "/", None).await.location(), "/login");
    assert_eq!(get(&site, "/datasets/sales", None).await.location(), "/login");
    assert_eq!(get(&site, "/account", None).await.location(), "/login");
    assert_eq!(get(&site, "/admin/accounts", None).await.location(), "/login");

    // Wrong password and unknown username are indistinguishable.
    for (username, password) in [("ada", "not the password"), ("nobody", ADMIN_PASSWORD)] {
        let reply = post(&site, "/login", None, &[("username", username), ("password", password)]).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(reply.body.contains("Incorrect username or password."));
        assert!(reply.cookie().is_none());
    }

    let reply = post(
        &site,
        "/login",
        None,
        &[("username", "ada"), ("password", ADMIN_PASSWORD)],
    )
    .await;
    let set_cookie = reply.headers[SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict") && set_cookie.contains("Path=/"));
    assert!(
        !set_cookie.contains("Secure"),
        "plain http on this computer must still work"
    );

    let (cookie, csrf) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    assert_eq!(get(&site, "/", Some(&cookie)).await.status, StatusCode::OK);
    assert_eq!(get(&site, "/login", Some(&cookie)).await.location(), "/");
    assert_eq!(get(&site, "/", Some("beam_session=forged")).await.location(), "/login");

    // Signing out needs the form token, then really ends the session.
    assert_eq!(
        post(&site, "/logout", Some(&cookie), &[("csrf", "wrong")]).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(get(&site, "/", Some(&cookie)).await.status, StatusCode::OK);
    let out = post(&site, "/logout", Some(&cookie), &[("csrf", &csrf)]).await;
    assert_eq!(out.location(), "/login");
    assert!(out.headers[SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(get(&site, "/", Some(&cookie)).await.location(), "/login");
}

#[tokio::test]
async fn repeated_failures_lock_sign_in_for_that_username_only() {
    let site = site(false, 100);
    for _ in 0..5 {
        let reply = post(
            &site,
            "/login",
            None,
            &[("username", "ada"), ("password", "wrong password")],
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    // Even the right password is refused while locked, on pages and on the feed.
    let locked = post(
        &site,
        "/login",
        None,
        &[("username", "ADA"), ("password", ADMIN_PASSWORD)],
    )
    .await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(locked.cookie().is_none());
    let basic = Call {
        basic: Some(("ada", ADMIN_PASSWORD)),
        ..Call::default()
    };
    assert_eq!(
        call(&site, "GET", "/odata/", basic).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Other accounts are unaffected.
    sign_in(&site, "bob", USER_PASSWORD).await;
}

// ------------------------------------------------------------------- roles

#[tokio::test]
async fn users_read_data_but_cannot_manage_accounts() {
    let site = site(false, 100);
    let (cookie, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let home = get(&site, "/", Some(&cookie)).await;
    assert!(home.body.contains("href=\"/datasets/sales\""));
    assert!(
        !home.body.contains("/admin/accounts"),
        "the menu hides what the role cannot use"
    );
    assert_eq!(
        get(&site, "/datasets/sales", Some(&cookie)).await.status,
        StatusCode::OK
    );

    assert_eq!(
        get(&site, "/admin/accounts", Some(&cookie)).await.status,
        StatusCode::FORBIDDEN
    );
    let ada = account_id(&site, "ada").to_string();
    let bob = account_id(&site, "bob");
    let attempts: [(String, Vec<(&str, &str)>); 4] = [
        (
            "/admin/accounts".into(),
            vec![("username", "mallory"), ("password", USER_PASSWORD), ("role", "admin")],
        ),
        (format!("/admin/accounts/{bob}/role"), vec![("role", "admin")]),
        (format!("/admin/accounts/{ada}/active"), vec![("active", "false")]),
        (
            format!("/admin/accounts/{ada}/password"),
            vec![("password", "taken over now")],
        ),
    ];
    for (uri, fields) in &attempts {
        assert_eq!(
            submit(&site, &cookie, &csrf, uri, fields).await.status,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    let accounts = site.state.accounts.list().unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts.iter().any(|a| a.username == "bob" && a.role == Role::User));
    assert!(site.state.accounts.verify("ada", ADMIN_PASSWORD).unwrap().is_some());
}

#[tokio::test]
async fn forged_forms_are_refused() {
    let site = site(false, 100);
    let (cookie, csrf) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    let carol = [("username", "carol"), ("password", USER_PASSWORD), ("role", "user")];
    let wrong_token = "0".repeat(64);

    assert_eq!(
        submit(&site, &cookie, "", "/admin/accounts", &carol).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        submit(&site, &cookie, &wrong_token, "/admin/accounts", &carol)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let mut with_token = carol.to_vec();
    with_token.push(("csrf", &csrf));
    // A valid token still fails when the browser says another site sent the form.
    for origin in ["cross-site", "same-site"] {
        let forged = Call {
            cookie: Some(&cookie),
            form: Some(&with_token),
            fetch_site: Some(origin),
            ..Call::default()
        };
        assert_eq!(
            call(&site, "POST", "/admin/accounts", forged).await.status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(site.state.accounts.count().unwrap(), 2);

    let genuine = Call {
        cookie: Some(&cookie),
        form: Some(&with_token),
        fetch_site: Some("same-origin"),
        ..Call::default()
    };
    assert_eq!(
        call(&site, "POST", "/admin/accounts", genuine).await.location(),
        "/admin/accounts?ok=created"
    );
    assert_eq!(site.state.accounts.count().unwrap(), 3);
}

// ------------------------------------------------------ account management

#[tokio::test]
async fn administrators_manage_accounts() {
    let site = site(false, 100);
    let (admin, csrf) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    let listing = get(&site, "/admin/accounts", Some(&admin)).await;
    assert!(listing.body.contains("bob") && listing.body.contains("Add an account"));

    for (username, password, role, outcome) in [
        ("carol", USER_PASSWORD, "user", "ok=created"),
        ("carol", USER_PASSWORD, "user", "error=taken"),
        ("x", USER_PASSWORD, "user", "error=username"),
        ("dave", "short", "user", "error=password"),
        ("dave", USER_PASSWORD, "owner", "error=role"),
    ] {
        let fields = [("username", username), ("password", password), ("role", role)];
        let reply = submit(&site, &admin, &csrf, "/admin/accounts", &fields).await;
        assert_eq!(reply.location(), format!("/admin/accounts?{outcome}"));
    }
    assert!(get(&site, "/admin/accounts?error=taken", Some(&admin))
        .await
        .body
        .contains("already in use"));

    let carol = account_id(&site, "carol");
    let (carol_cookie, _) = sign_in(&site, "carol", USER_PASSWORD).await;
    let as_carol = || Call {
        basic: Some(("carol", USER_PASSWORD)),
        ..Call::default()
    };
    assert_eq!(
        call(&site, "GET", "/odata/sales", as_carol()).await.status,
        StatusCode::OK
    );

    // Disabling ends the browser session and the feed access immediately.
    let active_uri = format!("/admin/accounts/{carol}/active");
    let disabled = submit(&site, &admin, &csrf, &active_uri, &[("active", "false")]).await;
    assert_eq!(disabled.location(), "/admin/accounts?ok=disabled");
    assert_eq!(get(&site, "/", Some(&carol_cookie)).await.location(), "/login");
    assert_eq!(
        call(&site, "GET", "/odata/sales", as_carol()).await.status,
        StatusCode::UNAUTHORIZED
    );
    let refused = post(
        &site,
        "/login",
        None,
        &[("username", "carol"), ("password", USER_PASSWORD)],
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    let enabled = submit(&site, &admin, &csrf, &active_uri, &[("active", "true")]).await;
    assert_eq!(enabled.location(), "/admin/accounts?ok=enabled");

    // A reset replaces the password and signs the account out.
    let (carol_cookie, _) = sign_in(&site, "carol", USER_PASSWORD).await;
    let password_uri = format!("/admin/accounts/{carol}/password");
    let reset = submit(
        &site,
        &admin,
        &csrf,
        &password_uri,
        &[("password", "freshly reset pass")],
    )
    .await;
    assert_eq!(reset.location(), "/admin/accounts?ok=reset");
    assert_eq!(get(&site, "/", Some(&carol_cookie)).await.location(), "/login");
    assert_eq!(
        call(&site, "GET", "/odata/sales", as_carol()).await.status,
        StatusCode::UNAUTHORIZED
    );
    sign_in(&site, "carol", "freshly reset pass").await;

    // Promotion takes effect on the promoted account's next request.
    let (carol_cookie, _) = sign_in(&site, "carol", "freshly reset pass").await;
    assert_eq!(
        get(&site, "/admin/accounts", Some(&carol_cookie)).await.status,
        StatusCode::FORBIDDEN
    );
    let role_uri = format!("/admin/accounts/{carol}/role");
    let promote = submit(&site, &admin, &csrf, &role_uri, &[("role", "admin")]).await;
    assert_eq!(promote.location(), "/admin/accounts?ok=role");
    assert_eq!(
        get(&site, "/admin/accounts", Some(&carol_cookie)).await.status,
        StatusCode::OK
    );

    let missing = submit(&site, &admin, &csrf, "/admin/accounts/9999/role", &[("role", "user")]).await;
    assert_eq!(missing.location(), "/admin/accounts?error=not-found");
}

#[tokio::test]
async fn the_last_administrator_cannot_lock_everyone_out() {
    let site = site(false, 100);
    let (admin, csrf) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    let ada = account_id(&site, "ada");
    let demote = submit(
        &site,
        &admin,
        &csrf,
        &format!("/admin/accounts/{ada}/role"),
        &[("role", "user")],
    )
    .await;
    assert_eq!(demote.location(), "/admin/accounts?error=last-admin");
    let disable = submit(
        &site,
        &admin,
        &csrf,
        &format!("/admin/accounts/{ada}/active"),
        &[("active", "false")],
    )
    .await;
    assert_eq!(disable.location(), "/admin/accounts?error=last-admin");
    assert!(get(&site, "/admin/accounts?error=last-admin", Some(&admin))
        .await
        .body
        .contains("at least one active administrator"));
    assert_eq!(get(&site, "/admin/accounts", Some(&admin)).await.status, StatusCode::OK);

    // Resetting your own password keeps you signed in, on a new session.
    let own_password = format!("/admin/accounts/{ada}/password");
    let reset = submit(
        &site,
        &admin,
        &csrf,
        &own_password,
        &[("password", "a brand new password")],
    )
    .await;
    assert_eq!(reset.location(), "/admin/accounts?ok=reset");
    assert_eq!(get(&site, "/", Some(&admin)).await.location(), "/login");
    assert_eq!(get(&site, "/", reset.cookie().as_deref()).await.status, StatusCode::OK);
}

#[tokio::test]
async fn changing_your_own_password() {
    let site = site(false, 100);
    let (cookie, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let (other_device, _) = sign_in(&site, "bob", USER_PASSWORD).await;
    let new = "a whole new password";
    for (current, password, confirm, outcome) in [
        ("not my password", new, new, "error=current"),
        (USER_PASSWORD, new, "different", "error=mismatch"),
        (USER_PASSWORD, "short", "short", "error=password"),
    ] {
        let fields = [("current", current), ("password", password), ("confirm", confirm)];
        let reply = submit(&site, &cookie, &csrf, "/account/password", &fields).await;
        assert_eq!(reply.location(), format!("/account?{outcome}"));
    }
    assert!(site.state.accounts.verify("bob", USER_PASSWORD).unwrap().is_some());

    // Prime the feed's memory of the old password, then change it.
    assert_eq!(feed(&site, "/odata/").await.status, StatusCode::OK);
    let fields = [("current", USER_PASSWORD), ("password", new), ("confirm", new)];
    let changed = submit(&site, &cookie, &csrf, "/account/password", &fields).await;
    assert_eq!(changed.location(), "/account?ok=password");
    let fresh = changed.cookie().expect("a replacement session");
    assert!(get(&site, "/account?ok=password", Some(&fresh))
        .await
        .body
        .contains("Your password has been changed"));
    assert_eq!(get(&site, "/", Some(&cookie)).await.location(), "/login");
    assert_eq!(get(&site, "/", Some(&other_device)).await.location(), "/login");
    assert_eq!(
        feed(&site, "/odata/").await.status,
        StatusCode::UNAUTHORIZED,
        "the old password must stop working on the feed"
    );
    sign_in(&site, "bob", "a whole new password").await;
}

// ------------------------------------------------------------------- pages

#[tokio::test]
async fn dataset_pages_show_sort_filter_and_page_through_rows() {
    let site = site(false, 100);
    let (cookie, _) = sign_in(&site, "bob", USER_PASSWORD).await;
    let view = |uri: &'static str| {
        let (site, cookie) = (&site, cookie.as_str());
        async move { get(site, uri, Some(cookie)).await }
    };

    let table = view("/datasets/sales").await;
    assert!(table.body.contains("Rows 1–4 of 4"));
    assert!(table.body.contains("<td class=\"num\">10.5</td>") && table.body.contains("<td class=\"null\">—</td>"));
    assert!(
        table.body.contains("<td>2024-01-06 08:30:00</td>"),
        "timestamps read naturally on a page"
    );
    // Data is shown as text, never interpreted as markup.
    assert!(table.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!table.body.contains("<script"));

    let sorted = view("/datasets/sales?sort=amount&dir=desc").await;
    assert!(sorted.body.find(">40.0<").unwrap() < sorted.body.find(">10.5<").unwrap());
    assert!(sorted.body.contains("aria-sort=\"descending\""));

    let filtered = view("/datasets/sales?column=region&op=contains&value=eu").await;
    assert!(filtered.body.contains("Rows 1–2 of 2"));
    let numeric = view("/datasets/sales?column=amount&op=gt&value=25").await;
    assert!(numeric.body.contains("Rows 1–2 of 2"));
    let dated = view("/datasets/sales?column=sold_on&op=ge&value=2024-03-01").await;
    assert!(dated.body.contains("Rows 1–2 of 2"));
    let quoted = view("/datasets/sales?column=region&op=eq&value=x%27%3B+DROP+VIEW+sales%3B+--").await;
    assert!(quoted.body.contains("No rows match."));
    assert!(view("/datasets/sales").await.body.contains("Rows 1–4 of 4"));

    assert!(view("/datasets/sales?column=amount&op=gt&value=lots")
        .await
        .body
        .contains("Enter a number."));
    assert!(view("/datasets/sales?column=amount&op=contains&value=1")
        .await
        .body
        .contains("works on text columns"));
    assert!(view("/datasets/sales?column=sold_on&op=eq&value=2024-99-99")
        .await
        .body
        .contains("could not be applied"));
    assert_eq!(
        view("/datasets/sales?column=nope&op=eq&value=1&sort=nope").await.status,
        StatusCode::OK
    );

    let first = view("/datasets/many").await;
    assert!(first.body.contains("Rows 1–50 of 120") && first.body.contains("href=\"/datasets/many?page=2\""));
    assert!(!first.body.contains("Previous"));
    let last = view("/datasets/many?page=3&sort=id&dir=asc").await;
    assert!(last.body.contains("Rows 101–120 of 120") && !last.body.contains("Next"));
    assert!(last
        .body
        .contains("href=\"/datasets/many?sort=id&amp;dir=asc&amp;page=2\""));
    assert!(view("/datasets/many?page=99").await.body.contains("No rows match."));

    assert_eq!(view("/datasets/nope").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_are_locked_down_and_assets_are_served() {
    let site = site(false, 100);
    let login = get(&site, "/login", None).await;
    let policy = login.headers["content-security-policy"].to_str().unwrap();
    assert!(policy.contains("default-src 'none'") && policy.contains("frame-ancestors 'none'"));
    assert!(!policy.contains("script-src"), "no page runs scripts");
    assert_eq!(login.headers["x-frame-options"], "DENY");
    assert_eq!(login.headers["x-content-type-options"], "nosniff");
    assert_eq!(login.headers["cache-control"], "no-store");
    assert!(!login.body.contains("<script"));

    let css = get(&site, "/static/app.css", None).await;
    assert_eq!(
        (css.status, css.headers[CONTENT_TYPE].to_str().unwrap()),
        (StatusCode::OK, "text/css; charset=utf-8")
    );
    let logo = get(&site, "/static/logo.svg", None).await;
    assert!(logo.body.contains("<svg") && logo.headers["cache-control"].to_str().unwrap().contains("max-age"));
    assert_eq!(get(&site, "/healthz", None).await.body, "ok");
    assert_eq!(get(&site, "/nothing/here", None).await.status, StatusCode::NOT_FOUND);
}

// -------------------------------------------------------------------- feed

#[tokio::test]
async fn the_feed_needs_an_account() {
    let site = site(false, 100);
    for uri in [
        "/odata",
        "/odata/",
        "/odata/$metadata",
        "/odata/sales",
        "/odata/sales/$count",
    ] {
        let anonymous = get(&site, uri, None).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(anonymous.headers[WWW_AUTHENTICATE], "Basic realm=\"Open Beam\"");
        assert_eq!(anonymous.headers["OData-Version"], "4.0");
        let wrong = Call {
            basic: Some(("bob", "not bob's password")),
            ..Call::default()
        };
        assert_eq!(
            call(&site, "GET", uri, wrong).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
        assert_eq!(feed(&site, uri).await.status, StatusCode::OK, "{uri}");
    }
    // A signed-in browser can open feed addresses too.
    let (cookie, _) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    assert_eq!(get(&site, "/odata/sales", Some(&cookie)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn feed_describes_and_returns_typed_rows() {
    let site = site(false, 100);
    let document = feed(&site, "/odata/").await.json();
    assert_eq!(document["@odata.context"], "http://beam.test:9000/odata/$metadata");
    assert_eq!(document["value"][0]["name"], "sales");

    let xml = feed(&site, "/odata/$metadata").await.body;
    assert!(xml.contains("<Property Name=\"order_id\" Type=\"Edm.Int64\" Nullable=\"false\"/>"));
    assert!(xml.contains("<Property Name=\"sold_on\" Type=\"Edm.Date\"/>"));
    assert!(xml.contains("<Property Name=\"shipped_at\" Type=\"Edm.DateTimeOffset\" Precision=\"6\"/>"));

    let body = feed(&site, "/odata/sales").await.json();
    assert_eq!(body["@odata.context"], "http://beam.test:9000/odata/$metadata#sales");
    assert!(body.get("@odata.nextLink").is_none());
    assert_eq!(
        body["value"][0],
        json!({
            "order_id": 1, "region": "EU", "amount": 10.5,
            "sold_on": "2024-01-05", "shipped_at": "2024-01-06T08:30:00.000000Z"
        })
    );
    assert_eq!(body["value"][3]["shipped_at"], Value::Null);

    let uri = "/odata/sales?$filter=region%20eq%20'EU'%20and%20sold_on%20ge%202024-02-01\
               &$select=order_id,amount&$orderby=amount%20desc&$count=true";
    let filtered = feed(&site, uri).await.json();
    assert_eq!(
        filtered["@odata.context"],
        "http://beam.test:9000/odata/$metadata#sales(order_id,amount)"
    );
    assert_eq!(filtered["@odata.count"], 1);
    assert_eq!(filtered["value"], json!([{ "order_id": 3, "amount": 30.25 }]));
    assert_eq!(
        feed(&site, "/odata/sales/$count?$filter=amount%20gt%2015").await.body,
        "3"
    );
}

#[tokio::test]
async fn feed_pages_follow_next_links_to_the_end() {
    let site = site(false, 3);
    let first = feed(&site, "/odata/sales?$orderby=order_id%20desc").await.json();
    assert_eq!(first["value"].as_array().unwrap().len(), 3);
    let next = first["@odata.nextLink"].as_str().unwrap();
    assert_eq!(
        next,
        "http://beam.test:9000/odata/sales?%24orderby=order_id+desc&%24skip=3"
    );

    let second = feed(&site, next.strip_prefix("http://beam.test:9000").unwrap())
        .await
        .json();
    assert_eq!(second["value"].as_array().unwrap().len(), 1);
    assert_eq!(second["value"][0]["order_id"], 1);
    assert!(second.get("@odata.nextLink").is_none());

    let top = feed(&site, "/odata/sales?$top=2").await.json();
    assert_eq!(top["value"].as_array().unwrap().len(), 2);
    assert!(top.get("@odata.nextLink").is_none());
    let big = feed(&site, "/odata/sales?$top=4").await.json();
    assert!(big["@odata.nextLink"]
        .as_str()
        .unwrap()
        .ends_with("?%24top=1&%24skip=3"));
}

#[tokio::test]
async fn hostile_feed_input_is_rejected_or_inert() {
    let site = site(false, 100);
    let injection = "/odata/sales?$filter=region%20eq%20'x'';%20DROP%20VIEW%20sales;%20--'";
    assert_eq!(feed(&site, injection).await.json()["value"], json!([]));
    assert_eq!(
        feed(&site, "/odata/sales").await.json()["value"]
            .as_array()
            .unwrap()
            .len(),
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
        let reply = feed(&site, uri).await;
        assert_eq!(reply.status, expected, "{uri}: {}", reply.body);
        assert!(reply.body.contains("\"error\""), "{uri}");
        assert!(
            !reply.body.contains("sales.csv"),
            "error bodies must not reveal file paths"
        );
    }
}
