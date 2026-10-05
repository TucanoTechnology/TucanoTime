//! Accounting sync (#33). Invoices and payments are copied to QuickBooks
//! Online / Xero through the `AccountingSync` port (the #18 seam family).
//!
//! Live provider calls are deliberately out of scope for this repo's CI:
//! adapters build the provider-shaped document and hand it to a `Transport`
//! trait. Production wires `HttpTransport` (ureq + OAuth bearer from the
//! vault, #77); tests inject recording/failing stubs. Sync is **idempotent**
//! (keyed by provider + invoice; the same invoice maps to a stable remote
//! reference), **retryable** (failures are recorded, a daily job re-pushes
//! them), and **non-blocking** (a failed sync never fails the invoice flow —
//! status is visible through `GET /sync/accounting`).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{Customer, Invoice};
use crate::store::Store;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("provider transport failed: {0}")]
    Transport(String),
    #[error("unknown accounting provider")]
    UnknownProvider,
}

/// The facts an accounting provider needs about one invoice.
#[derive(Debug, Clone)]
pub struct InvoiceDoc<'a> {
    pub invoice: &'a Invoice,
    pub customer: &'a Customer,
}

/// Low-level POST of a JSON document; returns the provider's object id.
pub trait Transport: Send + Sync {
    fn post(&self, path: &str, body: serde_json::Value) -> Result<String, SyncError>;
}

pub trait AccountingSync: Send + Sync {
    fn name(&self) -> &'static str;
    /// Creates (or, idempotently, re-delivers) the invoice. `suggested_id` is
    /// a deterministic uuid derived from our invoice number so a retry — or a
    /// second sync of the same invoice — cannot create a duplicate.
    fn push_invoice(&self, doc: &InvoiceDoc, suggested_id: &str) -> Result<String, SyncError>;
    fn push_payment(
        &self,
        remote_invoice_id: &str,
        amount_minor: u64,
        currency: &str,
        reference: &str,
        paid_at: DateTime<Utc>,
    ) -> Result<String, SyncError>;
}

// ---------------------------------------------------------------- registry --

/// Registry of enabled providers (#101: shared generic `Registry`).
pub type AccountingRegistry = crate::providers::Registry<dyn AccountingSync>;

impl crate::providers::NamedProvider for dyn AccountingSync {
    fn name(&self) -> &str {
        AccountingSync::name(self)
    }
}

/// Builds the registry from vault/env/config (#94 precedence): a provider is
/// enabled when its OAuth token exists (`qbo.token` / `xero.token`, or
/// `TUCANO_QBO_TOKEN` / `TUCANO_XERO_TOKEN`); base URLs resolve as
/// env > config.json > the providers' public APIs.
pub fn registry_from_vault(
    vault: Option<&crate::vault::SecretVault>,
    cfg: &crate::appconfig::AppConfig,
) -> Arc<AccountingRegistry> {
    let mut providers: Vec<Arc<dyn AccountingSync>> = Vec::new();
    if let Some(token) = crate::providers::resolve_secret(vault, "qbo.token", "TUCANO_QBO_TOKEN") {
        let base = crate::providers::resolve_setting(
            cfg,
            vault,
            "qbo_base_url",
            "qbo.base_url",
            "TUCANO_QBO_BASE_URL",
        )
        .unwrap_or_else(|| "https://quickbooks.api.intuit.com/v3/company/default".into());
        providers.push(Arc::new(QboProvider::new(Arc::new(HttpTransport {
            base_url: base,
            bearer: token,
        }))));
    }
    if let Some(token) = crate::providers::resolve_secret(vault, "xero.token", "TUCANO_XERO_TOKEN")
    {
        let base = crate::providers::resolve_setting(
            cfg,
            vault,
            "xero_base_url",
            "xero.base_url",
            "TUCANO_XERO_BASE_URL",
        )
        .unwrap_or_else(|| "https://api.xero.com/api.xro/4.0".into());
        providers.push(Arc::new(XeroProvider::new(Arc::new(HttpTransport {
            base_url: base,
            bearer: token,
        }))));
    }
    Arc::new(AccountingRegistry::new(providers))
}

/// Deterministic idempotency key for one invoice on one provider.
pub fn invoice_key(provider: &str, number: &str) -> String {
    format!("{provider}:{number}")
}

