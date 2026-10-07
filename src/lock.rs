//! Entry-lock port. A single seam answers "is this entry frozen, and why?".
//! Invoices (#8) and timesheet submissions (#16) are lock providers here;
//! the entry edit/delete handlers consult this one check so an entry cannot
//! be changed through any route once locked. NOTE (#190): expense
//! reimbursement claims (#24) enforce their lock with a direct scan in
//! `api::expenses` rather than through this seam — folding them in as a
//! third provider is a follow-up, the module no longer claims otherwise.
//!
//! `NoLocks` is the default: with no invoice/submission feature shipped yet,
//! nothing is locked. Real providers implement `EntryLock` and are composed by
//! `CombinedLocks` (first lock wins).

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum LockReason {
    Invoiced { id: String },
    Submitted { id: String },
}

impl LockReason {
    pub fn message(&self) -> String {
        match self {
            LockReason::Invoiced { id } => {
                format!("entry is invoiced ({id}); unlock the invoice first")
            }
            LockReason::Submitted { id } => {
                format!("entry is in a submitted timesheet ({id}); reject/withdraw it first")
            }
        }
    }
}

/// The backing collection could not be read, so the lock state of an entry
/// cannot be verified (#185). Handlers must treat this as **fail closed**:
/// refuse the write (503), never allow it because verification failed.
#[derive(Debug)]
pub struct LockUnavailable(pub String);

pub trait EntryLock: Send + Sync {
    /// `Ok(Some(reason))` if the entry is locked and must not be edited or
    /// deleted; `Ok(None)` if provably unlocked; `Err` if the state cannot be
    /// verified — callers must fail closed (#185).
    fn entry_lock(&self, entry_id: Uuid) -> Result<Option<LockReason>, LockUnavailable>;

    /// Every locked entry id in **one** pass (#218). Callers that ask about
    /// many entries per request (submission creation) must not drive
    /// `entry_lock` per id — the invoice/submission providers each re-read
    /// their whole collection, making the loop O(entries x documents).
    /// Implementations must agree with `entry_lock` (covered by a unit test).
    fn locked_entries(&self) -> Result<std::collections::HashSet<Uuid>, LockUnavailable>;
}

/// Default: nothing is locked.
#[derive(Debug, Default)]
pub struct NoLocks;

impl EntryLock for NoLocks {
    fn entry_lock(&self, _entry_id: Uuid) -> Result<Option<LockReason>, LockUnavailable> {
        Ok(None)
    }
    fn locked_entries(&self) -> Result<std::collections::HashSet<Uuid>, LockUnavailable> {
        Ok(std::collections::HashSet::new())
    }
}

/// Any provider that locks the entry wins; a provider that cannot verify
/// fails the whole check closed (#185).
pub struct CombinedLocks {
    providers: Vec<Box<dyn EntryLock>>,
}

impl CombinedLocks {
    pub fn new(providers: Vec<Box<dyn EntryLock>>) -> Self {
        Self { providers }
    }
}

