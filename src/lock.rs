//! Entry-lock port. A single seam answers "is this entry frozen, and why?".
//! Invoices (#8), timesheet submissions (#16) and expense reimbursements (#24)
//! each become lock providers; the entry edit/delete handlers consult this one
//! check so an entry cannot be changed through any route once locked.
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

pub trait EntryLock: Send + Sync {
    /// `Some(reason)` if the entry is locked and must not be edited or deleted.
    fn entry_lock(&self, entry_id: Uuid) -> Option<LockReason>;
}

/// Default: nothing is locked.
#[derive(Debug, Default)]
pub struct NoLocks;

impl EntryLock for NoLocks {
    fn entry_lock(&self, _entry_id: Uuid) -> Option<LockReason> {
        None
    }
}

/// Any provider that locks the entry wins.
pub struct CombinedLocks {
    providers: Vec<Box<dyn EntryLock>>,
}

impl CombinedLocks {
    pub fn new(providers: Vec<Box<dyn EntryLock>>) -> Self {
        Self { providers }
    }
}

impl EntryLock for CombinedLocks {
    fn entry_lock(&self, entry_id: Uuid) -> Option<LockReason> {
        self.providers.iter().find_map(|p| p.entry_lock(entry_id))
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
    fn entry_lock(&self, entry_id: Uuid) -> Option<LockReason> {
        self.store
            .list_invoices()
            .ok()?
            .iter()
            .find(|inv| {
                inv.status == crate::domain::InvoiceStatus::Issued
                    && inv.lines.iter().any(|l| l.entry_id == entry_id)
            })
            .map(|inv| LockReason::Invoiced {
                id: inv.number.clone(),
            })
    }
}
