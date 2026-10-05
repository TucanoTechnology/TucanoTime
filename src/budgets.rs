//! Project budgets + overspend alerts (#30). A burn report compares tracked
//! billable time/amount against each project's optional budget; a daily job
//! notifies admins when a project crosses 80% / 100%.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::User;
use crate::domain::{Customer, Entry, Project, Task, effective_rates};
use crate::scheduler::Job;
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BudgetRow {
    pub customer: String,
    pub project: String,
    pub currency: String,
    pub budget_hours: Option<f64>,
    pub burn_hours: f64,
    pub hours_pct: Option<f64>,
    pub budget_amount_minor: Option<u64>,
    pub burn_amount_minor: u64,
    pub amount_pct: Option<f64>,
    pub over: bool,
}

fn pct(burn: f64, budget: Option<f64>) -> Option<f64> {
    budget.filter(|b| *b > 0.0).map(|b| (burn / b) * 100.0)
}

/// Burn vs budget for every project that has a budget.
pub fn burn_report(
    projects: &[(Uuid, Project)],
    entries: &[Entry],
    customers: &[Customer],
    tasks: &[Task],
    users: &[User],
) -> Vec<BudgetRow> {
    let mut rows = Vec::new();
    for (cid, p) in projects {
        if p.budget_hours.is_none() && p.budget_amount_minor.is_none() {
            continue;
        }
        let customer = customers.iter().find(|c| c.id == *cid);
        let mut burn_h = 0u64;
        let mut burn_amt = 0u64;
        for e in entries
            .iter()
            .filter(|e| &e.customer_id == cid && e.project_code == p.code && e.billable)
        {
            burn_h += u64::from(e.hours.0);
            if let Some(cust) = customer {
                let task = e.task_code.as_ref().and_then(|tc| {
                    tasks
                        .iter()
                        .find(|t| t.project_code == e.project_code && t.code == *tc)
                });
                let urate = e
                    .user_id
                    .and_then(|uid| users.iter().find(|u| u.id == uid))
                    .map(|u| u.default_rate_minor);
                let (_, rate) = effective_rates(e, cust, Some(p), task, urate);
                burn_amt += e.hours.amount_minor(rate);
            }
        }
        let burn_hours = burn_h as f64 / 100.0;
        let budget_hours = p.budget_hours.map(|h| f64::from(h) / 100.0);
        let h_pct = pct(burn_hours, budget_hours);
        let a_pct = pct(burn_amt as f64, p.budget_amount_minor.map(|a| a as f64));
        let over = h_pct.is_some_and(|v| v >= 100.0) || a_pct.is_some_and(|v| v >= 100.0);
        rows.push(BudgetRow {
            customer: customer.map(|c| c.name.clone()).unwrap_or_default(),
            project: p.code.0.clone(),
            currency: p.currency.0.clone(),
            budget_hours,
            burn_hours: (burn_hours * 100.0).round() / 100.0,
            hours_pct: h_pct.map(|v| v.round()),
            budget_amount_minor: p.budget_amount_minor,
            burn_amount_minor: burn_amt,
            amount_pct: a_pct.map(|v| v.round()),
            over,
        });
    }
    rows
}

/// Daily job: notify admins when a budgeted project crosses 80% or 100%.
pub struct BudgetAlertJob {
    store: Arc<Store>,
}

impl BudgetAlertJob {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl Job for BudgetAlertJob {
    fn name(&self) -> &str {
        "budget-alerts"
    }
    fn interval_secs(&self) -> i64 {
        24 * 3600
    }
    fn run(&self, now: DateTime<Utc>) {
        let customers = match self.store.list_customers() {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut projects: Vec<(uuid::Uuid, Project)> = Vec::new();
        let mut tasks = Vec::new();
        for c in &customers {
            if let Ok(ps) = self.store.list_projects(c.id) {
                for p in ps {
                    if let Ok(ts) = self.store.list_tasks(c.id, &p.code.0) {
                        tasks.extend(ts);
                    }
                    projects.push((c.id, p));
                }
            }
        }
        let today = now.date_naive();
        let from = today - chrono::Duration::days(3650);
        let entries = self.store.list_range(from, today).unwrap_or_default();
        let users = self.store.list_users().unwrap_or_default();
        let admins: Vec<Uuid> = users
            .iter()
            .filter(|u| u.role == crate::auth::Role::Admin && u.active)
            .map(|u| u.id)
            .collect();
        for row in burn_report(&projects, &entries, &customers, &tasks, &users) {
            let p = row
                .hours_pct
                .unwrap_or(0.0)
                .max(row.amount_pct.unwrap_or(0.0));
            if p < 80.0 {
                continue;
            }
            let level = if p >= 100.0 {
                "over budget"
            } else {
                "approaching budget"
            };
            let n = crate::domain::Notification {
                id: uuid::Uuid::new_v4(),
                kind: "budget".into(),
                title: format!("{} / {} is {}", row.customer, row.project, level),
                body: format!("{}% of budget used.", p.round() as i64),
                created_at: now,
                read: false,
            };
            for a in &admins {
                let _ = self.store.push_notification(*a, &n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Currency, Hours, ProjectCode};
    use chrono::{NaiveDate, TimeZone};

    fn proj(budget_h: Option<u32>) -> (uuid::Uuid, Project) {
        let cid = uuid::Uuid::new_v4();
        (
            cid,
            Project {
                customer_id: cid,
                code: ProjectCode::parse("P1").unwrap(),
                name: "P1".into(),
                currency: Currency("EUR".into()),
                rate_minor: 6000,
                active: true,
                budget_hours: budget_h,
                budget_amount_minor: None,
            },
        )
    }

    #[test]
    fn burn_computes_percentage() {
        let (cid, p) = proj(Some(1000)); // budget 10h
        let customer = Customer {
            id: cid,
            name: "ACME".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 6000,
            active: true,
            email: String::new(),
        };
        let entries = vec![Entry {
            id: uuid::Uuid::new_v4(),
            date: NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            customer_id: cid,
            user_id: None,
            project_code: ProjectCode::parse("P1").unwrap(),
            task_code: None,
            hours: Hours(600), // 6h
            note: String::new(),
            billable: true,
            source: crate::domain::Source::Manual,
            created_at: Utc.timestamp_opt(1, 0).unwrap(),
            updated_at: Utc.timestamp_opt(1, 0).unwrap(),
        }];
        let rows = burn_report(&[(cid, p)], &entries, &[customer], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].burn_hours, 6.0);
        assert_eq!(rows[0].hours_pct, Some(60.0));
        assert!(!rows[0].over);
    }
}
