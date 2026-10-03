// The REST API: routing, extractors and handlers. Handlers validate, then
// hand off to `store`; nothing touches the filesystem except through it, and
// nothing returns internal detail to a client.

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::{self, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::{
    self, PublicUser, Role, Session, User, normalise_email, token_from_cookie_header,
    valid_password, verify_password,
};
use crate::domain::{
    Category, CategoryInput, Currency, Customer, CustomerInput, Entry, EntryInput, Expense,
    ExpenseInput, FieldError, InvoiceError, InvoiceStatus, Project, ProjectCode, ProjectInput,
    Task, TaskInput, generate_invoice, validate_category_input, validate_customer_input,
    validate_entry_input, validate_expense_input, validate_project_input, validate_task_input,
};
use crate::domain::{Submission, SubmissionState};
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

/// The authenticated user, resolved from the session cookie.
#[derive(Debug, Clone)]
pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        authenticate(state, &parts.headers).map(AuthUser)
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
    pub store: Arc<Store>,
    pub clock: Arc<dyn crate::clock::Clock>,
    pub locks: Arc<dyn crate::lock::EntryLock>,
    pub session: Arc<Session>,
}

impl AppState {
    /// Convenience wiring with an ephemeral session key (dev/tests). Production
    /// uses `with_session` and a configured secret.
    pub fn new(store: Store) -> Self {
        Self::with_session(store, Arc::new(ephemeral_session()))
    }

    pub fn with_session(store: Store, session: Arc<Session>) -> Self {
        let store = Arc::new(store);
        let locks = Arc::new(crate::lock::CombinedLocks::new(vec![
            Box::new(crate::lock::InvoiceLock::new(store.clone())),
            Box::new(crate::lock::SubmissionLock::new(store.clone())),
        ]));
        Self {
            store,
            clock: Arc::new(crate::clock::SystemClock),
            locks,
            session,
        }
    }
}

/// A random per-process session key; sessions do not survive a restart. A
/// hardening ticket covers requiring a configured secret in production.
fn ephemeral_session() -> Session {
    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    Session::new(key.to_vec(), 60 * 60 * 24, false)
}

/// Resolve the current user from the request's session cookie.
fn authenticate(state: &AppState, headers: &http::HeaderMap) -> Result<User, ApiError> {
    let unauth = || {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "authentication required",
        )
    };
    let header = headers
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(unauth)?;
    let token = token_from_cookie_header(header).ok_or_else(unauth)?;
    let uid = state
        .session
        .verify(token, state.clock.now())
        .ok_or_else(unauth)?;
    let user = state.store.get_user(uid)?.ok_or_else(unauth)?;
    if !user.active {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "disabled",
            "account is disabled",
        ));
    }
    Ok(user)
}

// ------------------------------------------------------------------- auth --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginInput {
    email: String,
    password: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapInput {
    name: String,
    email: String,
    password: String,
}

fn set_cookie(state: &AppState, user: &User) -> (http::HeaderMap, PublicUser) {
    let token = state.session.issue(user, state.clock.now());
    let mut headers = http::HeaderMap::new();
    if let Ok(v) = state.session.cookie(&token, 60 * 60 * 24).parse() {
        headers.insert(http::header::SET_COOKIE, v);
    }
    (headers, PublicUser::from(user))
}

/// Create the first administrator. Open only while no users exist.
pub async fn bootstrap(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<BootstrapInput>,
) -> ApiResult {
    if app.store.has_users()? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "initialised",
            "an administrator already exists",
        ));
    }
    let email = normalise_email(&input.email)
        .ok_or_else(|| ApiError::validation(vec![FieldError::new("email", "invalid email")]))?;
    if !valid_password(&input.password) {
        return Err(ApiError::validation(vec![FieldError::new(
            "password",
            "must be at least 8 characters",
        )]));
    }
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(ApiError::validation(vec![FieldError::new(
            "name",
            "required, at most 120 characters",
        )]));
    }
    let user = new_user(name, &email, &input.password, Role::Admin, true, 0, &app)?;
    let (headers, pubuser) = set_cookie(&app, &user);
    Ok((StatusCode::CREATED, headers, Json(pubuser)).into_response())
}

