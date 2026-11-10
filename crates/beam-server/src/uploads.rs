//! Adding data from the browser, and sharing it with colleagues.
//!
//! An upload is checked, written under a name the server chooses, parsed once
//! by the engine, and only then recorded as belonging to the person who added
//! it. Nothing an uploader types becomes part of a file path.

// Handlers return the page to show as the `Err` side of small helpers.
#![allow(clippy::result_large_err)]

use crate::app::AppState;
use crate::auth::Signed;
use crate::catalog::{self, Entry};
use crate::table::{self, Tab};
use crate::web::{check_csrf, layout, message_page, notice, page, redirect, require_signed_in};
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_accounts::{Account, AccountError, Mode};
use beam_engine::Format;
use maud::html;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::Arc;

/// Largest unpacked size accepted for an Excel workbook. Workbooks are zip
/// archives; this refuses ones that declare an implausible unpacked size.
const MAX_UNPACKED_XLSX: u64 = 256 * 1024 * 1024;
const MAX_XLSX_ENTRIES: usize = 10_000;

type App = State<Arc<AppState>>;

/// What kind of file was sent, judged by its name and confirmed by its bytes.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Csv,
    Parquet,
    Excel,
}

fn kind_of(file_name: &str, bytes: &[u8]) -> Result<Kind, &'static str> {
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let kind = match extension.as_str() {
        "csv" | "tsv" | "txt" => Kind::Csv,
        "parquet" => Kind::Parquet,
        "xlsx" => Kind::Excel,
        "xls" => return Err("Older .xls workbooks are not supported. Save it as .xlsx in Excel and try again."),
        _ => return Err("Choose a CSV, Excel (.xlsx) or Parquet file."),
    };
    if bytes.is_empty() {
        return Err("That file is empty.");
    }
    let looks_right = match kind {
        Kind::Csv => !bytes.iter().take(64 * 1024).any(|b| *b == 0),
        Kind::Parquet => bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"),
        Kind::Excel => bytes.starts_with(b"PK\x03\x04"),
    };
    if looks_right {
        Ok(kind)
    } else {
        Err("That file's contents do not match its name. Check it opens in its usual program.")
    }
}

/// Quotes one CSV field when it needs it.
pub fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// Converts the first sheet of a workbook to CSV for the engine to read.
fn excel_to_csv(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    use calamine::{Data, Reader, Xlsx};
    let unreadable = "That workbook could not be read. Check it opens in Excel.";
    {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| unreadable)?;
        if archive.len() > MAX_XLSX_ENTRIES {
            return Err("That workbook is too large to add.");
        }
        let mut unpacked: u64 = 0;
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index).map_err(|_| unreadable)?;
            unpacked = unpacked.saturating_add(entry.size());
        }
        if unpacked > MAX_UNPACKED_XLSX {
            return Err("That workbook is too large to add.");
        }
    }
    let mut workbook: Xlsx<_> = calamine::open_workbook_from_rs(Cursor::new(bytes)).map_err(|_| unreadable)?;
    let range = workbook
        .worksheet_range_at(0)
        .ok_or("That workbook has no sheets.")?
        .map_err(|_| unreadable)?;
    let mut out = Vec::new();
    for row in range.rows() {
        let fields: Vec<String> = row
            .iter()
            .map(|cell| match cell {
                Data::Empty | Data::Error(_) => String::new(),
                Data::String(s) => csv_field(s),
                Data::Int(i) => i.to_string(),
                Data::Float(f) => f.to_string(),
                Data::Bool(b) => b.to_string(),
                Data::DateTime(dt) if dt.is_datetime() => {
                    let (y, mo, d, h, mi, s, _) = dt.to_ymd_hms_milli();
                    if (h, mi, s) == (0, 0, 0) {
                        format!("{y:04}-{mo:02}-{d:02}")
                    } else {
                        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
                    }
                }
                Data::DateTime(dt) => dt.as_f64().to_string(),
                Data::DateTimeIso(s) | Data::DurationIso(s) => csv_field(s),
            })
            .collect();
        writeln!(out, "{}", fields.join(",")).map_err(|_| unreadable)?;
    }
    if out.iter().all(|b| matches!(b, b',' | b'\n')) {
        return Err("The first sheet of that workbook is empty.");
    }
    Ok(out)
}

