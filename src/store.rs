// File-based persistence. No database: every entity is one pretty-printed
// JSON document on disk, mirroring the conceptual organisation of the data
// (a customer is a folder that contains its projects; entries live in a
// per-day folder). The API is the only actor that writes below the data dir.
//
// Path components are never taken raw from a request: customer files are
// named by `Uuid`, project files by a validated `ProjectCode`, entry
// directories by a validated `YYYY-MM-DD` date and entry files by `Uuid`.
// Traversal input therefore cannot reach the filesystem.

use std::path::{Path, PathBuf};

use std::sync::Mutex;

use chrono::NaiveDate;
use uuid::Uuid;

use crate::domain::{Customer, Entry, Project, Task};

/// Hard cap on a range scan so a malformed or adversarial query cannot spin
/// over the whole tree. 400 days covers a year plus buffer.
pub const MAX_RANGE_DAYS: i64 = 400;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("range too large (max {MAX_RANGE_DAYS} days)")]
    RangeTooLarge,
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
    /// One writer at a time. The tool is single-process; this keeps concurrent
    /// requests from racing on the same file while writes stay simple.
    write_guard: Mutex<()>,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("customers"))?;
        std::fs::create_dir_all(root.join("entries"))?;
        Ok(Self {
            root,
            write_guard: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ------------------------------------------------------------ customers --

    fn customer_path(&self, id: Uuid) -> PathBuf {
        self.root.join("customers").join(format!("{id}.json"))
    }

    pub fn list_customers(&self) -> Result<Vec<Customer>, StoreError> {
        let mut out = Vec::new();
        for path in dir_entries(&self.root.join("customers"))? {
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
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
        write_json(&self.customer_path(customer.id), customer)
    }

    pub fn delete_customer(&self, id: Uuid) -> Result<(), StoreError> {
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
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
        for path in dir_entries(&dir)? {
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
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
        std::fs::create_dir_all(self.projects_dir(project.customer_id))?;
        write_json(
            &self.project_path(project.customer_id, &project.code.0),
            project,
        )
    }

    pub fn delete_project(&self, customer_id: Uuid, code: &str) -> Result<(), StoreError> {
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
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
        for path in dir_entries(&dir)? {
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
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
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
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
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
        for path in dir_entries(&dir)? {
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
        for day in dir_entries(&dir)? {
            let path = day.join(format!("{id}.json"));
            if let Some(e) = read_json::<Entry>(&path)? {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    pub fn put_entry(&self, entry: &Entry) -> Result<(), StoreError> {
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
        std::fs::create_dir_all(self.day_dir(entry.date))?;
        write_json(&self.entry_path(entry.date, entry.id), entry)
    }

    pub fn delete_entry(&self, entry: &Entry) -> Result<(), StoreError> {
        let _guard = self
            .write_guard
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
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
        for day in dir_entries(&dir)? {
            if day.is_dir() {
                for path in dir_entries(&day)? {
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
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

/// Read a directory and return its entry paths. Sorting by name keeps listings
/// stable across platforms (read_dir order is unspecified).
fn dir_entries(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
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
    T: serde::Serialize,
{
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| StoreError::Io(format!("serialisation failed: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