pub async fn login(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<LoginInput>,
) -> ApiResult {
    let email = normalise_email(&input.email).unwrap_or_default();
    let user = app.store.get_user_by_email(&email)?;
    let ok = user
        .as_ref()
        .is_some_and(|u| u.active && verify_password(&input.password, &u.password_hash));
    if !ok {
        // Same response for unknown email and bad password (no user enumeration).
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid email or password",
        ));
    }
    let (headers, pubuser) = set_cookie(&app, &user.unwrap());
    Ok((headers, Json(pubuser)).into_response())
}

pub async fn logout(State(app): State<AppState>) -> ApiResult {
    let mut headers = http::HeaderMap::new();
    let cookie = format!(
        "{}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0",
        auth::SESSION_COOKIE
    );
    if app.session.secure() {
        // Rebuild with Secure when configured.
        let cookie = format!("{cookie}; Secure");
        if let Ok(v) = cookie.parse() {
            headers.insert(http::header::SET_COOKIE, v);
        }
    } else if let Ok(v) = cookie.parse() {
        headers.insert(http::header::SET_COOKIE, v);
    }
    Ok((headers, StatusCode::NO_CONTENT).into_response())
}

pub async fn me(State(app): State<AppState>, req: axum::extract::Request) -> ApiResult {
    let user = authenticate(&app, req.headers())?;
    Ok(Json(PublicUser::from(&user)).into_response())
}

/// Public: whether an administrator exists yet (drives first-run setup UI).
pub async fn auth_status(State(app): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::json!({ "initialised": app.store.has_users()? })).into_response())
}

fn new_user(
    name: &str,
    email: &str,
    password: &str,
    role: Role,
    active: bool,
    default_rate_minor: u64,
    app: &AppState,
) -> Result<User, ApiError> {
    let password_hash = auth::hash_password(password)
        .map_err(|_| ApiError::internal("password hashing failed".into()))?;
    let user = User {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        email: email.to_owned(),
        role,
        active,
        default_rate_minor,
        password_hash,
        created_at: app.clock.now(),
    };
    app.store.put_user(&user)?;
    Ok(user)
}

// ------------------------------------------------------------------ users --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserInput {
    name: String,
    email: String,
    password: String,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default = "default_active_true")]
    active: bool,
    #[serde(default)]
    default_rate_minor: u64,
}

fn default_active_true() -> bool {
    true
}

pub async fn list_users(State(app): State<AppState>) -> ApiResult {
    let users: Vec<PublicUser> = app
        .store
        .list_users()?
        .iter()
        .map(PublicUser::from)
        .collect();
    Ok(Json(serde_json::json!({ "users": users })).into_response())
}

pub async fn create_user(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<UserInput>,
) -> ApiResult {
    let email = normalise_email(&input.email)
        .ok_or_else(|| ApiError::validation(vec![FieldError::new("email", "invalid email")]))?;
    if app.store.get_user_by_email(&email)?.is_some() {
        return Err(ApiError::conflict("a user with that email exists"));
    }
    if !valid_password(&input.password) {
        return Err(ApiError::validation(vec![FieldError::new(
            "password",
            "must be at least 8 characters",
        )]));
    }
    let user = new_user(
        input.name.trim(),
        &email,
        &input.password,
        input.role.unwrap_or(Role::Member),
        input.active,
        input.default_rate_minor,
        &app,
    )?;
    Ok((StatusCode::CREATED, Json(PublicUser::from(&user))).into_response())
}

