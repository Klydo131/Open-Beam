//! Changing the model from the browser: an administrator writes it, sees
//! what the change would do, then applies it. Applying makes it live without
//! a restart and saves the file, or changes nothing at all.

#![allow(clippy::result_large_err)]

use crate::app::AppState;
use crate::model::{self, Model};
use crate::web::{check_csrf, layout, message_page, page, redirect, require_signed_in};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Form;
use beam_accounts::Role;
use maud::{html, Markup};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

type App = State<Arc<AppState>>;

/// What applying `text` would do, worked out without changing anything.
pub struct Plan {
    pub model: Option<Model>,
    pub problems: Vec<String>,
    /// Plain-words lines: what is added, changed and removed.
    pub changes: Vec<String>,
    /// Joined datasets to remove, with what is kept about each:
    /// `[descriptions, verified questions, watches]`.
    pub removed_joins: Vec<(String, [u64; 3])>,
    added_joins: Vec<String>,
    /// Joined datasets made again from a new recipe: what was written about
    /// them described the old data, so it goes too.
    remade_joins: Vec<String>,
}

/// Each link's joined dataset and what it is made from. A dataset whose
/// recipe changes, even only through an object's dataset or key, is made
/// again.
fn joins(model: &Model) -> BTreeMap<String, String> {
    model
        .links
        .iter()
        .filter_map(|l| {
            let (from, to) = (model.object(&l.from)?, model.object(&l.to)?);
            let recipe = format!(
                "{}|{}|{}|{}|{}",
                from.dataset,
                l.column,
                to.dataset,
                to.key,
                to.name.to_lowercase()
            );
            Some((model.joined(l), recipe))
        })
        .collect()
}

/// A fingerprint of the file as it was when the plan was made, so a plan
/// made before someone else's change is not applied over it.
pub fn digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The model file as it is now; a missing file reads as empty.
fn on_disk(state: &AppState) -> Result<String, String> {
    let path = state.model_path.as_deref().ok_or("this server has no model file")?;
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

pub fn plan(state: &AppState, text: &str) -> Plan {
    let current = state.model();
    let empty = Model::default();
    let old = current.as_deref().unwrap_or(&empty);
    let before = joins(old);
    // The datasets a model may describe: the configured ones, without the
    // joined datasets the current model added.
    let sets: Vec<_> = model::configured_sets(&state.engine)
        .into_iter()
        .filter(|s| !before.contains_key(&s.name))
        .collect();
    let mut plan = Plan {
        model: None,
        problems: Vec::new(),
        changes: Vec::new(),
        removed_joins: Vec::new(),
        added_joins: Vec::new(),
        remade_joins: Vec::new(),
    };
    let new = match model::parse(text, &sets) {
        Ok(new) => new,
        Err(problems) => {
            plan.problems = problems;
            return plan;
        }
    };
    let after = joins(&new);
    // A new joined name may be neither an upload's nor another dataset's.
    // Uploads that cannot be read refuse the change: a joined dataset that
    // took an upload's name would show its owner's notes to everyone.
    match state.accounts.uploads() {
        Err(e) => plan.problems.push(format!("cannot check the names of uploads: {e}")),
        Ok(uploads) => {
            let taken: Vec<String> = uploads
                .into_iter()
                .map(|u| u.name)
                .chain(
                    state
                        .engine
                        .datasets()
                        .into_iter()
                        .map(|d| d.name)
                        .filter(|n| !before.contains_key(n)),
                )
                .collect();
            for name in after.keys() {
                if taken.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                    plan.problems.push(format!(
                        "the joined dataset {name} would take the name of a dataset that already exists; give its link its own joined name"
                    ));
                }
            }
        }
    }
    if !plan.problems.is_empty() {
        return plan;
    }
    for (kind, old_items, new_items) in [
        (
            "object",
            describe(&old.objects, |o| &o.name),
            describe(&new.objects, |o| &o.name),
        ),
        (
            "link",
            describe(&old.links, |l| &l.name),
            describe(&new.links, |l| &l.name),
        ),
        (
            "metric",
            describe(&old.metrics, |m| &m.name),
            describe(&new.metrics, |m| &m.name),
        ),
        (
            "rule",
            describe(&old.rules, |r| &r.name),
            describe(&new.rules, |r| &r.name),
        ),
    ] {
        for (name, shape) in &new_items {
            match old_items.get(name) {
                None => plan.changes.push(format!("Adds {kind} {name}")),
                Some(old_shape) if old_shape != shape => plan.changes.push(format!("Changes {kind} {name}")),
                Some(_) => {}
            }
        }
        for name in old_items.keys().filter(|n| !new_items.contains_key(*n)) {
            plan.changes.push(format!("Removes {kind} {name}"));
        }
    }
    for (name, recipe) in &after {
        match before.get(name) {
            None => plan.changes.push(format!("Makes the joined dataset {name}")),
            Some(old_recipe) if old_recipe != recipe => {
                let [notes, questions, watches] = state.accounts.curation_count(name).unwrap_or_default();
                let mut line = format!("Makes the joined dataset {name} again, from its new link");
                if notes + questions + watches > 0 {
                    line.push_str(&format!(
                        ", removing its {}, {} and {}, which described the old data",
                        count(notes, "description"),
                        count(questions, "verified question"),
                        count(watches, "watch")
                    ));
                }
                plan.changes.push(line);
                plan.remade_joins.push(name.clone());
            }
            Some(_) => continue,
        }
        plan.added_joins.push(name.clone());
    }
    for name in before.keys().filter(|n| !after.contains_key(*n)) {
        let kept = state.accounts.curation_count(name).unwrap_or_default();
        plan.removed_joins.push((name.clone(), kept));
    }
    plan.model = Some(new);
    plan
}

