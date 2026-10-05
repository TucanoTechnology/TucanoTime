// The REST API: routing, extractors and handlers. Handlers validate, then
// hand off to `store`; nothing touches the filesystem except through it, and
// nothing returns internal detail to a client.
//
// This module owns the shared surface — `AppState`, the request extractors,
// the route guards and the cross-cutting helpers the handlers reuse. The
// endpoint bodies live in the feature submodules below, split along the
// section banners this file carried until #100. `lib.rs` keeps the central
// route table; the `pub use` re-exports at the bottom keep `api::<handler>`
// paths stable.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::{self, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::{
    PublicUser, Role, Session, User, normalise_email, token_from_cookie_header, valid_password,
    verify_password,
};
use crate::domain::{
    Category, CategoryInput, ClaimInput, ClaimState, Currency, Customer, CustomerInput, Entry,
    EntryInput, Expense, ExpenseClaim, ExpenseInput, FieldError, Hours, Invoice, InvoiceError,
    InvoiceStatus, Project, ProjectCode, ProjectInput, Source, StartTimerInput, Task, TaskInput,
    Timer, elapsed_hundredths, generate_invoice, validate_category_input, validate_customer_input,
    validate_entry_input, validate_expense_input, validate_project_input, validate_task_input,
};
use crate::domain::{Submission, SubmissionState};
use crate::error::ApiError;
use crate::report;
use crate::store::Store;

pub mod auth;
pub mod entries_reports;
pub mod expenses;
pub mod invoicing;
pub mod people;
pub mod timer_calendar;
pub mod vault_config;

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
    pub rate: Arc<crate::ratelimit::RateLimiter>,
    pub audit: Arc<crate::audit::AuditLog>,
    pub revocations: Arc<crate::revoke::Revocations>,
    pub vault: Option<Arc<crate::vault::SecretVault>>,
    /// Email transport (#35). Disabled unless SMTP is configured; tests inject
    /// a recording sender.
    pub email: Arc<dyn crate::email::EmailSender>,
    /// Payment providers (#34). Empty registry disables the endpoints.
    pub payments: Arc<crate::payments::PaymentRegistry>,
    /// Accounting sync providers (#33). Empty registry disables the endpoints.
    pub accounting: Arc<crate::accounting::AccountingRegistry>,
    /// Identity providers for SSO (#32). Empty registry disables SSO.
    pub sso: Arc<crate::sso::SsoRegistry>,
    /// Pending OAuth calendar consent flows (#36).
    pub oauth_flows: Arc<crate::calendar_oauth::OAuthFlows>,
    /// Externalised runtime configuration (#94): env > config.json > default.
    /// A `Mutex` swap so `PUT /admin/config` can hot-update request-time knobs.
    pub cfg: Arc<std::sync::Mutex<Arc<crate::appconfig::AppConfig>>>,
}

impl AppState {
    /// Convenience wiring with an ephemeral session key (dev/tests). Production
    /// uses `with_session` and a configured secret.
    pub fn new(store: Store) -> Self {
        Self::with_session(store, Arc::new(ephemeral_session()))
    }

    pub fn with_session(store: Store, session: Arc<Session>) -> Self {
        // Lenient path for tests/dev: resolve config + vault from env, ignoring
        // errors (main() uses `boot`, which arrives with hard-checked values).
        let cfg = Arc::new(
            crate::appconfig::AppConfig::load(store.root()).unwrap_or_else(|e| {
                tracing::error!("config.json unusable ({e}); starting with defaults");
                crate::appconfig::AppConfig::empty()
            }),
        );
        let vault = crate::vault::open_for_boot(
            store.root(),
            std::env::var("TUCANO_SECRET_KEY").ok(),
            std::env::var("TUCANO_SECRET_KEY_FILE").ok(),
        )
        .ok()
        .flatten();
        Self::boot(store, session, vault, cfg)
    }

