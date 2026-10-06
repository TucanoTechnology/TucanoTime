//! Recurring invoices & retainers (#26). A daily scheduler job checks each
//! active schedule; when due it generates a DRAFT invoice — either from the
//! period's tracked time + expenses, or a fixed retainer amount — and advances
//! the schedule's `last_period_end`. Drafts are not auto-issued (a human
//! reviews and issues them).

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::{
    Cadence, Invoice, InvoiceLine, InvoiceStatus, LineKind, RecurMode, RecurringSchedule,
    due_period, generate_invoice,
};
use crate::scheduler::Job;
use crate::store::Store;

pub struct RecurringJob {
    store: Arc<Store>,
}

impl RecurringJob {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// `Ok(None)` = nothing to bill this period (legitimate advance);
    /// `Err` = transient/structural failure, period must be retried.
    fn build_invoice(
        &self,
        schedule: &RecurringSchedule,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<Option<Invoice>, String> {
        let customer = self
            .store
            .get_customer(schedule.customer_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("customer {} missing", schedule.customer_id))?;
        match schedule.mode {
            RecurMode::Retainer => {
                let line = InvoiceLine {
                    kind: LineKind::Fixed,
                    date: to,
                    entry_id: None,
                    expense_id: None,
                    project_code: None,
                    task_code: None,
                    hours: None,
                    rate_minor: None,
                    amount_minor: schedule.retainer_amount_minor,
                    note: format!("{} retainer", cadence_label(schedule.cadence)),
                    quantity_hundredths: None,
                    unit_price_minor: None,
                    item_kind: None,
                };
                Ok(Some(Invoice {
                    id: uuid::Uuid::new_v4(),
                    number: String::new(),
                    customer_id: customer.id,
                    currency: schedule.currency.clone(),
                    period_from: from,
                    period_to: to,
                    total_minor: schedule.retainer_amount_minor,
                    lines: vec![line],
                    status: InvoiceStatus::Draft,
                    created_at: now,
                    issued_at: None,
                    due_date: None,
                    paid_at: None,
                    payment_reference: String::new(),
                    pdf: None,
                    payments: vec![],
                    write_off_reason: String::new(),
                    written_off_at: None,
                    tax_hundredths: 0,
                    discount_hundredths: 0,
                }))
            }
            RecurMode::Time => {
                let projects = self
                    .store
                    .list_projects(customer.id)
                    .map_err(|e| e.to_string())?;
                let mut tasks = Vec::new();
                for p in &projects {
                    if let Ok(ts) = self.store.list_tasks(customer.id, &p.code.0) {
                        tasks.extend(ts);
                    }
                }
                let users = self.store.list_users().map_err(|e| e.to_string())?;
                let entries = self.store.list_range(from, to).map_err(|e| e.to_string())?;
                let expenses = self.store.list_expenses().map_err(|e| e.to_string())?;
                let all_invoices = self.store.list_invoices().map_err(|e| e.to_string())?;
                let issued: Vec<&Invoice> = all_invoices
                    .iter()
                    .filter(|i| i.status == InvoiceStatus::Issued)
                    .collect();
                let excluded_entries: Vec<_> = issued
                    .iter()
                    .flat_map(|i| i.lines.iter())
                    .filter_map(|l| l.entry_id)
                    .collect();
                let excluded_expenses: Vec<_> = issued
                    .iter()
                    .flat_map(|i| i.lines.iter())
                    .filter_map(|l| l.expense_id)
                    .collect();
                let sources = crate::domain::InvoiceSources {
                    projects: &projects,
                    tasks: &tasks,
                    users: &users,
                    entries: &entries,
                    expenses: &expenses,
                    excluded_entries: &excluded_entries,
                    excluded_expenses: &excluded_expenses,
                    include_expenses: true,
                };
                match generate_invoice(String::new(), &customer, &sources, from, to, now) {
                    Ok(inv) => Ok(Some(inv)),
                    // Nothing billable is a normal empty period: advance.
                    Err(crate::domain::InvoiceError::NothingToInvoice) => Ok(None),
                    // Anything else (mixed currencies, store read) must not
                    // silently consume the period (review B2).
                    Err(e) => Err(format!("{e:?}")),
                }
            }
        }
    }
}

fn cadence_label(c: Cadence) -> &'static str {
    match c {
        Cadence::Weekly => "Weekly",
        Cadence::Monthly => "Monthly",
        Cadence::Quarterly => "Quarterly",
    }
}