impl EntryLock for CombinedLocks {
    fn entry_lock(&self, entry_id: Uuid) -> Result<Option<LockReason>, LockUnavailable> {
        for provider in &self.providers {
            if let Some(reason) = provider.entry_lock(entry_id)? {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }
    fn locked_entries(&self) -> Result<std::collections::HashSet<Uuid>, LockUnavailable> {
        // Union, fail-closed: one provider that cannot answer locks
        // everything out (#185 semantics carry over to bulk checks).
        let mut all = std::collections::HashSet::new();
        for provider in &self.providers {
            all.extend(provider.locked_entries()?);
        }
        Ok(all)
    }
}

/// Lock provider backed by *open* invoices (`InvoiceStatus::is_open()` =
/// `issued | partly_paid`, #8/#114): an entry referenced by one is frozen.
/// Settled (`paid`) and written-off invoices deliberately release their
/// entries — the issued document can no longer change (rate snapshots), and
/// the timesheet stays correctable (decision recorded in AGENTS.md /
/// adr-001). Reads are unaffected; only edit/delete consult this.
pub struct InvoiceLock {
    store: std::sync::Arc<crate::store::Store>,
}

impl InvoiceLock {
    pub fn new(store: std::sync::Arc<crate::store::Store>) -> Self {
        Self { store }
    }
}

impl EntryLock for InvoiceLock {
    fn entry_lock(&self, entry_id: Uuid) -> Result<Option<LockReason>, LockUnavailable> {
        let invoices = self
            .store
            .list_invoices()
            .map_err(|e| LockUnavailable(e.to_string()))?;
        Ok(invoices
            .iter()
            .find(|inv| {
                // Open invoices freeze their entries; #114 adds the
                // partly-paid state to the same lock family.
                inv.status.is_open() && inv.lines.iter().any(|l| l.entry_id == Some(entry_id))
            })
            .map(|inv| LockReason::Invoiced {
                id: inv.number.clone(),
            }))
    }

    fn locked_entries(&self) -> Result<std::collections::HashSet<Uuid>, LockUnavailable> {
        let invoices = self
            .store
            .list_invoices()
            .map_err(|e| LockUnavailable(e.to_string()))?;
        Ok(invoices
            .iter()
            .filter(|inv| inv.status.is_open())
            .flat_map(|inv| inv.lines.iter())
            .filter_map(|l| l.entry_id)
            .collect())
    }
}

/// Lock provider backed by submitted/approved timesheets (#16): an entry in a
/// submitted or approved submission is frozen until it is rejected/withdrawn.
pub struct SubmissionLock {
    store: std::sync::Arc<crate::store::Store>,
}

impl SubmissionLock {
    pub fn new(store: std::sync::Arc<crate::store::Store>) -> Self {
        Self { store }
    }
}

impl EntryLock for SubmissionLock {
    fn entry_lock(&self, entry_id: Uuid) -> Result<Option<LockReason>, LockUnavailable> {
        let submissions = self
            .store
            .list_submissions()
            .map_err(|e| LockUnavailable(e.to_string()))?;
        Ok(submissions
            .iter()
            .find(|s| {
                (s.state == crate::domain::SubmissionState::Submitted
                    || s.state == crate::domain::SubmissionState::Approved)
                    && s.entry_ids.contains(&entry_id)
            })
            .map(|s| LockReason::Submitted {
                id: s.week_start.format("%Y-%m-%d").to_string(),
            }))
    }

    fn locked_entries(&self) -> Result<std::collections::HashSet<Uuid>, LockUnavailable> {
        let submissions = self
            .store
            .list_submissions()
            .map_err(|e| LockUnavailable(e.to_string()))?;
        Ok(submissions
            .iter()
            .filter(|s| {
                s.state == crate::domain::SubmissionState::Submitted
                    || s.state == crate::domain::SubmissionState::Approved
            })
            .flat_map(|s| s.entry_ids.iter().copied())
            .collect())
    }
}

#[cfg(test)]
mod bulk_tests {
    // #218: locked_entries() must agree with per-entry entry_lock() for the
    // real providers — the submission path switched to the bulk call and the
    // dashboard switched to the GUI mirror; drift here would be silent.
    use super::*;
    use crate::domain::{
        Currency, Entry, Hours, Invoice, InvoiceLine, InvoiceStatus, LineKind, ProjectCode, Source,
        Submission, SubmissionState,
    };
    use chrono::{NaiveDate, TimeZone, Utc};

    fn entry_ref(id: Uuid) -> Entry {
        Entry {
            id,
            date: NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            customer_id: Uuid::new_v4(),
            user_id: None,
            project_code: ProjectCode("P1".into()),
            task_code: None,
            hours: Hours(100),
            note: String::new(),
            billable: true,
            source: Source::Manual,
            created_at: Utc.with_ymd_and_hms(2026, 10, 2, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 10, 2, 0, 0, 0).unwrap(),
        }
    }

    fn invoice_line(entry_id: Uuid) -> InvoiceLine {
        InvoiceLine {
            kind: LineKind::Time,
            date: NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            entry_id: Some(entry_id),
            expense_id: None,
            project_code: Some(ProjectCode("P1".into())),
            task_code: None,
            hours: Some(Hours(100)),
            rate_minor: Some(6000),
            amount_minor: 600,
            note: String::new(),
            quantity_hundredths: None,
            unit_price_minor: None,
            item_kind: None,
        }
    }

    fn invoice(number: &str, status: InvoiceStatus, entry_id: Uuid) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            number: number.into(),
            customer_id: Uuid::new_v4(),
            currency: Currency("EUR".into()),
            period_from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            lines: vec![invoice_line(entry_id)],
            total_minor: 600,
            status,
            created_at: Utc.with_ymd_and_hms(2026, 10, 7, 0, 0, 0).unwrap(),
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
        }
    }

    #[test]
    fn bulk_locked_entries_match_per_entry_checks() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::store::Store::open(dir.path().join("data")).unwrap());
        let (e_issued, e_partly, e_paid, e_sub, e_free) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for entry in [e_issued, e_partly, e_paid] {
            store.put_entry(&entry_ref(entry)).unwrap();
        }
        store
            .put_invoice(&invoice("INV-1", InvoiceStatus::Issued, e_issued))
            .unwrap();
        store
            .put_invoice(&invoice("INV-2", InvoiceStatus::PartlyPaid, e_partly))
            .unwrap();
        store
            .put_invoice(&invoice("INV-3", InvoiceStatus::Paid, e_paid))
            .unwrap();
        store
            .put_submission(&Submission {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                week_start: NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(),
                week_end: NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(),
                state: SubmissionState::Submitted,
                entry_ids: vec![e_sub],
                comment: String::new(),
                created_at: Utc::now(),
                submitted_at: None,
                decided_at: None,
            })
            .unwrap();

        let combined = CombinedLocks::new(vec![
            Box::new(InvoiceLock::new(store.clone())),
            Box::new(SubmissionLock::new(store.clone())),
        ]);
        let ids = [e_issued, e_partly, e_paid, e_sub, e_free];
        let bulk = combined.locked_entries().unwrap();
        for id in ids {
            let per_entry = combined.entry_lock(id).unwrap().is_some();
            assert_eq!(
                bulk.contains(&id),
                per_entry,
                "bulk vs per-entry disagree for {id} (issued/partly/submitted lock; paid/free do not)"
            );
        }
        assert!(bulk.contains(&e_issued) && bulk.contains(&e_partly) && bulk.contains(&e_sub));
        assert!(!bulk.contains(&e_paid) && !bulk.contains(&e_free));
    }
}
