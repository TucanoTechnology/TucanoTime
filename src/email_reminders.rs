//! Overdue-payment reminder job (#35). A daily scheduler job finds issued,
//! unpaid invoices past their due date and emails the customer a reminder,
//! honouring a configurable cadence (days between reminders per invoice). The
//! per-invoice "last reminded" date is persisted under the data dir so a
//! restart does not re-spam. Sending is behind the `EmailSender` port, so tests
//! inject a recorder and nothing is emailed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};

use crate::domain::Invoice;
use crate::email::{EmailMessage, EmailSender, due_for_reminder, render_reminder_email};
use crate::scheduler::Job;
use crate::store::Store;

pub struct EmailReminderJob {
    store: Arc<Store>,
    sender: Arc<dyn EmailSender>,
    cadence_days: i64,
    state_path: PathBuf,
    /// Issuing organisation, resolved from config at boot (#113).
    org: crate::pdf::Org,
}

impl EmailReminderJob {
    pub fn new(
        store: Arc<Store>,
        sender: Arc<dyn EmailSender>,
        cadence_days: i64,
        root: &Path,
        org: crate::pdf::Org,
    ) -> Self {
        Self {
            store,
            sender,
            cadence_days,
            state_path: root.join("email_reminders.json"),
            org,
        }
    }

    fn load_state(&self) -> HashMap<String, NaiveDate> {
        std::fs::read_to_string(&self.state_path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| {
                        NaiveDate::parse_from_str(&v, "%Y-%m-%d")
                            .ok()
                            .map(|d| (k, d))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_state(&self, state: &HashMap<String, NaiveDate>) {
        let serial: HashMap<String, String> = state
            .iter()
            .map(|(k, d)| (k.clone(), d.format("%Y-%m-%d").to_string()))
            .collect();
        // tmp+rename (review B7): concurrent backups must not see truncation.
        let tmp = self.state_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&serial).unwrap_or_default();
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.state_path);
        }
    }

    /// Pure selection: overdue invoices due for a reminder, given the map of
    /// last-reminded dates. Exposed for tests.
    pub fn plan(
        invoices: &[Invoice],
        state: &HashMap<String, NaiveDate>,
        today: NaiveDate,
        cadence_days: i64,
    ) -> Vec<(Invoice, i64)> {
        invoices
            .iter()
            .filter_map(|inv| {
                // #114: reminders chase the BALANCE — written-off and fully
                // paid invoices are out, partly paid ones are in.
                if !inv.status.is_open() || inv.balance_minor() == 0 {
                    return None;
                }
                let due = inv.due_date?;
                if due >= today {
                    return None;
                }
                let last = state.get(&inv.id.to_string()).copied();
                if due_for_reminder(last, today, cadence_days) {
                    Some((inv.clone(), (today - due).num_days()))
                } else {
                    None
                }
            })
            .collect()
    }
    /// The archived (or lazily-rendered) PDF for a reminder email. A store
    /// error must not drop the reminder: on failure we send without the
    /// attachment, exactly as the pre-#113 path did.
    fn resolve_pdf(
        &self,
        inv: &Invoice,
        customer: &crate::domain::Customer,
        now: DateTime<Utc>,
    ) -> Option<(String, Vec<u8>)> {
        match self.store.invoice_pdf_bytes(inv.id) {
            Ok(Some(bytes)) => Some((
                inv.pdf
                    .as_ref()
                    .map(|h| h.filename.clone())
                    .unwrap_or_else(|| format!("{}.pdf", inv.number)),
                bytes,
            )),
            Ok(None) => {
                // Same content source as the API lazy path (#116): current
                // template + customer notes merged into the doc.
                let template = self
                    .store
                    .read_json_rel::<crate::domain::InvoiceTemplate>("invoice_template.json")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let mut doc = crate::pdf::doc_for(inv, customer, &self.org);
                let vars = crate::template::vars_for(
                    inv,
                    &customer.name,
                    Some(inv.issued_at.unwrap_or(inv.created_at).date_naive()),
                );
                let content = crate::template::doc_content(&template, customer, &vars);
                crate::template::apply_doc_content(&mut doc, &content);
                let bytes = crate::pdf::render_invoice_pdf(&doc);
                let filename = match self.store.attach_invoice_pdf(inv.id, &bytes, now) {
                    Ok(updated) => updated
                        .pdf
                        .map(|h| h.filename)
                        .unwrap_or_else(|| format!("{}.pdf", inv.number)),
                    Err(_) => format!("{}.pdf", inv.number),
                };
                Some((filename, bytes))
            }
            Err(e) => {
                tracing::warn!(invoice = %inv.number, error = %e, "reminder pdf read failed");
                None
            }
        }
    }
}