fn describe<T: std::fmt::Debug>(items: &[T], name: impl Fn(&T) -> &String) -> BTreeMap<String, String> {
    items.iter().map(|i| (name(i).clone(), format!("{i:?}"))).collect()
}

/// Makes `text` the model: the joined datasets change, the file is saved,
/// and the new model is in force; or, on any failure, nothing changes.
/// `planned_from` is the [`digest`] of the file the preview was made from.
pub async fn apply(state: &Arc<AppState>, text: String, planned_from: String) -> Result<(), String> {
    let _one_at_a_time = state.model_edit.lock().await;
    let worker = state.clone();
    tokio::task::spawn_blocking(move || apply_now(&worker, &text, &planned_from))
        .await
        .map_err(|_| "the change could not be applied".to_string())??;
    *state.model_checks.lock().await = None;
    Ok(())
}

fn apply_now(state: &AppState, text: &str, planned_from: &str) -> Result<(), String> {
    let path = state.model_path.clone().ok_or("this server has no model file")?;
    if digest(&on_disk(state)?) != planned_from {
        return Err("The model was changed since this preview. Preview your change again.".into());
    }
    let plan = plan(state, text);
    let new = match plan.model {
        Some(new) if plan.problems.is_empty() => new,
        _ => return Err("The model has problems; preview it to see them.".into()),
    };
    let old = state.model();
    let engine = &state.engine;
    // Removed joins, and those made again from a new recipe.
    let removed: Vec<String> = plan
        .removed_joins
        .iter()
        .map(|(n, _)| n.clone())
        .chain(plan.added_joins.iter().filter(|n| engine.dataset(n).is_some()).cloned())
        .collect();
    let mut made: Vec<String> = Vec::new();
    // Puts back what was there before this change.
    let restore = |made: &[String]| {
        for name in made {
            let _ = engine.remove(name);
        }
        if let Some(old) = &old {
            for link in &old.links {
                let name = old.joined(link);
                if removed.contains(&name) && engine.dataset(&name).is_none() {
                    if let Err(e) = add_one(engine, old, link) {
                        eprintln!("model change: could not put back {name}: {e}; restart to restore it");
                    }
                }
            }
        }
    };
    let result = (|| {
        for name in &removed {
            engine.remove(name).map_err(|e| format!("cannot remove {name}: {e}"))?;
        }
        for link in &new.links {
            let name = new.joined(link);
            if plan.added_joins.contains(&name) {
                add_one(engine, &new, link).map_err(|e| format!("link {}: cannot make {name}: {e}", link.name))?;
                made.push(name);
            }
        }
        save(&path, text).map_err(|e| format!("cannot save the model file: {e}"))
    })();
    if let Err(e) = result {
        restore(&made);
        return Err(e);
    }
    *state.model.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(new));
    // What was written about a joined dataset that is gone no longer
    // describes anything, and must not reappear on a later dataset of that
    // name, such as an upload. A remade one now holds other data.
    for name in plan.removed_joins.iter().map(|(n, _)| n).chain(&plan.remade_joins) {
        if let Err(e) = state.accounts.forget_dataset(name) {
            eprintln!("model change: could not clear what was written about {name}: {e}");
        }
    }
    Ok(())
}

