// The REST API: routing, extractors and handlers. Handlers validate, then
// hand off to `store`; nothing touches the filesystem except through it, and
// nothing returns internal detail to a client.

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::domain::{
    Currency, Customer, CustomerInput, Entry, EntryInput, FieldError, Project, ProjectCode,
    ProjectInput, Task, TaskInput, validate_customer_input, validate_entry_input,
    validate_project_input, validate_task_input,
};
use crate::error::ApiError;
use crate::report;
use crate::store::Store;

pub type ApiResult = Result<Response, ApiError>;

/// JSON body that must arrive as exactly one unknown-field-rejecting object.
pub struct ValidJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ValidJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let body = Bytes::from_request(req, state)
            .await
            .map_err(|_| ApiError::bad_request("failed to read the request body"))?;
        let value = serde_json::from_slice::<T>(&body).map_err(deser_error)?;
        Ok(ValidJson(value))
    }
}

/// Turn a serde failure into the shared 422 shape, naming the field when the
/// message carries one (`unknown field \`x\`` / `invalid type ... for field \`y\``).
fn deser_error(e: serde_json::Error) -> ApiError {
    let msg = e.to_string();
    let field = msg
        .split('`')
        .nth(1)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_owned();
    let (code, fields) = if msg.contains("unknown field") {
        (
            "validation_failed",
            vec![FieldError::new(field, "unknown field")],
        )
    } else {
        (
            "validation_failed",
            vec![FieldError::new(field, "wrong type or format")],
        )
    };
    ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code,
        message: "payload failed validation".into(),
        fields,
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: std::sync::Arc<Store>,
    pub clock: std::sync::Arc<dyn crate::clock::Clock>,
    pub locks: std::sync::Arc<dyn crate::lock::EntryLock>,
}

impl AppState {
    /// Production wiring: real clock, no locks yet (invoices/submissions land
    /// later and register an `EntryLock` provider here).
    pub fn new(store: Store) -> Self {
        Self {
            store: std::sync::Arc::new(store),
            clock: std::sync::Arc::new(crate::clock::SystemClock),
            locks: std::sync::Arc::new(crate::lock::NoLocks),
        }
    }
}

pub(crate) fn parse_date(s: &str) -> Result<chrono::NaiveDate, ApiError> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| ApiError::bad_request(format!("'{s}' is not a date in YYYY-MM-DD form")))
}

fn parse_range(
    q: &HashMap<String, String>,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate), ApiError> {
    let from = q
        .get("from")
        .ok_or_else(|| ApiError::bad_request("query needs 'from' and 'to'"))?;
    let to = q
        .get("to")
        .ok_or_else(|| ApiError::bad_request("query needs 'from' and 'to'"))?;
    let (from, to) = (parse_date(from)?, parse_date(to)?);
    if from > to {
        return Err(ApiError::bad_request("'from' must not be after 'to'"));
    }
    Ok((from, to))
}

// ------------------------------------------------------------- customers --

pub async fn list_customers(State(app): State<AppState>) -> ApiResult {
    let customers = app.store.list_customers()?;
    Ok(Json(serde_json::json!({ "customers": customers })).into_response())
}

pub async fn create_customer(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<CustomerInput>,
) -> ApiResult {
    let draft = validate_customer_input(&input).map_err(ApiError::validation)?;
    let customer = Customer {
        id: Uuid::new_v4(),
        name: draft.name,
        currency: Currency(draft.currency),
        default_rate_minor: draft.default_rate_minor,
        active: draft.active,
    };
    app.store.put_customer(&customer)?;
    Ok((StatusCode::CREATED, Json(&customer)).into_response())
}

fn get_customer(store: &Store, id: Uuid) -> Result<Customer, ApiError> {
    store
        .get_customer(id)?
        .ok_or_else(|| ApiError::not_found("customer"))
}

pub async fn get_customer_handler(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    Ok(Json(get_customer(&app.store, id)?).into_response())
}