/// A dataset name derived from what the person typed or the file's name:
/// lower case, letters, digits and underscores, starting with a letter.
fn base_name(text: &str) -> String {
    let mut name = String::new();
    for c in text.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.ends_with('_') {
            name.push('_');
        }
        if name.len() >= 48 {
            break;
        }
    }
    let name = name.trim_matches('_').to_string();
    match name.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => name,
        Some(_) => format!("data_{name}"),
        None => "data".to_string(),
    }
}

fn random_file_name(extension: &str) -> Result<String, &'static str> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "Could not store that file. Please try again.")?;
    Ok(format!(
        "{}.{extension}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

// ------------------------------------------------------------------ pages

fn upload_page(signed: &Signed, state: &AppState, error: Option<&str>) -> Response {
    let advanced = signed.session.account.mode == Mode::Advanced;
    let limit_mb = state.max_upload_bytes / (1024 * 1024);
    let status = if error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / Add data" }
        h1 { "Add data" }
        @if let Some(error) = error { p.notice.error role="alert" { (error) } }
        section.panel.narrow {
            form method="post" action="/data" enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(signed.session.csrf);
                label for="file" { "File" }
                input #file name="file" type="file" required accept=".csv,.tsv,.txt,.xlsx,.parquet";
                p.hint { "CSV, Excel (.xlsx, first sheet) or Parquet, up to " (limit_mb) " MB. The first row should hold the column names." }
                @if advanced {
                    label for="name" { "Dataset name (optional)" }
                    input #name name="name" maxlength="48" placeholder="taken from the file name";
                    p.hint { "Letters, digits and underscores. This is also its name in Excel and Power BI." }
                }
                button type="submit" { "Add data" }
            }
        }
        section.panel.narrow {
            h2 { "Who can see it" }
            p { "Only you, and administrators. Share it with colleagues from its " strong { "Sharing" } " tab once it is added." }
        }
    };
    page(status, layout("Add data", Some(signed), body))
}

pub async fn upload_form(State(state): App, headers: HeaderMap) -> Response {
    match require_signed_in(&state, &headers) {
        Ok(signed) => upload_page(&signed, &state, None),
        Err(response) => response,
    }
}

pub async fn upload(State(state): App, headers: HeaderMap, mut multipart: Multipart) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let (mut csrf, mut name, mut file) = (String::new(), String::new(), None);
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let field_name = field.name().unwrap_or_default().to_string();
                let file_name = field.file_name().unwrap_or_default().to_string();
                let Ok(bytes) = field.bytes().await else {
                    return upload_page(
                        &signed,
                        &state,
                        Some("That file is too large, or the upload was interrupted."),
                    );
                };
                match field_name.as_str() {
                    "csrf" => csrf = String::from_utf8_lossy(&bytes).into_owned(),
                    "name" => name = String::from_utf8_lossy(&bytes).chars().take(64).collect(),
                    "file" if bytes.len() > state.max_upload_bytes => {
                        let limit = state.max_upload_bytes / (1024 * 1024);
                        let message = if limit == 0 {
                            "That file is too large to add.".to_string()
                        } else {
                            format!("That file is larger than {limit} MB.")
                        };
                        return upload_page(&signed, &state, Some(&message));
                    }
                    "file" => file = Some((file_name, bytes)),
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(_) => {
                return upload_page(
                    &signed,
                    &state,
                    Some("That file is too large, or the upload was interrupted."),
                )
            }
        }
    }
    if let Err(response) = check_csrf(&signed, &csrf) {
        return response;
    }
    let Some((file_name, bytes)) = file else {
        return upload_page(&signed, &state, Some("Choose a file to add."));
    };
    let kind = match kind_of(&file_name, &bytes) {
        Ok(kind) => kind,
        Err(message) => return upload_page(&signed, &state, Some(message)),
    };
    let wanted = if name.trim().is_empty() {
        file_name
            .rsplit_once('.')
            .map_or(file_name.as_str(), |(stem, _)| stem)
            .to_string()
    } else {
        name
    };

    let owner = signed.session.account.id;
    let worker = state.clone();
    let added = tokio::task::spawn_blocking(move || add(&worker, owner, &wanted, kind, &bytes))
        .await
        .unwrap_or(Err("Could not add that file. Please try again."));
    match added {
        Ok(name) => redirect(&format!("/datasets/{name}")),
        Err(message) => upload_page(&signed, &state, Some(message)),
    }
}

