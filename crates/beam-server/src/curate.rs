//! What people responsible for a dataset write and check about it:
//! descriptions of the data and its columns, verified questions, and
//! watches that check the data on a schedule.
//!
//! None of this needs an AI model. It is what makes answers dependable,
//! whether a person or an assistant asks: meanings are written down once,
//! questions that matter are answered the same checked way every time, and
//! problems are found by rules rather than noticed by chance.

// Page handlers return a ready response as their error, as in `web.rs`.
#![allow(clippy::result_large_err)]

use crate::app::AppState;
use crate::catalog::{self, Entry};
use crate::chart::format_number;
use crate::feed;
use crate::table::{self, filter_expression, Tab};
use crate::web::{check_csrf, layout, message_page, page, redirect, require_signed_in};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Form;
use beam_accounts::{Account, AccountError, Role, Watch, WatchKind, WatchStatus};
use beam_odata::{Aggregate, Bucket, EdmType, EntitySet, Property};
use maud::{html, Markup};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

type App = State<Arc<AppState>>;
type Pairs = Vec<(String, String)>;

/// Chart settings a verified question keeps.
const QUESTION_KEYS: &[&str] = &["group", "agg", "measure", "bucket", "kind", "column", "op", "value"];
/// Settings a watch keeps.
const WATCH_KEYS: &[&str] = &[
    "agg", "measure", "when", "date", "bucket", "keys", "column", "op", "value",
];
/// Periods read when comparing the latest one with the one before.
const MAX_PERIODS: u64 = 100_000;
/// Longest the hourly run of all watches may take; the rest wait an hour.
const SWEEP_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

/// Who may describe a dataset and decide its verified questions and
/// watches: an administrator, or the person who added it.
pub fn can_curate(account: &Account, entry: &Entry) -> bool {
    account.role == Role::Admin || entry.upload.as_ref().is_some_and(|u| u.owner_id == account.id)
}

fn setting<'a>(settings: &'a [(String, String)], key: &str) -> Option<&'a str> {
    settings
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn encode(pairs: &[(String, String)], keys: &[&str]) -> String {
    let mut out = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        if keys.contains(&key.as_str()) && !value.trim().is_empty() {
            out.append_pair(key, value.trim());
        }
    }
    out.finish()
}

fn decode(text: &str) -> Pairs {
    form_urlencoded::parse(text.as_bytes()).into_owned().collect()
}

fn aggregate_of(code: Option<&str>) -> Aggregate {
    match code {
        Some("sum") => Aggregate::Sum,
        Some("avg") => Aggregate::Average,
        Some("min") => Aggregate::Min,
        Some("max") => Aggregate::Max,
        _ => Aggregate::Count,
    }
}

fn bucket_of(code: Option<&str>) -> Bucket {
    match code {
        Some("day") => Bucket::Day,
        Some("quarter") => Bucket::Quarter,
        Some("year") => Bucket::Year,
        _ => Bucket::Month,
    }
}

/// The filter a question or watch keeps, as filter text for the planners.
fn filter_of(set: &EntitySet, settings: &[(String, String)]) -> Result<Option<String>, String> {
    let Some(column) = setting(settings, "column") else {
        return Ok(None);
    };
    let property = set
        .property(column)
        .ok_or_else(|| format!("The column {column} no longer exists."))?;
    let op = setting(settings, "op").unwrap_or("eq");
    let value = setting(settings, "value").unwrap_or("");
    filter_expression(property, op, value).map(Some).map_err(str::to_string)
}

/// "Sum of amount", "Number of rows", ...
fn what(settings: &[(String, String)]) -> String {
    let measure = setting(settings, "measure").unwrap_or("?");
    match setting(settings, "agg") {
        Some("sum") => format!("Sum of {measure}"),
        Some("avg") => format!("Average of {measure}"),
        Some("min") => format!("Smallest {measure}"),
        Some("max") => format!("Largest {measure}"),
        _ => "Number of rows".into(),
    }
}

fn filter_words(settings: &[(String, String)]) -> String {
    match setting(settings, "column") {
        Some(column) => format!(
            " (only rows where {column} {} {})",
            setting(settings, "op").unwrap_or("eq"),
            setting(settings, "value").unwrap_or("")
        ),
        None => String::new(),
    }
}