pub async fn update_customer(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<CustomerInput>,
) -> ApiResult {
    get_customer(&app.store, id)?;
    let draft = validate_customer_input(&input).map_err(ApiError::validation)?;
    let updated = Customer {
        id,
        name: draft.name,
        currency: Currency(draft.currency),
        default_rate_minor: draft.default_rate_minor,
        active: draft.active,
    };
    app.store.put_customer(&updated)?;
    Ok(Json(&updated).into_response())
}

pub async fn delete_customer(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_customer(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// -------------------------------------------------------------- projects --

pub async fn list_projects(State(app): State<AppState>, Path(cid): Path<Uuid>) -> ApiResult {
    get_customer(&app.store, cid)?;
    let projects = app.store.list_projects(cid)?;
    Ok(Json(serde_json::json!({ "projects": projects })).into_response())
}

pub async fn create_project(
    State(app): State<AppState>,
    Path(cid): Path<Uuid>,
    ValidJson(input): ValidJson<ProjectInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    let draft = validate_project_input(&input).map_err(ApiError::validation)?;
    if app.store.get_project(cid, &draft.code)?.is_some() {
        return Err(ApiError::conflict(format!(
            "project {} already exists",
            draft.code
        )));
    }
    let project = build_project(cid, &draft);
    app.store.put_project(&project)?;
    Ok((StatusCode::CREATED, Json(&project)).into_response())
}

fn build_project(cid: Uuid, draft: &crate::domain::ProjectDraft) -> Project {
    Project {
        customer_id: cid,
        code: ProjectCode(draft.code.clone()),
        name: draft.name.clone(),
        currency: Currency(draft.currency.clone()),
        rate_minor: draft.rate_minor,
        active: draft.active,
    }
}

pub async fn get_project_handler(
    State(app): State<AppState>,
    Path((cid, code)): Path<(Uuid, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    let project = app
        .store
        .get_project(cid, &code.0)?
        .ok_or_else(|| ApiError::not_found("project"))?;
    Ok(Json(project).into_response())
}

pub async fn update_project(
    State(app): State<AppState>,
    Path((cid, code)): Path<(Uuid, ProjectCode)>,
    ValidJson(input): ValidJson<ProjectInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_project(cid, &code.0)?.is_none() {
        return Err(ApiError::not_found("project"));
    }
    if input.code != code {
        return Err(ApiError::validation(vec![FieldError::new(
            "code",
            "path is authoritative; body code must match it",
        )]));
    }
    let draft = validate_project_input(&input).map_err(ApiError::validation)?;
    let project = build_project(cid, &draft);
    app.store.put_project(&project)?;
    Ok(Json(&project).into_response())
}

pub async fn delete_project(
    State(app): State<AppState>,
    Path((cid, code)): Path<(Uuid, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    app.store.delete_project(cid, &code.0)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ----------------------------------------------------------------- tasks --

pub async fn list_tasks(
    State(app): State<AppState>,
    Path((cid, pcode)): Path<(Uuid, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_project(cid, &pcode.0)?.is_none() {
        return Err(ApiError::not_found("project"));
    }
    let tasks = app.store.list_tasks(cid, &pcode.0)?;
    Ok(Json(serde_json::json!({ "tasks": tasks })).into_response())
}

pub async fn create_task(
    State(app): State<AppState>,
    Path((cid, pcode)): Path<(Uuid, ProjectCode)>,
    ValidJson(input): ValidJson<TaskInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_project(cid, &pcode.0)?.is_none() {
        return Err(ApiError::not_found("project"));
    }
    let draft = validate_task_input(&input).map_err(ApiError::validation)?;
    if app.store.get_task(cid, &pcode.0, &draft.code)?.is_some() {
        return Err(ApiError::conflict(format!(
            "task {} already exists",
            draft.code
        )));
    }
    let task = build_task(cid, &pcode, &draft);
    app.store.put_task(&task)?;
    Ok((StatusCode::CREATED, Json(&task)).into_response())
}

fn build_task(cid: Uuid, pcode: &ProjectCode, draft: &crate::domain::TaskDraft) -> Task {
    Task {
        customer_id: cid,
        project_code: pcode.clone(),
        code: ProjectCode(draft.code.clone()),
        name: draft.name.clone(),
        currency: draft.currency.as_ref().map(|c| Currency(c.clone())),
        rate_minor: draft.rate_minor,
        active: draft.active,
    }
}

pub async fn get_task_handler(
    State(app): State<AppState>,
    Path((cid, pcode, code)): Path<(Uuid, ProjectCode, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    let task = app
        .store
        .get_task(cid, &pcode.0, &code.0)?
        .ok_or_else(|| ApiError::not_found("task"))?;
    Ok(Json(task).into_response())
}

pub async fn update_task(
    State(app): State<AppState>,
    Path((cid, pcode, code)): Path<(Uuid, ProjectCode, ProjectCode)>,
    ValidJson(input): ValidJson<TaskInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_task(cid, &pcode.0, &code.0)?.is_none() {
        return Err(ApiError::not_found("task"));
    }
    if input.code != code {
        return Err(ApiError::validation(vec![FieldError::new(
            "code",
            "path is authoritative; body code must match it",
        )]));
    }
    let draft = validate_task_input(&input).map_err(ApiError::validation)?;
    let task = build_task(cid, &pcode, &draft);
    app.store.put_task(&task)?;
    Ok(Json(&task).into_response())
}

pub async fn delete_task(
    State(app): State<AppState>,
    Path((cid, pcode, code)): Path<(Uuid, ProjectCode, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    app.store.delete_task(cid, &pcode.0, &code.0)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --------------------------------------------------------------- entries --

/// Entry cross-references: the customer must exist and the project must
/// belong to it. Violations are field errors (422), not 404s, because the
/// payload names them.
fn validate_entry_refs(
    store: &Store,
    draft: &crate::domain::EntryDraft,
) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    let customer = store.get_customer(draft.customer_id).ok().flatten();
    if customer.is_none() {
        errors.push(FieldError::new("customer_id", "customer does not exist"));
    }
    let project = store
        .get_project(draft.customer_id, &draft.project_code)
        .ok()
        .flatten();
    if project.is_none() {
        errors.push(FieldError::new(
            "project_code",
            "project does not exist for this customer",
        ));
    }
    // A task, when present, must belong to the entry's project.
    if let Some(task_code) = &draft.task_code {
        let task = store
            .get_task(draft.customer_id, &draft.project_code, task_code)
            .ok()
            .flatten();
        if task.is_none() {
            errors.push(FieldError::new(
                "task_code",
                "task does not exist for this project",
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub async fn list_entries(
    State(app): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let entries = match (q.get("date"), q.get("from"), q.get("to")) {
        (Some(date), None, None) => app.store.list_by_date(parse_date(date)?)?,
        (None, Some(_), Some(_)) => {
            let (from, to) = parse_range(&q)?;
            app.store.list_range(from, to)?
        }
        _ => {
            return Err(ApiError::bad_request(
                "query needs 'date' or 'from' and 'to'",
            ));
        }
    };
    let items: Vec<serde_json::Value> = entries.iter().map(entry_json).collect();
    Ok(Json(serde_json::json!({ "entries": items })).into_response())
}

fn entry_json(e: &Entry) -> serde_json::Value {
    serde_json::to_value(e).unwrap_or(serde_json::Value::Null)
}

pub async fn create_entry(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<EntryInput>,
) -> ApiResult {
    let draft = validate_entry_input(&input).map_err(ApiError::validation)?;
    validate_entry_refs(&app.store, &draft).map_err(ApiError::validation)?;
    let now = app.clock.now();
    let entry = Entry {
        id: Uuid::new_v4(),
        date: draft.date,
        customer_id: draft.customer_id,
        project_code: ProjectCode(draft.project_code),
        task_code: draft.task_code.map(ProjectCode),
        hours: draft.hours,
        note: draft.note,
        billable: draft.billable,
        source: crate::domain::Source::Manual,
        created_at: now,
        updated_at: now,
    };
    app.store.put_entry(&entry)?;
    Ok((StatusCode::CREATED, Json(entry_json(&entry))).into_response())
}

pub async fn get_entry(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let entry = app
        .store
        .get_entry(id)?
        .ok_or_else(|| ApiError::not_found("entry"))?;
    Ok(Json(entry_json(&entry)).into_response())
}

pub async fn update_entry(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<EntryInput>,
) -> ApiResult {
    let existing = app
        .store
        .get_entry(id)?
        .ok_or_else(|| ApiError::not_found("entry"))?;
    if let Some(reason) = app.locks.entry_lock(id) {
        return Err(ApiError::conflict(reason.message()));
    }
    let draft = validate_entry_input(&input).map_err(ApiError::validation)?;
    validate_entry_refs(&app.store, &draft).map_err(ApiError::validation)?;
    let updated = Entry {
        id,
        date: draft.date,
        customer_id: draft.customer_id,
        project_code: ProjectCode(draft.project_code),
        task_code: draft.task_code.map(ProjectCode),
        hours: draft.hours,
        note: draft.note,
        billable: draft.billable,
        source: existing.source,
        created_at: existing.created_at,
        updated_at: app.clock.now(),
    };
    // Moving an entry between days relocates its document: write the new
    // home, then remove the old one.
    app.store.put_entry(&updated)?;
    if updated.date != existing.date {
        let _ = app.store.delete_entry(&existing);
    }
    Ok(Json(entry_json(&updated)).into_response())
}

pub async fn delete_entry(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let entry = app
        .store
        .get_entry(id)?
        .ok_or_else(|| ApiError::not_found("entry"))?;
    if let Some(reason) = app.locks.entry_lock(id) {
        return Err(ApiError::conflict(reason.message()));
    }
    app.store.delete_entry(&entry)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --------------------------------------------------------------- reports --

pub async fn summary(
    State(app): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let (from, to) = parse_range(&q)?;
    let group = q.get("group").map(String::as_str).unwrap_or("customer");
    let kind = match group {
        "customer" => report::Group::Customer,
        "project" => report::Group::Project,
        "week" => report::Group::Week,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown group '{other}' (use customer, project or week)"
            )));
        }
    };
    let entries = app.store.list_range(from, to)?;
    let customers: Vec<Customer> = app.store.list_customers()?.into_iter().collect();
    let (projects, tasks) = gather_hierarchy(&app, &customers)?;
    let rows = report::summarise(&entries, &customers, &projects, &tasks, kind);
    Ok(Json(rows).into_response())
}

/// Every project and task for a set of customers, loaded once for reports.
type Hierarchy = (Vec<(Uuid, Project)>, Vec<Task>);

/// Load every project and task once, for report rate resolution.
fn gather_hierarchy(app: &AppState, customers: &[Customer]) -> Result<Hierarchy, ApiError> {
    let mut projects: Vec<(Uuid, Project)> = Vec::new();
    let mut tasks: Vec<Task> = Vec::new();
    for c in customers {
        for p in app.store.list_projects(c.id)? {
            tasks.extend(app.store.list_tasks(c.id, &p.code.0)?);
            projects.push((c.id, p));
        }
    }
    Ok((projects, tasks))
}

pub async fn export_csv(
    State(app): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let (from, to) = parse_range(&q)?;
    let filter = match q.get("customer_id") {
        Some(s) => Some(
            Uuid::parse_str(s)
                .map_err(|_| ApiError::bad_request("'customer_id' must be a UUID"))?,
        ),
        None => None,
    };
    let entries = app.store.list_range(from, to)?;
    let customers = app.store.list_customers()?;
    let (projects, tasks) = gather_hierarchy(&app, &customers)?;
    let csv = report::export_csv(&entries, &customers, &projects, &tasks, filter);
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        csv,
    )
        .into_response())
}

pub async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}