// ------------------------------------------------------------------ state --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncStatus {
    Synced,
    Failed,
}

/// One recorded sync attempt outcome (the visible status surface).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRecord {
    pub provider: String,
    /// "invoice" or "payment".
    pub kind: String,
    pub invoice_id: Uuid,
    pub invoice_number: String,
    pub remote_id: String,
    pub status: SyncStatus,
    #[serde(default)]
    pub error: String,
    pub attempts: u32,
    pub updated_at: DateTime<Utc>,
    /// Sub-key for `kind == "payment"` records: the ledger payment id
    /// (#114), so each recorded payment syncs with its real amount. Empty
    /// for invoice records and for the single legacy payment.
    #[serde(default)]
    pub detail: String,
}

// -------------------------------------------------------------- providers --

/// QuickBooks Online adapter: builds a QBO `Invoice` / `Payment` document.
pub struct QboProvider {
    transport: Arc<dyn Transport>,
}

/// Xero adapter: builds a Xero `Invoice` / `Payment` document.
pub struct XeroProvider {
    transport: Arc<dyn Transport>,
}

impl QboProvider {
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

impl XeroProvider {
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

/// QBO money is a plain decimal string.
fn qbo_amount(minor: u64) -> String {
    format!("{:.2}", minor as f64 / 100.0)
}

fn doc_amount(inv: &Invoice) -> f64 {
    inv.total_minor as f64 / 100.0
}

impl AccountingSync for QboProvider {
    fn name(&self) -> &'static str {
        "qbo"
    }
    fn push_invoice(&self, doc: &InvoiceDoc, suggested_id: &str) -> Result<String, SyncError> {
        let body = serde_json::json!({
            "IdDoc": {
                "TxnDate": doc.invoice.period_to.format("%Y-%m-%d").to_string(),
                "DueDate": doc.invoice.due_date.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                "DocNumber": doc.invoice.number,
                "CustomerRef": { "name": doc.customer.name },
                "CurrencyRef": { "value": doc.invoice.currency.0 },
                "TotalAmt": doc_amount(doc.invoice),
            },
            "Line": doc.invoice.lines.iter().map(|l| serde_json::json!({
                "Amount": l.amount_minor as f64 / 100.0,
                "Description": if l.note.is_empty() { l.project_code.as_ref().map(|c| c.0.clone()).unwrap_or_default() } else { l.note.clone() },
            })).collect::<Vec<_>>(),
            "synchronized_id": suggested_id,
        });
        self.transport.post("invoice", body)
    }
    fn push_payment(
        &self,
        remote_invoice_id: &str,
        amount_minor: u64,
        currency: &str,
        reference: &str,
        paid_at: DateTime<Utc>,
    ) -> Result<String, SyncError> {
        let body = serde_json::json!({
            "Payment": {
                "TxnDate": paid_at.format("%Y-%m-%d").to_string(),
                "TotalAmt": qbo_amount(amount_minor),
                "CurrencyRef": { "value": currency },
                "PaymentRefNum": reference,
                "LinkedTxn": [{ "TxnId": remote_invoice_id }],
            }
        });
        self.transport.post("payment", body)
    }
}

impl AccountingSync for XeroProvider {
    fn name(&self) -> &'static str {
        "xero"
    }
    fn push_invoice(&self, doc: &InvoiceDoc, suggested_id: &str) -> Result<String, SyncError> {
        let body = serde_json::json!({
            "Invoices": [{
                "Type": "ACCREC", // sales invoice (review B9: ACCPAY is a bill)
                "InvoiceNumber": doc.invoice.number,
                "ContactName": doc.customer.name,
                "Date": doc.invoice.period_to.format("%Y-%m-%d").to_string(),
                "DueDate": doc.invoice.due_date.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                "CurrencyCode": doc.invoice.currency.0,
                "Total": doc_amount(doc.invoice),
                "LineItems": doc.invoice.lines.iter().map(|l| serde_json::json!({
                    "Description": if l.note.is_empty() { "Services".to_string() } else { l.note.clone() },
                    "LineAmount": l.amount_minor as f64 / 100.0,
                })).collect::<Vec<_>>(),
            }],
            "synchronized_id": suggested_id,
        });
        self.transport.post("Invoices", body)
    }
    fn push_payment(
        &self,
        remote_invoice_id: &str,
        amount_minor: u64,
        currency: &str,
        reference: &str,
        paid_at: DateTime<Utc>,
    ) -> Result<String, SyncError> {
        let body = serde_json::json!({
            "Payments": [{
                "Date": paid_at.format("%Y-%m-%d").to_string(),
                "Amount": amount_minor as f64 / 100.0,
                "CurrencyCode": currency,
                "Reference": reference,
                "InvoiceNumber": remote_invoice_id,
            }]
        });
        self.transport.post("Payments", body)
    }
}