/// API summary options for a verified question's chart settings.
pub fn question_options(set: &EntitySet, settings: &str) -> Result<Pairs, String> {
    let settings = decode(settings);
    let mut out = vec![(
        "group".to_string(),
        setting(&settings, "group").unwrap_or_default().to_string(),
    )];
    let agg = setting(&settings, "agg").unwrap_or("count");
    out.push(("aggregate".into(), agg.into()));
    if agg != "count" {
        if let Some(measure) = setting(&settings, "measure") {
            out.push(("measure".into(), measure.into()));
        }
    }
    if let Some(bucket) = setting(&settings, "bucket") {
        out.push(("bucket".into(), bucket.into()));
    }
    if let Some(filter) = filter_of(set, &settings)? {
        out.push(("filter".into(), filter));
    }
    Ok(out)
}

// ------------------------------------------------------------- running watches

/// Runs one watch against the dataset it names. The outcome is recorded
/// and returned.
pub async fn run_watch(state: &Arc<AppState>, watch: &Watch) -> (WatchStatus, String, Option<f64>) {
    let outcome = match state.engine.dataset(&watch.dataset) {
        None => Err("The dataset no longer exists.".to_string()),
        Some(dataset) => check(state, &catalog::entity_set(&dataset), watch).await,
    };
    let (status, message, value) = match outcome {
        Ok(result) => result,
        Err(message) => (WatchStatus::Error, message, None),
    };
    if let Err(e) = state.accounts.record_watch(watch.id, status, &message, value) {
        eprintln!("could not record watch {}: {e}", watch.id);
    }
    (status, message, value)
}

async fn check(
    state: &Arc<AppState>,
    set: &EntitySet,
    watch: &Watch,
) -> Result<(WatchStatus, String, Option<f64>), String> {
    let settings = decode(&watch.settings);
    let filter = filter_of(set, &settings)?;
    let unusable = |e: beam_odata::QueryError| format!("This watch no longer fits the data: {e}.");
    let run = |summary: beam_odata::Summary, width: usize| async move {
        feed::run(state, summary.sql, &summary.params, width)
            .await
            .map_err(|failure| match failure {
                feed::QueryFailure::TimedOut => feed::TOO_LONG.to_string(),
                _ => "The check could not be run.".to_string(),
            })
    };
    let alert = |raised: bool| if raised { WatchStatus::Alert } else { WatchStatus::Ok };
    let limit = format_number(watch.threshold);
    match watch.kind {
        WatchKind::Total => {
            let aggregate = aggregate_of(setting(&settings, "agg"));
            let planned = beam_odata::plan_total(set, setting(&settings, "measure"), aggregate, filter.as_deref())
                .map_err(unusable)?;
            let rows = run(planned, 1).await?;
            let value = rows.first().and_then(|r| r[0].as_f64());
            let above = setting(&settings, "when") != Some("below");
            let Some(value) = value else {
                return Ok((
                    WatchStatus::Ok,
                    format!("{}{} has no value yet.", what(&settings), filter_words(&settings)),
                    None,
                ));
            };
            let raised = if above {
                value > watch.threshold
            } else {
                value < watch.threshold
            };
            let rule = if above { "above" } else { "below" };
            Ok((
                alert(raised),
                format!(
                    "{}{} is {}. Alerts when {rule} {limit}.",
                    what(&settings),
                    filter_words(&settings),
                    format_number(value)
                ),
                Some(value),
            ))
        }
        WatchKind::Change | WatchKind::Steady => {
            let date = setting(&settings, "date").unwrap_or_default();
            let is_date = set
                .property(date)
                .is_some_and(|p| matches!(p.edm, EdmType::Date | EdmType::DateTimeOffset));
            if !is_date {
                return Err(format!("The date column {date} no longer exists."));
            }
            let planned = beam_odata::plan_summary(
                set,
                date,
                setting(&settings, "measure"),
                aggregate_of(setting(&settings, "agg")),
                bucket_of(setting(&settings, "bucket")),
                filter.as_deref(),
                MAX_PERIODS,
            )
            .map_err(unusable)?;
            let rows = run(planned, 2).await?;
            let points: Vec<(String, f64)> = rows
                .iter()
                .filter_map(|r| Some((r[0].as_str()?.to_string(), r[1].as_f64()?)))
                .filter(|(label, _)| label != "(empty)")
                .collect();
            if watch.kind == WatchKind::Steady {
                return Ok(steady(&settings, &points));
            }
            let [.., (before_label, before), (latest_label, latest)] = points.as_slice() else {
                return Ok((WatchStatus::Ok, "Not enough periods to compare yet.".into(), None));
            };
            let (raised, how) = if *before == 0.0 {
                (*latest != 0.0, "from 0".to_string())
            } else {
                let percent = (latest - before) / before.abs() * 100.0;
                let direction = if percent < 0.0 { "down" } else { "up" };
                (
                    percent.abs() >= watch.threshold,
                    format!("{direction} {}%", format_number(percent.abs())),
                )
            };
            Ok((
                alert(raised),
                format!(
                    "{}{} for {latest_label} is {}, {how} on {before_label} ({}). Alerts at a change of {limit}% or more. The latest period may not be complete.",
                    what(&settings),
                    filter_words(&settings),
                    format_number(*latest),
                    format_number(*before),
                ),
                Some(*latest),
            ))
        }
        WatchKind::Repeats => {
            let keys: Vec<&str> = setting(&settings, "keys")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .collect();
            let planned = beam_odata::plan_repeats(set, &keys, filter.as_deref()).map_err(unusable)?;
            let rows = run(planned, 2).await?;
            let groups = rows.first().and_then(|r| r[0].as_f64()).unwrap_or(0.0);
            let extra = rows.first().and_then(|r| r[1].as_f64()).unwrap_or(0.0);
            Ok((
                alert(groups > watch.threshold),
                format!(
                    "{} combination(s) of {} appear more than once{}, {} extra row(s). Alerts when more than {limit}.",
                    format_number(groups),
                    keys.join(", "),
                    filter_words(&settings),
                    format_number(extra),
                ),
                Some(groups),
            ))
        }
    }
}

