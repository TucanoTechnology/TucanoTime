//! In-process scheduler (#61). A background tokio task ticks periodically and
//! runs due jobs; a job's last-run time is persisted under the data dir so a
//! restart does not double-fire. Jobs (reminders #22, recurring invoices #26,
//! budget alerts #30) implement `Job` and are registered at startup.
//!
//! The scheduling decision is a pure `tick(now)` so it is deterministic under
//! test with a `FixedClock` / explicit timestamps.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};

pub trait Job: Send + Sync {
    fn name(&self) -> &str;
    fn interval_secs(&self) -> i64;
    fn run(&self, now: DateTime<Utc>);
}

pub struct Scheduler {
    jobs: Vec<std::sync::Arc<dyn Job>>,
    path: PathBuf,
    last: Mutex<HashMap<String, i64>>,
}

impl Scheduler {
    pub fn new(root: &Path, jobs: Vec<std::sync::Arc<dyn Job>>) -> Self {
        let path = root.join("scheduler.json");
        let last = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, i64>>(&s).ok())
            .unwrap_or_default();
        Self {
            jobs,
            path,
            last: Mutex::new(last),
        }
    }

    /// Run every job whose interval has elapsed since its last run, then
    /// persist the updated schedule. Idempotent for a given `now`.
    pub fn tick(&self, now: DateTime<Utc>) {
        let now_ts = now.timestamp();
        let mut fired: Vec<String> = Vec::new();
        {
            let mut last = lock(&self.last);
            for job in &self.jobs {
                let prev = last.get(job.name()).copied().unwrap_or(i64::MIN / 2);
                if now_ts - prev >= job.interval_secs() {
                    last.insert(job.name().to_string(), now_ts);
                    fired.push(job.name().to_string());
                }
            }
            let _ = fs::write(&self.path, serde_json::to_vec(&*last).unwrap_or_default());
        }
        for name in fired {
            if let Some(job) = self.jobs.iter().find(|j| j.name() == name) {
                job.run(now);
            }
        }
    }

    /// Spawn the periodic loop (checks every `poll_secs`). No-op if there are
    /// no jobs, but still cheap.
    pub fn spawn(self: std::sync::Arc<Self>, poll_secs: u64) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
                self.tick(Utc::now());
            }
        });
    }
}

fn lock(m: &Mutex<HashMap<String, i64>>) -> MutexGuard<'_, HashMap<String, i64>> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct CountingJob {
        name: &'static str,
        interval: i64,
        count: AtomicU32,
    }
    impl Job for CountingJob {
        fn name(&self) -> &str {
            self.name
        }
        fn interval_secs(&self) -> i64 {
            self.interval
        }
        fn run(&self, _now: DateTime<Utc>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn runs_on_interval_not_before() {
        let dir = tempfile::tempdir().unwrap();
        let job = std::sync::Arc::new(CountingJob {
            name: "j",
            interval: 3600,
            count: AtomicU32::new(0),
        });
        let sched = Scheduler::new(dir.path(), vec![job.clone()]);
        sched.tick(t(0)); // first run
        sched.tick(t(1800)); // half interval -> no run
        assert_eq!(job.count.load(Ordering::SeqCst), 1);
        sched.tick(t(3600)); // full interval -> run
        assert_eq!(job.count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn last_run_persists_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let job = std::sync::Arc::new(CountingJob {
            name: "j",
            interval: 3600,
            count: AtomicU32::new(0),
        });
        Scheduler::new(dir.path(), vec![job.clone()]).tick(t(0));
        assert_eq!(job.count.load(Ordering::SeqCst), 1);
        // A fresh scheduler reads persisted last-run and won't re-fire immediately.
        let job2 = std::sync::Arc::new(CountingJob {
            name: "j",
            interval: 3600,
            count: AtomicU32::new(0),
        });
        Scheduler::new(dir.path(), vec![job2.clone()]).tick(t(60));
        assert_eq!(
            job2.count.load(Ordering::SeqCst),
            0,
            "should not double-fire after restart"
        );
    }
}
