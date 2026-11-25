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

use beam_accounts::Account;
use serde_json::{json, Value};
use std::net::IpAddr;

/// Writes one event. `who` is the account acting, when known; `fields`
/// says what, and must hold no secret.
pub fn record(event: &str, outcome: &str, who: Option<&Account>, from: Option<IpAddr>, fields: Value) {
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
    if let (Value::Object(extra), Value::Object(out)) = (fields, &mut line) {
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
}
