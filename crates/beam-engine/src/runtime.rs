//! How the engine keeps itself within bounds and reports on itself:
//! memory and spill limits, a gate on how many queries run at once, and
//! the counters behind the System page and `/metrics`.
//!
//! Everything here is bounded: counters are fixed-size, the slow-query log
//! keeps a fixed number of entries, and nothing grows with the number of
//! queries run.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How much the engine may use. Every field has a safe default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Memory the engine may use, in bytes. `None` means half the
    /// computer's memory, as DuckDB's own guidance suggests for a server.
    pub memory_limit_bytes: Option<u64>,
    /// Largest total of temporary files written while a query is too big
    /// for memory, in bytes.
    pub spill_limit_bytes: u64,
    /// Queries that may run at once. Others wait their turn, up to the time
    /// limit, because every query shares one memory budget.
    pub max_concurrent: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_limit_bytes: None,
            spill_limit_bytes: 10 * 1024 * 1024 * 1024,
            max_concurrent: 4,
        }
    }
}

/// Reads DuckDB's way of writing sizes, such as `12.5 GiB` or `512.0 MB`.
pub(crate) fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(text.len());
    let number: f64 = text[..split].trim().parse().ok()?;
    let unit = text[split..].trim().to_ascii_lowercase();
    let scale: f64 = match unit.as_str() {
        "" | "b" | "bytes" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    (number.is_finite() && number >= 0.0).then_some((number * scale) as u64)
}

/// Lets at most a fixed number of queries run at once.
pub(crate) struct Gate {
    running: Mutex<usize>,
    freed: Condvar,
    max: usize,
}

/// A place in the gate, given back when dropped, however the query ends.
pub(crate) struct Slot<'a>(&'a Gate);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut running = self.0.running.lock().unwrap_or_else(|e| e.into_inner());
        *running = running.saturating_sub(1);
        self.0.freed.notify_one();
    }
}

impl Gate {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            running: Mutex::new(0),
            freed: Condvar::new(),
            max: max.max(1),
        }
    }

    /// Waits up to `wait` for a place; `None` when none came free.
    pub(crate) fn enter(&self, wait: Duration) -> Option<Slot<'_>> {
        let deadline = Instant::now() + wait;
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        while *running >= self.max {
            let left = deadline.checked_duration_since(Instant::now())?;
            let (guard, timeout) = self
                .freed
                .wait_timeout(running, left)
                .unwrap_or_else(|e| e.into_inner());
            running = guard;
            if timeout.timed_out() && *running >= self.max {
                return None;
            }
        }
        *running += 1;
        Some(Slot(self))
    }

    pub(crate) fn running(&self) -> usize {
        *self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn max(&self) -> usize {
        self.max
    }
}

/// Upper bounds of the latency buckets, in milliseconds. The last bucket
/// holds everything slower.
pub const LATENCY_BUCKETS_MS: [u64; 10] = [10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000];
/// Slow queries kept for the System page.
const SLOW_LOG: usize = 20;
/// A query at least this slow goes in the slow-query log.
const SLOW_MS: u64 = 250;

/// How a query ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
    TimedOut,
    OutOfMemory,
    Busy,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::TimedOut => "timed out",
            Outcome::OutOfMemory => "out of memory",
            Outcome::Busy => "busy",
        }
    }
}

/// One entry in the slow-query log. The statement has its quoted values
/// replaced by `?`, so the log never holds the data people asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowQuery {
    pub statement: String,
    pub millis: u64,
    pub outcome: Outcome,
    /// Seconds since the engine opened.
    pub at_seconds: u64,
}

/// Fixed-size counters, safe to update from any thread.
pub(crate) struct Metrics {
    started: Instant,
    pub(crate) total: AtomicU64,
    pub(crate) failed: AtomicU64,
    pub(crate) timed_out: AtomicU64,
    pub(crate) out_of_memory: AtomicU64,
    pub(crate) busy: AtomicU64,
    pub(crate) micros: AtomicU64,
    pub(crate) buckets: [AtomicU64; LATENCY_BUCKETS_MS.len() + 1],
    slow: Mutex<VecDeque<SlowQuery>>,
}

