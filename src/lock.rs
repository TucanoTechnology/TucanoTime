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
}

/// Default: nothing is locked.
#[derive(Debug, Default)]
pub struct NoLocks;

impl EntryLock for NoLocks {
    fn entry_lock(&self, _entry_id: Uuid) -> Result<Option<LockReason>, LockUnavailable> {
        Ok(None)
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
}

/// Lock provider backed by issued invoices: an entry referenced by an
/// `issued` invoice is frozen (#8). Reads are unaffected; only edit/delete
/// consult this.
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
}
