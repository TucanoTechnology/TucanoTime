//! In-process rate limiting for authentication endpoints (#47).
//!
//! A fixed-window failure counter per key (e.g. `login:<email>` or
//! `bootstrap`). Successful auth clears the key; repeated failures within the
//! window lock it out. In-memory and per-process — a multi-process deployment
//! shares the data dir but not this map, which is acceptable for a small
//! self-hosted tool (a hardening follow-up could back it with the filesystem).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct Bucket {
    failures: u32,
    window_start: Option<Instant>,
}

pub struct RateLimiter {
    inner: Mutex<HashMap<String, Bucket>>,
    max_failures: u32,
    window: Duration,
}

impl RateLimiter {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_failures,
            window,
        }
    }

    /// True when the key is currently locked out (too many recent failures).
    pub fn is_locked(&self, key: &str) -> bool {
        let map = self.inner.lock().expect("rate limiter lock");
        match map.get(key) {
            Some(b) => {
                let within = b.window_start.is_some_and(|t| t.elapsed() < self.window);
                within && b.failures >= self.max_failures
            }
            None => false,
        }
    }

    /// Record a failed attempt for the key, starting a fresh window if the old
    /// one elapsed.
    pub fn record_failure(&self, key: &str) {
        let mut map = self.inner.lock().expect("rate limiter lock");
        let now = Instant::now();
        let b = map.entry(key.to_string()).or_default();
        if b.window_start.is_none_or(|t| t.elapsed() >= self.window) {
            b.window_start = Some(now);
            b.failures = 0;
        }
        b.failures += 1;
    }

    /// Clear the key on a successful attempt.
    pub fn reset(&self, key: &str) {
        let mut map = self.inner.lock().expect("rate limiter lock");
        map.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_max_failures_and_resets() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        let key = "login:a@b.co";
        assert!(!rl.is_locked(key));
        rl.record_failure(key);
        rl.record_failure(key);
        assert!(!rl.is_locked(key), "2 of 3 failures");
        rl.record_failure(key);
        assert!(rl.is_locked(key), "3rd failure locks");
        rl.reset(key);
        assert!(!rl.is_locked(key), "success clears");
    }
}
