//! Reminders (#22). A daily scheduler job checks each user and pushes
//! notifications: "no time logged today" and (on Fridays) "submit your
//! timesheet". The decision is a pure function so it is unit-testable.

use std::sync::Arc;

use chrono::{Datelike, NaiveDate, Weekday};

use crate::domain::Notification;
use crate::scheduler::Job;
use crate::store::Store;

/// The reminder decisions given what happened for a user on `now`'s day/week.
pub fn decide(
    now: NaiveDate,
    has_entries_today: bool,
    week_has_entries: bool,
    week_submitted: bool,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut out = Vec::new();
    if !has_entries_today {
        out.push((
            "no_time_today",
            "No time logged today",
            "You haven't recorded any time yet today.",
        ));
    }
    if now.weekday() == Weekday::Fri && week_has_entries && !week_submitted {
        out.push((
            "submit_timesheet",
            "Submit your timesheet",
            "It's Friday — submit this week's timesheet for approval.",
        ));
    }
    out
}

pub fn monday_of(d: NaiveDate) -> NaiveDate {
    let offset = (d.weekday().num_days_from_monday()) as i64;
    d - chrono::Duration::days(offset)
}

pub struct ReminderJob {
    store: Arc<Store>,
}

impl ReminderJob {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl Job for ReminderJob {
    fn name(&self) -> &str {
        "reminders"
    }
    fn interval_secs(&self) -> i64 {
        24 * 3600 // daily
    }
    fn run(&self, now: chrono::DateTime<chrono::Utc>) {
        let Ok(users) = self.store.list_users() else {
            return;
        };
        let today = now.date_naive();
        let week_start = monday_of(today);
        let week_end = week_start + chrono::Duration::days(6);
        let week_entries = self
            .store
            .list_range(week_start, week_end)
            .unwrap_or_default();
        let submissions = self.store.list_submissions().unwrap_or_default();
        for u in users.iter().filter(|u| u.active) {
            let has_today = week_entries
                .iter()
                .any(|e| e.user_id == Some(u.id) && e.date == today);
            let week_has = week_entries.iter().any(|e| e.user_id == Some(u.id));
            let submitted = submissions.iter().any(|s| {
                s.user_id == u.id
                    && s.week_start == week_start
                    && matches!(
                        s.state,
                        crate::domain::SubmissionState::Submitted
                            | crate::domain::SubmissionState::Approved
                    )
            });
            for (kind, title, body) in decide(today, has_today, week_has, submitted) {
                let n = Notification {
                    id: uuid::Uuid::new_v4(),
                    kind: kind.to_string(),
                    title: title.to_string(),
                    body: body.to_string(),
                    created_at: now,
                    read: false,
                };
                let _ = self.store.push_notification(u.id, &n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn no_time_triggers_daily() {
        let r = decide(date(2026, 10, 1), false, true, true); // Thu
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "no_time_today");
    }

    #[test]
    fn friday_submission_reminder_only_when_needed() {
        let fri = date(2026, 10, 2); // a Friday
        // has time today, week has entries, not submitted -> submit reminder
        let r = decide(fri, true, true, false);
        assert!(r.iter().any(|x| x.0 == "submit_timesheet"));
        // already submitted -> no submit reminder
        let r2 = decide(fri, true, true, true);
        assert!(!r2.iter().any(|x| x.0 == "submit_timesheet"));
    }

    #[test]
    fn monday_of_is_correct() {
        assert_eq!(monday_of(date(2026, 10, 2)), date(2026, 9, 28)); // Fri -> Mon
        assert_eq!(monday_of(date(2026, 10, 5)), date(2026, 10, 5)); // already Monday
    }
}

#[cfg(test)]
mod job_tests {
    use super::*;
    use crate::auth::{Role, User};
    use chrono::{TimeZone, Utc};

    #[test]
    fn job_pushes_no_time_reminder() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data")).unwrap());
        let user = User {
            id: uuid::Uuid::new_v4(),
            name: "A".into(),
            email: "a@b.co".into(),
            role: Role::Member,
            active: true,
            default_rate_minor: 0,
            cost_rate_minor: 0,
            password_hash: String::new(),
            created_at: Utc::now(),
            session_version: 1,
        };
        store.put_user(&user).unwrap();
        let now = Utc.timestamp_opt(1_730_000_000, 0).unwrap(); // a weekday
        ReminderJob::new(store.clone()).run(now);
        let notes = store.list_notifications(user.id).unwrap();
        assert!(notes.iter().any(|n| n.kind == "no_time_today"), "{notes:?}");
    }
}