impl Job for EmailReminderJob {
    fn name(&self) -> &str {
        "email-reminders"
    }
    fn interval_secs(&self) -> i64 {
        24 * 3600 // daily
    }
    fn run(&self, now: DateTime<Utc>) {
        let today = now.date_naive();
        let invoices = match self.store.list_invoices() {
            Ok(v) => v,
            Err(_) => return,
        };
        let customers = match self.store.list_customers() {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut state = self.load_state();
        for (inv, days_over) in Self::plan(&invoices, &state, today, self.cadence_days) {
            let Some(customer) = customers.iter().find(|c| c.id == inv.customer_id) else {
                continue;
            };
            // #139: prefer the billing contact, fall back to the top-level email.
            let billing_email = customer.billing_email().to_string();
            if billing_email.is_empty() {
                continue;
            }
            // Resolve the PDF to attach (#113): the archived bytes when the
            // invoice has them, else render from the snapshot-locked document
            // and archive on first sight. The reminder must carry the same
            // document the customer was issued, never a freshly derived one.
            let attachment = self.resolve_pdf(&inv, customer, now);
            let amount = crate::api::money_for_email(inv.balance_minor(), &inv.currency.0);
            let due = inv.due_date.map(|d| d.to_string()).unwrap_or_default();
            let text = render_reminder_email(
                &customer.name,
                &inv.number,
                &amount,
                &due,
                days_over,
                &self.org.name,
                attachment.is_some(),
            );
            let msg = EmailMessage {
                to: billing_email,
                subject: format!("Overdue invoice {}", inv.number),
                text,
                html: None,
                attachment,
            };
            match self.sender.send(&msg) {
                Ok(()) => {
                    state.insert(inv.id.to_string(), today);
                }
                Err(e) => tracing::warn!(invoice = %inv.number, error = %e, "reminder send failed"),
            }
        }
        self.save_state(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Currency, Invoice, InvoiceStatus};
    use chrono::TimeZone;

    fn issued_due(id: &str, due: NaiveDate, status: InvoiceStatus) -> Invoice {
        Invoice {
            id: id.parse().unwrap(),
            number: format!("INV-{id}"),
            customer_id: uuid::Uuid::new_v4(),
            currency: Currency("EUR".into()),
            period_from: due,
            period_to: due,
            lines: vec![],
            total_minor: 10000,
            status,
            created_at: Utc.timestamp_opt(1, 0).unwrap(),
            issued_at: None,
            due_date: Some(due),
            paid_at: None,
            payment_reference: String::new(),
            pdf: None,
            payments: vec![],
            write_off_reason: String::new(),
            written_off_at: None,
        }
    }

    #[test]
    fn plan_selects_only_overdue_and_unreminded() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let due = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(); // 9 days over
        let not_due = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let invoices = vec![
            issued_due(
                "00000000-0000-0000-0000-000000000001",
                due,
                InvoiceStatus::Issued,
            ),
            issued_due(
                "00000000-0000-0000-0000-000000000002",
                not_due,
                InvoiceStatus::Issued,
            ),
            issued_due(
                "00000000-0000-0000-0000-000000000003",
                due,
                InvoiceStatus::Paid,
            ),
        ];
        let mut state = HashMap::new();
        // Invoice 1 reminded 3 days ago -> not due at 7-day cadence.
        state.insert(
            invoices[0].id.to_string(),
            NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
        );
        let plan = EmailReminderJob::plan(&invoices, &state, today, 7);
        assert!(plan.is_empty()); // 1 gated by cadence, 2 not overdue, 3 paid

        // Clear the recent reminder -> invoice 1 becomes due.
        state.remove(&invoices[0].id.to_string());
        let plan = EmailReminderJob::plan(&invoices, &state, today, 7);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].1, 9);
    }
}