fn add_one(engine: &beam_engine::Engine, model: &Model, link: &model::Link) -> Result<(), String> {
    let (Some(from), Some(to)) = (model.object(&link.from), model.object(&link.to)) else {
        return Err("undeclared object".into());
    };
    engine
        .add_lookup(
            &model.joined(link),
            &from.dataset,
            &link.column,
            &to.dataset,
            &to.key,
            &to.name.to_lowercase(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Writes beside the file and renames over it, so a crash leaves either the
/// old file or the new one, never half of one.
fn save(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.partial", std::process::id()));
    let partial = path.with_file_name(name);
    let written = (|| {
        let mut file = std::fs::File::create(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&partial, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

// ------------------------------------------------------------------ pages

/// "1 watch", "2 watches".
fn count(n: u64, one: &str) -> String {
    match (n, one.ends_with("ch")) {
        (1, _) => format!("1 {one}"),
        (_, true) => format!("{n} {one}es"),
        _ => format!("{n} {one}s"),
    }
}

fn admin(state: &AppState, headers: &HeaderMap) -> Result<crate::auth::Signed, Response> {
    let signed = require_signed_in(state, headers)?;
    if signed.session.account.role != Role::Admin {
        return Err(message_page(
            StatusCode::FORBIDDEN,
            "Administrators only",
            "Only administrators can change the model.",
        ));
    }
    if state.model_path.is_none() {
        return Err(message_page(
            StatusCode::NOT_FOUND,
            "No model file",
            "Name a model file with ontology in beam.toml to change the model here.",
        ));
    }
    Ok(signed)
}

fn editor(csrf: &str, text: &str) -> Markup {
    html! {
        form.stack method="post" action="/model/plan" {
            input type="hidden" name="csrf" value=(csrf);
            label for="text" { "Model" }
            textarea #text name="text" rows="24" spellcheck="false" maxlength="262144" { (text) }
            p.hint { "Objects, links, metrics and rules, as in ontology.example.toml. Nothing changes until you apply the preview." }
            button type="submit" { "Preview change" }
        }
    }
}

pub async fn edit_route(State(state): App, headers: HeaderMap) -> Response {
    let signed = match admin(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let text = match on_disk(&state) {
        Ok(text) => text,
        Err(e) => return message_page(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read the model", &e),
    };
    let body = html! {
        p.crumbs { a href="/model" { "Model" } " / Edit" }
        h1 { "Edit the model" }
        section.panel { (editor(&signed.session.csrf, &text)) }
    };
    page(StatusCode::OK, layout("Edit the model", Some(&signed), body))
}

impl EditForm {
    /// Browsers send line breaks as CRLF; the file keeps LF, so saving
    /// from the page does not rewrite every line.
    fn normalised(mut self) -> Self {
        self.text = self.text.replace("\r\n", "\n");
        self
    }
}

#[derive(Deserialize)]
pub struct EditForm {
    csrf: String,
    text: String,
    #[serde(default)]
    digest: String,
}

fn plan_page(signed: &crate::auth::Signed, text: &str, plan: &Plan, error: Option<&str>, digest: &str) -> Markup {
    let csrf = &signed.session.csrf;
    html! {
        p.crumbs { a href="/model" { "Model" } " / Edit" }
        h1 { "Preview the change" }
        @if let Some(error) = error { p.notice.error role="alert" { (error) } }
        @if !plan.problems.is_empty() {
            section.panel.attention {
                h2 { "Problems to fix" }
                ul { @for problem in &plan.problems { li { (problem) } } }
            }
        } @else {
            section.panel {
                h2 { "What applying it does" }
                @if plan.changes.is_empty() && plan.removed_joins.is_empty() {
                    p.empty { "Nothing: this is the model already in force." }
                } @else {
                    ul {
                        @for change in &plan.changes { li { (change) } }
                        @for (name, [notes, questions, watches]) in &plan.removed_joins {
                            li { "Removes the joined dataset " (name)
                                @if notes + questions + watches > 0 {
                                    ", with its " (count(*notes, "description")) ", " (count(*questions, "verified question")) " and " (count(*watches, "watch")) ""
                                } }
                        }
                    }
                }
                form method="post" action="/model/apply" {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="digest" value=(digest);
                    input type="hidden" name="text" value=(text);
                    button type="submit" { "Apply change" }
                }
            }
        }
        section.panel { h2 { "Keep editing" } (editor(csrf, text)) }
    }
}

pub async fn plan_route(State(state): App, headers: HeaderMap, Form(form): Form<EditForm>) -> Response {
    let signed = match admin(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let on_disk = match on_disk(&state) {
        Ok(text) => text,
        Err(e) => return message_page(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read the model", &e),
    };
    let form = form.normalised();
    let worker = state.clone();
    let text = form.text.clone();
    let Ok(plan) = tokio::task::spawn_blocking(move || plan(&worker, &text)).await else {
        return message_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "The preview could not be made.",
        );
    };
    let body = plan_page(&signed, &form.text, &plan, None, &digest(&on_disk));
    page(StatusCode::OK, layout("Preview the change", Some(&signed), body))
}

pub async fn apply_route(State(state): App, headers: HeaderMap, Form(form): Form<EditForm>) -> Response {
    let signed = match admin(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let form = form.normalised();
    match apply(&state, form.text.clone(), form.digest.clone()).await {
        Ok(()) => redirect("/model"),
        Err(error) => {
            let worker = state.clone();
            let text = form.text.clone();
            let Ok(plan) = tokio::task::spawn_blocking(move || plan(&worker, &text)).await else {
                return message_page(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong", &error);
            };
            let body = plan_page(&signed, &form.text, &plan, Some(&error), &form.digest);
            page(StatusCode::CONFLICT, layout("Preview the change", Some(&signed), body))
        }
    }
}
