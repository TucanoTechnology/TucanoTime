// File-based persistence. No database: every entity is one pretty-printed
// JSON document on disk, mirroring the conceptual organisation of the data
// (a customer is a folder that contains its projects; entries live in a
// per-day folder). The API is the only actor that writes below the data dir.
//
// Path components are never taken raw from a request: customer files are
// named by `Uuid`, project files by a validated `ProjectCode`, entry
// directories by a validated `YYYY-MM-DD` date and entry files by `Uuid`.
// Traversal input therefore cannot reach the filesystem.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};
use fs2::FileExt;
use uuid::Uuid;

use crate::auth::User;
use crate::domain::{
    Category, Customer, Entry, Expense, ExpenseClaim, Invoice, InvoiceStatus, Notification,
    Project, RecurringSchedule, Submission, Task, Timer,
};

/// Hard cap on a range scan so a malformed or adversarial query cannot spin
/// over the whole tree. 400 days covers a year plus buffer.
pub const MAX_RANGE_DAYS: i64 = 400;

/// Hard cap on documents in any single collection directory (#50). A personal
/// timesheet is far below this; it bounds a pathological/corrupted tree so no
/// request does an unbounded directory walk.
pub const MAX_DOCS: usize = 100_000;

/// Default cross-process writer-lock acquisition deadline (#62).
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_millis(5000);
/// Poll interval between `try_lock_exclusive` attempts.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("state conflict: {0}")]
    Conflict(String),
    #[error("range too large (max {MAX_RANGE_DAYS} days)")]
    RangeTooLarge,
    #[error("write lock busy")]
    LockTimeout,
    #[error("collection too large (max {MAX_DOCS})")]
    TooManyItems,
    #[error("io failure: {0}")]
    Io(String),
}

// `Io` carries the operation context only; the underlying error is logged
// server-side, never serialised to a client (see security rules).
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

pub struct Store {
    root: PathBuf,
    /// Intra-process serialiser: one thread mutates at a time.
    write_guard: Mutex<()>,
    /// Cross-process writer-lock deadline (#62).
    lock_timeout: Duration,
    /// Per-collection document cap (#50); env-overridable for tests.
    max_docs: usize,
}

/// Held for the duration of a mutating operation: the process-local mutex plus
/// the advisory file lock. Dropping it releases both (the flock on fd close).
struct WriteGuard<'a> {
    _mutex: MutexGuard<'a, ()>,
    _lock: File,
}

/// One flat JSON document collection under the data root: every entity of
/// type `T` lives at `<root>/<DIR>/<id>.json` (#101). The twelve
/// `dir_entries -> read_json -> sort` / `write_lock -> create_dir_all ->
/// write_json` / `exists -> NotFound -> remove` blocks this file carried now
/// live once, in the primitives below, so the lock + atomicity discipline
/// (and the #97 transaction seam) has a single home.
pub trait Entity: serde::Serialize + serde::de::DeserializeOwned + Sized {
    /// Collection directory below the data root, e.g. `"invoices"`.
    const DIR: &'static str;
    /// Document stem: the file is `<id>.json`. Usually the entity `Uuid`;
    /// `Timer` keys by its owner instead.
    fn id(&self) -> String;
}

impl Entity for User {
    const DIR: &'static str = "users";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for Invoice {
    const DIR: &'static str = "invoices";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for Category {
    const DIR: &'static str = "categories";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for Expense {
    const DIR: &'static str = "expenses";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for Submission {
    const DIR: &'static str = "submissions";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for ExpenseClaim {
    const DIR: &'static str = "claims";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for RecurringSchedule {
    const DIR: &'static str = "schedules";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for Customer {
    const DIR: &'static str = "customers";
    fn id(&self) -> String {
        self.id.to_string()
    }
}

