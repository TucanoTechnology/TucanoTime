// Library surface: everything `main.rs` wires up, exposed so integration
// tests can build the router against a temp data dir without spawning a
// process.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rust_embed::RustEmbed;
use tower_http::limit::RequestBodyLimitLayer;

pub mod accounting;
pub mod api;
pub mod appconfig;
pub mod audit;
pub mod auth;
pub mod backup;
pub mod budgets;
pub mod calendar;
pub mod calendar_oauth;
pub mod clock;
pub mod domain;
pub mod email;
pub mod email_reminders;
pub mod error;
pub mod lock;
pub mod payments;
pub mod pdf;
pub mod providers;
pub mod ratelimit;
pub mod recurring;
pub mod reminders;
pub mod report;
pub mod revoke;
pub mod scheduler;
pub mod sso;
pub mod store;
pub mod template;
pub mod vault;

use api::AppState;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

const OPENAPI: &str = include_str!("../openapi.json");

pub fn build_router(state: AppState) -> Router {
    // Public: liveness, the contract + docs, and the auth endpoints. The GUI
    // assets (fallback) are public so the login screen can load.
    let public = Router::new()
        .route("/healthz", get(api::healthz))
        .route("/openapi.json", get(openapi))
        .route("/docs", get(docs))
        .route("/auth/bootstrap", axum::routing::post(api::bootstrap))
        .route("/auth/login", axum::routing::post(api::login))
        .route("/auth/logout", axum::routing::post(api::logout))
        .route("/auth/status", get(api::auth_status))
        .route("/auth/me", get(api::me))
        // SSO (#32): pre-session; the assertion authenticates itself.
        .route("/auth/sso/providers", get(api::sso_providers))
        .route(
            "/auth/sso/assertion",
            axum::routing::post(api::sso_assertion),
        )
        // Provider webhooks: authenticated by signature, not session (#34).
        .route(
            "/payments/webhook/{provider}",
            axum::routing::post(api::payment_webhook),
        );

    // Authenticated: all timesheet data.
    let protected = Router::new()
        .route(
            "/customers",
            get(api::list_customers).post(api::create_customer),
        )
        .route(
            "/customers/{id}",
            get(api::get_customer_handler)
                .put(api::update_customer)
                .delete(api::delete_customer),
        )
        .route(
            "/customers/{cid}/projects",
            get(api::list_projects).post(api::create_project),
        )
        .route(
            "/customers/{cid}/projects/{code}",
            get(api::get_project_handler)
                .put(api::update_project)
                .delete(api::delete_project),
        )
        .route(
            "/customers/{cid}/projects/{pcode}/tasks",
            get(api::list_tasks).post(api::create_task),
        )
        .route(
            "/customers/{cid}/projects/{pcode}/tasks/{code}",
            get(api::get_task_handler)
                .put(api::update_task)
                .delete(api::delete_task),
        )
        .route("/entries", get(api::list_entries).post(api::create_entry))
        .route(
            "/entries/{id}",
            get(api::get_entry)
                .put(api::update_entry)
                .delete(api::delete_entry),
        )
        .route("/reports/summary", get(api::summary))
        .route("/reports/export.csv", get(api::export_csv))
        .route(
            "/categories",
            get(api::list_categories).post(api::create_category),
        )
        .route(
            "/categories/{id}",
            axum::routing::delete(api::delete_category),
        )
        .route(
            "/expenses",
            get(api::list_expenses).post(api::create_expense),
        )
        .route(
            "/expenses/{id}",
            get(api::get_expense_handler).delete(api::delete_expense),
        )
        .route(
            "/submissions",
            get(api::list_submissions).post(api::create_submission),
        )
        .route(
            "/submissions/{id}/decision",
            axum::routing::post(api::decide_submission),
        )
        .route("/claims", get(api::list_claims).post(api::create_claim))
        .route(
            "/claims/{id}/submit",
            axum::routing::post(api::submit_claim),
        )
        .route(
            "/timer",
            get(api::get_timer)
                .post(api::start_timer)
                .delete(api::discard_timer),
        )
        .route("/timer/stop", axum::routing::post(api::stop_timer))
        .route("/calendar/events", get(api::calendar_events))
        .route("/notifications", get(api::list_notifications))
        .route(
            "/notifications/read",
            axum::routing::post(api::mark_notifications_read),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::require_auth,
        ));

    // Admin-only: user management and invoicing (#51).
    let admin = Router::new()
        .route("/users", get(api::list_users).post(api::create_user))
        .route("/users/{id}", axum::routing::delete(api::delete_user))
        .route("/audit", get(api::audit_log))
        // Management reporting (cost rates, budgets) is admin-only (review A4):
        // members see their own work via /reports/summary, not margins.
        .route("/reports/profitability", get(api::profitability))
        .route("/reports/budgets", get(api::budget_report))
        .route(
            "/invoices",
            get(api::list_invoices).post(api::create_invoice),
        )
        .route(
            "/invoices/manual",
            axum::routing::post(api::create_manual_invoice),
        )
        .route(
            "/invoices/{id}",
            get(api::get_invoice_handler)
                .delete(api::delete_invoice)
                .put(api::update_invoice_draft),
        )
        .route(
            "/invoices/{id}/issue",
            axum::routing::post(api::issue_invoice),
        )
        .route("/invoices/{id}/pdf", get(api::invoice_pdf))
        .route("/invoices/{id}/pay", axum::routing::post(api::pay_invoice))
        .route(
            "/invoices/{id}/write-off",
            axum::routing::post(api::write_off_invoice),
        )
        .route(
            "/invoices/{id}/email",
            axum::routing::post(api::send_invoice_email),
        )
        .route(
            "/invoices/{id}/email-copy",
            axum::routing::post(api::send_invoice_email_copy),
        )
        .route(
            "/invoices/{id}/checkout",
            axum::routing::post(api::create_checkout),
        )
        .route(
            "/invoices/{id}/sync",
            axum::routing::post(api::sync_invoice),
        )
        .route("/sync/accounting", get(api::sync_status))
        // Invoice document templates (#116): org singleton + live preview.
        .route(
            "/admin/invoice-template",
            get(api::invoice_template_get).put(api::invoice_template_put),
        )
        // Company identity (#138).
        .route("/admin/org", get(api::org_get).put(api::org_put))
        .route("/invoices/{id}/document", get(api::invoice_document))
        .route("/invoices/summary", get(api::invoice_summary))
        .route("/invoices/report", get(api::invoice_report_handler))
        .route("/invoices/export.csv", get(api::invoice_export_csv))
        .route(
            "/claims/{id}/decision",
            axum::routing::post(api::decide_claim),
        )
        .route(
            "/admin/config",
            get(api::admin_config).put(api::update_config),
        )
        .route("/admin/secrets", get(api::list_secrets))
        .route(
            "/admin/secrets/{key}",
            axum::routing::put(api::set_secret).delete(api::delete_secret),
        )
        .route(
            "/schedules",
            get(api::list_schedules).post(api::create_schedule),
        )
        .route(
            "/schedules/{id}",
            axum::routing::delete(api::delete_schedule).put(api::update_schedule),
        )
        .route("/calendar/oauth/start", get(api::calendar_oauth_start))
        .route(
            "/calendar/oauth/callback",
            get(api::calendar_oauth_callback),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::require_admin,
        ));

    public
        .merge(protected)
        .merge(admin)
        .fallback(static_assets)
        .layer(axum::middleware::from_fn(csrf_guard))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(RequestBodyLimitLayer::new(256 * 1024))
        .with_state(state)
}

