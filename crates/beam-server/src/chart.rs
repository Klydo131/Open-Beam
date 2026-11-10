//! Charts: a grouped summary of a dataset, drawn on the server as SVG.
//!
//! Like every other page, a chart needs no script. The same numbers are
//! repeated in a table underneath, which is also what a screen reader uses.

use crate::app::AppState;
use crate::catalog;
use crate::feed;
use crate::table::{self, filter_expression, group_digits, Tab};
use crate::web::{self, layout, page, require_signed_in};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_odata::{Aggregate, Bucket, EdmType, Property};
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

/// Most groups drawn as bars, and as points on a line.
const MAX_BARS: u64 = 24;
const MAX_POINTS: u64 = 120;

const AGGREGATES: &[(&str, &str, Aggregate)] = &[
    ("count", "Number of rows", Aggregate::Count),
    ("sum", "Sum of", Aggregate::Sum),
    ("avg", "Average of", Aggregate::Average),
    ("min", "Smallest", Aggregate::Min),
    ("max", "Largest", Aggregate::Max),
];
const BUCKETS: &[(&str, &str, Bucket)] = &[
    ("day", "day", Bucket::Day),
    ("month", "month", Bucket::Month),
    ("quarter", "quarter", Bucket::Quarter),
    ("year", "year", Bucket::Year),
];

#[derive(Deserialize, Default)]
pub struct ChartQuery {
    group: Option<String>,
    agg: Option<String>,
    measure: Option<String>,
    bucket: Option<String>,
    kind: Option<String>,
    column: Option<String>,
    op: Option<String>,
    value: Option<String>,
}

fn is_numeric(property: &Property) -> bool {
    matches!(
        property.edm,
        EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
    )
}

fn is_date(property: &Property) -> bool {
    matches!(property.edm, EdmType::Date | EdmType::DateTimeOffset)
}

