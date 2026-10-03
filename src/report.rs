// Read-only aggregation and CSV export. Pure functions over already-loaded
// documents so the totals can be unit tested without touching disk.
//
// Amounts use each entry's effective rate (project override, else customer
// default) at read time. A future invoice snapshots these numbers; nothing in
// reports mutates stored data.

use std::collections::BTreeMap;

use chrono::Datelike;
use uuid::Uuid;

use crate::auth::User;
use crate::domain::{Customer, Entry, Task, effective_rates};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Customer,
    Project,
    Week,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SummaryRow {
    pub key: String,
    pub label: String,
    pub currency: String,
    pub hours: f64,
    pub amount_minor: u64,
    pub entries: usize,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Summary {
    pub group: String,
    pub rows: Vec<SummaryRow>,
    pub total_hours: f64,
}

fn group_key(entry: &Entry, kind: Group, customer_name: &str, code: &str) -> (String, String) {
    match kind {
        Group::Customer => (entry.customer_id.to_string(), customer_name.to_owned()),
        Group::Project => (
            format!("{}:{code}", entry.customer_id),
            format!("{customer_name} / {code}"),
        ),
        Group::Week => {
            let iso = entry.date.iso_week();
            (
                format!("{}-W{:02}", iso.year(), iso.week()),
                format!("{}-W{:02}", iso.year(), iso.week()),
            )
        }
    }
}

pub fn summarise(
    entries: &[Entry],
    customers: &[Customer],
    projects: &[(Uuid, crate::domain::Project)],
    tasks: &[Task],
    users: &[User],
    kind: Group,
) -> Summary {
    let customer_by_id: BTreeMap<Uuid, &Customer> = customers.iter().map(|c| (c.id, c)).collect();
    let project_for = |e: &Entry| -> Option<&crate::domain::Project> {
        projects
            .iter()
            .find(|(cid, p)| *cid == e.customer_id && p.code == e.project_code)
            .map(|(_, p)| p)
    };
    let task_for = |e: &Entry| -> Option<&Task> {
        let tc = e.task_code.as_ref()?;
        tasks.iter().find(|t| {
            t.customer_id == e.customer_id && t.project_code == e.project_code && t.code == *tc
        })
    };
    let user_rate = |e: &Entry| -> Option<u64> {
        e.user_id
            .and_then(|uid| users.iter().find(|u| u.id == uid))
            .map(|u| u.default_rate_minor)
    };

    // Rows keyed by the group key; a currency is fixed per row by grouping
    // currency into the key when it can vary within one logical group (a
    // single customer/project always carries one currency, so grouping by
    // customer or project never mixes currencies; weeks can).
    let mut acc: BTreeMap<(String, String), AccRow> = BTreeMap::new();
    for e in entries {
        let Some(customer) = customer_by_id.get(&e.customer_id) else {
            continue; // dangling reference: excluded from totals, never fabricated.
        };
        let project = project_for(e);
        let (currency, rate) = effective_rates(e, customer, project, task_for(e), user_rate(e));
        let label_customer = &customer.name;
        let (mut key, label) = group_key(e, kind, label_customer, &e.project_code.0);
        if kind == Group::Week {
            key = format!("{key}|{}", currency.0);
        }
        let row = acc.entry((key.clone(), label)).or_insert_with(|| AccRow {
            currency: currency.0.clone(),
            hours: 0.0,
            amount: 0,
            entries: 0,
        });
        row.hours += e.hours.0 as f64 / 100.0;
        row.amount += e.hours.amount_minor(rate);
        row.entries += 1;
    }

    let mut rows: Vec<SummaryRow> = acc
        .into_iter()
        .map(|((key, label), r)| SummaryRow {
            key,
            label,
            currency: r.currency,
            hours: round2(r.hours),
            amount_minor: r.amount,
            entries: r.entries,
        })
        .collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));

    Summary {
        group: group_name(kind).to_owned(),
        rows,
        total_hours: round2(entries.iter().map(|e| e.hours.0 as f64 / 100.0).sum()),
    }
}

#[derive(Debug)]
struct AccRow {
    currency: String,
    hours: f64,
    amount: u64,
    entries: usize,
}

fn group_name(kind: Group) -> &'static str {
    match kind {
        Group::Customer => "customer",
        Group::Project => "project",
        Group::Week => "week",
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// CSV export: one row per entry with the rate and amount that a report shows.
pub fn export_csv(
    entries: &[Entry],
    customers: &[Customer],
    projects: &[(Uuid, crate::domain::Project)],
    tasks: &[Task],
    users: &[User],
    customer_filter: Option<Uuid>,
) -> String {
    let customer_by_id: BTreeMap<Uuid, &Customer> = customers.iter().map(|c| (c.id, c)).collect();
    let project_for = |e: &Entry| -> Option<&crate::domain::Project> {
        projects
            .iter()
            .find(|(cid, p)| *cid == e.customer_id && p.code == e.project_code)
            .map(|(_, p)| p)
    };
    let task_for = |e: &Entry| -> Option<&Task> {
        let tc = e.task_code.as_ref()?;
        tasks.iter().find(|t| {
            t.customer_id == e.customer_id && t.project_code == e.project_code && t.code == *tc
        })
    };
    let user_rate = |e: &Entry| -> Option<u64> {
        e.user_id
            .and_then(|uid| users.iter().find(|u| u.id == uid))
            .map(|u| u.default_rate_minor)
    };

    let mut out = String::new();
    out.push_str(
        "date,customer,project_code,task_code,hours,billable,currency,hourly_rate,amount,note\n",
    );
    for e in entries {
        if let Some(fid) = customer_filter
            && e.customer_id != fid
        {
            continue;
        }
        let Some(customer) = customer_by_id.get(&e.customer_id) else {
            continue;
        };
        let project = project_for(e);
        let (currency, rate) = effective_rates(e, customer, project, task_for(e), user_rate(e));
        let hours = e.hours.0 as f64 / 100.0;
        let amount = e.hours.amount_minor(rate);
        let fields = [
            e.date.format("%Y-%m-%d").to_string(),
            customer.name.clone(),
            e.project_code.0.clone(),
            e.task_code
                .as_ref()
                .map(|c| c.0.clone())
                .unwrap_or_default(),
            format!("{:.2}", hours),
            if e.billable {
                "yes".to_string()
            } else {
                "no".to_string()
            },
            currency.0.clone(),
            format!("{:.2}", rate as f64 / 100.0),
            format!("{:.2}", amount as f64 / 100.0),
            e.note.clone(),
        ];
        let joined = fields
            .iter()
            .map(|f| csv_escape(f))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&joined);
        out.push('\n');
    }
    out
}

/// Quote a CSV field and defuse spreadsheet formula injection (a leading
/// `=`, `+`, `-`, `@`, tab or CR becomes a text prefix).
fn csv_escape(field: &str) -> String {
    let mut s = field.to_owned();
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        s = format!("'{s}");
    }
    if s.contains(['"', ',', '\n', '\r']) {
        s = format!("\"{}\"", s.replace('"', "\"\""));
    }
    s
}
