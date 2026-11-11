//! Review before saving: a newly added file is a private draft until its
//! owner has looked at it and pressed Save.
//!
//! The review shows what the file contains and the problems worth knowing
//! about before anyone relies on it: empty rows, rows that repeat exactly
//! (often the same thing reported twice), and columns with no values. The
//! owner decides what to tidy. Tidying never edits the file; it is applied
//! whenever the dataset is read, so what was left out can always be checked.

// Handlers return the page to show as the `Err` side of small helpers.
#![allow(clippy::result_large_err)]

use crate::app::AppState;
use crate::auth::Signed;
use crate::uploads::{base_name, format_of, unused_name};
use crate::web::{check_csrf, layout, message_page, notice, page, redirect, require_signed_in};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_accounts::{Cleaning, Draft, Store};
use beam_engine::{ColumnType, Engine};
use maud::html;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Rows shown in the preview.
pub const PREVIEW_ROWS: usize = 20;
/// Drafts nobody saves or discards are removed after this many days.
pub const DRAFT_DAYS: u32 = 7;

type App = State<Arc<AppState>>;

fn type_label(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Boolean => "Yes / no",
        ColumnType::Int32 | ColumnType::Int64 => "Whole number",
        ColumnType::Float64 | ColumnType::Decimal { .. } => "Number",
        ColumnType::Text => "Text",
        ColumnType::Date => "Date",
        ColumnType::Timestamp => "Date and time",
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The draft, if it belongs to `signed`. Anyone else's draft looks missing,
/// administrators included: a draft has not entered the database yet.
fn owned_draft(state: &AppState, signed: &Signed, id: i64) -> Result<Draft, Response> {
    match state.accounts.draft(id) {
        Ok(Some(draft)) if draft.owner_id == signed.session.account.id => Ok(draft),
        _ => Err(message_page(
            StatusCode::NOT_FOUND,
            "Draft not found",
            "It may have been saved or discarded already.",
        )),
    }
}

fn suggested_name(state: &AppState, draft: &Draft) -> String {
    let stem = draft
        .original_name
        .rsplit_once('.')
        .map_or(draft.original_name.as_str(), |(stem, _)| stem);
    unused_name(state, stem).unwrap_or_else(|| base_name(stem))
}

pub async fn review(
    State(state): App,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let draft = match owned_draft(&state, &signed, id) {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let preview = match state.remembered_preview(&draft.file) {
        Some(preview) => Ok(Ok(preview)),
        None => {
            let worker = state.clone();
            let file = draft.file.clone();
            tokio::task::spawn_blocking(move || {
                let preview = worker.engine.preview(&file, format_of(&file), PREVIEW_ROWS)?;
                worker.remember_preview(&file, preview.clone());
                Ok::<_, beam_engine::EngineError>(Arc::new(preview))
            })
            .await
        }
    };
    let Ok(Ok(preview)) = preview else {
        return message_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "This draft cannot be read",
            "Discard it from the Datasets page and add the file again.",
        );
    };
    let name = query
        .get("name")
        .cloned()
        .unwrap_or_else(|| suggested_name(&state, &draft));
    let problems = preview.empty_rows > 0 || preview.duplicate_rows > 0 || !preview.empty_columns.is_empty();
    let csrf = &signed.session.csrf;

    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / Review" }
        h1 { "Review before saving" }
        (notice(&query))
        p.lead { "From " strong { (draft.original_name) } ". Only you can see this draft. Nothing is saved as a dataset until you press " strong { "Save" } "." }
        div.stats {
            div { strong { (preview.row_count) } span { "rows" } }
            div { strong { (preview.columns.len()) } span { "columns" } }
            div class=(if problems { "warn" } else { "good" }) {
                strong { (if problems { "Check" } else { "Ready" }) }
                span { (if problems { "a few things to look at" } else { "no problems found" }) }
            }
        }
        form.panel method="post" action={ "/data/drafts/" (draft.id) "/save" } {
            input type="hidden" name="csrf" value=(csrf);
            h2 { "Checks" }
            @if !problems {
                p { "No empty rows, no repeated rows, and every column has values." }
            }
            @if preview.empty_rows > 0 {
                div.finding {
                    p { strong { (plural(preview.empty_rows, "row is", "rows are")) " completely empty." } " Usually these are gaps at the end of a spreadsheet." }
                    label.check { input type="checkbox" name="drop_empty" value="yes" checked; "Leave out empty rows" }
                }
            }
            @if preview.duplicate_rows > 0 {
                div.finding {
                    p {
                        strong { (plural(preview.duplicate_rows, "row repeats", "rows repeat")) " an earlier row exactly." }
                        " If each row should be one person or one event, these were probably reported twice. If identical rows can genuinely happen, keep them."
                    }
                    label.check { input type="checkbox" name="drop_duplicates" value="yes"; "Keep only one copy of repeated rows" }
                }
            }
            @if !preview.empty_columns.is_empty() {
                div.finding {
                    p { strong { "No values in: " (preview.empty_columns.join(", ")) "." } " The column is kept so the layout matches your file." }
                }
            }
            p.hint { "Tidying does not change your file. It is applied whenever the dataset is read, so it can always be checked." }
            h2 { "Name" }
            label for="name" { "Dataset name" }
            input #name name="name" value=(name) maxlength="48" required;
            p.hint { "Letters, digits and underscores. This is also its name in Excel and Power BI." }
            div.actions {
                button type="submit" { "Save dataset" }
            }
        }
        section.panel {
            h2 { "Columns" }
            ul.columns-found {
                @for column in &preview.columns {
                    li { strong { (column.name) } span.muted { (type_label(column.ty)) } }
                }
            }
        }
        section {
            h2.section { "First " (preview.rows.len()) " rows" }
            div.tablewrap {
                table {
                    thead { tr { @for column in &preview.columns { th { (column.name) } } } }
                    tbody {
                        @for row in &preview.rows {
                            tr {
                                @for value in row {
                                    @match value {
                                        Value::String(text) => td { (text) },
                                        _ => td.null { "—" },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        form.panel.narrow method="post" action={ "/data/drafts/" (draft.id) "/discard" } {
            input type="hidden" name="csrf" value=(csrf);
            h2 { "Not the right file?" }
            p { "Discarding deletes this draft. Your original file is not affected." }
            button.quiet type="submit" { "Discard draft" }
        }
    };
    page(StatusCode::OK, layout("Review before saving", Some(&signed), body))
}

#[derive(Deserialize)]
pub struct SaveForm {
    csrf: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    drop_empty: Option<String>,
    #[serde(default)]
    drop_duplicates: Option<String>,
}

pub async fn save(State(state): App, Path(id): Path<i64>, headers: HeaderMap, Form(form): Form<SaveForm>) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let draft = match owned_draft(&state, &signed, id) {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let typed = form.name.trim();
    let name = if typed.is_empty() {
        suggested_name(&state, &draft)
    } else {
        base_name(typed)
    };
    let back = |error: &str| {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("error", error).append_pair("name", &name);
        redirect(&format!("/data/drafts/{id}?{}", query.finish()))
    };
    if state
        .engine
        .datasets()
        .iter()
        .any(|d| d.name.eq_ignore_ascii_case(&name))
    {
        return back("name-taken");
    }
    let cleaning = Cleaning {
        drop_empty_rows: form.drop_empty.as_deref() == Some("yes"),
        drop_duplicate_rows: form.drop_duplicates.as_deref() == Some("yes"),
    };
    let engine_cleaning = beam_engine::Cleaning {
        drop_empty_rows: cleaning.drop_empty_rows,
        drop_duplicate_rows: cleaning.drop_duplicate_rows,
    };
    if let Err(e) = state
        .engine
        .add_upload_cleaned(&name, &draft.file, format_of(&draft.file), engine_cleaning)
    {
        eprintln!("draft {} not saved: {e}", draft.file);
        return back("failed");
    }
    match state.accounts.save_draft(draft.id, &name, cleaning) {
        Ok(_) => {
            state.forget_preview(&draft.file);
            redirect(&format!("/datasets/{name}"))
        }
        Err(e) => {
            let _ = state.engine.remove(&name);
            back(if matches!(e, beam_accounts::AccountError::NameTaken) {
                "name-taken"
            } else {
                "failed"
            })
        }
    }
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

pub async fn discard(
    State(state): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let draft = match owned_draft(&state, &signed, id) {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    remove_draft(&state.engine, &state.accounts, &draft);
    state.forget_preview(&draft.file);
    redirect("/?ok=discarded")
}

fn remove_draft(engine: &Engine, accounts: &Store, draft: &Draft) {
    if let Some(dir) = engine.upload_dir() {
        if let Err(e) = std::fs::remove_file(dir.join(&draft.file)) {
            eprintln!("could not remove {}: {e}", draft.file);
        }
    }
    let _ = accounts.delete_draft(draft.id);
}

/// Deletes drafts nobody saved or discarded within `days` days, files included.
pub fn remove_expired_drafts(engine: &Engine, accounts: &Store, days: u32) {
    for draft in accounts.expired_drafts(days).unwrap_or_default() {
        remove_draft(engine, accounts, &draft);
    }
}
