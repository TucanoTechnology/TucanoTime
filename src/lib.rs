// Library surface: everything `main.rs` wires up, exposed so integration
// tests can build the router against a temp data dir without spawning a
// process.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rust_embed::RustEmbed;
use tower_http::limit::RequestBodyLimitLayer;

pub mod api;
pub mod audit;
pub mod auth;
pub mod clock;
pub mod domain;
pub mod error;
pub mod lock;
pub mod notify;
pub mod ratelimit;
pub mod report;
pub mod revoke;
pub mod store;

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
        .route("/auth/me", get(api::me));

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
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::require_auth,
        ));

    // Admin-only: user management and invoicing (#51).
    let admin = Router::new()
        .route("/users", get(api::list_users).post(api::create_user))
        .route("/users/{id}", axum::routing::delete(api::delete_user))
        .route("/audit", get(api::audit_log))
        .route(
            "/invoices",
            get(api::list_invoices).post(api::create_invoice),
        )
        .route(
            "/invoices/{id}",
            get(api::get_invoice_handler).delete(api::delete_invoice),
        )
        .route(
            "/invoices/{id}/issue",
            axum::routing::post(api::issue_invoice),
        )
        .route("/invoices/{id}/pay", axum::routing::post(api::pay_invoice))
        .route("/invoices/summary", get(api::invoice_summary))
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

/// Baseline security response headers (#48). The docs page loads Swagger UI
/// from unpkg, so the CSP allowlists it for scripts/styles/fonts.
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
        "default-src 'self'; script-src 'self' https://unpkg.com; \
         style-src 'self' 'unsafe-inline' https://unpkg.com; img-src 'self' data:; \
         font-src 'self' https://unpkg.com; connect-src 'self'; frame-ancestors 'none'; \
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
    let exempt = path == "/auth/login" || path == "/auth/bootstrap";
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
            [(axum::http::header::CONTENT_TYPE, content_type(path))],
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