impl Entity for Timer {
    const DIR: &'static str = "timers";
    fn id(&self) -> String {
        self.user_id.to_string()
    }
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with_lock_timeout(root, DEFAULT_LOCK_TIMEOUT)
    }

    pub fn with_lock_timeout(
        root: impl AsRef<Path>,
        lock_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        // Best-effort hygiene (review D6): a crash between tmp-write and
        // rename can strand `*.tmp` files; they are not documents.
        for dir in [
            "customers",
            "users",
            "invoices",
            "categories",
            "expenses",
            "submissions",
            "claims",
        ] {
            prune_tmp_files(&root.join(dir));
        }
        prune_tmp_files(&root.join("entries"));
        if let Ok(days) = std::fs::read_dir(root.join("entries")) {
            for day in days.filter_map(|e| e.ok()) {
                if day.path().is_dir() {
                    prune_tmp_files(&day.path());
                }
            }
        }
        std::fs::create_dir_all(root.join("customers"))?;
        std::fs::create_dir_all(root.join("entries"))?;
        std::fs::create_dir_all(root.join("users"))?;
        std::fs::create_dir_all(root.join("categories"))?;
        std::fs::create_dir_all(root.join("expenses"))?;
        let max_docs = std::env::var("TUCANO_MAX_DOCS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(MAX_DOCS);
        Ok(Self {
            root,
            write_guard: Mutex::new(()),
            lock_timeout,
            max_docs,
        })
    }

    /// Acquire the writer lock for a mutating operation: the process-local
    /// mutex (cheap, serialises threads) then the cross-process advisory flock
    /// on `.tucanotime.lock`, retried until the deadline. Returns
    /// `LockTimeout` (→ 503) if another process holds it too long.
    fn write_lock(&self) -> Result<WriteGuard<'_>, StoreError> {
        let mutex = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".tucanotime.lock"))?;
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(LOCK_POLL_INTERVAL);
                }
                Err(_) => return Err(StoreError::LockTimeout),
            }
        }
        Ok(WriteGuard {
            _mutex: mutex,
            _lock: lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Generic JSON document read at a root-relative path (auxiliary state
    /// files like the accounting sync status, #33). `Ok(None)` when absent.
    pub fn read_json_rel<T: serde::de::DeserializeOwned>(
        &self,
        rel: &str,
    ) -> Result<Option<T>, StoreError> {
        read_json(&self.root.join(rel))
    }

    /// One-locked read-modify-write of a root-relative JSON document:
    /// `update` sees the current bytes and returns the replacement. Used for
    /// small ledgers where list-then-put across two locks would race (review
    /// D3/B4 — accounting sync records).
    pub fn update_json_rel<T, F>(&self, rel: &str, update: F) -> Result<T, StoreError>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
        F: FnOnce(Option<T>) -> T,
    {
        let _guard = self.write_lock()?;
        let path = self.root.join(rel);
        let current = read_json::<T>(&path)?;
        let next = update(current);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_json(&path, &next)?;
        Ok(next)
    }

    /// Generic atomic JSON write at a root-relative path, under the write lock.
    pub fn write_json_rel<T: serde::Serialize + ?Sized>(
        &self,
        rel: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_json(&path, value)
    }

    /// Override the per-collection document cap (used by tests; production uses
    /// the default or `TUCANO_MAX_DOCS`).
    pub fn with_max_docs(mut self, max: usize) -> Self {
        self.max_docs = max;
        self
    }

    // ------------------------------------------------ collection primitives --

    fn doc_dir<T: Entity>(&self) -> PathBuf {
        self.root.join(T::DIR)
    }

    fn doc_path<T: Entity>(&self, id: &str) -> PathBuf {
        self.doc_dir::<T>().join(format!("{id}.json"))
    }

    /// Read every document in a collection directory. A missing directory is
    /// an empty collection (it is created on first write); dot files and
    /// stranded `.tmp` files are not documents (`dir_entries` skips them).
    /// The caller owns the ordering.
    pub(crate) fn list_docs<T: Entity>(&self) -> Result<Vec<T>, StoreError> {
        self.walk_docs(&self.doc_dir::<T>(), read_json::<T>)
    }

    /// The shared `dir_entries -> json filter -> decode -> collect` walk,
    /// with an injectable decoder: nested collections (projects) plug their
    /// own legacy-aware decode hook here (`domain::project_from_bytes`).
    pub(crate) fn walk_docs<T>(
        &self,
        dir: &Path,
        decode: impl Fn(&Path) -> Result<Option<T>, StoreError>,
    ) -> Result<Vec<T>, StoreError> {
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(v) = decode(&path)? {
                out.push(v);
            }
        }
        Ok(out)
    }

    pub(crate) fn get_doc<T: Entity>(&self, id: &str) -> Result<Option<T>, StoreError> {
        read_json(&self.doc_path::<T>(id))
    }

    /// Lock, ensure the directory, atomic write — the one put path for
    /// flat collections.
    pub(crate) fn put_doc<T: Entity>(&self, value: &T) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.doc_dir::<T>())?;
        write_json(&self.doc_path::<T>(&value.id()), value)
    }

    /// Lock, exists → `NotFound`, remove — the one delete path. Guards that
    /// need to inspect other collections take `write_lock` themselves and
    /// reuse `remove_doc_locked`.
    pub(crate) fn remove_doc<T: Entity>(&self, id: &str) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        self.remove_doc_locked::<T>(id)
    }

    pub(crate) fn remove_doc_locked<T: Entity>(&self, id: &str) -> Result<(), StoreError> {
        let path = self.doc_path::<T>(id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // ---------------------------------------------------------- side indexes --
    //
    // Two full scans had crept onto hot paths (review D3): payment webhooks
    // resolve invoices by human number, and login/SSO resolve users by email.
    // Both are dot-file maps maintained under the same write lock as the
    // documents they index — crash-safe tmp+rename via `write_json`, skipped
    // by `dir_entries` so an index is never mistaken for a document. A
    // missing or stale index never loses data: reads verify the fast path
    // against the document itself, fall back to the old scan, and self-heal.

    fn invoice_index_path(&self) -> PathBuf {
        self.root.join("invoices").join(".idx.numbers.json")
    }

    fn email_index_path(&self) -> PathBuf {
        self.root.join("users").join(".idx.emails.json")
    }

    /// Map `number -> id` for one invoice. The caller holds the write lock.
    fn index_invoice_locked(&self, invoice: &Invoice) -> Result<(), StoreError> {
        let path = self.invoice_index_path();
        let mut idx: HashMap<String, String> = read_json(&path)?.unwrap_or_default();
        let id = invoice.id.to_string();
        idx.retain(|_, v| v != &id);
        idx.insert(invoice.number.clone(), id);
        write_json(&path, &idx)
    }

    /// Drop a deleted invoice from the index (its number is never recycled,
    /// review B3 — the entry simply disappears with the document).
    fn unindex_invoice_locked(&self, id: &str) -> Result<(), StoreError> {
        let path = self.invoice_index_path();
        if let Some(mut idx) = read_json::<HashMap<String, String>>(&path)? {
            idx.retain(|_, v| v != id);
            write_json(&path, &idx)?;
        }
        Ok(())
    }

    /// Rebuild the whole number index from the documents (self-heal after a
    /// pre-index data dir or detected drift). Duplicate numbers (a restored
    /// file) resolve to the earliest-created invoice, matching the scan.
    fn rebuild_invoice_index(&self) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let mut best: HashMap<String, (String, chrono::DateTime<chrono::Utc>)> = HashMap::new();
        for inv in self.list_invoices()? {
            match best.get(&inv.number) {
                Some((_, at)) if *at <= inv.created_at => {}
                _ => {
                    best.insert(inv.number.clone(), (inv.id.to_string(), inv.created_at));
                }
            }
        }
        let idx: HashMap<String, String> = best
            .into_iter()
            .map(|(number, (id, _))| (number, id))
            .collect();
        std::fs::create_dir_all(self.doc_dir::<Invoice>())?;
        write_json(&self.invoice_index_path(), &idx)
    }

    /// Map `email(lower) -> id` for one user. The caller holds the write lock.
    fn index_user_locked(&self, user: &User) -> Result<(), StoreError> {
        let path = self.email_index_path();
        let mut idx: HashMap<String, String> = read_json(&path)?.unwrap_or_default();
        let id = user.id.to_string();
        idx.retain(|_, v| v != &id);
        idx.insert(user.email.to_lowercase(), id);
        write_json(&path, &idx)
    }

    fn unindex_user_locked(&self, id: &str) -> Result<(), StoreError> {
        let path = self.email_index_path();
        if let Some(mut idx) = read_json::<HashMap<String, String>>(&path)? {
            idx.retain(|_, v| v != id);
            write_json(&path, &idx)?;
        }
        Ok(())
    }

    fn rebuild_email_index(&self) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let mut idx: HashMap<String, String> = HashMap::new();
        for u in self.list_users()? {
            // list_users is name-sorted; the scan's first match wins, so keep
            // the first insertion per address.
            idx.entry(u.email.to_lowercase())
                .or_insert_with(|| u.id.to_string());
        }
        std::fs::create_dir_all(self.doc_dir::<User>())?;
        write_json(&self.email_index_path(), &idx)
    }

    // ---------------------------------------------------------------- users --

    pub fn list_users(&self) -> Result<Vec<User>, StoreError> {
        let mut out = self.list_docs::<User>()?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn has_users(&self) -> Result<bool, StoreError> {
        Ok(!self.list_users()?.is_empty())
    }

    pub fn get_user(&self, id: Uuid) -> Result<Option<User>, StoreError> {
        self.get_doc::<User>(&id.to_string())
    }

    pub fn get_user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        // Fast path: the email side index, verified against the document.
        if let Some(id) =
            read_json::<HashMap<String, String>>(&self.email_index_path())?.and_then(|idx| {
                idx.get(&email.to_lowercase())
                    .and_then(|s| Uuid::parse_str(s).ok())
            })
            && let Some(user) = self.get_user(id)?
            && user.email.eq_ignore_ascii_case(email)
        {
            return Ok(Some(user));
        }
        // The scan stays authoritative (pre-index dirs, restored docs, drift).
        let found = self
            .list_users()?
            .into_iter()
            .find(|u| u.email.eq_ignore_ascii_case(email));
        // Reaching a hit through the scan means the fast path missed while a
        // matching document exists: the index is absent or stale — heal it.
        if found.is_some() {
            self.rebuild_email_index()?;
        }
        Ok(found)
    }

    pub fn put_user(&self, user: &User) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.doc_dir::<User>())?;
        write_json(&self.doc_path::<User>(&user.id.to_string()), user)?;
        self.index_user_locked(user)
    }

    pub fn delete_user(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        self.remove_doc_locked::<User>(&id.to_string())?;
        self.unindex_user_locked(&id.to_string())
    }

    // ------------------------------------------------------------- invoices --

    pub fn list_invoices(&self) -> Result<Vec<Invoice>, StoreError> {
        let mut out = self.list_docs::<Invoice>()?;
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.number.cmp(&b.number))
        });
        Ok(out)
    }

    pub fn get_invoice(&self, id: Uuid) -> Result<Option<Invoice>, StoreError> {
        self.get_doc::<Invoice>(&id.to_string())
    }

    /// First invoice with this number (numbers are sequential but a restored
    /// file could duplicate one; the earliest wins). Used by payment webhooks
    /// (#34), which know the invoice by its human number: the number index
    /// turns that scan into one lookup (#102).
    pub fn find_invoice_by_number(&self, number: &str) -> Result<Option<Invoice>, StoreError> {
        let idx = read_json::<HashMap<String, String>>(&self.invoice_index_path())?;
        if let Some(id) = idx
            .as_ref()
            .and_then(|m| m.get(number))
            .and_then(|s| Uuid::parse_str(s).ok())
            && let Some(invoice) = self.get_invoice(id)?
            && invoice.number == number
        {
            return Ok(Some(invoice));
        }
        // Authoritative scan (also covers an absent or drifted index).
        let mut found: Option<Invoice> = None;
        for inv in self.list_invoices()? {
            if inv.number == number && found.as_ref().is_none_or(|f| inv.created_at < f.created_at)
            {
                found = Some(inv);
            }
        }
        // A hit through the scan means the fast path missed: rebuild.
        if found.is_some() {
            self.rebuild_invoice_index()?;
        }
        Ok(found)
    }

    pub fn put_invoice(&self, invoice: &Invoice) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.doc_dir::<Invoice>())?;
        write_json(&self.doc_path::<Invoice>(&invoice.id.to_string()), invoice)?;
        self.index_invoice_locked(invoice)
    }

    pub fn delete_invoice(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        self.remove_doc_locked::<Invoice>(&id.to_string())?;
        self.unindex_invoice_locked(&id.to_string())
    }

    /// Atomically assign the next invoice number and persist under one writer
    /// lock, so two processes never mint the same number (#62) and a deleted
    /// invoice never frees its number for reuse (review B3): the sequence is
    /// kept in `invoices/.seq.json`, never derived from the current count.
    pub fn create_invoice(&self, invoice: Invoice) -> Result<Invoice, StoreError> {
        let _guard = self.write_lock()?;
        self.create_invoice_inner(invoice)
    }

    // ----------------------------------------------------------- categories --

    pub fn list_categories(&self) -> Result<Vec<Category>, StoreError> {
        let mut out = self.list_docs::<Category>()?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get_category(&self, id: Uuid) -> Result<Option<Category>, StoreError> {
        self.get_doc::<Category>(&id.to_string())
    }

    pub fn put_category(&self, category: &Category) -> Result<(), StoreError> {
        self.put_doc(category)
    }

    pub fn delete_category(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        if !self.doc_path::<Category>(&id.to_string()).exists() {
            return Err(StoreError::NotFound);
        }
        if self
            .list_expenses()?
            .iter()
            .any(|e| e.category_id == Some(id))
        {
            return Err(StoreError::AlreadyExists(
                "category still used by expenses".into(),
            ));
        }
        self.remove_doc_locked::<Category>(&id.to_string())
    }

    // ------------------------------------------------------------- expenses --

    pub fn list_expenses(&self) -> Result<Vec<Expense>, StoreError> {
        let mut out = self.list_docs::<Expense>()?;
        out.sort_by_key(|e| std::cmp::Reverse((e.date, e.created_at)));
        Ok(out)
    }

    pub fn get_expense(&self, id: Uuid) -> Result<Option<Expense>, StoreError> {
        self.get_doc::<Expense>(&id.to_string())
    }

    pub fn put_expense(&self, expense: &Expense) -> Result<(), StoreError> {
        self.put_doc(expense)
    }

    pub fn delete_expense(&self, id: Uuid) -> Result<(), StoreError> {
        self.remove_doc::<Expense>(&id.to_string())
    }

    // ---------------------------------------------------------- submissions --

    pub fn list_submissions(&self) -> Result<Vec<Submission>, StoreError> {
        let mut out = self.list_docs::<Submission>()?;
        out.sort_by_key(|s| std::cmp::Reverse(s.week_start));
        Ok(out)
    }

    pub fn get_submission(&self, id: Uuid) -> Result<Option<Submission>, StoreError> {
        self.get_doc::<Submission>(&id.to_string())
    }

    pub fn put_submission(&self, submission: &Submission) -> Result<(), StoreError> {
        self.put_doc(submission)
    }

    // --------------------------------------------------------------- claims --

    pub fn list_claims(&self) -> Result<Vec<ExpenseClaim>, StoreError> {
        let mut out = self.list_docs::<ExpenseClaim>()?;
        out.sort_by_key(|c| std::cmp::Reverse(c.created_at));
        Ok(out)
    }

    pub fn get_claim(&self, id: Uuid) -> Result<Option<ExpenseClaim>, StoreError> {
        self.get_doc::<ExpenseClaim>(&id.to_string())
    }

    pub fn put_claim(&self, claim: &ExpenseClaim) -> Result<(), StoreError> {
        self.put_doc(claim)
    }

    // ---------------------------------------------------------------- timers --

    pub fn get_timer(&self, user_id: Uuid) -> Result<Option<Timer>, StoreError> {
        self.get_doc::<Timer>(&user_id.to_string())
    }

    pub fn put_timer(&self, timer: &Timer) -> Result<(), StoreError> {
        self.put_doc(timer)
    }

    pub fn delete_timer(&self, user_id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.doc_path::<Timer>(&user_id.to_string());
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    // -------------------------------------------------------- notifications --

    fn notifications_path(&self, user_id: Uuid) -> PathBuf {
        self.root
            .join("notifications")
            .join(format!("{user_id}.json"))
    }

    pub fn list_notifications(&self, user_id: Uuid) -> Result<Vec<Notification>, StoreError> {
        Ok(read_json::<Vec<Notification>>(&self.notifications_path(user_id))?.unwrap_or_default())
    }

    pub fn push_notification(&self, user_id: Uuid, n: &Notification) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let mut list: Vec<Notification> =
            read_json(&self.notifications_path(user_id))?.unwrap_or_default();
        list.push(n.clone());
        // Bound the stored list (newest kept).
        if list.len() > 200 {
            let start = list.len() - 200;
            list = list[start..].to_vec();
        }
        std::fs::create_dir_all(self.root.join("notifications"))?;
        write_json(&self.notifications_path(user_id), &list)
    }

    pub fn mark_notifications_read(&self, user_id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let mut list: Vec<Notification> =
            read_json(&self.notifications_path(user_id))?.unwrap_or_default();
        for n in list.iter_mut() {
            n.read = true;
        }
        std::fs::create_dir_all(self.root.join("notifications"))?;
        write_json(&self.notifications_path(user_id), &list)
    }

    // ------------------------------------------------------------- schedules --

    pub fn list_schedules(&self) -> Result<Vec<RecurringSchedule>, StoreError> {
        let mut out = self.list_docs::<RecurringSchedule>()?;
        out.sort_by_key(|s| s.created_at);
        Ok(out)
    }

    pub fn get_schedule(&self, id: Uuid) -> Result<Option<RecurringSchedule>, StoreError> {
        self.get_doc::<RecurringSchedule>(&id.to_string())
    }

    pub fn put_schedule(&self, s: &RecurringSchedule) -> Result<(), StoreError> {
        self.put_doc(s)
    }

    pub fn delete_schedule(&self, id: Uuid) -> Result<(), StoreError> {
        self.remove_doc::<RecurringSchedule>(&id.to_string())
    }

    // ------------------------------------------------------------ customers --

    pub fn list_customers(&self) -> Result<Vec<Customer>, StoreError> {
        let mut out = self.list_docs::<Customer>()?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get_customer(&self, id: Uuid) -> Result<Option<Customer>, StoreError> {
        self.get_doc::<Customer>(&id.to_string())
    }

    pub fn put_customer(&self, customer: &Customer) -> Result<(), StoreError> {
        self.put_doc(customer)
    }

    pub fn delete_customer(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        // A customer owns its project folder; removing it would orphan those
        // documents, so the caller must empty it first.
        let projects = self.list_projects(id).unwrap_or_default();
        if !projects.is_empty() {
            return Err(StoreError::AlreadyExists(
                "customer still has projects; delete them first".into(),
            ));
        }
        if self.has_entries_for_customer(id)? {
            return Err(StoreError::AlreadyExists(
                "customer still has time entries; delete or re-point them first".into(),
            ));
        }
        let doc = self.doc_path::<Customer>(&id.to_string());
        if !doc.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_dir_all(self.root.join("customers").join(id.to_string()))?;
        std::fs::remove_file(&doc)?;
        Ok(())
    }

    // ------------------------------------------------------------- projects --

    fn projects_dir(&self, customer_id: Uuid) -> PathBuf {
        self.root
            .join("customers")
            .join(customer_id.to_string())
            .join("projects")
    }

    fn project_path(&self, customer_id: Uuid, code: &str) -> PathBuf {
        self.projects_dir(customer_id).join(format!("{code}.json"))
    }

    pub fn list_projects(&self, customer_id: Uuid) -> Result<Vec<Project>, StoreError> {
        let customer = self
            .get_customer(customer_id)?
            .ok_or(StoreError::NotFound)?;
        // The nested-collection case (#101): parent key + injected decode hook
        // (`read_project` resolves pre-#11 documents from the customer).
        let mut out = self.walk_docs(&self.projects_dir(customer_id), |path| {
            self.read_project(path, &customer)
        })?;
        out.sort_by(|a, b| a.code.cmp(&b.code));
        Ok(out)
    }

    /// Read one project document, resolving any pre-#11 missing currency/rate
    /// from the owning customer.
    fn read_project(
        &self,
        path: &Path,
        customer: &Customer,
    ) -> Result<Option<Project>, StoreError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let project = crate::domain::project_from_bytes(&bytes, customer)
                    .map_err(|e| StoreError::Io(format!("corrupt project document: {e}")))?;
                Ok(Some(project))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_project(
        &self,
        customer_id: Uuid,
        code: &str,
    ) -> Result<Option<Project>, StoreError> {
        let customer = self
            .get_customer(customer_id)?
            .ok_or(StoreError::NotFound)?;
        self.read_project(&self.project_path(customer_id, code), &customer)
    }

    pub fn put_project(&self, project: &Project) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.projects_dir(project.customer_id))?;
        write_json(
            &self.project_path(project.customer_id, &project.code.0),
            project,
        )
    }

    pub fn delete_project(&self, customer_id: Uuid, code: &str) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.project_path(customer_id, code);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        if !self.list_tasks(customer_id, code)?.is_empty() {
            return Err(StoreError::AlreadyExists(
                "project still has tasks; delete them first".into(),
            ));
        }
        if self.has_entries_for_project(customer_id, code)? {
            return Err(StoreError::AlreadyExists(
                "project still has time entries; delete or re-point them first".into(),
            ));
        }
        std::fs::remove_file(&path)?;
        // Remove the project's task folder (tasks live inside it).
        let _ = std::fs::remove_dir_all(self.tasks_dir(customer_id, code));
        Ok(())
    }

    // ---------------------------------------------------------------- tasks --

    fn tasks_dir(&self, customer_id: Uuid, project_code: &str) -> PathBuf {
        self.projects_dir(customer_id)
            .join(project_code)
            .join("tasks")
    }

    fn task_path(&self, customer_id: Uuid, project_code: &str, code: &str) -> PathBuf {
        self.tasks_dir(customer_id, project_code)
            .join(format!("{code}.json"))
    }

    pub fn list_tasks(
        &self,
        customer_id: Uuid,
        project_code: &str,
    ) -> Result<Vec<Task>, StoreError> {
        let mut out = self.walk_docs(
            &self.tasks_dir(customer_id, project_code),
            read_json::<Task>,
        )?;
        out.sort_by(|a, b| a.code.cmp(&b.code));
        Ok(out)
    }

    pub fn get_task(
        &self,
        customer_id: Uuid,
        project_code: &str,
        code: &str,
    ) -> Result<Option<Task>, StoreError> {
        read_json(&self.task_path(customer_id, project_code, code))
    }

    pub fn put_task(&self, task: &Task) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.tasks_dir(task.customer_id, &task.project_code.0))?;
        write_json(
            &self.task_path(task.customer_id, &task.project_code.0, &task.code.0),
            task,
        )
    }

    pub fn delete_task(
        &self,
        customer_id: Uuid,
        project_code: &str,
        code: &str,
    ) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.task_path(customer_id, project_code, code);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        if self.has_entries_for_task(customer_id, project_code, code)? {
            return Err(StoreError::AlreadyExists(
                "task still has time entries; delete or re-point them first".into(),
            ));
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // -------------------------------------------------------------- entries --

    fn day_dir(&self, date: NaiveDate) -> PathBuf {
        self.root
            .join("entries")
            .join(date.format("%Y-%m-%d").to_string())
    }

    fn entry_path(&self, date: NaiveDate, id: Uuid) -> PathBuf {
        self.day_dir(date).join(format!("{id}.json"))
    }

    pub fn list_by_date(&self, date: NaiveDate) -> Result<Vec<Entry>, StoreError> {
        let mut out = self.walk_docs(&self.day_dir(date), read_json::<Entry>)?;
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn list_range(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<Entry>, StoreError> {
        if (to - from).num_days() > MAX_RANGE_DAYS {
            return Err(StoreError::RangeTooLarge);
        }
        let mut out = Vec::new();
        let mut date = from;
        while date <= to {
            out.extend(self.list_by_date(date)?);
            date = date
                .succ_opt()
                .ok_or_else(|| StoreError::Io("date range overflow".into()))?;
        }
        out.sort_by_key(|e| (e.date, e.created_at));
        Ok(out)
    }

    /// Entries are addressable by id alone, but stored under their date, so
    /// this walks the day folders (bounded by MAX_RANGE_DAYS in either
    /// direction of today, which is where live data lives).
    pub fn get_entry(&self, id: Uuid) -> Result<Option<Entry>, StoreError> {
        let dir = self.root.join("entries");
        if !dir.exists() {
            return Ok(None);
        }
        for day in dir_entries(&dir, self.max_docs)? {
            let path = day.join(format!("{id}.json"));
            if let Some(e) = read_json::<Entry>(&path)? {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    pub fn put_entry(&self, entry: &Entry) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.day_dir(entry.date))?;
        write_json(&self.entry_path(entry.date, entry.id), entry)
    }

    pub fn delete_entry(&self, entry: &Entry) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.entry_path(entry.date, entry.id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    fn has_entries_for_customer(&self, customer_id: Uuid) -> Result<bool, StoreError> {
        Ok(self
            .scan_all_entries()?
            .iter()
            .any(|e| e.customer_id == customer_id))
    }

    fn has_entries_for_project(&self, customer_id: Uuid, code: &str) -> Result<bool, StoreError> {
        Ok(self
            .scan_all_entries()?
            .iter()
            .any(|e| e.customer_id == customer_id && e.project_code.0 == code))
    }

    fn has_entries_for_task(
        &self,
        customer_id: Uuid,
        project_code: &str,
        task_code: &str,
    ) -> Result<bool, StoreError> {
        Ok(self.scan_all_entries()?.iter().any(|e| {
            e.customer_id == customer_id
                && e.project_code.0 == project_code
                && e.task_code.as_ref().is_some_and(|t| t.0 == task_code)
        }))
    }

    /// Every entry document, across all day folders, without the
    /// `MAX_RANGE_DAYS` window. For load-bearing background jobs (budget
    /// burn, review B1) — not for HTTP handlers.
    pub fn list_all_entries(&self) -> Result<Vec<Entry>, StoreError> {
        self.scan_all_entries()
    }

    fn scan_all_entries(&self) -> Result<Vec<Entry>, StoreError> {
        let dir = self.root.join("entries");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for day in dir_entries(&dir, self.max_docs)? {
            if day.is_dir() {
                for path in dir_entries(&day, self.max_docs)? {
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if out.len() >= self.max_docs {
                        return Err(StoreError::TooManyItems);
                    }
                    if let Some(e) = read_json::<Entry>(&path)? {
                        out.push(e);
                    }
                }
            }
        }
        Ok(out)
    }
}

// -------------------------------------------- multi-document transactions --
//
// The fs2 write lock is non-reentrant, so handlers must NOT compose
// read-check-write across several public methods: two locked steps can
// interleave with another writer and leave half the pair applied (review
// B4). Each of these takes the lock once and applies the whole unit.

impl Store {
    /// Issue an invoice atomically: refuse unless the stored invoice is a
    /// draft, then persist the transition — no issue-vs-pay race window.
    pub fn issue_invoice(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        due: NaiveDate,
    ) -> Result<Invoice, StoreError> {
        let _guard = self.write_lock()?;
        let Some(mut invoice) = read_json::<Invoice>(&self.doc_path::<Invoice>(&id.to_string()))?
        else {
            return Err(StoreError::NotFound);
        };
        if invoice.status != InvoiceStatus::Draft {
            return Err(StoreError::Conflict(
                "only a draft invoice can be issued".into(),
            ));
        }
        invoice.status = InvoiceStatus::Issued;
        invoice.issued_at = Some(now);
        invoice.due_date = Some(due);
        write_json(&self.doc_path::<Invoice>(&id.to_string()), &invoice)?;
        Ok(invoice)
    }

    /// Mark paid atomically with the issued-state check.
    pub fn pay_invoice(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        reference: String,
    ) -> Result<Invoice, StoreError> {
        let _guard = self.write_lock()?;
        let Some(mut invoice) = read_json::<Invoice>(&self.doc_path::<Invoice>(&id.to_string()))?
        else {
            return Err(StoreError::NotFound);
        };
        if invoice.status != InvoiceStatus::Issued {
            return Err(StoreError::Conflict(
                "only an issued invoice can be marked paid".into(),
            ));
        }
        invoice.status = InvoiceStatus::Paid;
        invoice.paid_at = Some(now);
        invoice.payment_reference = reference;
        write_json(&self.doc_path::<Invoice>(&id.to_string()), &invoice)?;
        Ok(invoice)
    }

    /// Create the numbered invoice **and** advance the schedule under one
    /// lock (review B2): either both happen or neither, so a failed write
    /// can never double-bill a period or silently skip it.
    pub fn create_invoice_and_advance(
        &self,
        invoice: Invoice,
        schedule: &RecurringSchedule,
    ) -> Result<Invoice, StoreError> {
        let _guard = self.write_lock()?;
        let invoice = self.create_invoice_inner(invoice)?;
        std::fs::create_dir_all(self.root.join("schedules"))?;
        write_json(
            &self.doc_path::<RecurringSchedule>(&schedule.id.to_string()),
            schedule,
        )?;
        Ok(invoice)
    }

    /// Write an entry in one locked step; when the date moved, the old
    /// day-folder document is removed in the same transaction (review B4:
    /// never an entry living in two folders).
    pub fn save_entry(
        &self,
        existing_date: Option<NaiveDate>,
        updated: &Entry,
    ) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.day_dir(updated.date))?;
        write_json(&self.entry_path(updated.date, updated.id), updated)?;
        if let Some(old_date) = existing_date
            && old_date != updated.date
        {
            let old = self.entry_path(old_date, updated.id);
            if old.exists() {
                std::fs::remove_file(old)?;
            }
        }
        Ok(())
    }

    /// Log the timer's entry and clear the timer under one lock: a failure
    /// leaves the timer intact rather than double-logging on retry.
    pub fn finish_timer(&self, user_id: Uuid, entry: &Entry) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.day_dir(entry.date))?;
        write_json(&self.entry_path(entry.date, entry.id), entry)?;
        let path = self.doc_path::<Timer>(&user_id.to_string());
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// First-user (bootstrap) creation: the has-users check and the write
    /// share one lock, closing the double-admin TOCTOU (review A12/B4).
    pub fn put_user_if_none(&self, user: &User) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        if !self.list_users()?.is_empty() {
            return Err(StoreError::AlreadyExists("initialised".into()));
        }
        write_json(&self.doc_path::<User>(&user.id.to_string()), user)?;
        self.index_user_locked(user)
    }

    /// Number + persist an invoice with the lock **already** held.
    fn create_invoice_inner(&self, mut invoice: Invoice) -> Result<Invoice, StoreError> {
        std::fs::create_dir_all(self.root.join("invoices"))?;
        let seq_path = self.root.join("invoices").join(".seq.json");
        let stored: u64 = read_json(&seq_path)?.unwrap_or(0);
        let n = stored.max(self.list_invoices()?.len() as u64) + 1;
        write_json(&seq_path, &n)?;
        invoice.number = format!("INV-{n:04}");
        write_json(&self.doc_path::<Invoice>(&invoice.id.to_string()), &invoice)?;
        self.index_invoice_locked(&invoice)?;
        Ok(invoice)
    }
}

