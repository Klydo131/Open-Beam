//! Present: a dataset's verified questions as slides, one per screen, and
//! each chart as an SVG file for PowerPoint.
//!
//! - Slides are full-screen sections that snap into place as you scroll or
//!   press Page Down (CSS scroll snap; no script). F11 makes them full screen.
//! - Printing gives one 16:9 page per slide, so "Save as PDF" makes a deck.
//! - "Download for PowerPoint" gives the chart as SVG, which PowerPoint for
//!   Microsoft 365, 2019 and later inserts and can turn into editable shapes
//!   ("Convert to Shape"). The file carries its own colours, since
//!   PowerPoint does not have this site's stylesheet.
//! - "Download as Excel" gives a workbook: an overview sheet, then one
//!   sheet per question holding its figures as an Excel table and a native
//!   Excel chart over them. Power BI loads the tables (its Excel connector
//!   picks up named tables; without them it can only guess).
//! - An overview slide shows every chart at once, small, for a look at the
//!   whole before the detail (a dashboard's "single-page view").
//! - Every slide is worked out from current data, as verified questions
//!   always are, and says who verified it and when.

use crate::app::AppState;
use crate::catalog;
use crate::web::{message_page, page, require_signed_in};
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_accounts::Question;
use maud::{html, Markup, DOCTYPE};
use rust_xlsxwriter::{Chart, ChartType, Format, Table, TableColumn, Workbook};
use std::sync::Arc;

type App = State<Arc<AppState>>;

/// A question's chart from current data: its points, whether it reads best
/// as a line, and whether points past the Chart view's limit were left out;
/// or why it cannot be drawn now.
type Drawn = Result<(Vec<(String, f64)>, bool, bool), String>;

/// A question's chart, from current data: its points, and whether it reads
/// best as a line.
async fn drawn(
    state: &Arc<AppState>,
    account: &beam_accounts::Account,
    set: &beam_odata::EntitySet,
    question: &Question,
) -> Drawn {
    let options = crate::curate::question_options(set, &question.settings)?;
    let answer = crate::api::summary(state, account, &set.name, &options)
        .await
        .map_err(|e| e.message)?;
    let mut points: Vec<(String, f64)> = answer["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| Some((g["label"].as_str()?.to_string(), g["value"].as_f64()?)))
        .collect();
    let line = question.settings.split('&').any(|p| p == "kind=line");
    // The same limit as the Chart view, so a slide shows what was verified.
    let group = options.iter().find(|(k, _)| k == "group").map(|(_, v)| v.as_str());
    let ordered = group
        .and_then(|g| set.property(g))
        .is_some_and(|p| !matches!(p.edm, beam_odata::EdmType::String | beam_odata::EdmType::Boolean));
    let limit = crate::chart::point_limit(ordered);
    let cut = points.len() > limit;
    points.truncate(limit);
    Ok((points, line, cut))
}

fn chart(points: &[(String, f64)], line: bool, title: &str) -> Markup {
    if line {
        crate::chart::line_chart(points, title)
    } else {
        crate::chart::bar_chart(points, title)
    }
}

pub async fn deck(State(state): App, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let account = &signed.session.account;
    let Some(entry) = catalog::find(&state, account, &name) else {
        return message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        );
    };
    let set = &entry.set;
    let questions = state.accounts.questions(&set.name).unwrap_or_default();
    let about = state
        .accounts
        .notes(&set.name)
        .unwrap_or_default()
        .into_iter()
        .find(|n| n.column.is_empty())
        .map(|n| n.text);
    let mut slides = Vec::new();
    for question in &questions {
        slides.push((question, drawn(&state, account, set, question).await));
    }
    // Title, an overview when there is more than one, then the questions.
    let first = if slides.len() > 1 { 3 } else { 2 };
    let total = if slides.is_empty() { 1 } else { slides.len() + first - 1 };
    let base = format!("/datasets/{}", set.name);
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (set.name) " · Present · Open Beam" }
                link rel="stylesheet" href="/static/app.css";
                link rel="icon" href="/static/logo.svg" type="image/svg+xml";
            }
            body.presenting {
                main.deck {
                    section.slide.title-slide #slide-1 {
                        p.slide-kicker { "Open Beam" }
                        h1 { (set.name) }
                        @if let Some(about) = &about { p.slide-lede { (about) } }
                        p.slide-meta { (slides.len()) " verified " (if slides.len() == 1 { "question" } else { "questions" }) ", worked out from the current data." }
                        @if slides.is_empty() {
                            p { "No verified questions yet. Draw an answer on the " a href={ (base) "/chart" } { "Chart" } " view and save it as a verified question; it becomes a slide here." }
                        } @else {
                            p.slide-actions {
                                a.button href={ (base) "/present.xlsx" } download { "Download as Excel, with charts" }
                                " "
                                span.hint { "Opens in Excel; Power BI loads its tables. To print a PDF deck, use your browser's Print." }
                            }
                        }
                        (controls(&base, 1, total))
                    }
                    @if slides.len() > 1 {
                        section.slide #slide-2 {
                            h2 { "At a glance" }
                            div.overview {
                                @for (index, (question, result)) in slides.iter().enumerate() {
                                    a.overview-card href={ "#slide-" (index + 3) } {
                                        span { (question.question) }
                                        @if let Ok((points, line, _)) = result {
                                            @if !points.is_empty() { (chart(points, *line, &question.question)) }
                                        }
                                    }
                                }
                            }
                            (controls(&base, 2, total))
                        }
                    }
                    @for (index, (question, result)) in slides.iter().enumerate() {
                        @let number = index + first;
                        section.slide id={ "slide-" (number) } {
                            h2 { (question.question) }
                            @match result {
                                Ok((points, line, cut)) if !points.is_empty() => {
                                    div.slide-chart { (chart(points, *line, &question.question)) }
                                    @if *cut { p.slide-meta { "The first " (points.len()) " groups are shown, as on the Chart view." } }
                                }
                                Ok(_) => p.empty { "No rows match this question now." },
                                Err(why) => p.notice.error { "This question no longer fits the data: " (why) },
                            }
                            p.slide-meta {
                                "Verified by " (question.approved_by.as_deref().unwrap_or("someone no longer here"))
                                " on " (question.approved_at.get(..10).unwrap_or(&question.approved_at))
                                " · dataset " (set.name)
                                @if matches!(result, Ok((points, _, _)) if !points.is_empty()) {
                                    " · " a href={ (base) "/present/" (question.id) ".svg" } download { "Download for PowerPoint" }
                                }
                            }
                            (controls(&base, number, total))
                        }
                    }

                }
            }
        }
    };
    page(StatusCode::OK, markup)
}