/// Stores, parses and records an upload. Undoes each step if a later one fails.
fn add(state: &AppState, owner: i64, wanted: &str, kind: Kind, bytes: &[u8]) -> Result<String, &'static str> {
    let dir = state
        .engine
        .upload_dir()
        .ok_or("Adding data is turned off on this server.")?;
    let (contents, extension, format) = match kind {
        Kind::Excel => (std::borrow::Cow::Owned(excel_to_csv(bytes)?), "csv", Format::Csv),
        Kind::Csv => (std::borrow::Cow::Borrowed(bytes), "csv", Format::Csv),
        Kind::Parquet => (std::borrow::Cow::Borrowed(bytes), "parquet", Format::Parquet),
    };
    let file = random_file_name(extension)?;
    let path = dir.join(&file);
    let stored = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut handle| handle.write_all(&contents));
    if stored.is_err() {
        let _ = std::fs::remove_file(&path);
        return Err("Could not store that file. Please try again.");
    }

    // Pick a name nobody is using: sales, sales_2, sales_3, …
    let base = base_name(wanted);
    let names: Vec<String> = state
        .engine
        .datasets()
        .into_iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();
    let name = (1..1000)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}_{n}") })
        .find(|candidate| !names.contains(candidate))
        .ok_or("Too many datasets share that name. Choose another name.")?;

    if let Err(e) = state.engine.add_upload(&name, &file, format) {
        eprintln!("upload {file} not added: {e}");
        let _ = std::fs::remove_file(&path);
        return Err("That file could not be read as a table. Check that its first row holds the column names.");
    }
    if let Err(e) = state.accounts.create_upload(&name, owner, &file) {
        eprintln!("upload {file} not recorded: {e}");
        let _ = state.engine.remove(&name);
        let _ = std::fs::remove_file(&path);
        return Err("Could not add that file. Please try again.");
    }
    Ok(name)
}

// ---------------------------------------------------------------- sharing

/// The dataset, if `signed` may manage it; otherwise the page to show instead.
fn managed(state: &AppState, signed: &Signed, name: &str) -> Result<Entry, Response> {
    let not_found = || {
        message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        )
    };
    let entry = catalog::find(state, &signed.session.account, name).ok_or_else(not_found)?;
    if entry.upload.is_none() {
        return Err(message_page(
            StatusCode::FORBIDDEN,
            "Built-in dataset",
            "This dataset comes from the server's configuration. Everyone with an account can see it.",
        ));
    }
    if !table::can_manage(signed, &entry) {
        return Err(message_page(
            StatusCode::FORBIDDEN,
            "Not your dataset",
            "Only the person who added this dataset, or an administrator, can change who sees it.",
        ));
    }
    Ok(entry)
}

