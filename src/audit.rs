//! Append-only audit log for security-relevant events (#52).
//!
//! Writes JSON lines to `audit.log` under the data dir. Never records secrets
//! (passwords, tokens) — only event kind, subject (email/id) and a timestamp
//! sourced from the injected `Clock` so tests are deterministic.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::json;

/// Rotate once the log passes this size (D3, #102). One generation is kept
/// (`audit.log.1`); the log is operational detail, not a record of truth.
const MAX_LOG_BYTES: u64 = 1 << 20;
/// `recent()` reads at most this many trailing bytes instead of the whole
/// file; a grow loop covers the pathological chunk-boundary case.
const TAIL_CHUNK: u64 = 256 * 1024;

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
        // usable guard (we only ever append whole lines). The O_APPEND single
        // write stays as-is: it is safe across processes (one line, one
        // atomic write at end of file).
        let _guard = match self.lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        self.rotate_if_oversized();
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Size-based rotation (#102): `audit.log` -> `audit.log.1`, one
    /// generation kept. Runs under the append lock, so the rename can never
    /// interleave with a line being written.
    fn rotate_if_oversized(&self) {
        let oversized = std::fs::metadata(&self.path)
            .map(|m| m.len() > MAX_LOG_BYTES)
            .unwrap_or(false);
        if oversized {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
    }

    /// The most recent `n` events, newest first. Reads the tail of the file,
    /// not the whole thing (#102): a chunk double-up from the end until the
    /// requested count is satisfied or the start of the file is included.
    pub fn recent(&self, n: usize) -> Vec<serde_json::Value> {
        let Ok(mut f) = File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(size) = f.metadata().map(|m| m.len()) else {
            return Vec::new();
        };
        let mut back = size.min(TAIL_CHUNK);
        loop {
            let mut bytes = Vec::new();
            let ok = f
                .seek(SeekFrom::End(-(back as i64)))
                .and_then(|_| f.read_to_end(&mut bytes))
                .is_ok();
            if !ok {
                return Vec::new();
            }
            // A chunk boundary may cut a multi-byte character; lossy is fine
            // (only the dropped fragment can be affected).
            let buf = String::from_utf8_lossy(&bytes);
            // A chunk that starts mid-file begins inside a line: drop the
            // fragment before the first newline.
            let complete = if back < size {
                match buf.find('\n') {
                    Some(pos) => &buf[pos + 1..],
                    None => "",
                }
            } else {
                &buf[..]
            };
            let lines: Vec<&str> = complete.lines().rev().collect();
            if lines.len() >= n || back >= size {
                return lines
                    .into_iter()
                    .take(n)
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
            }
            back = (back * 2).min(size);
        }
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

    #[test]
    fn recent_reads_only_the_tail_but_stays_correct() {
        // #102: tail read must return the same newest-first window a whole
        // file read would, even well past one chunk.
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path());
        let now = Utc::now();
        for i in 0..4000 {
            log.record("ev", &format!("subject-{i}"), now);
        }
        let recent = log.recent(200);
        assert_eq!(recent.len(), 200);
        assert_eq!(recent[0]["subject"], "subject-3999");
        assert_eq!(recent[199]["subject"], "subject-3800");
        // More requests than the file holds still works (whole-file path).
        assert_eq!(log.recent(100_000).len(), 4000);
    }

    #[test]
    fn size_rotation_keeps_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path());
        let now = Utc::now();
        // Each line is ~95 bytes; 1 MiB crosses MAX_LOG_BYTES mid-run.
        for i in 0..13000 {
            log.record("rotation", &format!("s-{i}"), now);
        }
        assert!(
            dir.path().join("audit.log.1").exists(),
            "rotation must keep one generation"
        );
        let rotated_len = std::fs::metadata(dir.path().join("audit.log.1"))
            .unwrap()
            .len();
        assert!(rotated_len > 1 << 20);
        // The live file restarted: recent() sees only post-rotation lines.
        let recent = log.recent(3);
        let subjects: Vec<&str> = recent
            .iter()
            .map(|v| v["subject"].as_str().unwrap())
            .collect();
        assert_eq!(subjects, ["s-12999", "s-12998", "s-12997"]);
    }
}
