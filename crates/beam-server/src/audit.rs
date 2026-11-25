//! The security record: one line per event that matters for finding out
//! what happened, after OWASP ASVS 5.0 chapter V16.
//!
//! - Each line is a JSON object, so text from a request (a username, a
//!   path) can never start a line of its own and forge an entry (V16.4.1).
//! - Each says when (UTC), where (the visitor's address), who and what
//!   (V16.2.1, V16.2.2).
//! - Nothing secret is written: no password, token, cookie or form token
//!   (V16.2.5). A username typed at sign-in is written only when it names
//!   an account, since people sometimes type their password there.
//! - Lines go to standard error, for the service manager (systemd, Docker)
//!   to keep and send on to a separate system (V16.4.3).
//! - Repeated refusals of one kind, from one address, for one account are
//!   written at most once a [`WINDOW`], the next line counting the ones
//!   left out, so a flood of guesses cannot flood the record.

use beam_accounts::Account;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Repeated refusals are written at most once in this long.
pub const WINDOW: Duration = Duration::from_secs(60);
/// Outcomes that come in floods and are written at most once a window.
const REPEATING: [&str; 3] = ["locked", "busy", "refused"];
/// Text from a request is cut to this many characters.
const MAX_TEXT: usize = 200;
/// Distinct repeating sources remembered before the table starts afresh.
const MAX_SOURCES: usize = 10_000;

tokio::task_local! {
    /// The visitor's address for the request being handled.
    static PEER: Option<IpAddr>;
}

/// Runs `work` with `peer` as the address every record in it carries.
pub async fn with_peer<F: std::future::Future>(peer: Option<IpAddr>, work: F) -> F::Output {
    PEER.scope(peer, work).await
}

/// A source of repeated refusals: when its last line was written, how
/// many like it were left out since, and that line, to write the count with.
struct Source {
    since: Instant,
    left_out: u64,
    line: Value,
}

static SEEN: Mutex<Option<HashMap<String, Source>>> = Mutex::new(None);

/// Whether to write an event now, and how many like it were left out
/// since the last one written. `key` names the kind, source and account.
/// Counts that would be lost when the table starts afresh are returned in
/// `pending` for writing.
fn admit(
    table: &mut HashMap<String, Source>,
    key: String,
    line: &Value,
    now: Instant,
    pending: &mut Vec<Value>,
) -> Option<u64> {
    if table.len() >= MAX_SOURCES && !table.contains_key(&key) {
        pending.extend(table.drain().filter_map(|(_, s)| counted(s)));
    }
    match table.get_mut(&key) {
        Some(source) if now.duration_since(source.since) < WINDOW => {
            source.left_out += 1;
            None
        }
        Some(source) => {
            let skipped = source.left_out;
            source.since = now;
            source.left_out = 0;
            Some(skipped)
        }
        None => {
            table.insert(
                key,
                Source {
                    since: now,
                    left_out: 0,
                    line: line.clone(),
                },
            );
            Some(0)
        }
    }
}

/// The line that reports a source's left-out count, if it has one.
fn counted(source: Source) -> Option<Value> {
    (source.left_out > 0).then(|| {
        let mut line = source.line;
        line["left_out"] = json!(source.left_out);
        line
    })
}

/// Lines for the counts of sources whose window has passed, which are then
/// forgotten. Without this a burst that stops would never be counted.
fn due(table: &mut HashMap<String, Source>, now: Instant) -> Vec<Value> {
    let ended: Vec<String> = table
        .iter()
        .filter(|(_, s)| now.duration_since(s.since) >= WINDOW)
        .map(|(k, _)| k.clone())
        .collect();
    ended
        .into_iter()
        .filter_map(|key| table.remove(&key).and_then(counted))
        .collect()
}

/// Writes the counts of repeated refusals whose window has passed. Called
/// once a [`WINDOW`] by the server.
pub fn flush() {
    let lines = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        due(seen.get_or_insert_with(HashMap::new), Instant::now())
    };
    for line in lines {
        emit(line);
    }
}

fn shortened(value: Value) -> Value {
    match value {
        Value::String(text) if text.chars().count() > MAX_TEXT => {
            Value::String(text.chars().take(MAX_TEXT).chain(['…']).collect())
        }
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, shortened(v))).collect()),
        other => other,
    }
}

/// Writes one event. `who` is the account acting, when known; `fields`
/// says what, and must hold no secret.
pub fn record(event: &str, outcome: &str, who: Option<&Account>, from: Option<IpAddr>, fields: Value) {
    let from = from.or_else(|| PEER.try_with(|p| *p).ok().flatten());
    let mut line = json!({ "audit": event, "outcome": outcome });
    if let Some(account) = who {
        line["account"] = json!(account.username);
        line["account_id"] = json!(account.id);
    }
    if let Some(ip) = from {
        line["from"] = json!(ip.to_string());
    }
    if let (Value::Object(extra), Value::Object(out)) = (shortened(fields), &mut line) {
        for (key, value) in extra {
            out.entry(key).or_insert(value);
        }
    }
    // Tests run side by side in one process and would share this table,
    // so they see every line; `admit` and `due` are tested on their own.
    if REPEATING.contains(&outcome) && !cfg!(test) {
        // What was refused is part of the key, so refusals on different
        // pages are each written; the path is not, so varying it cannot
        // get round the limit.
        let mut what = line.clone();
        if let Value::Object(map) = &mut what {
            map.remove("path");
        }
        let key = what.to_string();
        let mut pending = Vec::new();
        let admitted = {
            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
            admit(
                seen.get_or_insert_with(HashMap::new),
                key,
                &what,
                Instant::now(),
                &mut pending,
            )
        };
        for earlier in pending {
            emit(earlier);
        }
        match admitted {
            Some(0) => {}
            Some(skipped) => line["left_out"] = json!(skipped),
            None => return,
        }
    }
    emit(line);
}