/// Previous, next and leave, on every slide; hidden when printed.
fn controls(base: &str, number: usize, total: usize) -> Markup {
    html! {
        nav.slide-nav aria-label="Slides" {
            a href={ (base) "/about" } { "Leave" }
            span.slide-count { (number) " / " (total) }
            @if number > 1 { a href={ "#slide-" (number - 1) } { "Previous" } }
            @if number < total { a href={ "#slide-" (number + 1) } { "Next" } }
        }
    }
}

/// Colours written into the file itself, for programs without this site's
/// stylesheet: a plain, print-friendly palette.
const STANDALONE: [(&str, &str); 8] = [
    ("bar", r##"fill="#b86e00""##),
    ("label", r##"fill="#1d2129""##),
    ("value", r##"fill="#1d2129""##),
    ("tick", r##"fill="#5d6470""##),
    ("axis", r##"stroke="#5d6470" stroke-width="1""##),
    ("grid", r##"stroke="#d5d9e0" stroke-width="1""##),
    ("line", r##"fill="none" stroke="#b86e00" stroke-width="2.5""##),
    ("dot", r##"fill="#ffffff" stroke="#b86e00" stroke-width="2""##),
];

/// A chart as a file of its own: namespace, white background, colours and
/// font set on the elements rather than by a stylesheet.
fn standalone(svg: &str) -> String {
    let mut out = svg.replacen(
        "<svg class=\"chart\"",
        "<svg xmlns=\"http://www.w3.org/2000/svg\" font-family=\"Segoe UI, Helvetica, Arial, sans-serif\" font-size=\"13\"",
        1,
    );
    for (class, attributes) in STANDALONE {
        out = out.replace(&format!("class=\"{class}\""), attributes);
    }
    // A white page under the chart, as on a slide.
    if let Some(at) = out.find('>') {
        out.insert_str(at + 1, "<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>");
    }
    out
}

pub async fn svg(State(state): App, Path((name, file)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let account = &signed.session.account;
    let (Some(entry), Some(id)) = (
        catalog::find(&state, account, &name),
        file.strip_suffix(".svg").and_then(|id| id.parse::<i64>().ok()),
    ) else {
        return message_page(StatusCode::NOT_FOUND, "Not found", "There is no such chart.");
    };
    let set = &entry.set;
    let Some(question) = state
        .accounts
        .questions(&set.name)
        .unwrap_or_default()
        .into_iter()
        .find(|q| q.id == id)
    else {
        return message_page(
            StatusCode::NOT_FOUND,
            "Not found",
            "There is no such verified question.",
        );
    };
    let (points, line) = match drawn(&state, account, set, &question).await {
        Ok((points, line, _)) if !points.is_empty() => (points, line),
        _ => {
            return message_page(
                StatusCode::CONFLICT,
                "No chart",
                "This question has no chart to download now.",
            )
        }
    };
    let body = standalone(&chart(&points, line, &question.question).into_string());
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{}-{id}.svg\"", set.name)) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    response
}

/// Excel holds at most 32,767 characters in a cell; longer text is cut,
/// and says so, rather than failing the whole file.
fn fits_a_cell(text: &str) -> String {
    const MAX: usize = 32_767;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    text.chars().take(MAX - 1).chain(['…']).collect()
}

/// The deck as an Excel workbook: an overview sheet, then a sheet per
/// question with its figures as a named table and a native chart over them.
fn workbook_bytes(dataset: &str, slides: &[(&Question, Drawn)]) -> Result<Vec<u8>, rust_xlsxwriter::XlsxError> {
    let mut book = Workbook::new();
    let bold = Format::new().set_bold();
    let title = Format::new().set_bold().set_font_size(14);
    let overview = book.add_worksheet().set_name("Overview")?;
    overview.write_string_with_format(0, 0, dataset, &title)?;
    overview.write_string(
        1,
        0,
        "Verified questions, worked out from the data when this file was made.",
    )?;
    for (column, heading) in ["Sheet", "Question", "Verified by", "Verified on"].iter().enumerate() {
        overview.write_string_with_format(3, column as u16, *heading, &bold)?;
    }
    for (index, (question, _)) in slides.iter().enumerate() {
        let row = 4 + index as u32;
        overview.write_string(row, 0, format!("Q{}", index + 1))?;
        overview.write_string(row, 1, &question.question)?;
        overview.write_string(row, 2, question.approved_by.as_deref().unwrap_or(""))?;
        overview.write_string(row, 3, question.approved_at.get(..10).unwrap_or(&question.approved_at))?;
    }
    overview.set_column_width(1, 50)?;
    for (index, (question, result)) in slides.iter().enumerate() {
        let name = format!("Q{}", index + 1);
        let sheet = book.add_worksheet().set_name(&name)?;
        // Text from people is written as text, never as a formula.
        sheet.write_string_with_format(0, 0, &question.question, &title)?;
        let verified = format!(
            "Verified by {} on {}",
            question.approved_by.as_deref().unwrap_or("someone no longer here"),
            question.approved_at.get(..10).unwrap_or(&question.approved_at)
        );
        sheet.write_string(1, 0, verified)?;
        match result {
            Ok((points, line, cut)) if !points.is_empty() => {
                let last = 3 + points.len() as u32;
                for (offset, (label, value)) in points.iter().enumerate() {
                    sheet.write_string(4 + offset as u32, 0, fits_a_cell(label))?;
                    sheet.write_number(4 + offset as u32, 1, *value)?;
                }
                // A table name may not look like a cell, and Q1 is one.
                let table = Table::new().set_name(format!("Question{}", index + 1)).set_columns(&[
                    TableColumn::new().set_header("Group"),
                    TableColumn::new().set_header("Value"),
                ]);
                sheet.add_table(3, 0, last, 1, &table)?;
                if *cut {
                    sheet.write_string(
                        last + 2,
                        0,
                        format!("The first {} groups, as on the Chart view.", points.len()),
                    )?;
                }
                sheet.set_column_width(0, 24)?;
                let mut chart = Chart::new(if *line { ChartType::Line } else { ChartType::Bar });
                chart
                    .add_series()
                    .set_categories((name.as_str(), 4, 0, last, 0))
                    .set_values((name.as_str(), 4, 1, last, 1));
                chart.title().set_name(&question.question);
                chart.legend().set_hidden();
                sheet.insert_chart(3, 3, &chart)?;
            }
            Ok(_) => {
                sheet.write_string(3, 0, "No rows match this question now.")?;
            }
            Err(why) => {
                sheet.write_string(3, 0, format!("This question no longer fits the data: {why}"))?;
            }
        }
    }
    book.save_to_buffer()
}

pub async fn workbook(State(state): App, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let account = &signed.session.account;
    let Some(entry) = catalog::find(&state, account, &name) else {
        return message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        );
    };
    let set = &entry.set;
    let questions = state.accounts.questions(&set.name).unwrap_or_default();
    let mut slides = Vec::new();
    for question in &questions {
        slides.push((question, drawn(&state, account, set, question).await));
    }
    let bytes = match workbook_bytes(&set.name, &slides) {
        Ok(bytes) => bytes,
        Err(_) => {
            return message_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "The workbook could not be made.",
            )
        }
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{}-slides.xlsx\"", set.name)) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standalone_chart_carries_its_own_colours() {
        let points = vec![("North".to_string(), 3.0), ("South".to_string(), -1.0)];
        let svg = standalone(&crate::chart::bar_chart(&points, "Sales by region").into_string());
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""), "{svg}");
        assert!(!svg.contains("class="), "no class is left to a stylesheet: {svg}");
        assert!(svg.contains("fill=\"#b86e00\"") && svg.contains("fill=\"#ffffff\""));
        let line = standalone(&crate::chart::line_chart(&points, "Trend").into_string());
        assert!(!line.contains("class="), "{line}");
    }

    #[test]
    fn a_label_longer_than_a_cell_is_cut_not_fatal() {
        let question = Question {
            id: 1,
            dataset: "d".into(),
            question: "Long".into(),
            settings: String::new(),
            approved_by: None,
            approved_at: "2026-11-25 00:00:00".into(),
        };
        let long = "x".repeat(40_000);
        let slides = [(&question, Ok((vec![(long, 1.0)], false, false)))];
        assert!(workbook_bytes("d", &slides).is_ok());
        assert_eq!(fits_a_cell(&"y".repeat(40_000)).chars().count(), 32_767);
        assert_eq!(fits_a_cell("short"), "short");
    }
}