/// Control totals and the five-number summary of the number columns: the
/// figures to reconcile against the source, and where most values lie.
async fn profile_section(state: &Arc<AppState>, set: &EntitySet, numbers: &[&Property]) -> Markup {
    if numbers.is_empty() {
        return html! {};
    }
    let shown: Vec<&str> = numbers
        .iter()
        .take(beam_odata::MAX_PROFILE_COLUMNS)
        .map(|p| p.name.as_str())
        .collect();
    // Worked out once per change to the datasets, not on every visit: each
    // profile reads the whole dataset twice.
    // A profile that could not be worked out is remembered as empty, so a
    // dataset too large for the time limit does not rerun it on every visit.
    let figures = match state.remembered_profile(&set.name) {
        Some(figures) => Some(figures).filter(|f| !f.is_empty()),
        None => {
            // Read before the query, so a change made while it runs makes
            // the result look old rather than current.
            let generation = state.engine.generation();
            let worked_out = match beam_odata::plan_profile(set, &shown) {
                Ok(planned) => {
                    let width = 1 + shown.len() * beam_odata::PROFILE_WIDTH;
                    feed::run(state, planned.sql, &planned.params, width)
                        .await
                        .ok()
                        .and_then(|rows| {
                            rows.first()
                                .map(|row| Arc::new(row.iter().map(Value::as_f64).collect::<Vec<_>>()))
                        })
                }
                Err(_) => None,
            };
            state.remember_profile(&set.name, generation, worked_out.clone().unwrap_or_default());
            worked_out
        }
    };
    let show = |v: Option<f64>| v.map_or("—".to_string(), format_number);
    html! {
        section.panel {
            h2 { "Profile" }
            @match figures {
                None => p.empty { "The profile could not be worked out in time." },
                Some(figures) => {
                    p.hint {
                        (show(figures[0])) " rows. Compare the row count and totals with the source to check nothing was lost or doubled. "
                        "The middle five figures show where values lie: half of them are between the quarter marks."
                    }
                    div.tablewrap {
                        table {
                            thead { tr {
                                th { "Column" } th.num { "Values" } th.num { "Total" } th.num { "Smallest" }
                                th.num { "Lower quarter" } th.num { "Middle" } th.num { "Upper quarter" } th.num { "Largest" }
                                th.num title="Values more than 1.5 times the spread between the quarter marks beyond them (Tukey's fences)" { "Far out" }
                            } }
                            tbody {
                                @for (index, name) in shown.iter().enumerate() {
                                    @let at = 1 + index * beam_odata::PROFILE_WIDTH;
                                    tr {
                                        th scope="row" { (name) }
                                        @for offset in 0..beam_odata::PROFILE_WIDTH {
                                            td.num { (show(figures.get(at + offset).copied().flatten())) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if numbers.len() > shown.len() {
                        p.hint { "The first " (shown.len()) " number columns are shown." }
                    }
                    p.hint { "Far out values are worth a look, not necessarily wrong: they lie unusually far from the rest (Tukey's fences, from 1977)." }
                }
            }
        }
    }
}

/// Judges the latest period of `points` (oldest first) against the range
/// the measure usually moves in; see [`crate::spc`].
fn steady(settings: &[(String, String)], points: &[(String, f64)]) -> (WatchStatus, String, Option<f64>) {
    let values: Vec<f64> = points.iter().map(|(_, v)| *v).collect();
    let subject = format!("{}{}", what(settings), filter_words(settings));
    let Some(verdict) = crate::spc::judge(&values) else {
        return (
            WatchStatus::Ok,
            format!(
                "{subject}: not enough periods yet. The usual range is worked out from at least {} earlier periods.",
                crate::spc::MIN_BASELINE
            ),
            None,
        );
    };
    let label = points.last().map_or("", |(l, _)| l.as_str());
    let range = format!(
        "usually between {} and {} (worked out from {} earlier periods)",
        format_number(verdict.lower),
        format_number(verdict.upper),
        verdict.baseline
    );
    let latest = format_number(verdict.latest);
    let (status, finding) = match verdict.signal {
        crate::spc::Signal::None => (WatchStatus::Ok, format!("{label} is {latest}, within the usual range: {range}.")),
        crate::spc::Signal::Outside => (
            WatchStatus::Alert,
            format!("{label} is {latest}, outside the usual range: {range}. Something out of the ordinary happened."),
        ),
        crate::spc::Signal::Run { above } => (
            WatchStatus::Alert,
            format!(
                "The last {} periods were all {} the usual middle of {}: the measure has shifted. {label} is {latest}; {range}.",
                crate::spc::RUN,
                if above { "above" } else { "below" },
                format_number(verdict.middle)
            ),
        ),
    };
    (
        status,
        format!("{subject}: {finding} The latest period may not be complete."),
        Some(verdict.latest),
    )
}

/// Runs every watch. Called hourly and at start-up.
pub async fn run_all(state: &Arc<AppState>) {
    let started = std::time::Instant::now();
    // Longest-unchecked first, so a run cut short by the budget still
    // reaches every watch over the following hours.
    let mut watches = state.accounts.watches(None).unwrap_or_default();
    watches.sort_by(|a, b| a.last_run.cmp(&b.last_run));
    for watch in &watches {
        if started.elapsed() > SWEEP_BUDGET {
            eprintln!("watch run stopped after {SWEEP_BUDGET:?}; the rest run next hour");
            break;
        }
        run_watch(state, watch).await;
    }
}

// ------------------------------------------------------- shared with the API

fn note_map(state: &AppState, dataset: &str) -> HashMap<String, String> {
    state
        .accounts
        .notes(dataset)
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.column, n.text))
        .collect()
}

/// Descriptions and verified questions, added to a dataset's API description.
pub fn describe(state: &AppState, entry: &Entry, columns: &mut [Value]) -> Value {
    let notes = note_map(state, &entry.set.name);
    for column in columns.iter_mut() {
        if let Some(text) = column["name"].as_str().and_then(|n| notes.get(n)) {
            column["description"] = json!(text);
        }
    }
    let questions: Vec<Value> = state
        .accounts
        .questions(&entry.set.name)
        .unwrap_or_default()
        .into_iter()
        .map(|q| json!({ "id": q.id, "question": q.question, "verified_by": q.approved_by, "verified_at": q.approved_at }))
        .collect();
    json!({ "description": notes.get(""), "verified_questions": questions })
}

/// Watches on datasets `account` can see, alerts first.
pub fn visible_watches(state: &AppState, account: &Account) -> Vec<Watch> {
    let names: Vec<String> = catalog::visible(state, account)
        .into_iter()
        .map(|e| e.set.name.to_ascii_lowercase())
        .collect();
    let mut watches: Vec<Watch> = state
        .accounts
        .watches(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| names.contains(&w.dataset.to_ascii_lowercase()))
        .collect();
    watches.sort_by_key(|w| match w.status {
        Some(WatchStatus::Alert) => 0,
        Some(WatchStatus::Error) => 1,
        None => 2,
        Some(WatchStatus::Ok) => 3,
    });
    watches
}

pub fn watch_json(watch: &Watch) -> Value {
    json!({
        "id": watch.id,
        "dataset": watch.dataset,
        "name": watch.name,
        "kind": watch.kind.as_str(),
        "status": watch.status.map(WatchStatus::as_str),
        "message": watch.message,
        "value": watch.value,
        "last_run": watch.last_run,
        "created_by": watch.created_by,
    })
}

// ------------------------------------------------------------------ the pages

fn status_badge(status: Option<WatchStatus>) -> Markup {
    let (class, text) = match status {
        Some(WatchStatus::Alert) => ("badge alert", "Alert"),
        Some(WatchStatus::Error) => ("badge error", "Cannot check"),
        Some(WatchStatus::Ok) => ("badge ok", "OK"),
        None => ("badge", "Not run yet"),
    };
    html! { span class=(class) { (text) } }
}

fn not_found() -> Response {
    message_page(
        StatusCode::NOT_FOUND,
        "Dataset not found",
        "There is no dataset with that name.",
    )
}

fn notice(query: &HashMap<String, String>) -> Markup {
    let ok = match query.get("ok").map(String::as_str) {
        Some("notes") => Some("Descriptions saved."),
        Some("question") => Some("Saved as a verified question."),
        Some("removed") => Some("Removed."),
        Some("watch") => Some("Watch added and run once."),
        Some("ran") => Some("Watch run."),
        _ => None,
    };
    let error = query.get("error").map(|code| match code.as_str() {
        "text" => "That text is too long or contains characters that are not allowed.",
        "full" => "This dataset already has the most allowed. Remove one you no longer need.",
        "watch" => "That watch is not complete. Check the columns and the number.",
        _ => "That could not be saved.",
    });
    html! {
        @if let Some(text) = ok { p.notice role="status" { (text) } }
        @if let Some(text) = error { p.notice.error role="alert" { (text) } }
    }
}

fn error_code(error: &AccountError) -> &'static str {
    match error {
        AccountError::InvalidText => "text",
        AccountError::TooMany(_) => "full",
        _ => "failed",
    }
}

pub async fn about(
    State(state): App,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let Some(entry) = catalog::find(&state, &signed.session.account, &name) else {
        return not_found();
    };
    let set = &entry.set;
    let curator = can_curate(&signed.session.account, &entry);
    let notes = note_map(&state, &set.name);
    let questions = state.accounts.questions(&set.name).unwrap_or_default();
    let watches = state.accounts.watches(Some(&set.name)).unwrap_or_default();
    let csrf = &signed.session.csrf;
    let base = format!("/datasets/{}", set.name);
    let dates: Vec<&Property> = set
        .properties
        .iter()
        .filter(|p| matches!(p.edm, EdmType::Date | EdmType::DateTimeOffset))
        .collect();
    let numbers: Vec<&Property> = set
        .properties
        .iter()
        .filter(|p| {
            // `_row` only numbers the rows; adding it up means nothing.
            p.name != beam_engine::ROW_KEY
                && matches!(
                    p.edm,
                    EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
                )
        })
        .collect();
    let profile = profile_section(&state, set, &numbers).await;

    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / " (set.name) }
        h1 { (set.name) }
        (table::dataset_tabs(set, Tab::About, &[], table::can_manage(&signed, &entry)))
        (notice(&query))

        section.panel {
            h2 { "What this data means" }
            @if curator {
                form method="post" action={ (base) "/about" } {
                    input type="hidden" name="csrf" value=(csrf);
                    label for="description" { "About this dataset" }
                    textarea #description name="description" rows="4" maxlength=(beam_accounts::MAX_NOTE_CHARS)
                        placeholder="What one row is, where the data comes from, and how it should and should not be added up." {
                        (notes.get("").map(String::as_str).unwrap_or(""))
                    }
                    div.tablewrap {
                        table {
                            thead { tr { th { "Column" } th { "Meaning" } } }
                            tbody {
                                @for (index, property) in set.properties.iter().enumerate() {
                                    tr {
                                        td { label for={ "col" (index) } { (property.name) } }
                                        td { input #{ "col" (index) } name={ "col" (index) } maxlength=(beam_accounts::MAX_NOTE_CHARS / 4)
                                            value=(notes.get(&property.name).map(String::as_str).unwrap_or("")); }
                                    }
                                }
                            }
                        }
                    }
                    button type="submit" { "Save descriptions" }
                }
            } @else {
                @match notes.get("") {
                    Some(text) => p.prose { (text) },
                    None => p.empty { "No description yet. The person who added this dataset, or an administrator, can write one." },
                }
                @if notes.keys().any(|k| !k.is_empty()) {
                    dl {
                        @for property in &set.properties {
                            @if let Some(text) = notes.get(&property.name) { dt { (property.name) } dd { (text) } }
                        }
                    }
                }
            }
        }

        (profile)

        section.panel {
            h2 { "Verified questions" }
            p.hint { "Answers someone responsible for this data has checked. They are worked out again from the current data each time, the same way." }
            @if questions.is_empty() {
                p.empty {
                    "None yet."
                    @if curator { " Draw the answer on the " a href={ (base) "/chart" } { "Chart" } " view, then save it as a verified question there." }
                }
            } @else {
                ul.plain {
                    @for q in &questions {
                        li {
                            a href={ (base) "/chart?" (q.settings) } { (q.question) }
                            " "
                            span.hint { "verified by " (q.approved_by.as_deref().unwrap_or("a removed account")) " on " (q.approved_at.get(..10).unwrap_or(&q.approved_at)) }
                            @if curator {
                                form.inline method="post" action={ (base) "/questions/" (q.id) "/remove" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    button.quiet type="submit" { "Remove" }
                                }
                            }
                        }
                    }
                }
            }
        }

        section.panel {
            h2 { "Watches" }
            p.hint { "Checks that run every hour and say when something needs a look." }
            @if watches.is_empty() { p.empty { "None yet." } }
            @for w in &watches {
                div.watch {
                    p { (status_badge(w.status)) " " strong { (w.name) } }
                    @if let Some(message) = &w.message { p { (message) } }
                    p.hint {
                        @if let Some(run) = &w.last_run { "Checked " (run) " UTC. " }
                        @if let Some(by) = &w.created_by { "Set up by " (by) "." }
                    }
                    @if curator {
                        form.inline method="post" action={ (base) "/watches/" (w.id) "/run" } {
                            input type="hidden" name="csrf" value=(csrf);
                            button.quiet type="submit" { "Check now" }
                        }
                        form.inline method="post" action={ (base) "/watches/" (w.id) "/remove" } {
                            input type="hidden" name="csrf" value=(csrf);
                            button.quiet type="submit" { "Remove" }
                        }
                    }
                }
            }
            @if curator {
                details {
                    summary { "Add a watch" }
                    form.stack method="post" action={ (base) "/watches" } {
                        input type="hidden" name="csrf" value=(csrf);
                        label for="wname" { "Name" }
                        input #wname name="name" required maxlength=(beam_accounts::MAX_TITLE_CHARS) placeholder="for example: Records reported twice";
                        label for="wkind" { "Check that" }
                        select #wkind name="kind" {
                            option value="repeats" { "no record appears twice" }
                            option value="total" { "a total stays within a limit" }
                            @if !dates.is_empty() {
                                option value="steady" { "the latest period stays in its usual range (no number needed)" }
                                option value="change" { "the latest period changes little" }
                            }
                        }
                        label for="wkeys" { "Key columns, for repeats (comma-separated)" }
                        input #wkeys name="keys" list="columns" placeholder="for example: person_id, event_date";
                        datalist #columns { @for p in &set.properties { option value=(p.name) {} } }
                        label for="wagg" { "Total, for totals and changes" }
                        select #wagg name="agg" {
                            option value="count" { "Number of rows" }
                            @if !numbers.is_empty() {
                                option value="sum" { "Sum of" } option value="avg" { "Average of" }
                                option value="min" { "Smallest" } option value="max" { "Largest" }
                            }
                        }
                        @if !numbers.is_empty() {
                            select name="measure" aria-label="Number column" {
                                @for p in &numbers { option value=(p.name) { (p.name) } }
                            }
                        }
                        select name="when" aria-label="When to alert, for totals" {
                            option value="above" { "alert when above" } option value="below" { "alert when below" }
                        }
                        @if !dates.is_empty() {
                            label for="wdate" { "Date column and period, for changes and usual ranges" }
                            select #wdate name="date" { @for p in &dates { option value=(p.name) { (p.name) } } }
                            select name="bucket" aria-label="Period" {
                                option value="month" { "month" } option value="day" { "day" }
                                option value="quarter" { "quarter" } option value="year" { "year" }
                            }
                        }
                        label for="wthreshold" { "Number: the total limit, the percentage change, or how many repeats are acceptable. Not used for usual ranges." }
                        input #wthreshold name="threshold" type="number" step="any" required value="0";
                        button type="submit" { "Add watch" }
                    }
                }
            }
        }
    };
    page(
        StatusCode::OK,
        layout(&format!("{} · about", set.name), Some(&signed), body),
    )
}

/// Loads the dataset for a curator's form, after the usual checks.
fn curated(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    form: &[(String, String)],
) -> Result<(Account, Entry), Response> {
    let signed = require_signed_in(state, headers)?;
    check_csrf(&signed, setting(form, "csrf").unwrap_or(""))?;
    let entry = catalog::find(state, &signed.session.account, name).ok_or_else(not_found)?;
    if !can_curate(&signed.session.account, &entry) {
        return Err(message_page(
            StatusCode::FORBIDDEN,
            "Not your dataset",
            "Only the person who added this dataset, or an administrator, can change this.",
        ));
    }
    Ok((signed.session.account, entry))
}

pub async fn save_notes(
    State(state): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Pairs>,
) -> Response {
    let (account, entry) = match curated(&state, &headers, &name, &form) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let set = &entry.set;
    let base = format!("/datasets/{}/about", set.name);
    let mut wanted = vec![(String::new(), setting(&form, "description").unwrap_or("").to_string())];
    for (index, property) in set.properties.iter().enumerate() {
        let text = form
            .iter()
            .find(|(k, _)| *k == format!("col{index}"))
            .map(|(_, v)| v.clone());
        wanted.push((property.name.clone(), text.unwrap_or_default()));
    }
    for (column, text) in wanted {
        if let Err(e) = state.accounts.set_note(&set.name, &column, &text, account.id) {
            return redirect(&format!("{base}?error={}", error_code(&e)));
        }
    }
    redirect(&format!("{base}?ok=notes"))
}

pub async fn add_question(
    State(state): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Pairs>,
) -> Response {
    let (account, entry) = match curated(&state, &headers, &name, &form) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let set = &entry.set;
    let base = format!("/datasets/{}/about", set.name);
    let settings = encode(&form, QUESTION_KEYS);
    // Only settings that can actually be answered are kept.
    let usable = question_options(set, &settings).is_ok_and(|options| {
        let pick = |key: &str| options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        let aggregate = aggregate_of(setting(&decode(&settings), "agg"));
        beam_odata::plan_summary(
            set,
            pick("group").unwrap_or_default(),
            pick("measure"),
            aggregate,
            Bucket::Month,
            pick("filter"),
            1,
        )
        .is_ok()
    });
    if !usable {
        return redirect(&format!("{base}?error=failed"));
    }
    match state.accounts.add_question(
        &set.name,
        setting(&form, "question").unwrap_or(""),
        &settings,
        account.id,
    ) {
        Ok(_) => redirect(&format!("{base}?ok=question")),
        Err(e) => redirect(&format!("{base}?error={}", error_code(&e))),
    }
}

pub async fn remove_question(
    State(state): App,
    Path((name, id)): Path<(String, i64)>,
    headers: HeaderMap,
    Form(form): Form<Pairs>,
) -> Response {
    let (_, entry) = match curated(&state, &headers, &name, &form) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let _ = state.accounts.remove_question(&entry.set.name, id);
    redirect(&format!("/datasets/{}/about?ok=removed", entry.set.name))
}

pub async fn add_watch(
    State(state): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Pairs>,
) -> Response {
    let (account, entry) = match curated(&state, &headers, &name, &form) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let set = &entry.set;
    let base = format!("/datasets/{}/about", set.name);
    let Some(kind) = setting(&form, "kind").and_then(WatchKind::parse) else {
        return redirect(&format!("{base}?error=watch"));
    };
    // Keep only what this kind of watch uses, so its description stays true.
    let used: &[&str] = match kind {
        WatchKind::Total => &["agg", "measure", "when", "column", "op", "value"],
        WatchKind::Change | WatchKind::Steady => &["agg", "measure", "date", "bucket", "column", "op", "value"],
        WatchKind::Repeats => &["keys", "column", "op", "value"],
    };
    let mut kept: Pairs = form
        .iter()
        .filter(|(k, _)| used.contains(&k.as_str()))
        .cloned()
        .collect();
    // Only real period codes, and each key column once, are stored.
    kept.retain(|(k, v)| k != "bucket" || ["day", "month", "quarter", "year"].contains(&v.as_str()));
    for (_, value) in kept.iter_mut().filter(|(k, _)| k == "keys") {
        let mut seen: Vec<&str> = Vec::new();
        for column in value.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            if !seen.iter().any(|s| s.eq_ignore_ascii_case(column)) {
                seen.push(column);
            }
        }
        *value = seen.join(", ");
    }
    if setting(&kept, "agg").is_none_or(|a| a == "count") {
        kept.retain(|(k, _)| k != "measure");
    }
    let settings = encode(&kept, WATCH_KEYS);
    let threshold = setting(&form, "threshold").and_then(|t| t.parse::<f64>().ok());
    let Some(threshold) = threshold.filter(|t| t.is_finite()) else {
        return redirect(&format!("{base}?error=watch"));
    };
    // A usual range comes from the data, not from a number.
    let threshold = if kind == WatchKind::Steady { 0.0 } else { threshold };
    let name = setting(&form, "name").unwrap_or("");
    let candidate = Watch {
        id: 0,
        dataset: set.name.clone(),
        name: name.to_string(),
        kind,
        settings: settings.clone(),
        threshold,
        created_by: None,
        last_run: None,
        status: None,
        message: None,
        value: None,
    };
    // A watch that cannot run on today's data is refused rather than saved.
    if check(&state, set, &candidate).await.is_err() {
        return redirect(&format!("{base}?error=watch"));
    }
    match state
        .accounts
        .add_watch(&set.name, name, kind, &settings, threshold, account.id)
    {
        Ok(id) => {
            if let Some(watch) = state
                .accounts
                .watches(Some(&set.name))
                .unwrap_or_default()
                .into_iter()
                .find(|w| w.id == id)
            {
                run_watch(&state, &watch).await;
            }
            redirect(&format!("{base}?ok=watch"))
        }
        Err(e) => redirect(&format!("{base}?error={}", error_code(&e))),
    }
}