pub async fn sharing(
    State(state): App,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let entry = match managed(&state, &signed, &name) {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let Some(upload) = entry.upload.as_ref() else {
        return redirect("/");
    };
    let everyone = state.accounts.list().unwrap_or_default();
    let shared = state.accounts.shares(upload.id).unwrap_or_default();
    let contacts = state.accounts.contacts(signed.session.account.id).unwrap_or_default();
    let owner = everyone.iter().find(|a| a.id == upload.owner_id);
    let candidates: Vec<&Account> = everyone
        .iter()
        .filter(|a| a.active && a.id != upload.owner_id && !shared.contains(&a.id))
        .collect();
    let (close, others): (Vec<&Account>, Vec<&Account>) =
        candidates.into_iter().partition(|a| contacts.contains(&a.id));
    let csrf = &signed.session.csrf;
    let set = &entry.set;

    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / " (set.name) }
        h1 { (set.name) }
        (table::dataset_tabs(set, Tab::Share, &[], true))
        (notice(&query))
        section.panel.narrow {
            h2 { "Who can see this dataset" }
            ul.people {
                @if let Some(owner) = owner {
                    li { span { strong { (owner.name()) } " " span.muted { "@" (owner.username) } } span.badge { "Owner" } }
                }
                @for account in everyone.iter().filter(|a| shared.contains(&a.id)) {
                    li {
                        span { strong { (account.name()) } " " span.muted { "@" (account.username) } }
                        form.row method="post" action={ "/datasets/" (set.name) "/share/" (account.id) "/remove" } {
                            input type="hidden" name="csrf" value=(csrf);
                            button.quiet type="submit" { "Stop sharing" }
                        }
                    }
                }
            }
            p.hint { "Administrators can see every dataset." }
        }
        section.panel.narrow {
            h2 { "Share with a colleague" }
            @if close.is_empty() && others.is_empty() {
                p { "Everyone with an account can already see it." }
            } @else {
                form method="post" action={ "/datasets/" (set.name) "/share" } {
                    input type="hidden" name="csrf" value=(csrf);
                    label for="account" { "Colleague" }
                    select #account name="account" {
                        @if !close.is_empty() {
                            optgroup label="Your contacts" {
                                @for account in &close { option value=(account.id) { (account.name()) " (@" (account.username) ")" } }
                            }
                        }
                        @if !others.is_empty() {
                            optgroup label="Everyone else" {
                                @for account in &others { option value=(account.id) { (account.name()) " (@" (account.username) ")" } }
                            }
                        }
                    }
                    button type="submit" { "Share" }
                }
                p.hint { "They will see it on their Datasets page and in Excel and Power BI." }
            }
        }
        section.panel.narrow.danger {
            h2 { "Delete this dataset" }
            p { "Removes it for everyone, including from Excel and Power BI. The original file on your computer is not affected." }
            form method="post" action={ "/datasets/" (set.name) "/delete" } {
                input type="hidden" name="csrf" value=(csrf);
                label.check { input type="checkbox" name="confirm" value="yes" required; "I understand this cannot be undone" }
                button.danger type="submit" { "Delete dataset" }
            }
        }
    };
    page(
        StatusCode::OK,
        layout(&format!("{} sharing", set.name), Some(&signed), body),
    )
}

#[derive(Deserialize)]
pub struct ShareForm {
    csrf: String,
    account: i64,
}