impl Metrics {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            total: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            out_of_memory: AtomicU64::new(0),
            busy: AtomicU64::new(0),
            micros: AtomicU64::new(0),
            buckets: Default::default(),
            slow: Mutex::new(VecDeque::with_capacity(SLOW_LOG)),
        }
    }

    pub(crate) fn record(&self, statement: &str, took: Duration, outcome: Outcome) {
        self.total.fetch_add(1, Ordering::Relaxed);
        let counter = match outcome {
            Outcome::Ok => None,
            Outcome::Failed => Some(&self.failed),
            Outcome::TimedOut => Some(&self.timed_out),
            Outcome::OutOfMemory => Some(&self.out_of_memory),
            Outcome::Busy => Some(&self.busy),
        };
        if let Some(counter) = counter {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.micros.fetch_add(micros, Ordering::Relaxed);
        let millis = micros / 1000;
        let bucket = LATENCY_BUCKETS_MS
            .iter()
            .position(|limit| millis <= *limit)
            .unwrap_or(LATENCY_BUCKETS_MS.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        if millis >= SLOW_MS && !statement.is_empty() {
            let mut slow = self.slow.lock().unwrap_or_else(|e| e.into_inner());
            if slow.len() == SLOW_LOG {
                slow.pop_front();
            }
            slow.push_back(SlowQuery {
                statement: redact(statement),
                millis,
                outcome,
                at_seconds: self.started.elapsed().as_secs(),
            });
        }
    }

    pub(crate) fn slow(&self) -> Vec<SlowQuery> {
        let slow = self.slow.lock().unwrap_or_else(|e| e.into_inner());
        slow.iter().rev().cloned().collect()
    }

    pub(crate) fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

/// A statement with every quoted value replaced by `'?'`, cut to a readable
/// length. Values reach the engine as bound parameters almost everywhere;
/// this covers statements typed by PostgreSQL clients.
pub(crate) fn redact(statement: &str) -> String {
    const MAX: usize = 300;
    let mut out = String::with_capacity(statement.len().min(MAX + 1));
    let mut in_quote = false;
    for c in statement.chars() {
        match (in_quote, c) {
            (false, '\'') => {
                in_quote = true;
                out.push_str("'?");
            }
            (true, '\'') => {
                in_quote = false;
                out.push('\'');
            }
            (true, _) => {}
            (false, c) if c.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            (false, c) => out.push(c),
        }
        if out.chars().count() > MAX {
            out = out.chars().take(MAX).collect();
            out.push('…');
            return out;
        }
    }
    out.trim().to_string()
}

/// Everything the System page shows about the engine, read at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub uptime: Duration,
    pub datasets: usize,
    pub cached_datasets: usize,
    pub cache_bytes: u64,
    pub memory_limit_bytes: u64,
    pub memory_used_bytes: u64,
    pub spill_used_bytes: u64,
    pub spill_limit_bytes: u64,
    pub queries_running: usize,
    pub max_concurrent: usize,
    pub queries_total: u64,
    pub queries_failed: u64,
    pub queries_timed_out: u64,
    pub queries_out_of_memory: u64,
    pub queries_busy: u64,
    pub query_seconds_total: f64,
    /// Count of queries at or under each bucket's limit, then the slower
    /// ones; not cumulative.
    pub latency_buckets: Vec<u64>,
    pub slow: Vec<SlowQuery>,
}

impl Stats {
    /// The smallest bucket limit, in milliseconds, under which at least
    /// `share` of queries finished; `None` before any query or when the
    /// answer lies in the open-ended last bucket.
    pub fn percentile_ms(&self, share: f64) -> Option<u64> {
        let total: u64 = self.latency_buckets.iter().sum();
        if total == 0 {
            return None;
        }
        let wanted = (total as f64 * share).ceil() as u64;
        let mut seen = 0;
        for (index, count) in self.latency_buckets.iter().enumerate() {
            seen += count;
            if seen >= wanted {
                return LATENCY_BUCKETS_MS.get(index).copied();
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_duckdb_writes_them() {
        assert_eq!(parse_size("12.5 GiB"), Some(13_421_772_800));
        assert_eq!(parse_size("512.0 MB"), Some(512_000_000));
        assert_eq!(parse_size("100 KiB"), Some(102_400));
        assert_eq!(parse_size("7"), Some(7));
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("-1 GB"), None);
    }

    #[test]
    fn the_gate_admits_only_so_many_and_frees_places_on_drop() {
        let gate = Gate::new(2);
        let a = gate.enter(Duration::ZERO).unwrap();
        let _b = gate.enter(Duration::ZERO).unwrap();
        assert!(gate.enter(Duration::from_millis(20)).is_none(), "full");
        drop(a);
        assert!(gate.enter(Duration::ZERO).is_some(), "a dropped slot is free again");
        assert_eq!(gate.running(), 1, "and the temporary one went back too");
    }

    #[test]
    fn metrics_stay_bounded_and_never_keep_quoted_values() {
        let metrics = Metrics::new();
        for n in 0..1_000 {
            let sql = format!("SELECT * FROM t WHERE name = 'secret {n}'");
            metrics.record(&sql, Duration::from_millis(300), Outcome::Ok);
        }
        let slow = metrics.slow();
        assert_eq!(slow.len(), SLOW_LOG, "the log never grows past its size");
        assert!(slow.iter().all(|s| !s.statement.contains("secret")), "{slow:?}");
        assert_eq!(slow[0].statement, "SELECT * FROM t WHERE name = '?'");
        metrics.record("", Duration::from_millis(3), Outcome::TimedOut);
        assert_eq!(metrics.total.load(Ordering::Relaxed), 1_001);
        assert_eq!(metrics.timed_out.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.buckets[0].load(Ordering::Relaxed), 1);
        assert_eq!(metrics.buckets[4].load(Ordering::Relaxed), 1_000);
    }

    #[test]
    fn percentiles_read_from_the_buckets() {
        let stats = |buckets: Vec<u64>| Stats {
            uptime: Duration::ZERO,
            datasets: 0,
            cached_datasets: 0,
            cache_bytes: 0,
            memory_limit_bytes: 0,
            memory_used_bytes: 0,
            spill_used_bytes: 0,
            spill_limit_bytes: 0,
            queries_running: 0,
            max_concurrent: 1,
            queries_total: 0,
            queries_failed: 0,
            queries_timed_out: 0,
            queries_out_of_memory: 0,
            queries_busy: 0,
            query_seconds_total: 0.0,
            latency_buckets: buckets,
            slow: Vec::new(),
        };
        let s = stats(vec![90, 5, 5, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(s.percentile_ms(0.5), Some(10));
        assert_eq!(s.percentile_ms(0.95), Some(50));
        assert_eq!(s.percentile_ms(0.99), Some(100));
        assert_eq!(stats(vec![0; 11]).percentile_ms(0.5), None);
    }
}
