//! Append-only audit log for security-relevant events (#52).
//!
//! Writes JSON lines to `audit.log` under the data dir. Never records secrets
//! (passwords, tokens) — only event kind, subject (email/id) and a timestamp
//! sourced from the injected `Clock` so tests are deterministic.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::json;

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join("audit.log"),
            lock: Mutex::new(()),
        }
    }

    /// Append one event. `subject` is an email or id, never a secret.
    pub fn record(&self, event: &str, subject: &str, now: DateTime<Utc>) {
        let line = json!({ "ts": now.to_rfc3339(), "event": event, "subject": subject });
        // Hold the append lock for this line; a poisoned lock still yields a
        // usable guard (we only ever append whole lines).
        let _guard = match self.lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The most recent `n` events, newest first.
    pub fn recent(&self, n: usize) -> Vec<serde_json::Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .rev()
            .take(n)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path());
        let now = Utc::now();
        log.record("login_failed", "a@b.co", now);
        log.record("login_ok", "a@b.co", now);
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["event"], "login_ok"); // newest first
        assert_eq!(recent[1]["subject"], "a@b.co");
    }
}