    /// The canonical constructor (#94): main() resolves config + vault with
    /// fail-fast policy and hands them over already validated.
    #[must_use]
    pub fn boot(
        store: Store,
        session: Arc<Session>,
        vault: Option<Arc<crate::vault::SecretVault>>,
        cfg: Arc<crate::appconfig::AppConfig>,
    ) -> Self {
        let store = Arc::new(store);
        let locks = Arc::new(crate::lock::CombinedLocks::new(vec![
            Box::new(crate::lock::InvoiceLock::new(store.clone())),
            Box::new(crate::lock::SubmissionLock::new(store.clone())),
        ]));
        let audit = Arc::new(crate::audit::AuditLog::new(store.root()));
        let revocations = Arc::new(crate::revoke::Revocations::new(store.root()));
        // Email adapter (#35): SMTP if configured (vault or env), else disabled.
        let email: Arc<dyn crate::email::EmailSender> =
            match crate::email::SmtpConfig::from_sources(vault.as_deref()) {
                Some(cfg) => Arc::new(crate::email::SmtpEmailSender::new(cfg)),
                None => Arc::new(crate::email::DisabledEmailSender),
            };
        // Payment providers (#34): enabled when their webhook secrets exist.
        let payments = crate::payments::registry_from_vault(vault.as_deref());
        // Accounting providers (#33): enabled when an OAuth token exists.
        let accounting = crate::accounting::registry_from_vault(vault.as_deref(), &cfg);
        // SSO identity providers (#32): enabled when OIDC/SAML config exists.
        let sso = Arc::new(crate::sso::registry_from_vault(vault.as_deref(), &cfg));
        Self {
            clock: Arc::new(crate::clock::SystemClock),
            locks,
            session,
            // Lock a login identity after 8 failures within 5 minutes (#47).
            rate: Arc::new(crate::ratelimit::RateLimiter::new(
                8,
                std::time::Duration::from_secs(300),
            )),
            audit,
            revocations,
            vault,
            email,
            payments,
            accounting,
            sso,
            oauth_flows: Arc::new(crate::calendar_oauth::OAuthFlows::new()),
            cfg: Arc::new(std::sync::Mutex::new(cfg)),
            store,
        }
    }

    /// Snapshot of the live configuration (#94).
    #[must_use]
    pub fn cfg(&self) -> Arc<crate::appconfig::AppConfig> {
        match self.cfg.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Hot-swaps the configuration after a `PUT /admin/config` (#94).
    pub fn reload_config(&self, cfg: Arc<crate::appconfig::AppConfig>) {
        match self.cfg.lock() {
            Ok(mut g) => *g = cfg,
            Err(p) => *p.into_inner() = cfg,
        }
    }

    /// Swaps the email transport (tests inject a `RecordingEmailSender`).
    #[must_use]
    pub fn with_email(mut self, sender: Arc<dyn crate::email::EmailSender>) -> Self {
        self.email = sender;
        self
    }

    /// Swaps the payment registry (tests inject providers with known secrets).
    #[must_use]
    pub fn with_payments(mut self, registry: Arc<crate::payments::PaymentRegistry>) -> Self {
        self.payments = registry;
        self
    }

    /// Swaps the accounting registry (tests inject stub transports).
    #[must_use]
    pub fn with_accounting(mut self, registry: Arc<crate::accounting::AccountingRegistry>) -> Self {
        self.accounting = registry;
        self
    }

    /// Swaps the SSO registry (tests inject stub identity providers).
    #[must_use]
    pub fn with_sso(mut self, registry: Arc<crate::sso::SsoRegistry>) -> Self {
        self.sso = registry;
        self
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
    let claims = state
        .session
        .verify(token, state.clock.now())
        .ok_or_else(unauth)?;
    if state.revocations.is_revoked(&claims.jti, state.clock.now()) {
        return Err(unauth()); // logged out (#45)
    }
    let user = state.store.get_user(claims.uid)?.ok_or_else(unauth)?;
    if !user.active {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "disabled",
            "account is disabled",
        ));
    }
    Ok(user)
}

/// Private-per-user (#51): an admin sees/edits everything; a member only their
/// own records. `owner` is the record's `user_id` (None = legacy/unattributed).
fn visible_to(actor: &User, owner: Option<Uuid>) -> bool {
    actor.role == Role::Admin || owner == Some(actor.id)
}

/// Admin-only action (#51): approvals and invoicing.
fn ensure_admin(actor: &User) -> Result<(), ApiError> {
    if actor.role == Role::Admin {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrator role required",
        ))
    }
}

// ----------------------------------------------------------- shared helpers --

/// Non-identity attributes for creating a user.
pub(crate) struct NewUser {
    pub(crate) role: Role,
    pub(crate) active: bool,
    pub(crate) default_rate_minor: u64,
    pub(crate) cost_rate_minor: u64,
}

pub(crate) fn new_user(
    name: &str,
    email: &str,
    password: &str,
    p: NewUser,
    app: &AppState,
) -> Result<User, ApiError> {
    let password_hash = crate::auth::hash_password(password)
        .map_err(|_| ApiError::internal("password hashing failed".into()))?;
    let user = User {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        email: email.to_owned(),
        role: p.role,
        active: p.active,
        default_rate_minor: p.default_rate_minor,
        cost_rate_minor: p.cost_rate_minor,
        password_hash,
        created_at: app.clock.now(),
    };
    app.store.put_user(&user)?;
    Ok(user)
}

/// Shared `#[serde(default = ...)]` target: an optional boolean that defaults
/// to active/true.
pub(crate) fn default_active_true() -> bool {
    true
}