/// Stamps a line with the time and writes it.
fn emit(mut line: Value) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    line["at"] = json!(crate::net::rfc3339(seconds));
    let text = line.to_string();
    #[cfg(test)]
    written().lock().unwrap_or_else(|e| e.into_inner()).push(text.clone());
    eprintln!("{text}");
}

/// Every line written in this test run, oldest first.
#[cfg(test)]
pub fn written() -> &'static std::sync::Mutex<Vec<String>> {
    static LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    &LINES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_json_object_whatever_the_text_holds() {
        let forged = "x\n{\"audit\":\"sign_in\",\"outcome\":\"success\"}";
        record("access_denied", "refused", None, None, json!({ "path": forged }));
        let line = written()
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|l| l.contains("access_denied"))
            .cloned()
            .unwrap();
        assert!(!line.contains('\n'));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["path"], forged);
        assert_eq!(parsed["audit"], "access_denied");
        assert!(parsed["at"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn a_field_cannot_overwrite_who_or_when() {
        record("x_test", "ok", None, None, json!({ "at": "1970", "audit": "sign_in" }));
        let line = written()
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|l| l.contains("x_test"))
            .cloned()
            .unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["audit"], "x_test");
        assert_ne!(parsed["at"], "1970");
    }

    #[test]
    fn repeated_refusals_are_written_once_a_window_with_a_count() {
        let mut table = HashMap::new();
        let mut pending = Vec::new();
        let start = Instant::now();
        let line = json!({ "audit": "sign_in", "outcome": "locked", "account": "bob" });
        let key = || "bob-locked".to_string();
        assert_eq!(admit(&mut table, key(), &line, start, &mut pending), Some(0));
        for n in 1..=500 {
            assert_eq!(
                admit(&mut table, key(), &line, start + Duration::from_millis(n), &mut pending),
                None
            );
        }
        // Another account is written at once.
        assert_eq!(
            admit(&mut table, "carol-locked".into(), &line, start, &mut pending),
            Some(0)
        );
        // After the window, the next is written with the count left out.
        assert_eq!(admit(&mut table, key(), &line, start + WINDOW, &mut pending), Some(500));
        assert_eq!(
            admit(
                &mut table,
                key(),
                &line,
                start + WINDOW + Duration::from_secs(1),
                &mut pending
            ),
            None
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn a_burst_that_stops_is_still_counted() {
        let mut table = HashMap::new();
        let mut pending = Vec::new();
        let start = Instant::now();
        let line = json!({ "audit": "sign_in", "outcome": "locked", "account": "bob" });
        admit(&mut table, "k".into(), &line, start, &mut pending);
        for _ in 0..7 {
            admit(&mut table, "k".into(), &line, start, &mut pending);
        }
        // Nothing is due inside the window; after it, one line with the count.
        assert!(due(&mut table, start + Duration::from_secs(1)).is_empty());
        let lines = due(&mut table, start + WINDOW);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["left_out"], 7);
        assert_eq!(lines[0]["account"], "bob");
        assert!(table.is_empty(), "a counted source is forgotten");
    }

    #[test]
    fn counts_are_written_before_the_table_starts_afresh() {
        let mut table = HashMap::new();
        let mut pending = Vec::new();
        let start = Instant::now();
        let line = json!({ "audit": "x" });
        for n in 0..MAX_SOURCES {
            admit(&mut table, n.to_string(), &line, start, &mut pending);
        }
        admit(&mut table, "0".into(), &line, start, &mut pending);
        admit(&mut table, "new".into(), &line, start, &mut pending);
        assert_eq!(pending.len(), 1, "the one source with a count is written");
        assert_eq!(pending[0]["left_out"], 1);
    }

    #[test]
    fn text_from_a_request_is_cut_short() {
        let long = "a".repeat(5_000);
        record("x_long", "ok", None, None, json!({ "path": long }));
        let line = written()
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|l| l.contains("x_long"))
            .cloned()
            .unwrap();
        assert!(line.len() < 400, "{}", line.len());
        assert!(line.contains('…'));
    }

    #[tokio::test]
    async fn the_request_address_is_added_when_not_given() {
        let ip: IpAddr = "198.51.100.23".parse().unwrap();
        with_peer(Some(ip), async { record("x_peer", "ok", None, None, json!({})) }).await;
        let line = written()
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|l| l.contains("x_peer"))
            .cloned()
            .unwrap();
        assert!(line.contains("198.51.100.23"), "{line}");
    }
}