/// Baseline security response headers (#48). Swagger UI is vendored under
/// web/ (review A9) so no third-party origin can ever execute in the admin
/// GUI's cookie scope — the CSP is now strictly 'self'.
async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header::{
        CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
    };
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(X_CONTENT_TYPE_OPTIONS, "nosniff".parse().expect("static"));
    h.insert(REFERRER_POLICY, "no-referrer".parse().expect("static"));
    h.insert(X_FRAME_OPTIONS, "DENY".parse().expect("static"));
    h.insert(
        CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         font-src 'self'; connect-src 'self'; frame-ancestors 'none'; \
         base-uri 'self'; form-action 'self'"
            .parse()
            .expect("static"),
    );
    res
}

/// CSRF guard (#46): mutating requests must carry the `X-CSRF-Protection: 1`
/// header. Combined with the SameSite=Lax session cookie, a cross-site form
/// post cannot pass. The pre-auth login/bootstrap endpoints are exempt (no
/// session to abuse yet).
async fn csrf_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let mutating = matches!(
        method,
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::DELETE
    );
    // Login/bootstrap are pre-session; payment webhooks are authenticated by
    // provider signature instead of the cookie+CSRF pair (#34).
    let exempt = path == "/auth/login"
        || path == "/auth/bootstrap"
        || path == "/auth/sso/assertion" // pre-session; the assertion is its own proof (review A6)
        || path.starts_with("/payments/webhook/");
    if mutating && !exempt {
        let ok = req
            .headers()
            .get("x-csrf-protection")
            .and_then(|v| v.to_str().ok())
            == Some("1");
        if !ok {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": { "code": "csrf", "message": "missing X-CSRF-Protection header" }
                })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

async fn openapi() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        OPENAPI,
    )
}

async fn docs() -> Response {
    match Assets::get("swagger.html") {
        Some(file) => (
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            String::from_utf8_lossy(file.data.as_ref()).into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serves the embedded GUI. Unknown non-API paths 404 with the JSON error
/// shape so no path or asset enumeration leaks internals.
async fn static_assets(uri: axum::http::Uri) -> Response {
    let path = match uri.path() {
        "/" => "index.html",
        p => p.trim_start_matches('/'),
    };
    match Assets::get(path) {
        Some(file) => (
            [
                (axum::http::header::CONTENT_TYPE, content_type(path)),
                // Names are stable across image updates, so force a cheap
                // revalidation instead of heuristic caching (review A10).
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            file.data,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": { "code": "not_found", "message": "resource does not exist" }
            })),
        )
            .into_response(),
    }
}

/// Content type by extension. The web set is fixed and owned, so a small map
/// beats pulling a MIME-guessing dependency into the tree.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}