fn prune_tmp_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".tmp"))
            && let Ok(md) = p.metadata()
            && md.is_file()
        {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Read a directory and return its document paths, bounded by `max` (#50).
/// Sorting by name keeps listings stable across platforms. Dot files (state
/// such as `invoices/.seq.json`) and crashed-write `*.tmp` leftovers (review
/// D6) are **not** documents: they are skipped here and pruned at open.
fn dir_entries(dir: &Path, max: usize) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            !(name.starts_with('.') || name.ends_with(".tmp"))
        })
        .collect();
    if paths.len() > max {
        return Err(StoreError::TooManyItems);
    }
    paths.sort();
    Ok(paths)
}

fn read_json<T>(path: &Path) -> Result<Option<T>, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    match std::fs::read(path) {
        Ok(bytes) => {
            let value = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Io(format!("corrupt document {}: {e}", path.display())))?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json<T>(path: &Path, value: &T) -> Result<(), StoreError>
where
    T: serde::Serialize + ?Sized,
{
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| StoreError::Io(format!("serialisation failed: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cust() -> Customer {
        Customer {
            id: Uuid::new_v4(),
            name: "X".into(),
            currency: crate::domain::Currency("EUR".into()),
            default_rate_minor: 1,
            active: true,
            email: String::new(),
        }
    }

    #[test]
    fn collection_cap_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data"))
            .unwrap()
            .with_max_docs(2);
        store.put_customer(&cust()).unwrap();
        store.put_customer(&cust()).unwrap();
        store.put_customer(&cust()).unwrap(); // 3rd file
        let err = store.list_customers().expect_err("3 > cap of 2");
        assert!(matches!(err, StoreError::TooManyItems), "{err:?}");
    }
}

#[cfg(test)]
mod invoice_seq_tests {
    use super::*;
    use crate::domain::{Currency, Invoice, InvoiceStatus};

    fn draft(number: &str) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            number: number.into(),
            customer_id: Uuid::new_v4(),
            currency: Currency("EUR".into()),
            period_from: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            lines: vec![],
            total_minor: 0,
            status: InvoiceStatus::Draft,
            created_at: chrono::Utc::now(),
            issued_at: None,
            due_date: None,
            paid_at: None,
            payment_reference: String::new(),
        }
    }

    #[test]
    fn deleted_invoice_never_frees_its_number() {
        // Review B3: number must be monotonic, not count-derived.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let a = store.create_invoice(draft("_")).unwrap();
        let b = store.create_invoice(draft("_")).unwrap();
        assert_eq!(
            (a.number.as_str(), b.number.as_str()),
            ("INV-0001", "INV-0002")
        );
        store.delete_invoice(a.id).unwrap();
        let c = store.create_invoice(draft("_")).unwrap();
        assert_eq!(c.number, "INV-0003", "numbers must not be recycled");
        // The counter file is not mistaken for an invoice document.
        assert_eq!(store.list_invoices().unwrap().len(), 2);
    }
}

