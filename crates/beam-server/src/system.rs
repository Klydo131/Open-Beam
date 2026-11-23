//! How the server is doing: memory, spill, queries and the slowest recent
//! ones, for administrators. Shown as a page and, for monitoring tools such
//! as Prometheus and Grafana, as plain-text metrics at `/metrics`.
//!
//! Nothing here holds data people asked about: the slow-query log keeps
//! statements with their quoted values replaced.

use crate::app::AppState;
use crate::auth::Viewer;
use crate::chart::format_number;
use crate::web::{layout, message_page, page, require_signed_in};
use axum::extract::{Extension, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_accounts::{Role, WatchStatus};
use beam_engine::{Stats, LATENCY_BUCKETS_MS};
use maud::{html, Markup};
use std::fmt::Write as _;
use std::sync::Arc;

type App = State<Arc<AppState>>;

/// This process's resident memory and its peak, in bytes. Read from the
/// operating system where it offers it without extra tools (Linux).
fn process_memory() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", format_number(bytes as f64 / 1024.0 / 1024.0))
}

async fn read_stats(state: &Arc<AppState>) -> Option<Stats> {
    let worker = state.clone();
    tokio::task::spawn_blocking(move || worker.engine.stats()).await.ok()
}

fn meter(label: &str, used: u64, limit: u64) -> Markup {
    let share = if limit == 0 {
        0.0
    } else {
        (used as f64 / limit as f64).min(1.0)
    };
    let level = if share >= 0.9 {
        "high"
    } else if share >= 0.7 {
        "warm"
    } else {
        "fine"
    };
    html! {
        div.meter-row {
            div.meter-text { span { (label) } span.mono { (mib(used)) " of " (mib(limit)) } }
            meter class=(level) min="0" max="1" value=(format!("{share:.3}")) title=(format!("{:.0}%", share * 100.0)) {}
        }
    }
}