/// `1234567.5` becomes `1,234,567.5`; at most two decimal places.
pub fn format_number(value: f64) -> String {
    if !value.is_finite() {
        return "—".to_string();
    }
    let text = format!("{:.2}", value.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let fraction = fraction.trim_end_matches('0');
    let negative = value < 0.0 && (whole != "0" || !fraction.is_empty());
    let mut out = format!("{}{}", if negative { "-" } else { "" }, group_digits(whole));
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

fn coordinate(value: f64) -> String {
    format!("{value:.1}")
}

fn shorten(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        label.to_string()
    } else {
        let mut short: String = label.chars().take(max_chars - 1).collect();
        short.push('…');
        short
    }
}

/// Horizontal bars, one per group, with the value written beside each.
fn bar_chart(points: &[(String, f64)], title: &str) -> Markup {
    const LEFT: f64 = 210.0;
    const WIDTH: f64 = 440.0;
    const ROW: f64 = 30.0;
    let low = points.iter().map(|p| p.1).fold(0.0, f64::min);
    let high = points.iter().map(|p| p.1).fold(0.0, f64::max);
    let span = if high > low { high - low } else { 1.0 };
    let x = |value: f64| LEFT + WIDTH * (value - low) / span;
    let height = points.len() as f64 * ROW + 16.0;
    html! {
        svg.chart viewBox={ "0 0 760 " (coordinate(height)) } role="img" aria-label=(title) {
            @for (index, (label, value)) in points.iter().enumerate() {
                @let top = 8.0 + index as f64 * ROW;
                @let (from, to) = (x(0.0).min(x(*value)), x(0.0).max(x(*value)));
                text.label x="200" y=(coordinate(top + 15.0)) text-anchor="end" { (shorten(label, 28)) }
                rect.bar x=(coordinate(from)) y=(coordinate(top)) width=(coordinate((to - from).max(1.0))) height="20" rx="3" {
                    title { (label) ": " (format_number(*value)) }
                }
                @if *value >= 0.0 {
                    text.value x=(coordinate(to + 6.0)) y=(coordinate(top + 15.0)) { (format_number(*value)) }
                } @else {
                    text.value x=(coordinate(from - 6.0)) y=(coordinate(top + 15.0)) text-anchor="end" { (format_number(*value)) }
                }
            }
            line.axis x1=(coordinate(x(0.0))) x2=(coordinate(x(0.0))) y1="4" y2=(coordinate(height - 4.0)) {}
        }
    }
}

/// Rounds an axis outward to tidy numbers. Returns `(low, high, step)`.
fn tidy_axis(min: f64, max: f64) -> (f64, f64, f64) {
    let (min, max) = if max > min { (min, max) } else { (min - 1.0, max + 1.0) };
    let rough = (max - min) / 4.0;
    let magnitude = 10f64.powf(rough.log10().floor());
    let step = magnitude
        * match rough / magnitude {
            n if n < 1.5 => 1.0,
            n if n < 3.5 => 2.0,
            n if n < 7.5 => 5.0,
            _ => 10.0,
        };
    ((min / step).floor() * step, (max / step).ceil() * step, step)
}

/// A line through the groups in their natural order.
fn line_chart(points: &[(String, f64)], title: &str) -> Markup {
    const LEFT: f64 = 70.0;
    const RIGHT: f64 = 740.0;
    const TOP: f64 = 20.0;
    const BOTTOM: f64 = 280.0;
    let min = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let max = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let (low, high, step) = tidy_axis(min, max);
    let last = points.len().saturating_sub(1);
    let x = |index: usize| match last {
        0 => (LEFT + RIGHT) / 2.0,
        _ => LEFT + (RIGHT - LEFT) * index as f64 / last as f64,
    };
    let y = |value: f64| BOTTOM - (BOTTOM - TOP) * (value - low) / (high - low);
    let tick_count = ((high - low) / step).round() as usize;
    let path: Vec<String> = points
        .iter()
        .enumerate()
        .map(|(index, (_, value))| format!("{},{}", coordinate(x(index)), coordinate(y(*value))))
        .collect();
    // Label at most six positions along the bottom, always including both ends.
    let labelled: Vec<usize> = if last < 6 {
        (0..=last).collect()
    } else {
        (0..6).map(|slot| slot * last / 5).collect()
    };
    html! {
        svg.chart viewBox="0 0 760 320" role="img" aria-label=(title) {
            @for tick in 0..=tick_count {
                @let value = low + step * tick as f64;
                line.grid x1=(coordinate(LEFT)) x2=(coordinate(RIGHT)) y1=(coordinate(y(value))) y2=(coordinate(y(value))) {}
                text.tick x="62" y=(coordinate(y(value) + 4.0)) text-anchor="end" { (format_number(value)) }
            }
            polyline.line points=(path.join(" ")) {}
            @for (index, (label, value)) in points.iter().enumerate() {
                circle.dot cx=(coordinate(x(index))) cy=(coordinate(y(*value))) r="3.5" {
                    title { (label) ": " (format_number(*value)) }
                }
            }
            @for index in labelled {
                @let anchor = if index == 0 && last > 0 { "start" } else if index == last && last > 0 { "end" } else { "middle" };
                text.tick x=(coordinate(x(index))) y="302" text-anchor=(anchor) { (shorten(&points[index].0, 14)) }
            }
        }
    }
}

pub async fn chart(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ChartQuery>,
    headers: HeaderMap,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let Some(entry) = catalog::find(&state, &signed.session.account, &name) else {
        return web::message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        );
    };
    let set = &entry.set;

    // Every choice falls back to something that works, so a chart always appears.
    let group = query
        .group
        .as_deref()
        .and_then(|column| set.property(column))
        .or_else(|| set.properties.iter().find(|p| p.edm == EdmType::String))
        .unwrap_or(&set.properties[0]);
    let numeric: Vec<&Property> = set.properties.iter().filter(|p| is_numeric(p)).collect();
    let measure = query
        .measure
        .as_deref()
        .and_then(|column| numeric.iter().find(|p| p.name == column).copied())
        .or(numeric.first().copied());
    let pick = |code: Option<&str>| AGGREGATES.iter().find(|a| Some(a.0) == code);
    let mut aggregate = pick(query.agg.as_deref()).unwrap_or(&AGGREGATES[0]);
    let bucket = BUCKETS
        .iter()
        .find(|b| Some(b.0) == query.bucket.as_deref())
        .unwrap_or(&BUCKETS[1]);

    let mut problem = None;
    if aggregate.2 != Aggregate::Count && measure.is_none() {
        problem = Some("This dataset has no number columns to summarise, so rows are counted instead.");
        aggregate = &AGGREGATES[0];
    }
    let counting = aggregate.2 == Aggregate::Count;

    let filter_value = query.value.as_deref().unwrap_or("").trim();
    let filter_column = query.column.as_deref().and_then(|column| set.property(column));
    let mut filter_text = None;
    let mut carried: Vec<(&str, &str)> = Vec::new();
    if let (Some(property), false) = (filter_column, filter_value.is_empty()) {
        let op = query.op.as_deref().unwrap_or("");
        match filter_expression(property, op, filter_value) {
            Ok(expression) => {
                filter_text = Some(expression);
                carried = vec![("column", property.name.as_str()), ("op", op), ("value", filter_value)];
            }
            Err(message) => problem = Some(message),
        }
    }

    let ordered = !matches!(group.edm, EdmType::String | EdmType::Boolean);
    let limit = if ordered { MAX_POINTS } else { MAX_BARS };
    let planned = beam_odata::plan_summary(
        set,
        &group.name,
        measure.map(|p| p.name.as_str()).filter(|_| !counting),
        aggregate.2,
        bucket.2,
        filter_text.as_deref(),
        limit,
    );
    let mut points: Vec<(String, f64)> = Vec::new();
    let mut truncated = false;
    match planned {
        Ok(summary) => match feed::run(&state, summary.sql, &summary.params, 2).await {
            Ok(rows) => {
                truncated = rows.len() as u64 > limit;
                points = rows
                    .into_iter()
                    .take(limit as usize)
                    .filter_map(|row| match (&row[0], row[1].as_f64()) {
                        (Value::String(label), Some(value)) => Some((label.clone(), value)),
                        _ => None,
                    })
                    .collect();
            }
            Err(_) => problem = Some("That chart could not be drawn. Check the filter value and try again."),
        },
        Err(_) => problem = Some("That chart could not be drawn. Check the filter value and try again."),
    }

    let as_line = match query.kind.as_deref() {
        Some("line") => true,
        Some("bar") => false,
        _ => ordered,
    };
    let what = match (counting, measure) {
        (false, Some(measure)) => format!("{} {}", aggregate.1, measure.name),
        _ => "Number of rows".to_string(),
    };
    let by = if is_date(group) {
        format!("{} ({})", group.name, bucket.1)
    } else {
        group.name.clone()
    };
    let title = format!("{what} by {by}");

    let body = html! {
        p.crumbs { a href="/" { "Datasets" } " / " (set.name) }
        h1 { (set.name) }
        (table::dataset_tabs(set, Tab::Chart, &carried, table::can_manage(&signed, &entry)))
        form.filter method="get" action={ "/datasets/" (set.name) "/chart" } {
            @for (name, value) in &carried { input type="hidden" name=(name) value=(value); }
            label for="agg" { "Show" }
            select #agg name="agg" {
                @for (code, label, _) in AGGREGATES { option value=(code) selected[*code == aggregate.0] { (label) } }
            }
            @if !numeric.is_empty() {
                select name="measure" aria-label="Number column" {
                    @for property in &numeric {
                        option value=(property.name) selected[measure.is_some_and(|m| m.name == property.name)] { (property.name) }
                    }
                }
            }
            label for="group" { "by" }
            select #group name="group" {
                @for property in &set.properties { option value=(property.name) selected[property.name == group.name] { (property.name) } }
            }
            @if set.properties.iter().any(is_date) {
                label for="bucket" { "dates by" }
                select #bucket name="bucket" {
                    @for (code, label, _) in BUCKETS { option value=(code) selected[*code == bucket.0] { (label) } }
                }
            }
            label for="kind" { "as" }
            select #kind name="kind" {
                option value="bar" selected[!as_line] { "bars" }
                option value="line" selected[as_line] { "a line" }
            }
            button type="submit" { "Draw" }
        }
        @if !carried.is_empty() {
            p.hint { "Only rows where " strong { (carried[0].1) } " matches the filter set on the Table view are included." }
        }
        @if let Some(problem) = problem { p.notice.error role="alert" { (problem) } }
        @if points.is_empty() {
            p.empty { "There is nothing to chart." }
        } @else {
            section.panel {
                h2 { (title) }
                @if as_line { (line_chart(&points, &title)) } @else { (bar_chart(&points, &title)) }
                @if truncated {
                    p.hint { "Showing the first " (limit) " groups. Filter the data or group dates by a longer period to see the rest." }
                }
            }
            div.tablewrap {
                table {
                    thead { tr { th { (by) } th.num { (what) } } }
                    tbody { @for (label, value) in &points { tr { td { (label) } td.num { (format_number(*value)) } } } }
                }
            }
        }
    };
    page(
        StatusCode::OK,
        layout(&format!("{} chart", set.name), Some(&signed), body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_for_people() {
        for (value, text) in [
            (0.0, "0"),
            (12.0, "12"),
            (1234567.5, "1,234,567.5"),
            (-1234.567, "-1,234.57"),
            (0.004, "0"),
            (-0.004, "0"),
            (999.999, "1,000"),
            (f64::NAN, "—"),
        ] {
            assert_eq!(format_number(value), text, "{value}");
        }
        assert_eq!(shorten("short", 28), "short");
        assert_eq!(shorten("a label that is much too long to fit", 12), "a label tha…");
    }

    #[test]
    fn axes_round_outward_to_tidy_steps() {
        assert_eq!(tidy_axis(3.0, 97.0), (0.0, 100.0, 20.0));
        assert_eq!(tidy_axis(0.0, 8.0), (0.0, 8.0, 2.0));
        assert_eq!(tidy_axis(-12.0, 31.0), (-20.0, 40.0, 10.0));
        assert_eq!(tidy_axis(5.0, 5.0), (4.0, 6.0, 0.5));
    }
}