// ---------------------------------------------------------------- transport --

/// Production transport: POSTs to the provider's API with an OAuth bearer
/// token from the vault. Kept thin — exercised only behind stubs in tests.
pub struct HttpTransport {
    pub base_url: String,
    pub bearer: String,
}

impl Transport for HttpTransport {
    fn post(&self, path: &str, body: serde_json::Value) -> Result<String, SyncError> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), path);
        // Manual JSON body (ureq's json feature is off; matches calendar.rs usage).
        let payload = serde_json::to_vec(&body).map_err(|e| SyncError::Transport(e.to_string()))?;
        let text = ureq::post(&url)
            .header("authorization", &format!("Bearer {}", self.bearer))
            .header("content-type", "application/json")
            .send(payload)
            .map_err(|e| SyncError::Transport(e.to_string()))?
            .body_mut()
            .read_to_string()
            .map_err(|e| SyncError::Transport(e.to_string()))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| SyncError::Transport(e.to_string()))?;
        v.get("id")
            .or_else(|| v.get("Id"))
            .or_else(|| v.pointer("/Invoices/0/InvoiceID"))
            .and_then(|i| i.as_str())
            .map(str::to_string)
            .ok_or_else(|| SyncError::Transport("no id in provider response".into()))
    }
}

// ------------------------------------------------------------------ service --

/// One sync attempt to record, keyed idempotently by provider + kind + invoice.
pub struct SyncAttempt<'a> {
    pub provider: &'a str,
    pub kind: &'a str,
    pub invoice: &'a Invoice,
    pub remote_id: String,
    pub status: SyncStatus,
    pub error: String,
    pub now: DateTime<Utc>,
    /// See `SyncRecord::detail` (#114).
    pub detail: String,
}

/// Records a sync attempt (success or failure) idempotently keyed by
/// provider + kind + invoice.
pub fn record_sync(
    store: &Store,
    attempt: SyncAttempt<'_>,
) -> Result<SyncRecord, crate::store::StoreError> {
    let SyncAttempt {
        provider,
        kind,
        invoice,
        remote_id,
        status,
        error,
        now,
        detail,
    } = attempt;
    // Whole read-modify-write under one store lock (review B4/D3): two
    // concurrent syncs must never clobber each other's records.
    let mut outcome: Option<SyncRecord> = None;
    store.update_json_rel::<Vec<SyncRecord>, _>("sync/accounting.json", |current| {
        let mut records = current.unwrap_or_default();
        let existing = records.iter().position(|r| {
            r.provider == provider
                && r.kind == kind
                && r.invoice_id == invoice.id
                && r.detail == detail
        });
        let attempts = existing.map_or(1, |i| records[i].attempts + 1);
        let rec = SyncRecord {
            provider: provider.to_string(),
            kind: kind.to_string(),
            invoice_id: invoice.id,
            invoice_number: invoice.number.clone(),
            remote_id: remote_id.clone(),
            status,
            error: error.clone(),
            attempts,
            updated_at: now,
            detail: detail.clone(),
        };
        match existing {
            Some(i) => records[i] = rec.clone(),
            None => records.push(rec.clone()),
        }
        outcome = Some(rec);
        records
    })?;
    Ok(outcome.expect("closure always sets"))
}

pub fn list_records(store: &Store) -> Result<Vec<SyncRecord>, crate::store::StoreError> {
    Ok(store
        .read_json_rel::<Vec<SyncRecord>>("sync/accounting.json")?
        .unwrap_or_default())
}

pub fn put_records(store: &Store, records: &[SyncRecord]) -> Result<(), crate::store::StoreError> {
    store.write_json_rel("sync/accounting.json", records)
}

