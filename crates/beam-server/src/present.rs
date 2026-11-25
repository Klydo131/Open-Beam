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
use std::sync::Arc;

type App = State<Arc<AppState>>;

/// A question's chart, from current data: its points, and whether it reads
/// best as a line.
async fn drawn(
    state: &Arc<AppState>,
    account: &beam_accounts::Account,
    set: &beam_odata::EntitySet,
    question: &Question,
) -> Result<(Vec<(String, f64)>, bool), String> {
    let options = crate::curate::question_options(set, &question.settings)?;
    let answer = crate::api::summary(state, account, &set.name, &options)
        .await
        .map_err(|e| e.message)?;
    let points = answer["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| Some((g["label"].as_str()?.to_string(), g["value"].as_f64()?)))
        .collect();
    let line = question.settings.split('&').any(|p| p == "kind=line");
    Ok((points, line))
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
    let total = slides.len() + 1;
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
                        (controls(&base, 1, total))
                    }
                    @for (index, (question, result)) in slides.iter().enumerate() {
                        @let number = index + 2;
                        section.slide id={ "slide-" (number) } {
                            h2 { (question.question) }
                            @match result {
                                Ok((points, line)) if !points.is_empty() => {
                                    div.slide-chart { (chart(points, *line, &question.question)) }
                                }
                                Ok(_) => p.empty { "No rows match this question now." },
                                Err(why) => p.notice.error { "This question no longer fits the data: " (why) },
                            }
                            p.slide-meta {
                                "Verified by " (question.approved_by.as_deref().unwrap_or("someone no longer here"))
                                " on " (question.approved_at.get(..10).unwrap_or(&question.approved_at))
                                " · dataset " (set.name)
                                @if matches!(result, Ok((points, _)) if !points.is_empty()) {
                                    " · " a href={ (base) "/present/" (question.id) ".svg" } download { "Download for PowerPoint" }
                                }
                            }
                            (controls(&base, number, total))
                        }
                    }
                    @if slides.is_empty() {
                        section.slide #slide-2 {
                            h2 { "No verified questions yet" }
                            p { "Draw an answer on the " a href={ (base) "/chart" } { "Chart" } " view and save it as a verified question; it becomes a slide here." }
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
        Ok(drawn) if !drawn.0.is_empty() => drawn,
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
}