impl Job for RecurringJob {
    fn name(&self) -> &str {
        "recurring-invoices"
    }
    fn interval_secs(&self) -> i64 {
        24 * 3600
    }
    fn run(&self, now: DateTime<Utc>) {
        let today = now.date_naive();
        let Ok(schedules) = self.store.list_schedules() else {
            return;
        };
        for s in schedules.iter().filter(|s| s.active) {
            let Some((from, to)) = due_period(s.cadence, s.last_period_end, today) else {
                continue;
            };
            // Review B2: advance only on outcomes we understand, and write
            // invoice+schedule under one lock so a failure can never
            // double-bill or silently skip a period.
            let mut advanced = s.clone();
            advanced.last_period_end = Some(to);
            match self.build_invoice(s, from, to, now) {
                Ok(invoice) => {
                    if let Some(invoice) = invoice {
                        match self.store.create_invoice_and_advance(invoice, &advanced) {
                            Ok(inv) => {
                                tracing::info!(number = %inv.number, "recurring: drafted invoice")
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, schedule = %s.id, "recurring: invoice+schedule write failed; period retried next run");
                                continue; // do NOT advance
                            }
                        }
                    } else {
                        // Nothing to bill this period: advance alone.
                        let _ = self.store.put_schedule(&advanced);
                    }
                }
                Err(e) => {
                    // Transient build failure (store read, mixed currency):
                    // leave the schedule where it is so the period retries.
                    tracing::warn!(error = %e, schedule = %s.id, "recurring: build failed; period not skipped");
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RecurringSchedule;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn due_period_weekly() {
        // Never run -> a 7-day period ending today.
        let p = due_period(Cadence::Weekly, None, date(2026, 10, 7)).unwrap();
        assert_eq!(p, (date(2026, 10, 1), date(2026, 10, 7)));
        // Ran 3 days ago -> not due.
        assert!(due_period(Cadence::Weekly, Some(date(2026, 10, 4)), date(2026, 10, 7)).is_none());
        // Ran 7 days ago -> due, period from day after last.
        let p2 = due_period(Cadence::Weekly, Some(date(2026, 9, 30)), date(2026, 10, 7)).unwrap();
        assert_eq!(p2, (date(2026, 10, 1), date(2026, 10, 7)));
    }

    #[test]
    fn retainer_job_generates_draft_invoice() {
        use crate::domain::{Currency, Customer};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data")).unwrap());
        let cust = Customer {
            id: uuid::Uuid::new_v4(),
            name: "ACME".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 6000,
            active: true,
            email: String::new(),
            payment_terms: None,
            invoice_notes: String::new(),
            invoice_subject: String::new(),
            address: None,
            contacts: vec![],
            tax_hundredths: 0,
            discount_hundredths: 0,
        };
        store.put_customer(&cust).unwrap();
        let sched = RecurringSchedule {
            id: uuid::Uuid::new_v4(),
            customer_id: cust.id,
            cadence: Cadence::Monthly,
            mode: RecurMode::Retainer,
            retainer_amount_minor: 150000,
            currency: Currency("EUR".into()),
            active: true,
            last_period_end: None,
            created_at: Utc::now(),
        };
        store.put_schedule(&sched).unwrap();
        let now = Utc::now();
        RecurringJob::new(store.clone()).run(now);
        let invoices = store.list_invoices().unwrap();
        assert_eq!(invoices.len(), 1);
        assert_eq!(invoices[0].total_minor, 150000);
        assert_eq!(invoices[0].lines[0].kind, LineKind::Fixed);
        // Schedule advanced so it is not due again immediately.
        let s2 = store.get_schedule(sched.id).unwrap().unwrap();
        assert!(s2.last_period_end.is_some());
    }
}