pub async fn run_watch_now(
    State(state): App,
    Path((name, id)): Path<(String, i64)>,
    headers: HeaderMap,
    Form(form): Form<Pairs>,
) -> Response {
    let (_, entry) = match curated(&state, &headers, &name, &form) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let watches = state.accounts.watches(Some(&entry.set.name)).unwrap_or_default();
    if let Some(watch) = watches.iter().find(|w| w.id == id) {
        run_watch(&state, watch).await;
    }
    redirect(&format!("/datasets/{}/about?ok=ran", entry.set.name))
}

pub async fn remove_watch(
    State(state): App,
    Path((name, id)): Path<(String, i64)>,
    headers: HeaderMap,
    Form(form): Form<Pairs>,
) -> Response {
    let (_, entry) = match curated(&state, &headers, &name, &form) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let _ = state.accounts.remove_watch(&entry.set.name, id);
    redirect(&format!("/datasets/{}/about?ok=removed", entry.set.name))
}

/// Every watch the signed-in account can see, alerts first.
pub async fn overview(State(state): App, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let watches = visible_watches(&state, &signed.session.account);
    let body = html! {
        h1 { "Watches" }
        p.hint { "Checks on your datasets that run every hour. Add one from a dataset's About view." }
        @if watches.is_empty() {
            p.empty { "No watches on datasets you can see." }
        } @else {
            div.tablewrap {
                table {
                    thead { tr { th { "Status" } th { "Watch" } th { "Dataset" } th { "Latest result" } th { "Checked (UTC)" } } }
                    tbody {
                        @for w in &watches {
                            tr {
                                td { (status_badge(w.status)) }
                                td { (w.name) }
                                td { a href={ "/datasets/" (w.dataset) "/about" } { (w.dataset) } }
                                td { (w.message.as_deref().unwrap_or("")) }
                                td { (w.last_run.as_deref().unwrap_or("")) }
                            }
                        }
                    }
                }
            }
        }
    };
    page(StatusCode::OK, layout("Watches", Some(&signed), body))
}

/// The form on the Chart view that saves the chart on screen as a verified question.
pub fn save_question_form(set: &EntitySet, csrf: &str, current: &[(&str, &str)], title: &str) -> Markup {
    html! {
        details {
            summary { "Save as a verified question" }
            form.stack method="post" action={ "/datasets/" (set.name) "/questions" } {
                input type="hidden" name="csrf" value=(csrf);
                @for (key, value) in current { input type="hidden" name=(key) value=(value); }
                label for="question" { "The question this chart answers" }
                input #question name="question" required maxlength=(beam_accounts::MAX_TITLE_CHARS) value=(title);
                p.hint { "Saving says you have checked this answer. It is worked out again from the current data each time." }
                button type="submit" { "Save" }
            }
        }
    }
}
