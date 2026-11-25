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

/// Whether to write an event now, and how many like it were left out
/// since the last one written. `key` names the kind, source and account.
fn admit(table: &mut HashMap<String, (Instant, u64)>, key: String, now: Instant) -> Option<u64> {
    if table.len() >= MAX_SOURCES {
        table.clear();
    }
    match table.get_mut(&key) {
        Some((since, left_out)) if now.duration_since(*since) < WINDOW => {
            *left_out += 1;
            None
        }
        Some((since, left_out)) => {
            let skipped = *left_out;
            *since = now;
            *left_out = 0;
            Some(skipped)
        }
        None => {
            table.insert(key, (now, 0));
            Some(0)
        }
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
    let mut left_out = 0;
    // Tests run side by side in one process and would share this table,
    // so they see every line; `admit` itself is tested on its own.
    if REPEATING.contains(&outcome) && !cfg!(test) {
        static SEEN: Mutex<Option<HashMap<String, (Instant, u64)>>> = Mutex::new(None);
        // What was refused is part of the key, so refusals on different
        // pages are each written; the path is not, so varying it cannot
        // get round the limit.
        let mut what = fields.clone();
        if let Value::Object(map) = &mut what {
            map.remove("path");
        }
        let key = format!(
            "{event}|{outcome}|{}|{}|{what}",
            who.map_or(String::new(), |a| a.id.to_string()),
            from.map_or(String::new(), |ip| crate::net::client(ip).to_string())
        );
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        match admit(seen.get_or_insert_with(HashMap::new), key, Instant::now()) {
            Some(skipped) => left_out = skipped,
            None => return,
        }
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut line = json!({
        "audit": event,
        "outcome": outcome,
        "at": crate::net::rfc3339(seconds),
    });
    if let Some(account) = who {
        line["account"] = json!(account.username);
        line["account_id"] = json!(account.id);
    }
    if let Some(ip) = from {
        line["from"] = json!(ip.to_string());
    }
    if left_out > 0 {
        line["left_out"] = json!(left_out);
    }
    if let (Value::Object(extra), Value::Object(out)) = (shortened(fields), &mut line) {
        for (key, value) in extra {
            out.entry(key).or_insert(value);
        }
    }
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
        let start = Instant::now();
        let key = || "sign_in|locked|1|203.0.113.0".to_string();
        assert_eq!(admit(&mut table, key(), start), Some(0));
        for n in 1..=500 {
            assert_eq!(admit(&mut table, key(), start + Duration::from_millis(n)), None);
        }
        // Another source is written at once.
        assert_eq!(admit(&mut table, "sign_in|locked|2|".into(), start), Some(0));
        // After the window, the next is written with the count left out.
        assert_eq!(admit(&mut table, key(), start + WINDOW), Some(500));
        assert_eq!(admit(&mut table, key(), start + WINDOW + Duration::from_secs(1)), None);
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
