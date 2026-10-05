// File-based persistence. No database: every entity is one pretty-printed
// JSON document on disk, mirroring the conceptual organisation of the data
// (a customer is a folder that contains its projects; entries live in a
// per-day folder). The API is the only actor that writes below the data dir.
//
// Path components are never taken raw from a request: customer files are
// named by `Uuid`, project files by a validated `ProjectCode`, entry
// directories by a validated `YYYY-MM-DD` date and entry files by `Uuid`.
// Traversal input therefore cannot reach the filesystem.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use fs2::FileExt;
use uuid::Uuid;

use crate::auth::User;
use crate::domain::{
    Category, Customer, Entry, Expense, ExpenseClaim, Invoice, Notification, Project,
    RecurringSchedule, Submission, Task, Timer,
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

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with_lock_timeout(root, DEFAULT_LOCK_TIMEOUT)
    }

    pub fn with_lock_timeout(
        root: impl AsRef<Path>,
        lock_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
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

    // ---------------------------------------------------------------- users --

    fn user_path(&self, id: Uuid) -> PathBuf {
        self.root.join("users").join(format!("{id}.json"))
    }

    pub fn list_users(&self) -> Result<Vec<User>, StoreError> {
        let mut out = Vec::new();
        for path in dir_entries(&self.root.join("users"), self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(u) = read_json::<User>(&path)? {
                out.push(u);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn has_users(&self) -> Result<bool, StoreError> {
        Ok(!self.list_users()?.is_empty())
    }

    pub fn get_user(&self, id: Uuid) -> Result<Option<User>, StoreError> {
        read_json(&self.user_path(id))
    }

    pub fn get_user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        Ok(self
            .list_users()?
            .into_iter()
            .find(|u| u.email.eq_ignore_ascii_case(email)))
    }

    pub fn put_user(&self, user: &User) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        write_json(&self.user_path(user.id), user)
    }

    pub fn delete_user(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.user_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // ------------------------------------------------------------- invoices --

    fn invoice_path(&self, id: Uuid) -> PathBuf {
        self.root.join("invoices").join(format!("{id}.json"))
    }

    pub fn list_invoices(&self) -> Result<Vec<Invoice>, StoreError> {
        let dir = self.root.join("invoices");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(inv) = read_json::<Invoice>(&path)? {
                out.push(inv);
            }
        }
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.number.cmp(&b.number))
        });
        Ok(out)
    }

    pub fn get_invoice(&self, id: Uuid) -> Result<Option<Invoice>, StoreError> {
        read_json(&self.invoice_path(id))
    }

    /// First invoice with this number (numbers are sequential but a restored
    /// file could duplicate one; the earliest wins). Used by payment webhooks
    /// (#34), which know the invoice by its human number.
    pub fn find_invoice_by_number(&self, number: &str) -> Result<Option<Invoice>, StoreError> {
        let mut found: Option<Invoice> = None;
        for inv in self.list_invoices()? {
            if inv.number == number && found.as_ref().is_none_or(|f| inv.created_at < f.created_at)
            {
                found = Some(inv);
            }
        }
        Ok(found)
    }

    pub fn put_invoice(&self, invoice: &Invoice) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.root.join("invoices"))?;
        write_json(&self.invoice_path(invoice.id), invoice)
    }

    pub fn delete_invoice(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.invoice_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    /// Next sequential invoice number (`INV-0001`, …).
    pub fn next_invoice_number(&self) -> Result<String, StoreError> {
        let n = self.list_invoices()?.len() + 1;
        Ok(format!("INV-{n:04}"))
    }

    /// Atomically assign the next invoice number and persist it under the
    /// writer lock, so two processes never mint the same number (#62). The
    /// caller passes an invoice with a placeholder number.
    pub fn create_invoice(&self, mut invoice: Invoice) -> Result<Invoice, StoreError> {
        let _guard = self.write_lock()?;
        let n = self.list_invoices()?.len() + 1;
        invoice.number = format!("INV-{n:04}");
        std::fs::create_dir_all(self.root.join("invoices"))?;
        write_json(&self.invoice_path(invoice.id), &invoice)?;
        Ok(invoice)
    }

    // ----------------------------------------------------------- categories --

    fn category_path(&self, id: Uuid) -> PathBuf {
        self.root.join("categories").join(format!("{id}.json"))
    }

    pub fn list_categories(&self) -> Result<Vec<Category>, StoreError> {
        let mut out = Vec::new();
        for path in dir_entries(&self.root.join("categories"), self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(c) = read_json::<Category>(&path)? {
                out.push(c);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get_category(&self, id: Uuid) -> Result<Option<Category>, StoreError> {
        read_json(&self.category_path(id))
    }

    pub fn put_category(&self, category: &Category) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        write_json(&self.category_path(category.id), category)
    }

    pub fn delete_category(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.category_path(id);
        if !path.exists() {
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
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // ------------------------------------------------------------- expenses --

    fn expense_path(&self, id: Uuid) -> PathBuf {
        self.root.join("expenses").join(format!("{id}.json"))
    }

    pub fn list_expenses(&self) -> Result<Vec<Expense>, StoreError> {
        let mut out = Vec::new();
        for path in dir_entries(&self.root.join("expenses"), self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(e) = read_json::<Expense>(&path)? {
                out.push(e);
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse((e.date, e.created_at)));
        Ok(out)
    }

    pub fn get_expense(&self, id: Uuid) -> Result<Option<Expense>, StoreError> {
        read_json(&self.expense_path(id))
    }

    pub fn put_expense(&self, expense: &Expense) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        write_json(&self.expense_path(expense.id), expense)
    }

    pub fn delete_expense(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.expense_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // ---------------------------------------------------------- submissions --

    fn submission_path(&self, id: Uuid) -> PathBuf {
        self.root.join("submissions").join(format!("{id}.json"))
    }

    pub fn list_submissions(&self) -> Result<Vec<Submission>, StoreError> {
        let dir = self.root.join("submissions");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(s) = read_json::<Submission>(&path)? {
                out.push(s);
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.week_start));
        Ok(out)
    }

    pub fn get_submission(&self, id: Uuid) -> Result<Option<Submission>, StoreError> {
        read_json(&self.submission_path(id))
    }

    pub fn put_submission(&self, submission: &Submission) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.root.join("submissions"))?;
        write_json(&self.submission_path(submission.id), submission)
    }

    // --------------------------------------------------------------- claims --

    fn claim_path(&self, id: Uuid) -> PathBuf {
        self.root.join("claims").join(format!("{id}.json"))
    }

    pub fn list_claims(&self) -> Result<Vec<ExpenseClaim>, StoreError> {
        let dir = self.root.join("claims");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(c) = read_json::<ExpenseClaim>(&path)? {
                out.push(c);
            }
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.created_at));
        Ok(out)
    }

    pub fn get_claim(&self, id: Uuid) -> Result<Option<ExpenseClaim>, StoreError> {
        read_json(&self.claim_path(id))
    }

    pub fn put_claim(&self, claim: &ExpenseClaim) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.root.join("claims"))?;
        write_json(&self.claim_path(claim.id), claim)
    }

    // ---------------------------------------------------------------- timers --

    fn timer_path(&self, user_id: Uuid) -> PathBuf {
        self.root.join("timers").join(format!("{user_id}.json"))
    }

    pub fn get_timer(&self, user_id: Uuid) -> Result<Option<Timer>, StoreError> {
        read_json(&self.timer_path(user_id))
    }

    pub fn put_timer(&self, timer: &Timer) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.root.join("timers"))?;
        write_json(&self.timer_path(timer.user_id), timer)
    }

    pub fn delete_timer(&self, user_id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.timer_path(user_id);
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

    fn schedule_path(&self, id: Uuid) -> PathBuf {
        self.root.join("schedules").join(format!("{id}.json"))
    }

    pub fn list_schedules(&self) -> Result<Vec<RecurringSchedule>, StoreError> {
        let dir = self.root.join("schedules");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(s) = read_json::<RecurringSchedule>(&path)? {
                out.push(s);
            }
        }
        out.sort_by_key(|s| s.created_at);
        Ok(out)
    }

    pub fn get_schedule(&self, id: Uuid) -> Result<Option<RecurringSchedule>, StoreError> {
        read_json(&self.schedule_path(id))
    }

    pub fn put_schedule(&self, s: &RecurringSchedule) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        std::fs::create_dir_all(self.root.join("schedules"))?;
        write_json(&self.schedule_path(s.id), s)
    }

    pub fn delete_schedule(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.schedule_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // ------------------------------------------------------------ customers --

    fn customer_path(&self, id: Uuid) -> PathBuf {
        self.root.join("customers").join(format!("{id}.json"))
    }

    pub fn list_customers(&self) -> Result<Vec<Customer>, StoreError> {
        let mut out = Vec::new();
        for path in dir_entries(&self.root.join("customers"), self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(c) = read_json::<Customer>(&path)? {
                out.push(c);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get_customer(&self, id: Uuid) -> Result<Option<Customer>, StoreError> {
        read_json(&self.customer_path(id))
    }

    pub fn put_customer(&self, customer: &Customer) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        write_json(&self.customer_path(customer.id), customer)
    }

    pub fn delete_customer(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self.write_lock()?;
        let path = self.customer_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound);
        }
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
        std::fs::remove_dir_all(self.root.join("customers").join(id.to_string()))?;
        std::fs::remove_file(&path)?;
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
        let mut out = Vec::new();
        let dir = self.projects_dir(customer_id);
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(p) = self.read_project(&path, &customer)? {
                out.push(p);
            }
        }
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
        let dir = self.tasks_dir(customer_id, project_code);
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(t) = read_json::<Task>(&path)? {
                out.push(t);
            }
        }
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
        let dir = self.day_dir(date);
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for path in dir_entries(&dir, self.max_docs)? {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(e) = read_json::<Entry>(&path)? {
                out.push(e);
            }
        }
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

/// Read a directory and return its entry paths, bounded by `max` (#50).
/// Sorting by name keeps listings stable across platforms.
fn dir_entries(dir: &Path, max: usize) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
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
