//! Session revocation (#45). Stateless signed cookies can't be revoked before
//! expiry, so logout records the token's id (`jti`) here until it would expire
//! anyway. The set is persisted under the data dir so a restart keeps revoked
//! sessions revoked. Expired entries are pruned on load and on write.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Revoked {
    jti: String,
    exp: i64,
}

pub struct Revocations {
    path: PathBuf,
    inner: Mutex<Vec<Revoked>>,
}

impl Revocations {
    pub fn new(root: &Path) -> Self {
        let path = root.join("revoked.json");
        let list = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Revoked>>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(list),
        }
    }

    /// Record a revoked token and persist (pruning anything already expired).
    pub fn revoke(&self, jti: &str, exp: i64, now: DateTime<Utc>) {
        let now_ts = now.timestamp();
        let mut list = lock(&self.inner);
        list.retain(|r| r.exp > now_ts);
        if !list.iter().any(|r| r.jti == jti) {
            list.push(Revoked {
                jti: jti.to_string(),
                exp,
            });
        }
        write_atomic(&self.path, &serde_json::to_vec(&*list).unwrap_or_default());
    }

    /// True if this token id has been revoked and has not yet expired.
    ///
    /// Re-reads the file on check (review B6): a logout recorded by another
    /// process (or a previous boot sharing the volume) must take effect here
    /// too, otherwise revoked sessions keep working. The file is tiny and
    /// auth already touches disk for the user record.
    pub fn is_revoked(&self, jti: &str, now: DateTime<Utc>) -> bool {
        let now_ts = now.timestamp();
        let fresh: Vec<Revoked> = fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Revoked>>(&s).ok())
            .unwrap_or_default();
        if fresh.iter().any(|r| r.jti == jti && r.exp > now_ts) {
            return true;
        }
        // fall back to the in-memory view if the file read failed entirely
        let list = lock(&self.inner);
        list.iter().any(|r| r.jti == jti && r.exp > now_ts)
    }
}

/// tmp+rename so a concurrent backup never archives a truncated file (B7).
fn write_atomic(path: &std::path::Path, bytes: &[u8]) {
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn lock(m: &Mutex<Vec<Revoked>>) -> std::sync::MutexGuard<'_, Vec<Revoked>> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_then_check_and_prune_expired() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let r = Revocations::new(dir.path());
        let future = now.timestamp() + 3600;
        assert!(!r.is_revoked("abc", now));
        r.revoke("abc", future, now);
        assert!(r.is_revoked("abc", now));
        // Persisted: a fresh instance sees the revocation.
        let r2 = Revocations::new(dir.path());
        assert!(r2.is_revoked("abc", now));
        // Expired entries are not revoked.
        let past = now.timestamp() - 10;
        r2.revoke("old", past, now);
        assert!(!r2.is_revoked("old", now));
    }
}

#[cfg(test)]
mod cross_process_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn revocations_are_visible_across_instances_sharing_a_dir() {
        // Review B6: a second process (or later boot) must honour revocations
        // written by the first, because is_revoked re-reads the file.
        let dir = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).unwrap();
        let a = Revocations::new(dir.path());
        let b = Revocations::new(dir.path());
        a.revoke("token-1", now.timestamp() + 3600, now);
        assert!(b.is_revoked("token-1", now), "b must see a's revoke");
        assert!(!b.is_revoked("token-2", now));
    }
}