pub async fn share(
    State(state): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ShareForm>,
) -> Response {
    change_share(
        &state,
        &headers,
        &name,
        &form.csrf,
        |state, upload| state.accounts.share(upload, form.account),
        "shared",
    )
    .await
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

pub async fn unshare(
    State(state): App,
    Path((name, account)): Path<(String, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    change_share(
        &state,
        &headers,
        &name,
        &form.csrf,
        |state, upload| state.accounts.unshare(upload, account),
        "unshared",
    )
    .await
}

async fn change_share(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    name: &str,
    csrf: &str,
    change: impl FnOnce(&AppState, i64) -> Result<(), AccountError>,
    ok: &str,
) -> Response {
    let signed = match require_signed_in(state, headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, csrf) {
        return response;
    }
    let entry = match managed(state, &signed, name) {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let Some(upload) = entry.upload else {
        return redirect("/");
    };
    let outcome = match change(state, upload.id) {
        Ok(()) => format!("ok={ok}"),
        Err(_) => "error=not-found".to_string(),
    };
    redirect(&format!("/datasets/{name}/share?{outcome}"))
}

#[derive(Deserialize)]
pub struct DeleteForm {
    csrf: String,
    #[serde(default)]
    confirm: String,
}

pub async fn delete(
    State(state): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&signed, &form.csrf) {
        return response;
    }
    let entry = match managed(&state, &signed, &name) {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let Some(upload) = entry.upload else {
        return redirect("/");
    };
    if form.confirm != "yes" {
        return redirect(&format!("/datasets/{name}/share?error=confirm"));
    }
    if state.engine.remove(&upload.name).is_err() || state.accounts.delete_upload(upload.id).is_err() {
        return message_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "The dataset could not be deleted.",
        );
    }
    if let Some(dir) = state.engine.upload_dir() {
        if let Err(e) = std::fs::remove_file(dir.join(&upload.file)) {
            eprintln!("could not remove {}: {e}", upload.file);
        }
    }
    redirect("/?ok=deleted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_tidied_from_whatever_was_typed() {
        for (typed, name) in [
            ("Q1 Sales (final).xlsx", "q1_sales_final_xlsx"),
            ("2025 budget", "data_2025_budget"),
            ("../../etc/passwd", "etc_passwd"),
            ("Ünïcödé", "n_c_d"),
            ("", "data"),
            ("___", "data"),
        ] {
            assert_eq!(base_name(typed), name, "{typed}");
        }
        assert!(base_name(&"x".repeat(200)).len() <= 48);
    }

    #[test]
    fn file_kinds_are_checked_by_name_and_contents() {
        assert_eq!(kind_of("a.CSV", b"a,b\n1,2\n"), Ok(Kind::Csv));
        assert_eq!(kind_of("a.parquet", b"PAR1....PAR1"), Ok(Kind::Parquet));
        assert_eq!(kind_of("a.xlsx", b"PK\x03\x04rest"), Ok(Kind::Excel));
        assert!(kind_of("a.csv", b"").is_err());
        assert!(kind_of("a.csv", b"MZ\x00\x00binary").is_err());
        assert!(kind_of("a.parquet", b"a,b\n").is_err());
        assert!(kind_of("a.xlsx", b"a,b\n").is_err());
        assert!(kind_of("a.exe", b"a,b").is_err());
        assert!(kind_of("a.xls", b"a,b").unwrap_err().contains(".xlsx"));
        assert!(kind_of("noextension", b"a,b").is_err());
    }

    #[test]
    fn csv_fields_are_quoted_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
        assert_eq!(csv_field(" padded"), "\" padded\"");
    }

    #[test]
    fn excel_workbooks_become_csv_with_real_dates() {
        let mut workbook = rust_xlsxwriter::Workbook::new();
        let sheet = workbook.add_worksheet();
        sheet.write(0, 0, "region").unwrap();
        sheet.write(0, 1, "units").unwrap();
        sheet.write(0, 2, "sold on").unwrap();
        sheet.write(1, 0, "North, East").unwrap();
        sheet.write(1, 1, 12).unwrap();
        let date = rust_xlsxwriter::ExcelDateTime::parse_from_str("2025-03-01").unwrap();
        let format = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd");
        sheet.write_with_format(1, 2, &date, &format).unwrap();
        sheet.write(2, 0, "South").unwrap();
        sheet.write(2, 1, 4.5).unwrap();
        let bytes = workbook.save_to_buffer().unwrap();
        let csv = String::from_utf8(excel_to_csv(&bytes).unwrap()).unwrap();
        assert_eq!(csv, "region,units,sold on\n\"North, East\",12,2025-03-01\nSouth,4.5,\n");
        assert!(excel_to_csv(b"PK\x03\x04 not really a zip").is_err());
    }
}