/// Retry failed invoice syncs (daily job): any failed record is re-pushed
/// through its provider. Non-blocking: failures just update the record.
pub struct AccountingRetryJob {
    store: Arc<Store>,
    registry: Arc<AccountingRegistry>,
}

impl AccountingRetryJob {
    #[must_use]
    pub fn new(store: Arc<Store>, registry: Arc<AccountingRegistry>) -> Self {
        Self { store, registry }
    }
}

impl crate::scheduler::Job for AccountingRetryJob {
    fn name(&self) -> &'static str {
        "accounting-retry"
    }
    fn interval_secs(&self) -> i64 {
        24 * 3600
    }
    fn run(&self, now: DateTime<Utc>) {
        let Ok(records) = list_records(&self.store) else {
            return;
        };
        for rec in records
            .iter()
            .filter(|r| r.status == SyncStatus::Failed && r.attempts < 6)
        {
            let Some(provider) = self.registry.get(&rec.provider) else {
                continue;
            };
            let Ok(Some(invoice)) = self.store.get_invoice(rec.invoice_id) else {
                continue;
            };
            let Ok(customers) = self.store.list_customers() else {
                continue;
            };
            let Some(customer) = customers.iter().find(|c| c.id == invoice.customer_id) else {
                continue;
            };
            let key = invoice_key(provider.name(), &invoice.number);
            // Failed *payment* records retry by re-pushing the payment against
            // the already-synced invoice; invoice records re-push the invoice.
            if rec.kind == "payment" {
                let Some(invoice_rec) = records
                    .iter()
                    .find(|r| {
                        r.provider == rec.provider
                            && r.kind == "invoice"
                            && r.invoice_id == rec.invoice_id
                    })
                    .cloned()
                else {
                    continue;
                };
                // #114: re-push THAT payment with its real amount; the
                // empty-detail legacy record keeps the total/payment_date.
                let (amount, reference, paid_at) = match (
                    rec.detail.parse::<uuid::Uuid>().ok(),
                    invoice
                        .payments
                        .iter()
                        .find(|p| p.id.to_string() == rec.detail),
                ) {
                    (Some(_), Some(p)) => (p.amount_minor, p.reference.clone(), p.received_at),
                    _ => (
                        invoice.paid_minor(),
                        invoice.payment_reference.clone(),
                        invoice.paid_at.unwrap_or(now),
                    ),
                };
                match provider.push_payment(
                    &invoice_rec.remote_id,
                    amount,
                    &invoice.currency.0,
                    &reference,
                    paid_at,
                ) {
                    Ok(remote) => {
                        let _ = record_sync(
                            &self.store,
                            SyncAttempt {
                                provider: &rec.provider,
                                kind: "payment",
                                invoice: &invoice,
                                remote_id: remote,
                                status: SyncStatus::Synced,
                                error: String::new(),
                                now,
                                detail: rec.detail.clone(),
                            },
                        );
                    }
                    Err(e) => {
                        let _ = record_sync(
                            &self.store,
                            SyncAttempt {
                                provider: &rec.provider,
                                kind: "payment",
                                invoice: &invoice,
                                remote_id: String::new(),
                                status: SyncStatus::Failed,
                                error: e.to_string(),
                                now,
                                detail: rec.detail.clone(),
                            },
                        );
                    }
                }
                continue;
            }
            match provider.push_invoice(
                &InvoiceDoc {
                    invoice: &invoice,
                    customer,
                },
                &key,
            ) {
                Ok(remote) => {
                    let _ = record_sync(
                        &self.store,
                        SyncAttempt {
                            provider: &rec.provider,
                            kind: &rec.kind,
                            invoice: &invoice,
                            remote_id: remote,
                            status: SyncStatus::Synced,
                            error: String::new(),
                            now,
                            detail: String::new(),
                        },
                    );
                }
                Err(e) => {
                    let _ = record_sync(
                        &self.store,
                        SyncAttempt {
                            provider: &rec.provider,
                            kind: &rec.kind,
                            invoice: &invoice,
                            remote_id: String::new(),
                            status: SyncStatus::Failed,
                            error: e.to_string(),
                            now,
                            detail: String::new(),
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Currency, InvoiceStatus, LineKind};
    use chrono::TimeZone;

    /// Transport that records bodies and returns deterministic ids, or fails
    /// on command.
    struct StubTransport {
        posts: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
        fail: bool,
    }

    impl StubTransport {
        fn ok() -> Self {
            Self {
                posts: std::sync::Mutex::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                posts: std::sync::Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }

    impl Transport for StubTransport {
        fn post(&self, path: &str, body: serde_json::Value) -> Result<String, SyncError> {
            self.posts.lock().unwrap().push((path.to_string(), body));
            if self.fail {
                return Err(SyncError::Transport("boom".into()));
            }
            Ok(format!(
                "remote-{path}-{}",
                self.posts.lock().unwrap().len()
            ))
        }
    }

    fn sample_invoice() -> (Invoice, Customer) {
        let customer = Customer {
            id: Uuid::new_v4(),
            name: "ACME".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 6000,
            active: true,
            email: String::new(),
            payment_terms: None,
            invoice_notes: String::new(),
            invoice_subject: String::new(),
        };
        let inv = Invoice {
            id: Uuid::new_v4(),
            number: "INV-0042".into(),
            customer_id: customer.id,
            currency: Currency("EUR".into()),
            period_from: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            lines: vec![crate::domain::InvoiceLine {
                kind: LineKind::Time,
                date: chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
                entry_id: Some(Uuid::new_v4()),
                expense_id: None,
                project_code: None,
                task_code: None,
                hours: Some(crate::domain::Hours(300)),
                rate_minor: Some(6000),
                amount_minor: 18000,
                note: "work".into(),
            }],
            total_minor: 18000,
            status: InvoiceStatus::Issued,
            created_at: Utc.timestamp_opt(1, 0).unwrap(),
            issued_at: None,
            due_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 21).unwrap()),
            paid_at: None,
            payment_reference: String::new(),
            pdf: None,
            payments: vec![],
            write_off_reason: String::new(),
            written_off_at: None,
        };
        (inv, customer)
    }

    #[test]
    fn qbo_builds_invoice_and_payment_documents() {
        let t = Arc::new(StubTransport::ok());
        let p = QboProvider::new(t.clone());
        let (inv, cust) = sample_invoice();
        let remote = p
            .push_invoice(
                &InvoiceDoc {
                    invoice: &inv,
                    customer: &cust,
                },
                "qbo:INV-0042",
            )
            .unwrap();
        assert_eq!(remote, "remote-invoice-1");
        let posts = t.posts.lock().unwrap();
        let body = &posts[0].1;
        assert_eq!(body["IdDoc"]["DocNumber"], "INV-0042");
        assert_eq!(body["IdDoc"]["TotalAmt"], 180.0);
        assert_eq!(body["synchronized_id"], "qbo:INV-0042");
        drop(posts);
        let paid_at = Utc.timestamp_opt(1, 0).unwrap();
        let pid = p
            .push_payment(&remote, 18000, "EUR", "stripe:abc", paid_at)
            .unwrap();
        assert!(pid.starts_with("remote-payment"));
        assert_eq!(
            t.posts.lock().unwrap()[1].1["Payment"]["TotalAmt"],
            "180.00"
        );
    }

    #[test]
    fn xero_document_shape_and_failure_propagates() {
        let t = Arc::new(StubTransport::ok());
        let p = XeroProvider::new(t.clone());
        let (inv, cust) = sample_invoice();
        let remote = p
            .push_invoice(
                &InvoiceDoc {
                    invoice: &inv,
                    customer: &cust,
                },
                "k",
            )
            .unwrap();
        assert!(remote.starts_with("remote-Invoices"));
        let f = XeroProvider::new(Arc::new(StubTransport::failing()));
        assert!(matches!(
            f.push_invoice(
                &InvoiceDoc {
                    invoice: &inv,
                    customer: &cust
                },
                "k"
            ),
            Err(SyncError::Transport(_))
        ));
    }

    #[test]
    fn idempotency_key_is_stable_per_invoice_and_provider() {
        assert_eq!(invoice_key("qbo", "INV-7"), "qbo:INV-7");
        assert_ne!(invoice_key("qbo", "INV-7"), invoice_key("xero", "INV-7"));
    }
}