pub(crate) fn get_customer(store: &Store, id: Uuid) -> Result<Customer, ApiError> {
    store
        .get_customer(id)?
        .ok_or_else(|| ApiError::not_found("customer"))
}

pub(crate) fn entry_json(e: &Entry) -> serde_json::Value {
    serde_json::to_value(e).unwrap_or(serde_json::Value::Null)
}

/// Run blocking work on the blocking pool and join it, in one place (the
/// `spawn_blocking` + `JoinError` boilerplate lived in four handlers). The
/// closure maps its own domain error into the endpoint's `ApiError`; a
/// panicking task arrives here as the standard internal 500 — logged with
/// context, never leaked to the client.
pub(crate) async fn blocking<T: Send + 'static>(
    ctx: &str,
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ApiError::internal(format!("{ctx} panicked")))?
}

/// The payments/accounting/sso provider lookup shape: a disabled or unknown
/// provider is a 400 naming the kind, byte-identical to the three copies this
/// replaces.
pub(crate) fn provider_or_400<T>(found: Option<T>, kind: &str) -> Result<T, ApiError> {
    found.ok_or_else(|| ApiError::bad_request(format!("unknown or disabled {kind} provider")))
}

pub(crate) fn parse_date(s: &str) -> Result<chrono::NaiveDate, ApiError> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| ApiError::bad_request(format!("'{s}' is not a date in YYYY-MM-DD form")))
}

/// Resolve an optional `from`/`to` query pair, shared by the typed range
/// queries below. Same messages as the old map-based `parse_range`.
pub(crate) fn parse_range(
    from: Option<&String>,
    to: Option<&String>,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate), ApiError> {
    let missing = || ApiError::bad_request("query needs 'from' and 'to'");
    let (from, to) = (from.ok_or_else(missing)?, to.ok_or_else(missing)?);
    let (from, to) = (parse_date(from)?, parse_date(to)?);
    if from > to {
        return Err(ApiError::bad_request("'from' must not be after 'to'"));
    }
    Ok((from, to))
}

/// `?from=YYYY-MM-DD&to=YYYY-MM-DD` as a typed query (#100), replacing the
/// hand-rolled `Query<HashMap>` + manual lookups. Unknown params stay
/// tolerated and missing ones keep the documented message.
#[derive(Debug, serde::Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    pub(crate) from: Option<String>,
    #[serde(default)]
    pub(crate) to: Option<String>,
}

impl RangeQuery {
    /// Resolve both bounds; same errors as the map-based original.
    pub(crate) fn dates(&self) -> Result<(chrono::NaiveDate, chrono::NaiveDate), ApiError> {
        parse_range(self.from.as_ref(), self.to.as_ref())
    }
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

// Re-exports keep the central route table in `lib.rs` (and `crate::api::…`
// paths elsewhere in the crate) working unchanged after the #100 split.
pub use self::auth::{
    BootstrapInput, LoginInput, SsoAssertionInput, audit_log, auth_status, bootstrap, login,
    logout, me, sso_assertion, sso_providers,
};
pub use self::entries_reports::{
    budget_report, create_entry, delete_entry, export_csv, get_entry, list_entries, profitability,
    summary, update_entry,
};
pub use self::expenses::{
    ClaimDecisionInput, DecisionInput, SubmitInput, create_category, create_claim, create_expense,
    create_submission, decide_claim, decide_submission, delete_category, delete_expense,
    get_expense_handler, list_categories, list_claims, list_expenses, list_submissions,
    submit_claim,
};
pub use self::invoicing::{
    CheckoutInput, InvoiceInput, PayInput, SyncInput, create_checkout, create_invoice,
    delete_invoice, get_invoice_handler, invoice_document, invoice_export_csv, invoice_pdf,
    invoice_report_handler, invoice_summary, invoice_template_get, invoice_template_put,
    issue_invoice, list_invoices, money_for_email, pay_invoice, payment_webhook,
    send_invoice_email, send_invoice_email_copy, sync_invoice, sync_status,
};
pub use self::people::{
    UserInput, create_customer, create_project, create_task, create_user, delete_customer,
    delete_project, delete_task, delete_user, get_customer_handler, get_project_handler,
    get_task_handler, list_customers, list_projects, list_tasks, list_users, update_customer,
    update_project, update_task,
};
pub use self::timer_calendar::{
    ScheduleInput, calendar_events, calendar_oauth_callback, calendar_oauth_start, create_schedule,
    delete_schedule, discard_timer, get_timer, list_notifications, list_schedules,
    mark_notifications_read, start_timer, stop_timer,
};
pub use self::vault_config::{
    SecretInput, admin_config, delete_secret, list_secrets, set_secret, update_config,
};