pub async fn page_route(State(state): App, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    if signed.session.account.role != Role::Admin {
        return message_page(
            StatusCode::FORBIDDEN,
            "Administrators only",
            "Only administrators can see how the server is doing.",
        );
    }
    let Some(stats) = read_stats(&state).await else {
        return message_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "The figures could not be read.",
        );
    };
    let process = process_memory();
    let watches = state.accounts.watches(None).unwrap_or_default();
    let count = |s: WatchStatus| watches.iter().filter(|w| w.status == Some(s)).count();
    let p = |share| {
        stats.percentile_ms(share).map_or("over 30 s".to_string(), |ms| {
            format!("≤ {} ms", format_number(ms as f64))
        })
    };
    let mean = if stats.queries_total == 0 {
        "—".to_string()
    } else {
        format!(
            "{} ms",
            format_number(stats.query_seconds_total * 1000.0 / stats.queries_total as f64)
        )
    };
    let uptime = stats.uptime.as_secs();
    let body = html! {
        div.intro {
            div {
                p.eyebrow { "OPEN BEAM / SYSTEM" }
                h1 { "How the server is doing." }
                p.lede { "Up " (uptime / 3600) " h " ((uptime % 3600) / 60) " min. Figures are from this moment; "
                    a href="/admin/system" { "refresh" } " to update. Monitoring tools can read the same figures at " code { "/metrics" } "." }
            }
        }
        dl.counters {
            div { dt { "Queries" } dd { (stats.queries_total) } }
            div { dt { "Running now" } dd { (stats.queries_running) " / " (stats.max_concurrent) } }
            div class={ @if stats.queries_timed_out > 0 { "alerting" } } { dt { "Timed out" } dd { (stats.queries_timed_out) } }
            div class={ @if stats.queries_out_of_memory > 0 { "alerting" } } { dt { "Out of memory" } dd { (stats.queries_out_of_memory) } }
            div class={ @if stats.queries_busy > 0 { "alerting" } } { dt { "Turned away busy" } dd { (stats.queries_busy) } }
            div { dt { "Failed" } dd { (stats.queries_failed) } }
        }
        div.system-grid {
            section.panel {
                div.section-label { span { "01 / MEMORY" } span.tag { "BOUNDED" } }
                (meter("Query engine", stats.memory_used_bytes, stats.memory_limit_bytes))
                (meter("Temporary files (spill)", stats.spill_used_bytes, stats.spill_limit_bytes.max(1)))
                @match process {
                    Some((rss, peak)) => p.hint { "Whole server process: " (mib(rss)) " now, " (mib(peak)) " at most since it started." },
                    None => p.hint { "Whole-process memory is not reported on this operating system." },
                }
                p.hint { "When a query needs more than the engine's share, it spills to temporary files; past that limit it stops with a clear message instead of taking the computer's memory." }
            }
            section.panel {
                div.section-label { span { "02 / SPEED" } span.tag { "ALL QUERIES" } }
                dl.facts {
                    dt { "Average" } dd { (mean) }
                    dt { "Half finish in" } dd { (p(0.5)) }
                    dt { "95 in 100 finish in" } dd { (p(0.95)) }
                    dt { "99 in 100 finish in" } dd { (p(0.99)) }
                }
            }
            section.panel {
                div.section-label { span { "03 / STORAGE" } span.tag { "COLUMN-STORE COPIES" } }
                dl.facts {
                    dt { "Datasets" } dd { (stats.datasets) }
                    dt { "Kept as fast copies" } dd { (stats.cached_datasets) }
                    dt { "Space they use" } dd { (mib(stats.cache_bytes)) }
                }
                p.hint { "Each file dataset is copied once into a compressed column format, so queries scan it instead of reading the original file every time. A changed file gets a fresh copy." }
            }
            section.panel {
                div.section-label { span { "04 / WATCHES" } a.tag href="/watches" { "OPEN" } }
                dl.facts {
                    dt { "Alerts" } dd { (count(WatchStatus::Alert)) }
                    dt { "Cannot check" } dd { (count(WatchStatus::Error)) }
                    dt { "OK" } dd { (count(WatchStatus::Ok)) }
                }
            }
        }
        section.panel {
            div.section-label { span { "05 / SLOWEST RECENT QUERIES" } span.tag { "250 MS OR MORE · VALUES HIDDEN" } }
            @if stats.slow.is_empty() {
                p.empty { "None yet." }
            } @else {
                div.tablewrap {
                    table {
                        thead { tr { th.num { "Time" } th { "Result" } th { "Statement" } } }
                        tbody {
                            @for q in &stats.slow {
                                tr {
                                    td.num { (format_number(q.millis as f64)) " ms" }
                                    td { (q.outcome.as_str()) }
                                    td { code.wrap { (q.statement) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    page(StatusCode::OK, layout("System", Some(&signed), body))
}

/// The same figures in Prometheus' text format, for administrators.
pub async fn metrics_route(State(state): App, Extension(viewer): Extension<Viewer>) -> Response {
    if viewer.0.role != Role::Admin {
        return (StatusCode::FORBIDDEN, "Only administrators can read metrics.\n").into_response();
    }
    let Some(stats) = read_stats(&state).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "The figures could not be read.\n").into_response();
    };
    let watches = state.accounts.watches(None).unwrap_or_default();
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        prometheus(&stats, process_memory(), &watches),
    )
        .into_response()
}

pub fn prometheus(stats: &Stats, process: Option<(u64, u64)>, watches: &[beam_accounts::Watch]) -> String {
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, value: String| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}");
    };
    gauge(
        "beam_uptime_seconds",
        "Seconds since the server started.",
        stats.uptime.as_secs().to_string(),
    );
    gauge(
        "beam_engine_memory_bytes",
        "Memory the query engine is using.",
        stats.memory_used_bytes.to_string(),
    );
    gauge(
        "beam_engine_memory_limit_bytes",
        "Memory the query engine may use.",
        stats.memory_limit_bytes.to_string(),
    );
    gauge(
        "beam_spill_bytes",
        "Temporary files written for queries too large for memory.",
        stats.spill_used_bytes.to_string(),
    );
    gauge(
        "beam_spill_limit_bytes",
        "Most temporary files allowed.",
        stats.spill_limit_bytes.to_string(),
    );
    gauge(
        "beam_queries_running",
        "Queries running now.",
        stats.queries_running.to_string(),
    );
    gauge(
        "beam_queries_max_concurrent",
        "Queries that may run at once.",
        stats.max_concurrent.to_string(),
    );
    gauge("beam_datasets", "Datasets loaded.", stats.datasets.to_string());
    gauge(
        "beam_cached_datasets",
        "Datasets kept as column-store copies.",
        stats.cached_datasets.to_string(),
    );
    gauge(
        "beam_cache_bytes",
        "Disk space used by column-store copies.",
        stats.cache_bytes.to_string(),
    );
    if let Some((rss, peak)) = process {
        gauge(
            "beam_process_resident_bytes",
            "Resident memory of the server process.",
            rss.to_string(),
        );
        gauge(
            "beam_process_resident_peak_bytes",
            "Peak resident memory of the server process.",
            peak.to_string(),
        );
    }
    let ok = stats.queries_total.saturating_sub(
        stats.queries_failed + stats.queries_timed_out + stats.queries_out_of_memory + stats.queries_busy,
    );
    let _ = writeln!(
        out,
        "# HELP beam_queries_total Queries finished, by outcome.\n# TYPE beam_queries_total counter"
    );
    for (outcome, count) in [
        ("ok", ok),
        ("failed", stats.queries_failed),
        ("timed_out", stats.queries_timed_out),
        ("out_of_memory", stats.queries_out_of_memory),
        ("busy", stats.queries_busy),
    ] {
        let _ = writeln!(out, "beam_queries_total{{outcome=\"{outcome}\"}} {count}");
    }
    let _ = writeln!(
        out,
        "# HELP beam_query_duration_seconds Time each query took.\n# TYPE beam_query_duration_seconds histogram"
    );
    let mut cumulative = 0;
    for (index, count) in stats.latency_buckets.iter().enumerate() {
        cumulative += count;
        let le = LATENCY_BUCKETS_MS
            .get(index)
            .map_or("+Inf".to_string(), |ms| format!("{}", *ms as f64 / 1000.0));
        let _ = writeln!(out, "beam_query_duration_seconds_bucket{{le=\"{le}\"}} {cumulative}");
    }
    let _ = writeln!(out, "beam_query_duration_seconds_sum {}", stats.query_seconds_total);
    let _ = writeln!(out, "beam_query_duration_seconds_count {cumulative}");
    let _ = writeln!(
        out,
        "# HELP beam_watches Watches by their latest result.\n# TYPE beam_watches gauge"
    );
    for (label, status) in [
        ("alert", Some(WatchStatus::Alert)),
        ("error", Some(WatchStatus::Error)),
        ("ok", Some(WatchStatus::Ok)),
        ("not_run", None),
    ] {
        let n = watches.iter().filter(|w| w.status == status).count();
        let _ = writeln!(out, "beam_watches{{status=\"{label}\"}} {n}");
    }
    out
}