#[cfg(test)]
mod txn_tests {
    use super::*;
    use crate::domain::{Currency, InvoiceLine, InvoiceStatus, LineKind};
    use chrono::TimeZone;

    fn invoice() -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            number: "_".into(),
            customer_id: Uuid::new_v4(),
            currency: Currency("EUR".into()),
            period_from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            lines: vec![InvoiceLine {
                kind: LineKind::Fixed,
                date: NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
                entry_id: None,
                expense_id: None,
                project_code: None,
                task_code: None,
                hours: None,
                rate_minor: None,
                amount_minor: 100,
                note: String::new(),
            }],
            total_minor: 100,
            status: InvoiceStatus::Draft,
            created_at: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
            issued_at: None,
            due_date: None,
            paid_at: None,
            payment_reference: String::new(),
        }
    }

    fn user(email: &str) -> User {
        User {
            id: Uuid::new_v4(),
            name: "U".into(),
            email: email.into(),
            role: crate::auth::Role::Member,
            active: true,
            default_rate_minor: 0,
            cost_rate_minor: 0,
            password_hash: String::new(),
            created_at: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn put_user_if_none_is_atomic_first_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        store.put_user_if_none(&user("a@b.co")).unwrap();
        let err = store.put_user_if_none(&user("c@d.co")).unwrap_err();
        assert!(matches!(err, StoreError::AlreadyExists(_)));
        assert_eq!(store.list_users().unwrap().len(), 1);
    }

    #[test]
    fn issue_then_pay_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let inv = store.create_invoice(invoice()).unwrap();
        // Cannot pay a draft.
        let e = store
            .pay_invoice(inv.id, Utc::now(), "ref".into())
            .unwrap_err();
        assert!(matches!(e, StoreError::Conflict(_)));
        store
            .issue_invoice(
                inv.id,
                Utc::now(),
                NaiveDate::from_ymd_opt(2026, 10, 21).unwrap(),
            )
            .unwrap();
        let paid = store
            .pay_invoice(inv.id, Utc::now(), "stripe:1".into())
            .unwrap();
        assert_eq!(paid.status, InvoiceStatus::Paid);
        assert_eq!(paid.payment_reference, "stripe:1");
        // Double issue now conflicts.
        assert!(matches!(
            store.issue_invoice(
                inv.id,
                Utc::now(),
                NaiveDate::from_ymd_opt(2026, 10, 21).unwrap()
            ),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn update_json_rel_is_locked_rmw() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let out: Vec<u32> = store
            .update_json_rel("counter.json", |cur: Option<Vec<u32>>| {
                let mut v = cur.unwrap_or_default();
                v.push(v.len() as u32 + 1);
                v
            })
            .unwrap();
        assert_eq!(out, vec![1]);
        let out = store
            .update_json_rel("counter.json", |cur: Option<Vec<u32>>| {
                let mut v = cur.unwrap_or_default();
                v.push(v.len() as u32 + 1);
                v
            })
            .unwrap();
        assert_eq!(out, vec![1, 2]);
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::domain::{Invoice, InvoiceStatus};
    use chrono::TimeZone;

    fn user(email: &str) -> User {
        User {
            id: Uuid::new_v4(),
            name: "U".into(),
            email: email.into(),
            role: crate::auth::Role::Member,
            active: true,
            default_rate_minor: 0,
            cost_rate_minor: 0,
            password_hash: String::new(),
            created_at: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
        }
    }

    fn draft() -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            number: String::new(),
            customer_id: Uuid::new_v4(),
            currency: crate::domain::Currency("EUR".into()),
            period_from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            lines: vec![],
            total_minor: 0,
            status: InvoiceStatus::Draft,
            created_at: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
            issued_at: None,
            due_date: None,
            paid_at: None,
            payment_reference: String::new(),
        }
    }

    #[test]
    fn email_index_serves_lookup_and_self_heals() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let u = user("someone@Example.ORG");
        store.put_user(&u).unwrap();
        // Fast path (also case-insensitive).
        assert_eq!(
            store
                .get_user_by_email("someone@example.org")
                .unwrap()
                .unwrap()
                .id,
            u.id
        );
        // Simulate a pre-index data dir: the scan still finds the user and
        // the index is rebuilt.
        std::fs::remove_file(store.root.join("users").join(".idx.emails.json")).unwrap();
        assert_eq!(
            store
                .get_user_by_email("someone@example.org")
                .unwrap()
                .unwrap()
                .id,
            u.id
        );
        assert!(store.root.join("users").join(".idx.emails.json").exists());
        // Delete unindexes; unknown email stays None without rebuilding churn.
        store.delete_user(u.id).unwrap();
        assert!(
            store
                .get_user_by_email("someone@example.org")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invoice_number_index_find_create_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let a = store.create_invoice(draft()).unwrap();
        assert_eq!(a.number, "INV-0001");
        assert_eq!(
            store
                .find_invoice_by_number("INV-0001")
                .unwrap()
                .unwrap()
                .id,
            a.id
        );
        assert!(store.find_invoice_by_number("INV-0002").unwrap().is_none());
        // The index dot file is never mistaken for an invoice document.
        assert_eq!(store.list_invoices().unwrap().len(), 1);
        // Delete drops the mapping; a deleted number is not recycled anyway.
        store.delete_invoice(a.id).unwrap();
        assert!(store.find_invoice_by_number("INV-0001").unwrap().is_none());
        // Pre-index directory shape: rebuild happens on first hit.
        let b = store.create_invoice(draft()).unwrap();
        std::fs::remove_file(store.root.join("invoices").join(".idx.numbers.json")).unwrap();
        assert_eq!(
            store.find_invoice_by_number(&b.number).unwrap().unwrap().id,
            b.id
        );
        assert!(
            store
                .root
                .join("invoices")
                .join(".idx.numbers.json")
                .exists()
        );
    }

    #[test]
    fn bootstrap_indexes_the_first_admin() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        store.put_user_if_none(&user("admin@x.co")).unwrap();
        assert!(store.get_user_by_email("admin@x.co").unwrap().is_some());
    }
}
