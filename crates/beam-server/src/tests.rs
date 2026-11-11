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
    let uploads = dir.path().join("uploads");
    let engine = Engine::open_with_uploads(
        &[file("sales", sales, "order_id"), file("many", many, "id")],
        Some(&uploads),
    )
    .unwrap();
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
async fn charts_summarise_a_dataset_as_a_drawing_and_a_table() {
    let site = site(false, 100);
    assert_eq!(get(&site, "/datasets/sales/chart", None).await.location(), "/login");
    let (cookie, _) = sign_in(&site, "bob", USER_PASSWORD).await;
    let view = |uri: &'static str| {
        let (site, cookie) = (&site, cookie.as_str());
        async move { get(site, uri, Some(cookie)).await }
    };

    // With no choices made: count rows by the first text column, as bars.
    let default = view("/datasets/sales/chart").await;
    assert_eq!(default.status, StatusCode::OK);
    assert!(default.body.contains("<h2>Number of rows by region</h2>"));
    assert!(default.body.contains("<svg class=\"chart\"") && default.body.contains("<rect class=\"bar\""));
    assert!(default.body.contains("<td>EU</td><td class=\"num\">2</td>"));
    assert!(default.body.find("<td>EU</td>").unwrap() < default.body.find("<td>US</td>").unwrap());
    // Data is text inside the drawing too, never markup.
    assert!(default.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !default.body.contains("<script"));

    let summed = view("/datasets/sales/chart?agg=sum&measure=amount&group=region").await;
    assert!(summed.body.contains("<h2>Sum of amount by region</h2>"));
    assert!(summed.body.contains("<td>EU</td><td class=\"num\">40.75</td>"));
    let averaged = view("/datasets/sales/chart?agg=avg&measure=amount&group=region").await;
    assert!(averaged.body.contains("<td>EU</td><td class=\"num\">20.38</td>"));

    // Dates group by period, in date order, and draw as a line by default.
    let monthly = view("/datasets/sales/chart?agg=sum&measure=amount&group=sold_on&bucket=month").await;
    assert!(monthly.body.contains("<h2>Sum of amount by sold_on (month)</h2>"));
    assert!(monthly.body.contains("<polyline class=\"line\"") && !monthly.body.contains("<rect class=\"bar\""));
    assert!(monthly.body.contains("<td>2024-01</td><td class=\"num\">10.5</td>"));
    assert!(monthly.body.find("<td>2024-01</td>").unwrap() < monthly.body.find("<td>2024-04</td>").unwrap());
    let quarterly = view("/datasets/sales/chart?agg=sum&measure=amount&group=sold_on&bucket=quarter&kind=bar").await;
    assert!(quarterly.body.contains("<td>2024-Q1</td><td class=\"num\">60.75</td>"));
    assert!(quarterly.body.contains("<td>2024-Q2</td><td class=\"num\">40</td>"));
    assert!(quarterly.body.contains("<rect class=\"bar\"") && !quarterly.body.contains("<polyline"));
    let yearly = view("/datasets/sales/chart?agg=max&measure=amount&group=sold_on&bucket=year").await;
    assert!(yearly.body.contains("<td>2024</td><td class=\"num\">40</td>"));

    // A filter set on the table carries over, and limits what is charted.
    let table = view("/datasets/sales?column=region&op=eq&value=EU").await;
    assert!(table
        .body
        .contains("href=\"/datasets/sales/chart?column=region&amp;op=eq&amp;value=EU\""));
    let filtered =
        view("/datasets/sales/chart?column=region&op=eq&value=EU&agg=sum&measure=amount&group=sold_on&bucket=year")
            .await;
    assert!(
        filtered.body.contains("<td>2024</td><td class=\"num\">40.75</td>")
            && filtered.body.contains("Only rows where")
    );
    assert!(filtered
        .body
        .contains("href=\"/datasets/sales?column=region&amp;op=eq&amp;value=EU\""));
    let hostile = view("/datasets/sales/chart?column=region&op=eq&value=x%27%3B+DROP+VIEW+sales%3B+--").await;
    assert!(hostile.body.contains("There is nothing to chart."));
    assert!(view("/datasets/sales/chart")
        .await
        .body
        .contains("<td>EU</td><td class=\"num\">2</td>"));

    // Choices that make no sense fall back to something that draws.
    let nonsense = view("/datasets/sales/chart?group=nope&agg=nope&measure=region&bucket=nope&kind=nope").await;
    assert!(nonsense.body.contains("<h2>Number of rows by region</h2>"));
    assert!(view("/datasets/sales/chart?column=amount&op=gt&value=lots")
        .await
        .body
        .contains("Enter a number."));

    // Too many groups are cut off, and the page says so.
    let crowded = view("/datasets/many/chart?group=label").await;
    assert!(
        crowded.body.contains("Showing the first 24 groups")
            && crowded.body.matches("<rect class=\"bar\"").count() == 24
    );
    let ordered = view("/datasets/many/chart?group=id&agg=sum&measure=id").await;
    assert!(
        ordered.body.matches("<circle class=\"dot\"").count() == 120 && !ordered.body.contains("Showing the first")
    );

    assert_eq!(view("/datasets/nope/chart").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_are_locked_down_and_assets_are_served() {
    let site = site(false, 100);
    let login = get(&site, "/login", None).await;
    let policy = login.headers["content-security-policy"].to_str().unwrap();
    assert!(policy.contains("default-src 'none'") && policy.contains("frame-ancestors 'none'"));
    assert!(!policy.contains("script-src"), "no page runs scripts");
    assert_eq!(login.headers["x-frame-options"], "DENY");
    // Browsers send `Origin: null` under "no-referrer", which would make every form fail.
    assert_eq!(login.headers["referrer-policy"], "same-origin");
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

// --------------------------------------------------- uploads and sharing

const CYD_PASSWORD: &str = "a third long password";

/// Uploads a file and saves the resulting draft, as a person does with the
/// Add data page and then Save on the review page. Returns the reply to
/// whichever step stopped first.
async fn upload(site: &Site, cookie: &str, csrf: &str, file_name: &str, bytes: &[u8], name: &str) -> Reply {
    let drafted = upload_draft(site, cookie, csrf, file_name, bytes).await;
    let Some(id) = drafted.location().strip_prefix("/data/drafts/").map(str::to_string) else {
        return drafted;
    };
    submit(
        site,
        cookie,
        csrf,
        &format!("/data/drafts/{id}/save"),
        &[("name", name)],
    )
    .await
}

/// Sends a multipart form, as a browser does for a file upload.
async fn upload_draft(site: &Site, cookie: &str, csrf: &str, file_name: &str, bytes: &[u8]) -> Reply {
    let boundary = "----beamtestboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{csrf}\r\n").as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/data")
        .header(HOST, "beam.test:9000")
        .header(COOKIE, cookie)
        .header(CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(body))
        .unwrap();
    let response = site.app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn upload_files(site: &Site) -> usize {
    std::fs::read_dir(site.state.engine.upload_dir().unwrap())
        .unwrap()
        .count()
}

const BUDGET: &[u8] = b"team,amount,due\nOps,1200.5,2025-04-01\nSales,=CMD|' /C calc'!A0,2025-05-01\nOps,300,\n";

#[tokio::test]
async fn uploads_are_private_until_shared() {
    let site = site(false, 100);
    site.state.accounts.create("cyd", CYD_PASSWORD, Role::User).unwrap();
    let (bob, bob_csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let (cyd, cyd_csrf) = sign_in(&site, "cyd", CYD_PASSWORD).await;
    let (ada, _) = sign_in(&site, "ada", ADMIN_PASSWORD).await;

    let added = upload(&site, &bob, &bob_csrf, "Team Budget 2025.csv", BUDGET, "").await;
    assert_eq!(
        (added.status, added.location()),
        (StatusCode::SEE_OTHER, "/datasets/team_budget_2025")
    );
    assert_eq!(upload_files(&site), 1);
    let page = get(&site, "/datasets/team_budget_2025", Some(&bob)).await;
    assert!(page.body.contains("Rows 1–3 of 3") && page.body.contains(">Sharing<"));
    assert!(get(&site, "/", Some(&bob))
        .await
        .body
        .contains("<h2 class=\"section\">Yours</h2>"));

    // Nobody else sees it: not on pages, downloads, charts or the feed.
    for uri in [
        "/datasets/team_budget_2025",
        "/datasets/team_budget_2025/chart",
        "/datasets/team_budget_2025/export.csv",
        "/datasets/team_budget_2025/export.xlsx",
        "/datasets/team_budget_2025/share",
    ] {
        assert_eq!(get(&site, uri, Some(&cyd)).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(!get(&site, "/", Some(&cyd)).await.body.contains("team_budget_2025"));
    let as_cyd = || Call {
        basic: Some(("cyd", CYD_PASSWORD)),
        ..Call::default()
    };
    assert!(!call(&site, "GET", "/odata/", as_cyd())
        .await
        .body
        .contains("team_budget_2025"));
    assert!(!call(&site, "GET", "/odata/$metadata", as_cyd())
        .await
        .body
        .contains("team_budget_2025"));
    assert_eq!(
        call(&site, "GET", "/odata/team_budget_2025", as_cyd()).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&site, "GET", "/odata/team_budget_2025/$count", as_cyd())
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Someone who cannot see it cannot share or delete it either.
    let cyd = cyd.as_str();
    let cyd_id = account_id(&site, "cyd").to_string();
    assert_eq!(
        submit(
            &site,
            cyd,
            &cyd_csrf,
            "/datasets/team_budget_2025/share",
            &[("account", &cyd_id)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        submit(
            &site,
            cyd,
            &cyd_csrf,
            "/datasets/team_budget_2025/delete",
            &[("confirm", "yes")]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Administrators see everything, under "Added by others".
    let admin_home = get(&site, "/", Some(&ada)).await;
    assert!(admin_home.body.contains("Added by others") && admin_home.body.contains("team_budget_2025"));
    assert_eq!(
        get(&site, "/datasets/team_budget_2025/share", Some(&ada)).await.status,
        StatusCode::OK
    );

    // Sharing lets one colleague in, everywhere, and nobody else.
    let shared = submit(
        &site,
        &bob,
        &bob_csrf,
        "/datasets/team_budget_2025/share",
        &[("account", &cyd_id)],
    )
    .await;
    assert_eq!(shared.location(), "/datasets/team_budget_2025/share?ok=shared");
    assert_eq!(
        get(&site, "/datasets/team_budget_2025", Some(cyd)).await.status,
        StatusCode::OK
    );
    assert!(get(&site, "/", Some(cyd)).await.body.contains("Shared with you"));
    assert_eq!(
        call(&site, "GET", "/odata/team_budget_2025", as_cyd()).await.json()["value"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // Being shared with is not ownership: no Sharing tab, no sharing onwards, no deleting.
    assert!(!get(&site, "/datasets/team_budget_2025", Some(cyd))
        .await
        .body
        .contains(">Sharing<"));
    assert_eq!(
        get(&site, "/datasets/team_budget_2025/share", Some(cyd)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        submit(
            &site,
            cyd,
            &cyd_csrf,
            "/datasets/team_budget_2025/delete",
            &[("confirm", "yes")]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let ada_id = account_id(&site, "ada").to_string();
    assert_eq!(
        submit(
            &site,
            cyd,
            &cyd_csrf,
            "/datasets/team_budget_2025/share",
            &[("account", &ada_id)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    let stop = format!("/datasets/team_budget_2025/share/{cyd_id}/remove");
    assert_eq!(
        submit(&site, &bob, &bob_csrf, &stop, &[]).await.location(),
        "/datasets/team_budget_2025/share?ok=unshared"
    );
    assert_eq!(
        get(&site, "/datasets/team_budget_2025", Some(cyd)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&site, "GET", "/odata/team_budget_2025", as_cyd()).await.status,
        StatusCode::NOT_FOUND
    );

    // Built-in datasets cannot be shared or deleted from the browser.
    assert_eq!(
        get(&site, "/datasets/sales/share", Some(&ada)).await.status,
        StatusCode::FORBIDDEN
    );

    // Deleting needs the confirmation, then removes the data and the file.
    let unconfirmed = submit(&site, &bob, &bob_csrf, "/datasets/team_budget_2025/delete", &[]).await;
    assert_eq!(unconfirmed.location(), "/datasets/team_budget_2025/share?error=confirm");
    let deleted = submit(
        &site,
        &bob,
        &bob_csrf,
        "/datasets/team_budget_2025/delete",
        &[("confirm", "yes")],
    )
    .await;
    assert_eq!(deleted.location(), "/?ok=deleted");
    assert_eq!(
        get(&site, "/datasets/team_budget_2025", Some(&bob)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(upload_files(&site), 0);
    assert!(site.state.accounts.uploads().unwrap().is_empty());
}

#[tokio::test]
async fn uploads_reject_bad_files_and_forged_forms() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    assert!(get(&site, "/data/new", Some(&bob))
        .await
        .body
        .contains("CSV, Excel (.xlsx, first sheet) or Parquet"));
    assert_eq!(get(&site, "/data/new", None).await.location(), "/login");

    let forged = upload(&site, &bob, "not-the-token", "a.csv", BUDGET, "").await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    for (file, bytes, message) in [
        ("tool.exe", &b"MZ"[..], "Choose a CSV, Excel (.xlsx) or Parquet file."),
        ("old.xls", &b"\xd0\xcf"[..], "Save it as .xlsx"),
        ("empty.csv", &b""[..], "That file is empty."),
        ("binary.csv", &b"a,b\n\x00\x01"[..], "do not match its name"),
        ("fake.parquet", &b"a,b\n1,2\n"[..], "do not match its name"),
        ("fake.xlsx", &b"PK\x03\x04 broken"[..], "could not be read"),
        (
            "ragged.parquet",
            &b"PAR1 not really parquet PAR1"[..],
            "could not be read as a table",
        ),
    ] {
        let reply = upload(&site, &bob, &csrf, file, bytes, "").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{file}");
        assert!(reply.body.contains(message), "{file}: {}", reply.body);
    }
    assert_eq!(upload_files(&site), 0, "refused files leave nothing behind");

    // A name nobody can trick into a path, and a new name when one is taken.
    // A typed name is tidied into a plain name, and a name in use is refused on review.
    let tricky = upload(&site, &bob, &csrf, "x.csv", BUDGET, "../../sales").await;
    assert!(tricky.location().contains("error=name-taken"), "{}", tricky.location());
    let renamed = upload(&site, &bob, &csrf, "x.csv", BUDGET, "../../sales 2").await;
    assert_eq!(renamed.location(), "/datasets/sales_2");
    assert!(
        get(&site, "/datasets/sales", Some(&bob))
            .await
            .body
            .contains("Rows 1–4 of 4"),
        "the built-in dataset is untouched"
    );
    for entry in std::fs::read_dir(site.state.engine.upload_dir().unwrap()).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            name.len() == 36 && name.ends_with(".csv"),
            "server-chosen file name, got {name}"
        );
    }
}

#[tokio::test]
async fn uploads_respect_the_size_limit() {
    let mut site = site(false, 100);
    site.app = Router::new();
    let mut state = Arc::try_unwrap(site.state).ok().unwrap();
    state.max_upload_bytes = 1024;
    let state = Arc::new(state);
    site.app = crate::app::router(state.clone());
    site.state = state;
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let big = format!("a,b\n{}", "1,2\n".repeat(2000));
    let reply = upload(&site, &bob, &csrf, "big.csv", big.as_bytes(), "").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("That file is too large to add."));
    // Beyond the request limit the body is cut off before it is read at all.
    let huge = vec![b'a'; 200 * 1024];
    assert!(upload(&site, &bob, &csrf, "huge.csv", &huge, "")
        .await
        .status
        .is_client_error());
    assert!(site.state.accounts.uploads().unwrap().is_empty() && upload_files(&site) == 0);
}

#[tokio::test]
async fn excel_uploads_become_datasets_with_real_types() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let mut workbook = rust_xlsxwriter::Workbook::new();
    let sheet = workbook.add_worksheet();
    for (col, header) in ["member", "joined", "visits"].iter().enumerate() {
        sheet.write(0, col as u16, *header).unwrap();
    }
    let date = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd");
    for (row, (member, joined, visits)) in [("M-1", "2025-01-05", 3), ("M-2", "2025-02-11", 7)].iter().enumerate() {
        let r = row as u32 + 1;
        sheet.write(r, 0, *member).unwrap();
        sheet
            .write_with_format(
                r,
                1,
                rust_xlsxwriter::ExcelDateTime::parse_from_str(joined).unwrap(),
                &date,
            )
            .unwrap();
        sheet.write(r, 2, *visits).unwrap();
    }
    let bytes = workbook.save_to_buffer().unwrap();
    let added = upload(&site, &bob, &csrf, "Members.xlsx", &bytes, "").await;
    assert_eq!(added.location(), "/datasets/members");
    let xml = call(
        &site,
        "GET",
        "/odata/$metadata",
        Call {
            basic: Some(("bob", USER_PASSWORD)),
            ..Call::default()
        },
    )
    .await
    .body;
    assert!(xml.contains("<Property Name=\"joined\" Type=\"Edm.Date\"/>"), "{xml}");
    let chart = get(
        &site,
        "/datasets/members/chart?agg=sum&measure=visits&group=member",
        Some(&bob),
    )
    .await;
    assert!(chart.body.contains("<td>M-2</td><td class=\"num\">7</td>"));
}

#[tokio::test]
async fn uploads_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let accounts_file = dir.path().join("accounts.sqlite");
    let uploads = dir.path().join("uploads");
    {
        let store = Store::open(&accounts_file).unwrap();
        let bob = store.create_first_admin("bob", USER_PASSWORD).unwrap();
        let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
        std::fs::write(uploads.join("0a.csv"), BUDGET).unwrap();
        engine.add_upload("budget", "0a.csv", beam_engine::Format::Csv).unwrap();
        store.create_upload("budget", bob.id, "0a.csv").unwrap();
        store.create_upload("lost", bob.id, "gone.csv").unwrap();
    }
    let store = Store::open(&accounts_file).unwrap();
    let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
    crate::restore_uploads(&engine, &store);
    assert!(engine.dataset("budget").is_some_and(|d| d.uploaded));
    assert!(engine.dataset("lost").is_none(), "a missing file is skipped, not fatal");
}

// ---------------------------------------------------------------- export

#[tokio::test]
async fn downloads_match_the_view_on_screen() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let csv = get(
        &site,
        "/datasets/sales/export.csv?column=region&op=eq&value=EU&sort=amount&dir=desc&cols=order_id&cols=amount",
        Some(&bob),
    )
    .await;
    assert_eq!(csv.status, StatusCode::OK);
    assert_eq!(csv.headers[CONTENT_TYPE], "text/csv; charset=utf-8");
    assert_eq!(csv.headers["content-disposition"], "attachment; filename=\"sales.csv\"");
    assert_eq!(
        csv.body.trim_start_matches('\u{feff}'),
        "order_id,amount\r\n3,30.25\r\n1,10.5\r\n"
    );

    upload(&site, &bob, &csrf, "budget.csv", BUDGET, "").await;
    let risky = get(&site, "/datasets/budget/export.csv", Some(&bob)).await;
    assert!(
        risky.body.contains(",Sales,'=CMD|' /C calc'!A0,"),
        "formula neutralised: {}",
        risky.body
    );

    let request = Request::builder()
        .uri("/datasets/sales/export.xlsx?column=region&op=eq&value=EU")
        .header(HOST, "beam.test:9000")
        .header(COOKIE, &bob)
        .body(Body::empty())
        .unwrap();
    let response = site.app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"sales.xlsx\""
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    use calamine::Reader;
    let mut workbook: calamine::Xlsx<_> =
        calamine::open_workbook_from_rs(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let range = workbook.worksheet_range_at(0).unwrap().unwrap();
    assert_eq!(range.height(), 3, "header and the two EU rows");

    let bad = get(
        &site,
        "/datasets/sales/export.csv?column=amount&op=gt&value=lots",
        Some(&bob),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        get(&site, "/datasets/sales/export.csv", None).await.location(),
        "/login"
    );
}

// ------------------------------------------------------ settings and people

#[tokio::test]
async fn advanced_mode_adds_tools_without_adding_permissions() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let basic = get(&site, "/datasets/sales", Some(&bob)).await;
    assert!(
        basic.body.contains("Download Excel")
            && !basic.body.contains("Power Query")
            && !basic.body.contains("starts with")
    );
    assert!(get(&site, "/", Some(&bob)).await.body.contains(">Basic</a>"));

    let switched = submit(&site, &bob, &csrf, "/account/mode", &[("mode", "advanced")]).await;
    assert_eq!(switched.location(), "/account?ok=mode");
    let advanced = get(&site, "/datasets/sales?cols=region&cols=amount", Some(&bob)).await;
    assert!(
        advanced.body.contains("Power Query")
            && advanced.body.contains("starts with")
            && advanced.body.contains("Show these columns")
    );
    assert!(advanced.body.contains(
        "<th><a href=\"/datasets/sales?sort=region&amp;dir=asc&amp;cols=region&amp;cols=amount\">region</a></th>"
    ));
    assert!(!advanced.body.contains(">order_id</a>"), "only the chosen columns");
    assert!(advanced
        .body
        .contains("http://beam.test:9000/odata/sales?%24select=region%2Camount"));
    let empty = get(&site, "/datasets/sales?column=shipped_at&op=empty", Some(&bob)).await;
    assert!(empty.body.contains("Rows 1–1 of 1"));
    // Still a user: advanced mode opens no administrator pages.
    assert_eq!(
        get(&site, "/admin/accounts", Some(&bob)).await.status,
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        submit(&site, &bob, &csrf, "/account/mode", &[("mode", "expert")])
            .await
            .location(),
        "/account?error=failed"
    );
    assert_eq!(
        submit(&site, &bob, "wrong", "/account/mode", &[("mode", "basic")])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn profiles_and_contacts() {
    let site = site(false, 100);
    site.state.accounts.create("cyd", CYD_PASSWORD, Role::User).unwrap();
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let saved = submit(
        &site,
        &bob,
        &csrf,
        "/account/profile",
        &[("display_name", "Bob <b>Reyes</b>"), ("team", "Finance")],
    )
    .await;
    assert_eq!(saved.location(), "/account?ok=profile");
    let home = get(&site, "/", Some(&bob)).await;
    assert!(home.body.contains("Bob &lt;b&gt;Reyes&lt;/b&gt;") && !home.body.contains("<b>Reyes"));
    let too_long = "x".repeat(65);
    let refused = submit(
        &site,
        &bob,
        &csrf,
        "/account/profile",
        &[("display_name", &too_long), ("team", "")],
    )
    .await;
    assert_eq!(refused.location(), "/account?error=profile");

    let people = get(&site, "/people", Some(&bob)).await;
    assert!(people.body.contains("No contacts yet") && people.body.contains("@cyd") && people.body.contains("@ada"));
    assert!(get(&site, "/people?q=fin", Some(&bob)).await.body.contains("@bob"));
    assert!(!get(&site, "/people?q=fin", Some(&bob)).await.body.contains("@cyd"));

    let cyd = account_id(&site, "cyd");
    let add = submit(
        &site,
        &bob,
        &csrf,
        &format!("/people/{cyd}/contact"),
        &[("action", "add")],
    )
    .await;
    assert_eq!(add.location(), "/people?ok=contact-added");
    assert_eq!(
        site.state.accounts.contacts(account_id(&site, "bob")).unwrap(),
        vec![cyd]
    );
    // Contacts come first when sharing.
    upload(&site, &bob, &csrf, "plan.csv", BUDGET, "").await;
    let sharing = get(&site, "/datasets/plan/share", Some(&bob)).await;
    assert!(sharing.body.find("Your contacts").unwrap() < sharing.body.find("@cyd").unwrap());
    let me = account_id(&site, "bob");
    assert_eq!(
        submit(
            &site,
            &bob,
            &csrf,
            &format!("/people/{me}/contact"),
            &[("action", "add")]
        )
        .await
        .location(),
        "/people?error=not-found"
    );

    // Disabled accounts leave the directory and cannot be added.
    site.state.accounts.set_active(cyd, false).unwrap();
    assert!(!get(&site, "/people", Some(&bob)).await.body.contains("@cyd"));
    let remove = submit(
        &site,
        &bob,
        &csrf,
        &format!("/people/{cyd}/contact"),
        &[("action", "remove")],
    )
    .await;
    assert_eq!(remove.location(), "/people?ok=contact-removed");
    assert_eq!(get(&site, "/people", None).await.location(), "/login");
}

#[test]
fn a_double_clicked_start_needs_no_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let personal = config::Config::personal(dir.path());
    assert!(personal.datasets.is_empty() && personal.bind.ip().is_loopback() && personal.public_url.is_none());
    assert_eq!(personal.accounts, dir.path().join("accounts.sqlite"));
    assert_eq!(personal.uploads, dir.path().join("uploads"));
    assert!(
        config::check_exposure(&personal, 0).is_ok(),
        "this computer only, so first-run set-up is safe"
    );
    // A configuration file with no datasets is now allowed: people add their own.
    let path = dir.path().join("beam.toml");
    std::fs::write(&path, "bind = \"127.0.0.1:9999\"\n").unwrap();
    assert!(config::Config::load(&path).unwrap().datasets.is_empty());
}

// ---------------------------------------------------- review before saving

const REPORTS: &[u8] = b"member,event,note\nM-1,joined,\nM-2,joined,\n,,\nM-1,joined,\nM-3,left,\n,,\n";

#[tokio::test]
async fn new_files_wait_as_private_drafts_until_saved() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let (ada, _) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    let drafted = upload_draft(&site, &bob, &csrf, "Weekly Reports.csv", REPORTS).await;
    let review = drafted.location().to_string();
    assert!(review.starts_with("/data/drafts/"), "{review}");

    // Not a dataset yet: invisible everywhere, even to administrators.
    assert!(site.state.engine.datasets().iter().all(|d| !d.uploaded));
    assert!(!get(&site, "/", Some(&ada)).await.body.contains("Weekly Reports.csv"));
    assert_eq!(get(&site, &review, Some(&ada)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&site, &review, None).await.location(), "/login");
    let basic = Call {
        basic: Some(("bob", USER_PASSWORD)),
        ..Call::default()
    };
    assert!(!call(&site, "GET", "/odata/", basic).await.body.contains("weekly"));

    // Its owner is reminded on the home page and sees the checks.
    assert!(get(&site, "/", Some(&bob))
        .await
        .body
        .contains("Waiting for your review"));
    let page = get(&site, &review, Some(&bob)).await;
    assert!(
        page.body.contains("<strong>6</strong><span>rows</span>"),
        "{}",
        page.body
    );
    assert!(page.body.contains("2 rows are completely empty."));
    assert!(page.body.contains("1 row repeats an earlier row exactly."));
    assert!(page.body.contains("No values in: note."));
    assert!(page.body.contains("value=\"weekly_reports\""), "a name is suggested");
    assert!(
        page.body.contains("<td>M-3</td>")
            && page
                .body
                .contains("<strong>member</strong><span class=\"muted\">Text</span>")
    );

    // Saving needs the form token, then applies only the chosen tidying.
    let id = review.trim_start_matches("/data/drafts/");
    let save = format!("/data/drafts/{id}/save");
    assert_eq!(
        submit(&site, &bob, "wrong", &save, &[("name", "w")]).await.status,
        StatusCode::FORBIDDEN
    );
    let saved = submit(
        &site,
        &bob,
        &csrf,
        &save,
        &[("name", "Weekly Reports"), ("drop_empty", "yes")],
    )
    .await;
    assert_eq!(saved.location(), "/datasets/weekly_reports");
    assert!(get(&site, "/datasets/weekly_reports", Some(&bob))
        .await
        .body
        .contains("Rows 1–4 of 4"));
    let saved_upload = site.state.accounts.upload_named("weekly_reports").unwrap().unwrap();
    assert!(saved_upload.cleaning.drop_empty_rows && !saved_upload.cleaning.drop_duplicate_rows);
    assert!(!get(&site, "/", Some(&bob))
        .await
        .body
        .contains("Waiting for your review"));
    assert_eq!(
        get(&site, &review, Some(&bob)).await.status,
        StatusCode::NOT_FOUND,
        "the draft is gone"
    );

    // Both kinds of tidying together.
    let second = upload_draft(&site, &bob, &csrf, "again.csv", REPORTS)
        .await
        .location()
        .to_string();
    let second_save = format!("{second}/save");
    let both = submit(
        &site,
        &bob,
        &csrf,
        &second_save,
        &[("name", ""), ("drop_empty", "yes"), ("drop_duplicates", "yes")],
    )
    .await;
    assert_eq!(both.location(), "/datasets/again");
    assert!(get(&site, "/datasets/again", Some(&bob))
        .await
        .body
        .contains("Rows 1–3 of 3"));
}

#[tokio::test]
async fn drafts_can_be_discarded_and_expire() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let (ada, ada_csrf) = sign_in(&site, "ada", ADMIN_PASSWORD).await;
    let review = upload_draft(&site, &bob, &csrf, "oops.csv", REPORTS)
        .await
        .location()
        .to_string();
    assert_eq!(upload_files(&site), 1);

    let discard = format!("{review}/discard");
    assert_eq!(
        submit(&site, &ada, &ada_csrf, &discard, &[]).await.status,
        StatusCode::NOT_FOUND,
        "only the owner"
    );
    assert_eq!(
        submit(
            &site,
            &ada,
            &ada_csrf,
            &format!("{review}/save"),
            &[("name", "taken_over")]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        submit(&site, &bob, "wrong", &discard, &[]).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        submit(&site, &bob, &csrf, &discard, &[]).await.location(),
        "/?ok=discarded"
    );
    assert_eq!(upload_files(&site), 0, "the stored copy is deleted");
    assert!(site
        .state
        .accounts
        .drafts_of(account_id(&site, "bob"))
        .unwrap()
        .is_empty());

    upload_draft(&site, &bob, &csrf, "forgotten.csv", REPORTS).await;
    crate::review::remove_expired_drafts(&site.state.engine, &site.state.accounts, crate::review::DRAFT_DAYS);
    assert_eq!(upload_files(&site), 1, "a draft added today is kept");
    // With a limit of zero days every draft counts as expired.
    crate::review::remove_expired_drafts(&site.state.engine, &site.state.accounts, 0);
    assert_eq!(upload_files(&site), 0, "an expired draft's file is deleted");
    assert!(site
        .state
        .accounts
        .drafts_of(account_id(&site, "bob"))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn tidying_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let accounts_file = dir.path().join("accounts.sqlite");
    let uploads = dir.path().join("uploads");
    {
        let store = Store::open(&accounts_file).unwrap();
        let bob = store.create_first_admin("bob", USER_PASSWORD).unwrap();
        let _engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
        std::fs::write(uploads.join("0b.csv"), REPORTS).unwrap();
        let draft = store.create_draft(bob.id, "0b.csv", "r.csv").unwrap();
        let cleaning = beam_accounts::Cleaning {
            drop_empty_rows: true,
            drop_duplicate_rows: true,
        };
        store.save_draft(draft.id, "reports", cleaning).unwrap();
    }
    let store = Store::open(&accounts_file).unwrap();
    let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
    crate::restore_uploads(&engine, &store);
    assert_eq!(
        engine.query("SELECT count(*) FROM reports", &[], 1).unwrap()[0][0],
        json!(3)
    );
}

// ------------------------------------------- findings from independent review

#[tokio::test]
async fn simultaneous_guesses_cannot_get_past_the_lockout() {
    let site = site(false, 100);
    let attempts = (0..20).map(|n| {
        let password = if n == 19 {
            USER_PASSWORD.to_string()
        } else {
            format!("wrong guess {n}")
        };
        let site = &site;
        async move {
            post(site, "/login", None, &[("username", "bob"), ("password", &password)])
                .await
                .status
        }
    });
    let statuses = futures_join(attempts).await;
    let checked = statuses.iter().filter(|s| **s != StatusCode::TOO_MANY_REQUESTS).count();
    assert!(
        checked <= 5,
        "only five attempts may be checked at once, got {checked}: {statuses:?}"
    );
    assert!(statuses.iter().filter(|s| **s == StatusCode::TOO_MANY_REQUESTS).count() >= 15);
}

/// Runs futures concurrently on the current runtime and collects results in order.
async fn futures_join<F: std::future::Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = futures.map(Box::pin).collect();
    let mut results: Vec<Option<F::Output>> = pinned.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, result) in pinned.iter_mut().zip(results.iter_mut()) {
            if result.is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *result = Some(value),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    results.into_iter().map(|r| r.unwrap()).collect()
}

#[tokio::test]
async fn forms_posted_from_another_origin_are_refused() {
    let site = site(false, 100);
    let login = |origin: &'static str| {
        let request = Request::builder()
            .method("POST")
            .uri("/login")
            .header(HOST, "beam.test:9000")
            .header("origin", origin)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username=bob&password={}",
                USER_PASSWORD.replace(' ', "+")
            )))
            .unwrap();
        site.app.clone().oneshot(request)
    };
    assert_eq!(
        login("https://evil.example").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(login("null").await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        login("http://beam.test:9000").await.unwrap().status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn real_parquet_files_can_be_added_through_the_website() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("items.parquet");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY (SELECT range AS id, 'item ' || range AS label FROM range(7)) TO '{}' (FORMAT parquet)",
            path.to_str().unwrap()
        ))
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let saved = upload(&site, &bob, &csrf, "items.parquet", &bytes, "").await;
    assert_eq!(saved.location(), "/datasets/items", "{}", saved.body);
    assert!(get(&site, "/datasets/items", Some(&bob))
        .await
        .body
        .contains("Rows 1–7 of 7"));
}

#[tokio::test]
async fn charts_apply_is_empty_filters_and_owners_cannot_share_with_themselves() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    let chart = get(
        &site,
        "/datasets/sales/chart?column=shipped_at&op=empty&value=",
        Some(&bob),
    )
    .await;
    assert!(
        chart.body.contains("Only rows where"),
        "the filter is applied and said so"
    );
    assert!(chart
        .body
        .contains("<td>&lt;script&gt;alert(1)&lt;/script&gt;</td><td class=\"num\">1</td>"));
    assert!(
        !chart.body.contains("<td>EU</td>"),
        "rows with a shipping time are left out"
    );

    upload(&site, &bob, &csrf, "mine.csv", BUDGET, "").await;
    let me = account_id(&site, "bob").to_string();
    let refused = submit(&site, &bob, &csrf, "/datasets/mine/share", &[("account", &me)]).await;
    assert_eq!(refused.location(), "/datasets/mine/share?error=not-found");
    let upload = site.state.accounts.upload_named("mine").unwrap().unwrap();
    assert!(site.state.accounts.shares(upload.id).unwrap().is_empty());
}

#[tokio::test]
async fn reserved_words_as_column_names_still_filter_correctly() {
    let site = site(false, 100);
    let (bob, csrf) = sign_in(&site, "bob", USER_PASSWORD).await;
    upload(&site, &bob, &csrf, "words.csv", b"null,not,true\na,b,c\nd,e,f\n", "").await;
    let table = get(&site, "/datasets/words?column=null_&op=eq&value=a", Some(&bob)).await;
    assert!(table.body.contains("Rows 1–1 of 1"), "{}", table.body);
    let feed = call(
        &site,
        "GET",
        "/odata/words?$filter=not_%20eq%20'e'",
        Call {
            basic: Some(("bob", USER_PASSWORD)),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(feed.json()["value"].as_array().unwrap().len(), 1);
}
