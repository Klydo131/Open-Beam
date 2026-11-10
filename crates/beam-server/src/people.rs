//! People: everyone with an active account, and your own contacts.
//!
//! The directory is for working together: finding a colleague to share a
//! dataset with. It shows names, teams and roles, never activity. Account
//! management stays on the administrators' Accounts page.

use crate::app::AppState;
use crate::web::{check_csrf, layout, notice, page, redirect, require_signed_in, role_label};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_accounts::Account;
use maud::{html, Markup};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

type App = State<Arc<AppState>>;

fn matches(account: &Account, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || [
            Some(account.username.as_str()),
            account.display_name.as_deref(),
            account.team.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(&query))
}

fn person(account: &Account, is_contact: bool, me: i64, csrf: &str) -> Markup {
    html! {
        li.person {
            div {
                strong { (account.name()) }
                span.muted { " @" (account.username) }
                @if let Some(team) = &account.team { div.muted { (team) } }
            }
            div.row {
                span.badge { (role_label(account.role)) }
                @if account.id == me {
                    span.badge { "you" }
                } @else {
                    form.row method="post" action={ "/people/" (account.id) "/contact" } {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="action" value=(if is_contact { "remove" } else { "add" });
                        button.quiet type="submit" { (if is_contact { "Remove contact" } else { "Add contact" }) }
                    }
                }
            }
        }
    }
}

pub async fn people(State(state): App, Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let me = signed.session.account.id;
    let search = query
        .get("q")
        .map(String::as_str)
        .unwrap_or("")
        .chars()
        .take(64)
        .collect::<String>();
    let everyone: Vec<Account> = state
        .accounts
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.active)
        .collect();
    let contacts = state.accounts.contacts(me).unwrap_or_default();
    let mine: Vec<&Account> = everyone.iter().filter(|a| contacts.contains(&a.id)).collect();
    let found: Vec<&Account> = everyone.iter().filter(|a| matches(a, &search)).collect();
    let csrf = &signed.session.csrf;

    let body = html! {
        h1 { "People" }
        (notice(&query))
        p.lead { "Everyone with an account here. Add colleagues you work with to your contacts so they are first in the list when you share a dataset." }
        section.panel {
            h2 { "Your contacts" }
            @if mine.is_empty() {
                p.muted { "No contacts yet. Use " strong { "Add contact" } " below." }
            } @else {
                ul.people { @for account in &mine { (person(account, true, me, csrf)) } }
            }
        }
        section.panel {
            h2 { "Everyone" }
            form.filter method="get" action="/people" role="search" {
                label for="q" { "Find" }
                input #q name="q" value=(search) placeholder="name, username or team";
                button type="submit" { "Search" }
                @if !search.is_empty() { a.quiet.button href="/people" { "Clear" } }
            }
            @if found.is_empty() {
                p.empty { "Nobody matches." }
            } @else {
                ul.people { @for account in &found { (person(account, contacts.contains(&account.id), me, csrf)) } }
            }
        }
    };
    page(StatusCode::OK, layout("People", Some(&signed), body))
}

#[derive(Deserialize)]
pub struct ContactForm {
    csrf: String,
    action: String,
}

pub async fn contact(
    State(state): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<ContactForm>,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let me = signed.session.account.id;
    let outcome = match form.action.as_str() {
        "add" => state.accounts.add_contact(me, id).map(|_| "ok=contact-added"),
        "remove" => state.accounts.remove_contact(me, id).map(|_| "ok=contact-removed"),
        _ => Ok("error=failed"),
    };
    redirect(&format!("/people?{}", outcome.unwrap_or("error=not-found")))
}