pub async fn delete_user(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult {
    if actor.0.id == id {
        return Err(ApiError::conflict("you cannot delete your own account"));
    }
    app.store.delete_user(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --------------------------------------------------------------- invoices --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceInput {
    pub customer_id: Uuid,
    pub from: String,
    pub to: String,
}

pub async fn list_invoices(State(app): State<AppState>) -> ApiResult {
    let invoices = app.store.list_invoices()?;
    Ok(Json(serde_json::json!({ "invoices": invoices })).into_response())
}

/// Generate a draft invoice from the billable, not-yet-invoiced work in a period.
pub async fn create_invoice(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<InvoiceInput>,
) -> ApiResult {
    let customer = get_customer(&app.store, input.customer_id)?;
    let from = parse_date(&input.from)?;
    let to = parse_date(&input.to)?;
    if from > to {
        return Err(ApiError::bad_request("'from' must not be after 'to'"));
    }
    let projects = app.store.list_projects(customer.id)?;
    let mut tasks = Vec::new();
    for p in &projects {
        tasks.extend(app.store.list_tasks(customer.id, &p.code.0)?);
    }
    let users = app.store.list_users()?;
    let entries = app.store.list_range(from, to)?;
    // Entries already on an issued invoice are excluded from a new one.
    let excluded: Vec<Uuid> = app
        .store
        .list_invoices()?
        .iter()
        .filter(|i| i.status == InvoiceStatus::Issued)
        .flat_map(|i| i.lines.iter().map(|l| l.entry_id))
        .collect();
    let number = app.store.next_invoice_number()?;
    let sources = crate::domain::InvoiceSources {
        projects: &projects,
        tasks: &tasks,
        users: &users,
        entries: &entries,
        excluded: &excluded,
    };
    let invoice = generate_invoice(number, &customer, &sources, from, to, app.clock.now())
        .map_err(invoice_error)?;
    app.store.put_invoice(&invoice)?;
    Ok((StatusCode::CREATED, Json(invoice)).into_response())
}

fn invoice_error(e: InvoiceError) -> ApiError {
    match e {
        InvoiceError::NothingToInvoice => {
            ApiError::conflict("no billable, not-yet-invoiced work in this period")
        }
        InvoiceError::MixedCurrency { a, b } => ApiError::conflict(format!(
            "the period mixes currencies ({a} and {b}); an invoice is single-currency"
        )),
    }
}

pub async fn get_invoice_handler(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let invoice = app
        .store
        .get_invoice(id)?
        .ok_or_else(|| ApiError::not_found("invoice"))?;
    Ok(Json(invoice).into_response())
}

/// Issue a draft invoice: this locks its entries from edits/deletes (#18 seam).
pub async fn issue_invoice(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let mut invoice = app
        .store
        .get_invoice(id)?
        .ok_or_else(|| ApiError::not_found("invoice"))?;
    if invoice.status != InvoiceStatus::Draft {
        return Err(ApiError::conflict("only a draft invoice can be issued"));
    }
    invoice.status = InvoiceStatus::Issued;
    invoice.issued_at = Some(app.clock.now());
    app.store.put_invoice(&invoice)?;
    Ok(Json(invoice).into_response())
}

pub async fn delete_invoice(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let invoice = app
        .store
        .get_invoice(id)?
        .ok_or_else(|| ApiError::not_found("invoice"))?;
    if invoice.status != InvoiceStatus::Draft {
        return Err(ApiError::conflict("only a draft invoice can be deleted"));
    }
    app.store.delete_invoice(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// -------------------------------------------------------------- expenses --

pub async fn list_categories(State(app): State<AppState>) -> ApiResult {
    let categories = app.store.list_categories()?;
    Ok(Json(serde_json::json!({ "categories": categories })).into_response())
}

pub async fn create_category(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<CategoryInput>,
) -> ApiResult {
    let draft = validate_category_input(&input).map_err(ApiError::validation)?;
    let category = Category {
        id: Uuid::new_v4(),
        name: draft.name,
        default_billable: draft.default_billable,
        active: draft.active,
    };
    app.store.put_category(&category)?;
    Ok((StatusCode::CREATED, Json(category)).into_response())
}

pub async fn delete_category(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_category(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn list_expenses(
    State(app): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let mut expenses = app.store.list_expenses()?;
    if let Some(cid) = q.get("customer_id") {
        let cid = Uuid::parse_str(cid)
            .map_err(|_| ApiError::bad_request("'customer_id' must be a UUID"))?;
        expenses.retain(|e| e.customer_id == cid);
    }
    if let (Some(from), Some(to)) = (q.get("from"), q.get("to")) {
        let (from, to) = (parse_date(from)?, parse_date(to)?);
        expenses.retain(|e| e.date >= from && e.date <= to);
    }
    Ok(Json(serde_json::json!({ "expenses": expenses })).into_response())
}

pub async fn create_expense(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<ExpenseInput>,
) -> ApiResult {
    let draft = validate_expense_input(&input).map_err(ApiError::validation)?;
    let mut errors = Vec::new();
    if app.store.get_customer(draft.customer_id)?.is_none() {
        errors.push(FieldError::new("customer_id", "customer does not exist"));
    }
    if let Some(code) = &draft.project_code
        && app.store.get_project(draft.customer_id, code)?.is_none()
    {
        errors.push(FieldError::new(
            "project_code",
            "project does not exist for this customer",
        ));
    }
    if let Some(cat) = draft.category_id
        && app.store.get_category(cat)?.is_none()
    {
        errors.push(FieldError::new("category_id", "category does not exist"));
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let now = app.clock.now();
    let expense = Expense {
        id: Uuid::new_v4(),
        date: draft.date,
        customer_id: draft.customer_id,
        project_code: draft.project_code.map(ProjectCode),
        category_id: draft.category_id,
        amount_minor: draft.amount_minor,
        currency: Currency(draft.currency),
        billable: draft.billable,
        note: draft.note,
        receipt_name: draft.receipt_name,
        receipt_b64: draft.receipt_b64,
        created_at: now,
        updated_at: now,
    };
    app.store.put_expense(&expense)?;
    Ok((StatusCode::CREATED, Json(expense)).into_response())
}

pub async fn get_expense_handler(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let expense = app
        .store
        .get_expense(id)?
        .ok_or_else(|| ApiError::not_found("expense"))?;
    Ok(Json(expense).into_response())
}

pub async fn delete_expense(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_expense(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ----------------------------------------------------------- submissions --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitInput {
    pub week_start: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInput {
    pub decision: String,
    #[serde(default)]
    pub comment: String,
}

pub async fn list_submissions(State(app): State<AppState>) -> ApiResult {
    let submissions = app.store.list_submissions()?;
    Ok(Json(serde_json::json!({ "submissions": submissions })).into_response())
}

/// Submit the acting user's timesheet for a week; locks its entries.
pub async fn create_submission(
    State(app): State<AppState>,
    actor: AuthUser,
    ValidJson(input): ValidJson<SubmitInput>,
) -> ApiResult {
    let week_start = parse_date(&input.week_start)?;
    let week_end = week_start
        .checked_add_days(chrono::Days::new(6))
        .ok_or_else(|| ApiError::bad_request("invalid week"))?;
    let all = app.store.list_range(week_start, week_end)?;
    // Entries already locked (submitted/approved) can't be resubmitted.
    let entry_ids: Vec<Uuid> = all
        .iter()
        .filter(|e| e.user_id == Some(actor.0.id))
        .map(|e| e.id)
        .filter(|id| app.locks.entry_lock(*id).is_none())
        .collect();
    if entry_ids.is_empty() {
        return Err(ApiError::conflict(
            "no unlocked entries to submit this week",
        ));
    }
    let now = app.clock.now();
    let submission = Submission {
        id: Uuid::new_v4(),
        user_id: actor.0.id,
        week_start,
        week_end,
        state: SubmissionState::Submitted,
        entry_ids,
        comment: String::new(),
        created_at: now,
        submitted_at: Some(now),
        decided_at: None,
    };
    app.store.put_submission(&submission)?;
    Ok((StatusCode::CREATED, Json(submission)).into_response())
}

/// Approve or reject a submitted timesheet. Rejecting releases its locks.
pub async fn decide_submission(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<DecisionInput>,
) -> ApiResult {
    let mut submission = app
        .store
        .get_submission(id)?
        .ok_or_else(|| ApiError::not_found("submission"))?;
    if submission.state != SubmissionState::Submitted {
        return Err(ApiError::conflict(
            "only a submitted timesheet can be decided",
        ));
    }
    submission.state = match input.decision.as_str() {
        "approve" => SubmissionState::Approved,
        "reject" => SubmissionState::Rejected,
        other => {
            return Err(ApiError::validation(vec![FieldError::new(
                "decision",
                format!("'{other}' is not approve or reject"),
            )]));
        }
    };
    submission.comment = input.comment;
    submission.decided_at = Some(app.clock.now());
    app.store.put_submission(&submission)?;
    Ok(Json(submission).into_response())
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
    actor: AuthUser,
    ValidJson(input): ValidJson<EntryInput>,
) -> ApiResult {
    let draft = validate_entry_input(&input).map_err(ApiError::validation)?;
    validate_entry_refs(&app.store, &draft).map_err(ApiError::validation)?;
    let now = app.clock.now();
    let entry = Entry {
        id: Uuid::new_v4(),
        date: draft.date,
        customer_id: draft.customer_id,
        user_id: Some(actor.0.id),
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
        user_id: existing.user_id,
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
        "person" => report::Group::Person,
        "week" => report::Group::Week,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown group '{other}' (use customer, project, person or week)"
            )));
        }
    };
    let billable_filter = match q.get("billable").map(String::as_str) {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "'billable' must be true or false, got '{other}'"
            )));
        }
    };
    let entries = app.store.list_range(from, to)?;
    let customers: Vec<Customer> = app.store.list_customers()?.into_iter().collect();
    let (projects, tasks, users) = gather_hierarchy(&app, &customers)?;
    let rows = report::summarise(
        &entries,
        &customers,
        &projects,
        &tasks,
        &users,
        kind,
        billable_filter,
    );
    Ok(Json(rows).into_response())
}

/// Every project, task and user for a set of customers, loaded once for reports.
type Hierarchy = (Vec<(Uuid, Project)>, Vec<Task>, Vec<User>);

/// Load projects, tasks and users once, for report rate resolution.
fn gather_hierarchy(app: &AppState, customers: &[Customer]) -> Result<Hierarchy, ApiError> {
    let mut projects: Vec<(Uuid, Project)> = Vec::new();
    let mut tasks: Vec<Task> = Vec::new();
    for c in customers {
        for p in app.store.list_projects(c.id)? {
            tasks.extend(app.store.list_tasks(c.id, &p.code.0)?);
            projects.push((c.id, p));
        }
    }
    let users = app.store.list_users()?;
    Ok((projects, tasks, users))
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
    let (projects, tasks, users) = gather_hierarchy(&app, &customers)?;
    let csv = report::export_csv(&entries, &customers, &projects, &tasks, &users, filter);
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        csv,
    )
        .into_response())
}

/// Route guard: reject with 401 unless a valid session cookie is present.
pub async fn require_auth(
    State(app): State<AppState>,
    mut req: Request,
    next: axum::middleware::Next,
) -> Result<Response, ApiError> {
    let user = authenticate(&app, req.headers())?;
    req.extensions_mut().insert(user);
    Ok(next.run(req).await)
}

/// Route guard: like `require_auth`, plus an admin-role check (403 otherwise).
pub async fn require_admin(
    State(app): State<AppState>,
    mut req: Request,
    next: axum::middleware::Next,
) -> Result<Response, ApiError> {
    let user = authenticate(&app, req.headers())?;
    if user.role != Role::Admin {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrator role required",
        ));
    }
    req.extensions_mut().insert(user);
    Ok(next.run(req).await)
}

pub async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}
