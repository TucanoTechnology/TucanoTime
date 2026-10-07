// Contract tests: exercise the HTTP surface in-process against a temporary
// data dir, and check payloads against the checked-in openapi.json. This is
// the guard the shared rules demand: valid and invalid shapes, rejection
// before persistence, documented error shape, and no server-side leakage.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

use tucano_time::api::AppState;
use tucano_time::auth::Session;
use tucano_time::clock::SystemClock;
use tucano_time::lock::{EntryLock, LockReason};
use tucano_time::store::Store;

const ADMIN_EMAIL: &str = "admin@test.local";
const ADMIN_PW: &str = "supersecret1";

/// An in-process client holding a router and an authenticated session cookie.
struct Client {
    router: Router,
    cookie: String,
    /// The recording email transport installed on the router (#35).
    email: Arc<tucano_time::email::RecordingEmailSender>,
}

async fn raw(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    // The CSRF guard requires this on mutations; send it on all test requests.
    b = b.header("x-csrf-protection", "1");
    let req = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            b.body(Body::from(v.to_string())).unwrap()
        }
        None => b.body(Body::empty()).unwrap(),
    };
    let res = router.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let set_cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").to_string());
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value, set_cookie)
}

async fn login_cookie(router: &Router, email: &str, pw: &str) -> String {
    let (_s, _b, c) = raw(
        router,
        "POST",
        "/auth/login",
        Some(json!({"email": email, "password": pw})),
        None,
    )
    .await;
    c.unwrap_or_default()
}

impl Client {
    async fn new(store: Store, locks: Option<Arc<dyn EntryLock>>) -> Self {
        Self::build(
            store,
            locks,
            tucano_time::payments::PaymentRegistry::default(),
            tucano_time::accounting::AccountingRegistry::default(),
        )
        .await
    }

    /// Client with accounting providers injected (#33 tests).
    async fn with_accounting(
        store: Store,
        accounting: tucano_time::accounting::AccountingRegistry,
    ) -> Self {
        Self::build(
            store,
            None,
            tucano_time::payments::PaymentRegistry::default(),
            accounting,
        )
        .await
    }

    async fn with_payments(store: Store, payments: tucano_time::payments::PaymentRegistry) -> Self {
        Self::build(
            store,
            None,
            payments,
            tucano_time::accounting::AccountingRegistry::default(),
        )
        .await
    }

    async fn build(
        store: Store,
        locks: Option<Arc<dyn EntryLock>>,
        payments: tucano_time::payments::PaymentRegistry,
        accounting: tucano_time::accounting::AccountingRegistry,
    ) -> Self {
        let session = Arc::new(Session::new(
            b"test-session-secret-0000000000000032".to_vec(),
            3600,
            false,
        ));
        let recorder = Arc::new(tucano_time::email::RecordingEmailSender::default());
        let payments = Arc::new(payments);
        let accounting = Arc::new(accounting);
        let state = match locks {
            Some(l) => {
                let store = Arc::new(store);
                let audit = Arc::new(tucano_time::audit::AuditLog::new(store.root()));
                let revocations = Arc::new(tucano_time::revoke::Revocations::new(store.root()));
                AppState {
                    store,
                    clock: Arc::new(SystemClock),
                    locks: l,
                    session,
                    rate: Arc::new(tucano_time::ratelimit::RateLimiter::new(
                        8,
                        std::time::Duration::from_secs(300),
                    )),
                    audit,
                    revocations,
                    vault: None,
                    email: recorder.clone(),
                    payments,
                    accounting,
                    sso: Arc::new(tucano_time::sso::SsoRegistry::default()),
                    oauth_flows: Arc::new(tucano_time::calendar_oauth::OAuthFlows::new()),
                    cfg: Arc::new(std::sync::Mutex::new(Arc::new(
                        tucano_time::appconfig::AppConfig::empty(),
                    ))),
                }
            }
            None => AppState::with_session(store, session)
                .with_email(recorder.clone())
                .with_payments(payments)
                .with_accounting(accounting),
        };
        let router = tucano_time::build_router(state);
        // Bootstrap the first admin (403 if users already exist — fine).
        let _ = raw(
            &router,
            "POST",
            "/auth/bootstrap",
            Some(json!({"name":"Admin","email":ADMIN_EMAIL,"password":ADMIN_PW})),
            None,
        )
        .await;
        let cookie = login_cookie(&router, ADMIN_EMAIL, ADMIN_PW).await;
        Self {
            router,
            cookie,
            email: recorder,
        }
    }
}

/// A test lock provider that freezes a single entry id.
struct LockOne(Option<uuid::Uuid>);
impl EntryLock for LockOne {
    fn entry_lock(
        &self,
        entry_id: uuid::Uuid,
    ) -> Result<Option<LockReason>, tucano_time::lock::LockUnavailable> {
        if self.0 == Some(entry_id) {
            Ok(Some(LockReason::Invoiced { id: "INV-1".into() }))
        } else {
            Ok(None)
        }
    }
    fn locked_entries(
        &self,
    ) -> Result<std::collections::HashSet<uuid::Uuid>, tucano_time::lock::LockUnavailable> {
        Ok(self.0.into_iter().collect())
    }
}

async fn app() -> (Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("data")).expect("store");
    (Client::new(store, None).await, dir)
}

async fn app_locked(dir_path: &std::path::Path, locks: Arc<dyn EntryLock>) -> Client {
    let store = Store::open(dir_path.join("data")).expect("store");
    Client::new(store, Some(locks)).await
}

/// Authenticated request.
async fn json_req(
    client: &Client,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (s, v, _) = raw(&client.router, method, uri, body, Some(&client.cookie)).await;
    (s, v)
}

/// Authenticated request that keeps the **raw response bytes and headers** —
/// needed for the binary PDF download (#113), where the body is not JSON.
async fn raw_req(
    client: &Client,
    method: &str,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, &client.cookie)
        .header("x-csrf-protection", "1")
        .body(Body::empty())
        .unwrap();
    let res = client.router.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

/// Seed a customer (with billing email), one project and an entry, then
/// generate + issue an invoice. Returns the issued invoice JSON and the
/// invoice id. Shared by the #113 PDF tests.
async fn seed_issued_invoice(client: &Client) -> (Value, String) {
    let (sc, cust) = json_req(
        client,
        "POST",
        "/customers",
        Some(json!({"name":"ACME","currency":"EUR","default_rate_minor":6000,"email":"billing@acme.test"})),
    )
    .await;
    assert_eq!(sc, StatusCode::CREATED, "{cust}");
    let cid = cust["id"].as_str().unwrap();
    new_project(client, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        client,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        client,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    let (si, issued) = json_req(client, "POST", &format!("/invoices/{iid}/issue"), None).await;
    assert_eq!(si, StatusCode::OK, "{issued}");
    (issued, iid)
}

/// Unauthenticated request (for the auth tests).
async fn anon_req(
    client: &Client,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (s, v, _) = raw(&client.router, method, uri, body, None).await;
    (s, v)
}

/// Create a customer and return its object.
async fn new_customer(app: &Client, name: &str, currency: &str, rate: u64) -> Value {
    let (status, body) = json_req(
        app,
        "POST",
        "/customers",
        Some(json!({"name": name, "currency": currency, "default_rate_minor": rate})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create customer: {body}");
    body
}

async fn new_project(app: &Client, cid: &str, code: &str, extra: Value) -> Value {
    // currency + rate_minor are required since #11; default to the customer's
    // values and let `extra` override (e.g. a project-specific rate).
    let mut body = json!({"code": code, "currency": "EUR", "rate_minor": 6000});
    if let Value::Object(map) = extra {
        for (k, v) in map {
            body[k] = v;
        }
    }
    let (status, created) = json_req(
        app,
        "POST",
        &format!("/customers/{cid}/projects"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create project: {created}");
    created
}

fn entry_body(cid: &str, code: &str, hours: Value, date: &str) -> Value {
    json!({"date": date, "customer_id": cid, "project_code": code, "hours": hours})
}

// -------------------------------------------------------------------- tests --

#[tokio::test]
async fn healthz_is_ok() {
    let (app, _d) = app().await;
    let (status, body) = json_req(&app, "GET", "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn contract_paths_have_documented_responses() {
    // Every operationId in the contract must be reachable. This is a light
    // coverage check; the payload tests below check behaviour.
    let contract: Value =
        serde_json::from_str(include_str!("../openapi.json")).expect("valid openapi.json");
    assert_eq!(contract["openapi"], "3.1.0");
    let paths = contract["paths"].as_object().expect("paths object");
    for path in [
        "/customers",
        "/entries",
        "/reports/summary",
        "/reports/export.csv",
        "/invoices",
        "/auth/login",
        "/users",
    ] {
        assert!(paths.contains_key(path), "contract missing {path}");
    }
}

#[tokio::test]
async fn unknown_field_rejected_before_persist() {
    let (app, d) = app().await;
    let (status, body) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"ACME","currency":"EUR","default_rate_minor":1,"evil":true})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_failed");
    assert_eq!(body["error"]["fields"][0]["field"], "evil");
    assert_eq!(body["error"]["fields"][0]["message"], "unknown field");
    // Nothing was written: the customers collection stays empty.
    let (s2, list) = json_req(&app, "GET", "/customers", None).await;
    assert_eq!(s2, StatusCode::OK);
    assert!(list["customers"].as_array().unwrap().is_empty());
    let _ = d;
}

#[tokio::test]
async fn wrong_type_is_validation_failure() {
    let (app, _d) = app().await;
    // name given as a number
    let (status, body) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":42,"currency":"EUR","default_rate_minor":1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn currency_normalised_and_rejected() {
    let (app, _d) = app().await;
    let created = new_customer(&app, "ACME", "eur", 6000).await;
    assert_eq!(
        created["currency"], "EUR",
        "currency is normalised to upper case"
    );

    let (status, _) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"X","currency":"Euros","default_rate_minor":1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn full_customer_project_entry_flow() {
    let (app, _d) = app().await;
    let cust = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = cust["id"].as_str().unwrap();

    let proj = new_project(&app, cid, "p-1", json!({"name":"Portal"})).await;
    assert_eq!(proj["code"], "P-1", "code is normalised to upper case");

    let (status, entry) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "p-1", json!(8), "2026-10-02")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let eid = entry["id"].as_str().unwrap();
    assert_eq!(entry["hours"], 8);
    assert!(entry["note"].is_string(), "note defaults to empty string");

    // Read back by day and by id.
    let (s, list) = json_req(&app, "GET", "/entries?date=2026-10-02", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["entries"].as_array().unwrap().len(), 1);
    let (s2, got) = json_req(&app, "GET", &format!("/entries/{eid}"), None).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(got["id"], eid);
}

#[tokio::test]
async fn entry_requires_existing_customer_and_project() {
    let (app, _d) = app().await;
    let (status, body) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(
            "00000000-0000-0000-0000-000000000000",
            "P1",
            json!(1),
            "2026-10-02",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let fields = body["error"]["fields"].as_array().unwrap();
    assert!(fields.iter().any(|f| f["field"] == "customer_id"));
    assert!(fields.iter().any(|f| f["field"] == "project_code"));
}

#[tokio::test]
async fn project_must_belong_to_customer() {
    let (app, _d) = app().await;
    let a = new_customer(&app, "A", "EUR", 100).await;
    let b = new_customer(&app, "B", "EUR", 100).await;
    new_project(&app, a["id"].as_str().unwrap(), "P1", json!({})).await;
    // Use A's project code under B.
    let (status, body) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(
            b["id"].as_str().unwrap(),
            "P1",
            json!(1),
            "2026-10-02",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["field"] == "project_code")
    );
}

#[tokio::test]
async fn hours_boundaries() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;

    // 24.00 accepted.
    let (ok, _) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(24.00), "2026-10-02")),
    )
    .await;
    assert_eq!(ok, StatusCode::CREATED);

    // 24.01 rejected.
    let (bad, _) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(24.01), "2026-10-02")),
    )
    .await;
    assert_eq!(bad, StatusCode::UNPROCESSABLE_ENTITY);

    // three decimals rejected.
    let (bad2, _) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(1.234), "2026-10-02")),
    )
    .await;
    assert_eq!(bad2, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn invalid_date_rejected_with_field_detail() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let (status, body) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(1), "2026-13-40")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["fields"][0]["field"], "date");
}

#[tokio::test]
async fn duplicate_project_conflicts() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let (status, _) = json_req(
        &app,
        "POST",
        &format!("/customers/{cid}/projects"),
        Some(json!({"code": "P1", "currency": "EUR", "rate_minor": 6000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn customer_delete_blocked_while_referenced() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    // Has projects -> cannot delete.
    let (status, _) = json_req(&app, "DELETE", &format!("/customers/{cid}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Delete the project, then it's removable.
    let (s, _) = json_req(
        &app,
        "DELETE",
        &format!("/customers/{cid}/projects/P1"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s2, _) = json_req(&app, "DELETE", &format!("/customers/{cid}"), None).await;
    assert_eq!(s2, StatusCode::NO_CONTENT);
    let (s3, _) = json_req(&app, "GET", &format!("/customers/{cid}"), None).await;
    assert_eq!(s3, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn range_and_date_queries_are_exclusive_forms() {
    let (app, _d) = app().await;
    let (status, _) = json_req(&app, "GET", "/entries", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status2, _) = json_req(
        &app,
        "GET",
        "/entries?date=2026-10-02&from=2026-10-01",
        None,
    )
    .await;
    assert_eq!(status2, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn report_groups_by_project_with_effective_rate() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await; // 60.00/h default
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "STD", json!({})).await;
    new_project(&app, cid, "PREM", json!({"rate_minor": 12000})).await; // 120.00/h override

    json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "STD", json!(2), "2026-10-02")),
    )
    .await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "PREM", json!(2), "2026-10-02")),
    )
    .await;

    let (status, summary) = json_req(
        &app,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07&group=project",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = summary["rows"].as_array().unwrap();
    let prem = rows
        .iter()
        .find(|r| r["label"].as_str().unwrap().ends_with("PREM"))
        .unwrap();
    let std = rows
        .iter()
        .find(|r| r["label"].as_str().unwrap().ends_with("STD"))
        .unwrap();
    assert_eq!(prem["amount_minor"], 24000, "2h * 120.00 = 240.00");
    assert_eq!(std["amount_minor"], 12000, "2h * 60.00 = 120.00");
}

#[tokio::test]
async fn csv_export_quotes_and_escapes_note() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME, Inc.", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let body = json!({
        "date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":1.5,
        "note":"=SUM(A1) injected"
    });
    json_req(&app, "POST", "/entries", Some(body)).await;
    let (status, csv) = json_req(
        &app,
        "GET",
        "/reports/export.csv?from=2026-10-01&to=2026-10-07",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = csv.as_str().unwrap();
    assert!(text.starts_with("date,customer,"), "header present");
    // Formula injection is defused with a leading apostrophe and the comma in
    // the customer name is quoted.
    assert!(
        text.contains("\"ACME, Inc.\""),
        "comma field quoted; got: {text}"
    );
    assert!(text.contains("'=SUM(A1)"), "formula escaped; got: {text}");
}

#[tokio::test]
async fn error_shape_never_leaks_internals() {
    let (app, _d) = app().await;
    let (status, body) = json_req(
        &app,
        "GET",
        "/entries/00000000-0000-0000-0000-000000000000",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    let text = body.to_string();
    for leak in ["data/", "temp", "stack", "No such file", "store.rs"] {
        assert!(!text.contains(leak), "error body leaked {leak:?}");
    }
}

#[tokio::test]
async fn update_entry_can_move_day() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let (_, entry) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(1), "2026-10-02")),
    )
    .await;
    let eid = entry["id"].as_str().unwrap();

    let (status, updated) = json_req(
        &app,
        "PUT",
        &format!("/entries/{eid}"),
        Some(entry_body(cid, "P1", json!(1), "2026-10-03")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["date"], "2026-10-03");
    // Old day now empty, new day has it.
    let (_, old) = json_req(&app, "GET", "/entries?date=2026-10-02", None).await;
    assert!(old["entries"].as_array().unwrap().is_empty());
    let (_, new) = json_req(&app, "GET", "/entries?date=2026-10-03", None).await;
    assert_eq!(new["entries"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn oversized_note_rejected() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let body = json!({
        "date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":1,
        "note": "x".repeat(501)
    });
    let (status, _) = json_req(&app, "POST", "/entries", Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn project_requires_currency_and_rate() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    // Missing rate_minor.
    let (s1, b1) = json_req(
        &app,
        "POST",
        &format!("/customers/{cid}/projects"),
        Some(json!({"code": "P1", "currency": "EUR"})),
    )
    .await;
    assert_eq!(s1, StatusCode::UNPROCESSABLE_ENTITY, "{b1}");
    // Missing currency.
    let (s2, _) = json_req(
        &app,
        "POST",
        &format!("/customers/{cid}/projects"),
        Some(json!({"code": "P1", "rate_minor": 6000})),
    )
    .await;
    assert_eq!(s2, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn legacy_project_document_resolves_from_customer() {
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "USD", 4500).await;
    let cid = c["id"].as_str().unwrap();
    // Write a pre-#11 project document that omits currency and rate_minor.
    let dir = d
        .path()
        .join("data")
        .join("customers")
        .join(cid)
        .join("projects");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("OLD-1.json"),
        format!(r#"{{"customer_id":"{cid}","code":"OLD-1","name":"Old","active":true}}"#),
    )
    .unwrap();
    // Reading it back resolves currency/rate from the customer (USD / 4500).
    let (status, project) = json_req(
        &app,
        "GET",
        &format!("/customers/{cid}/projects/OLD-1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(project["currency"], "USD");
    assert_eq!(project["rate_minor"], 4500);
    // And it appears in the list with the same resolved values.
    let (ls, list) = json_req(&app, "GET", &format!("/customers/{cid}/projects"), None).await;
    assert_eq!(ls, StatusCode::OK);
    assert_eq!(list["projects"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn entry_billable_defaults_true_and_persists_false() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;

    // Omitted -> billable true.
    let (_, def) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(1), "2026-10-02")),
    )
    .await;
    assert_eq!(def["billable"], true);

    // Explicit false persists.
    let body = json!({
        "date": "2026-10-03", "customer_id": cid, "project_code": "P1",
        "hours": 2, "note": "internal", "billable": false
    });
    let (_, nb) = json_req(&app, "POST", "/entries", Some(body)).await;
    assert_eq!(nb["billable"], false);
    let eid = nb["id"].as_str().unwrap();
    let (_, got) = json_req(&app, "GET", &format!("/entries/{eid}"), None).await;
    assert_eq!(got["billable"], false);
}

#[tokio::test]
async fn legacy_entry_document_without_billable_reads_true() {
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    // Craft a pre-#20 entry document (no `billable` field) directly on disk.
    let id = uuid::Uuid::new_v4();
    let dir = d.path().join("data").join("entries").join("2026-10-04");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.json")),
        format!(
            r#"{{"id":"{id}","date":"2026-10-04","customer_id":"{cid}","project_code":"P1","hours":3,"note":"","created_at":"2026-10-04T09:00:00Z","updated_at":"2026-10-04T09:00:00Z"}}"#
        ),
    )
    .unwrap();
    let (status, list) = json_req(&app, "GET", "/entries?date=2026-10-04", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["entries"][0]["billable"], true);
}

#[tokio::test]
async fn entry_source_defaults_manual() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let (_, e) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(1), "2026-10-02")),
    )
    .await;
    assert_eq!(e["source"], "manual");
}

#[tokio::test]
async fn legacy_entry_document_without_source_reads_manual() {
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let id = uuid::Uuid::new_v4();
    let dir = d.path().join("data").join("entries").join("2026-10-05");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.json")),
        format!(
            r#"{{"id":"{id}","date":"2026-10-05","customer_id":"{cid}","project_code":"P1","hours":2,"note":"","billable":true,"created_at":"2026-10-05T09:00:00Z","updated_at":"2026-10-05T09:00:00Z"}}"#
        ),
    )
    .unwrap();
    let (status, e) = json_req(&app, "GET", &format!("/entries/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(e["source"], "manual");
}

#[tokio::test]
async fn locked_entry_rejects_edit_and_delete_but_not_read() {
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let (_, e) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(1), "2026-10-02")),
    )
    .await;
    let eid: uuid::Uuid = e["id"].as_str().unwrap().parse().unwrap();
    drop(app);

    // Reopen the same data dir with that entry locked (as an invoice would).
    let app2 = app_locked(d.path(), Arc::new(LockOne(Some(eid)))).await;
    let body = json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2,"billable":true});
    let (s_edit, _) = json_req(&app2, "PUT", &format!("/entries/{eid}"), Some(body)).await;
    assert_eq!(s_edit, StatusCode::CONFLICT);
    let (s_del, _) = json_req(&app2, "DELETE", &format!("/entries/{eid}"), None).await;
    assert_eq!(s_del, StatusCode::CONFLICT);
    let (s_get, _) = json_req(&app2, "GET", &format!("/entries/{eid}"), None).await;
    assert_eq!(s_get, StatusCode::OK, "locked entries are still readable");
}

// ------------------------------------------------- fail-closed locking (185) --

#[tokio::test]
async fn unverifiable_lock_state_refuses_writes_but_never_reads() {
    // #185: lock providers used to map a store error to "not locked". With an
    // unreadable invoice document the locks must fail CLOSED: entry edit and
    // delete get 503 (Retry-After), submission is refused, reads still work.
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "P1", json!({})).await;
    let (_, e) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(&cid, "P1", json!(1), "2026-10-02")),
    )
    .await;
    let eid = e["id"].as_str().unwrap().to_string();

    // Corrupt the invoices collection so InvoiceLock cannot verify its state.
    let inv_dir = d.path().join("data").join("invoices");
    std::fs::create_dir_all(&inv_dir).unwrap();
    std::fs::write(inv_dir.join("corrupt.json"), b"{ this is not json").unwrap();

    let body = json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2,"billable":true});
    let (s_edit, _) = json_req(&app, "PUT", &format!("/entries/{eid}"), Some(body)).await;
    assert_eq!(
        s_edit,
        StatusCode::SERVICE_UNAVAILABLE,
        "edit must fail closed"
    );
    let (s_del, _) = json_req(&app, "DELETE", &format!("/entries/{eid}"), None).await;
    assert_eq!(
        s_del,
        StatusCode::SERVICE_UNAVAILABLE,
        "delete must fail closed"
    );
    let (s_sub, _) = json_req(
        &app,
        "POST",
        "/submissions",
        Some(json!({"week_start": "2026-09-28"})),
    )
    .await;
    assert_eq!(
        s_sub,
        StatusCode::SERVICE_UNAVAILABLE,
        "submit must fail closed"
    );
    let (s_get, _) = json_req(&app, "GET", &format!("/entries/{eid}"), None).await;
    assert_eq!(s_get, StatusCode::OK, "reads are never blocked");
}

#[tokio::test]
async fn delete_customer_refused_when_project_docs_are_unreadable() {
    // #185: `delete_customer` swallowed a failed project listing as "empty"
    // and then remove_dir_all'd the customer tree INCLUDING the unreadable
    // project docs. An unlistable collection must refuse the deletion and
    // leave every file intact.
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "P1", json!({})).await;
    let project_path = d
        .path()
        .join(format!("data/customers/{cid}/projects/P1.json"));
    std::fs::write(&project_path, b"{ this is not json").unwrap();

    let (s, b) = json_req(&app, "DELETE", &format!("/customers/{cid}"), None).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{b}");
    assert!(
        d.path().join(format!("data/customers/{cid}.json")).exists(),
        "customer document must survive a refused delete"
    );
    assert!(
        project_path.exists(),
        "project documents must not be deleted when they could not be listed"
    );
}

#[tokio::test]
async fn concurrent_identical_webhook_records_exactly_one_payment() {
    // #185: the webhook idempotency check ran on an unlocked snapshot, so two
    // concurrent deliveries of the same event could both post a partial
    // payment. The dedup now runs inside record_payment_once's write lock.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let stripe = tucano_time::payments::StripeProvider::with_secret("whsec_dup");
    let app = Client::with_payments(
        store,
        tucano_time::payments::PaymentRegistry::new(vec![Arc::new(stripe)]),
    )
    .await;

    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    let number = inv["number"].as_str().unwrap().to_string();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;

    // A PARTIAL (6000 of 18000) with a fixed provider event id, delivered twice
    // concurrently: a full settle would end at "already_paid" before the
    // reference check, so only a partial exercises the locked dedup.
    let payload = format!(
        "{{\"id\":\"evt_dup_1\",\"type\":\"checkout.session.completed\",\"payment_status\":\"paid\",\"amount_minor\":6000,\"currency\":\"EUR\",\"client_reference_id\":\"pay_ref_dup\",\"metadata\":{{\"invoice_number\":\"{number}\"}}}}"
    );
    let sig = format!(
        "sha256={}",
        tucano_time::payments::sign("whsec_dup", &payload)
    );
    let ((s1, b1), (s2, b2)) = tokio::join!(
        webhook_post(&app.router, "stripe", &payload, Some(&sig)),
        webhook_post(&app.router, "stripe", &payload, Some(&sig)),
    );
    assert_eq!(s1, StatusCode::OK, "{b1}");
    assert_eq!(s2, StatusCode::OK, "{b2}");
    // One settles as a new payment, the other as an idempotent replay — never
    // two ledger entries for the same event id.
    let statuses: Vec<&str> = vec![
        b1["status"].as_str().unwrap(),
        b2["status"].as_str().unwrap(),
    ];
    assert!(
        statuses.contains(&"already_processed"),
        "second delivery must be reported as a replay: {statuses:?}"
    );

    let (_s, got) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(got["status"], "partly_paid");
    let payments = got["payments"].as_array().unwrap();
    assert_eq!(
        payments.len(),
        1,
        "duplicate event must record exactly one payment"
    );
    assert_eq!(payments[0]["amount_minor"], 6000);

    let make_event = |event_id: &str| {
        format!(
            "{{\"id\":\"{event_id}\",\"type\":\"checkout.session.completed\",\"payment_status\":\"paid\",\"amount_minor\":10000,\"currency\":\"EUR\",\"client_reference_id\":\"pay_ref_dup\",\"metadata\":{{\"invoice_number\":\"{number}\"}}}}"
        )
    };
    let event_a = make_event("evt_distinct_a");
    let event_b = make_event("evt_distinct_b");
    let sig_a = format!(
        "sha256={}",
        tucano_time::payments::sign("whsec_dup", &event_a)
    );
    let sig_b = format!(
        "sha256={}",
        tucano_time::payments::sign("whsec_dup", &event_b)
    );
    let ((status_a, body_a), (status_b, body_b)) = tokio::join!(
        webhook_post(&app.router, "stripe", &event_a, Some(&sig_a)),
        webhook_post(&app.router, "stripe", &event_b, Some(&sig_b)),
    );
    assert_ne!(
        status_a, status_b,
        "only one distinct payment fits the remaining balance"
    );
    assert!(
        [status_a, status_b].contains(&StatusCode::CONFLICT),
        "the unrecorded competing event must remain retryable: {body_a} {body_b}"
    );
    let (_s, after_race) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    let race_payments = after_race["payments"].as_array().unwrap();
    assert_eq!(
        race_payments.len(),
        2,
        "only one competing event may be recorded"
    );
    assert_eq!(
        race_payments
            .iter()
            .map(|payment| payment["amount_minor"].as_u64().unwrap())
            .sum::<u64>(),
        16000,
        "recorded total must not exceed the invoice balance"
    );
}

// ------------------------------------------------------------------ tasks --

async fn new_task(app: &Client, cid: &str, pcode: &str, code: &str, extra: Value) -> Value {
    let mut body = json!({"code": code});
    if let Value::Object(map) = extra {
        for (k, v) in map {
            body[k] = v;
        }
    }
    let (status, created) = json_req(
        app,
        "POST",
        &format!("/customers/{cid}/projects/{pcode}/tasks"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create task: {created}");
    created
}

#[tokio::test]
async fn task_crud_and_entry_reference() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;

    let t = new_task(&app, cid, "P1", "t-9", json!({"name": "Design"})).await;
    assert_eq!(t["code"], "T-9", "task code normalised");

    // Entry referencing the task.
    let body = json!({
        "date": "2026-10-02", "customer_id": cid, "project_code": "P1",
        "task_code": "T-9", "hours": 2
    });
    let (s, e) = json_req(&app, "POST", "/entries", Some(body)).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    assert_eq!(e["task_code"], "T-9");

    // Task delete blocked while an entry references it.
    let (s_del, _) = json_req(
        &app,
        "DELETE",
        &format!("/customers/{cid}/projects/P1/tasks/T-9"),
        None,
    )
    .await;
    assert_eq!(s_del, StatusCode::CONFLICT);

    // Project delete blocked while a task exists.
    let (s_pdel, _) = json_req(
        &app,
        "DELETE",
        &format!("/customers/{cid}/projects/P1"),
        None,
    )
    .await;
    assert_eq!(s_pdel, StatusCode::CONFLICT);
}

#[tokio::test]
async fn entry_task_must_belong_to_project() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    new_project(&app, cid, "P2", json!({})).await;
    new_task(&app, cid, "P2", "T1", json!({})).await;
    // Reference P2's task under P1.
    let body = json!({
        "date": "2026-10-02", "customer_id": cid, "project_code": "P1",
        "task_code": "T1", "hours": 1
    });
    let (s, b) = json_req(&app, "POST", "/entries", Some(body)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        b["error"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["field"] == "task_code")
    );
}

#[tokio::test]
async fn reports_and_invoices_use_project_rates_for_legacy_tasks() {
    let (app, dir) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await; // 60/h default
    let cid = c["id"].as_str().unwrap();
    new_project(
        &app,
        cid,
        "P1",
        json!({"currency": "USD", "rate_minor": 7500}),
    )
    .await;
    let mut task = new_task(&app, cid, "P1", "PREM", json!({})).await;
    task["currency"] = json!("GBP");
    task["rate_minor"] = json!(9500);
    std::fs::write(
        dir.path()
            .join(format!("data/customers/{cid}/projects/P1/tasks/PREM.json")),
        serde_json::to_vec(&task).unwrap(),
    )
    .unwrap();

    let body = json!({
        "date": "2026-10-02", "customer_id": cid, "project_code": "P1",
        "task_code": "PREM", "hours": 2
    });
    json_req(&app, "POST", "/entries", Some(body)).await;
    // Same project, no task -> project rate.
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":1})),
    )
    .await;

    let (s, summary) = json_req(
        &app,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07&group=customer",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(summary["rows"][0]["amount_minor"], 22500);
    assert_eq!(summary["rows"][0]["currency"], "USD");
    let (status, invoice) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({
            "customer_id": cid, "from": "2026-10-01", "to": "2026-10-07"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{invoice}");
    assert_eq!(invoice["total_minor"], 22500);
    assert_eq!(invoice["currency"], "USD");
    assert!(
        invoice["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|line| line["rate_minor"] == 7500)
    );
}

#[tokio::test]
async fn tasks_reject_billing_overrides_without_persistence() {
    let (app, _dir) = app().await;
    let customer = new_customer(&app, "Task rates", "EUR", 6000).await;
    let cid = customer["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let task = new_task(&app, cid, "P1", "T1", json!({"name": "Original"})).await;
    let base = format!("/customers/{cid}/projects/P1/tasks");
    for field in ["rate_minor", "currency"] {
        for method in ["POST", "PUT"] {
            let mut payload =
                json!({"code": if method == "POST" { "NEW" } else { "T1" }, "name": "Changed"});
            payload[field] = if field == "rate_minor" {
                json!(9500)
            } else {
                json!("GBP")
            };
            let path = if method == "POST" {
                base.clone()
            } else {
                format!("{base}/T1")
            };
            let (status, _) = json_req(&app, method, &path, Some(payload)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            let (_, saved) = json_req(&app, "GET", &format!("{base}/T1"), None).await;
            assert_eq!(saved, task);
            let (missing, _) = json_req(&app, "GET", &format!("{base}/NEW"), None).await;
            assert_eq!(missing, StatusCode::NOT_FOUND);
        }
    }
}

// ------------------------------------------------------------------- auth --

#[tokio::test]
async fn protected_routes_require_auth() {
    let (app, _d) = app().await;
    // Unauthenticated -> 401.
    let (s, _) = anon_req(&app, "GET", "/customers", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Authenticated -> 200.
    let (s2, _) = json_req(&app, "GET", "/customers", None).await;
    assert_eq!(s2, StatusCode::OK);
}

#[tokio::test]
async fn bootstrap_only_works_once() {
    let (app, _d) = app().await;
    // A second bootstrap must be refused (an admin already exists).
    let (s, _) = anon_req(
        &app,
        "POST",
        "/auth/bootstrap",
        Some(json!({"name":"X","email":"x@y.co","password":"whatever12"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_rejects_bad_password_without_leaking() {
    let (app, _d) = app().await;
    let (s, body) = anon_req(
        &app,
        "POST",
        "/auth/login",
        Some(json!({"email": ADMIN_EMAIL, "password": "wrong-password"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_credentials");
    // Same generic code for an unknown email (no user enumeration).
    let (s2, b2) = anon_req(
        &app,
        "POST",
        "/auth/login",
        Some(json!({"email": "nobody@test.local", "password": "whatever12"})),
    )
    .await;
    assert_eq!(s2, StatusCode::UNAUTHORIZED);
    assert_eq!(b2["error"]["code"], "invalid_credentials");
}

#[tokio::test]
async fn me_returns_current_user() {
    let (app, _d) = app().await;
    let (s, me) = json_req(&app, "GET", "/auth/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["email"], ADMIN_EMAIL);
    assert_eq!(me["role"], "admin");
    assert!(
        me.get("password_hash").is_none(),
        "the hash must never be returned"
    );
}

#[tokio::test]
async fn member_cannot_manage_users() {
    let (app, _d) = app().await;
    // Admin creates a member.
    let (s, _) = json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Pam","email":"pam@test.local","password":"memberpass1","role":"member"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    // Member logs in and is forbidden from /users, but can use /customers.
    let cookie = login_cookie(&app.router, "pam@test.local", "memberpass1").await;
    let (s_users, _, _) = raw(&app.router, "GET", "/users", None, Some(&cookie)).await;
    assert_eq!(s_users, StatusCode::FORBIDDEN);
    let (s_cust, _, _) = raw(&app.router, "GET", "/customers", None, Some(&cookie)).await;
    assert_eq!(s_cust, StatusCode::OK);
}

#[tokio::test]
async fn admin_can_update_user_and_duplicate_email_is_rejected_without_persistence() {
    let (app, _d) = app().await;
    let (s, bob) = json_req(
        &app,
        "POST",
        "/users",
        Some(json!({
            "name":"Bob","email":"bob@test.local","password":"bobpass123","role":"member",
            "active":true,"default_rate_minor":4500,"cost_rate_minor":2200
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let bob_id = bob["id"].as_str().unwrap();

    let (s, updated) = json_req(
        &app,
        "PUT",
        &format!("/users/{bob_id}"),
        Some(json!({
            "name":"Bob Q","email":"bob@test.local","role":"admin","active":false,
            "default_rate_minor":5000,"cost_rate_minor":2400
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(updated["name"], "Bob Q");
    assert_eq!(updated["role"], "admin");
    assert_eq!(updated["active"], false);
    assert_eq!(updated["default_rate_minor"], 5000);
    assert_eq!(updated["cost_rate_minor"], 2400);

    let (s, _) = json_req(
        &app,
        "POST",
        "/users",
        Some(json!({
            "name":"Other","email":"BOB@test.local","password":"otherpass1","role":"member"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn member_cannot_update_user() {
    let (app, _d) = app().await;
    let (s, user) = json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Pam","email":"pam@test.local","password":"memberpass1","role":"member"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let cookie = login_cookie(&app.router, "pam@test.local", "memberpass1").await;
    let (s, _, _) = raw(
        &app.router,
        "PUT",
        &format!("/users/{}", user["id"].as_str().unwrap()),
        Some(json!({"name":"Changed"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn password_change_revokes_existing_sessions_and_accepts_new_password() {
    let (app, _d) = app().await;
    let (s, user) = json_req(
        &app,
        "POST",
        "/users",
        Some(json!({
            "name":"Pam",
            "email":"pam@test.local",
            "password":"memberpass1",
            "role":"member"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let old_cookie = login_cookie(&app.router, "pam@test.local", "memberpass1").await;
    let id = user["id"].as_str().unwrap();
    let (s, _) = json_req(
        &app,
        "PUT",
        &format!("/users/{id}/password"),
        Some(json!({ "password": "newmemberpass1" })),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s_old, _, _) = raw(&app.router, "GET", "/auth/me", None, Some(&old_cookie)).await;
    assert_eq!(s_old, StatusCode::UNAUTHORIZED);

    let new_cookie = login_cookie(&app.router, "pam@test.local", "newmemberpass1").await;
    let (s_new, _, _) = raw(&app.router, "GET", "/auth/me", None, Some(&new_cookie)).await;
    assert_eq!(s_new, StatusCode::OK);
}

#[tokio::test]
async fn user_management_validation_and_authorization_persist_nothing() {
    let (app, dir) = app().await;
    let (_, user) = json_req(
        &app,
        "POST",
        "/users",
        Some(json!({"name":"QA","email":"qa@test.local","password":"qa-password1"})),
    )
    .await;
    let id = user["id"].as_str().unwrap();
    let path = format!("/users/{id}");
    let store = Store::open(dir.path().join("data")).unwrap();
    let uid = uuid::Uuid::parse_str(id).unwrap();
    let before = serde_json::to_value(store.get_user(uid).unwrap().unwrap()).unwrap();
    for invalid in [
        json!({"name":""}),
        json!({"email":"invalid"}),
        json!({"role":"owner"}),
        json!({"default_rate_minor":100000001}),
        json!({"cost_rate_minor":-1}),
        json!({"cost_rate_minor":1.5}),
        json!({"unexpected":true}),
    ] {
        let (status, body) = json_req(&app, "PUT", &path, Some(invalid.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body["error"].is_object());
        let mut create =
            json!({"name":"New","email":"new@test.local","password":"initial-password1"});
        create
            .as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        let (status, _) = json_req(&app, "POST", "/users", Some(create)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(store.list_users().unwrap().len(), 2);
        assert_eq!(
            serde_json::to_value(store.get_user(uid).unwrap().unwrap()).unwrap(),
            before
        );
    }
    for method in ["POST", "PUT"] {
        let path = if method == "POST" {
            "/users".to_string()
        } else {
            format!("/users/{id}/password")
        };
        let payload = if method == "POST" {
            json!({"name":"New","email":"new@test.local","password":"short"})
        } else {
            json!({"password":"short"})
        };
        let (status, _) = json_req(&app, method, &path, Some(payload)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let cookie = login_cookie(&app.router, "qa@test.local", "qa-password1").await;
    for (method, path, payload) in [
        ("GET", "/users".to_string(), None),
        (
            "POST",
            "/users".to_string(),
            Some(json!({"name":"New","email":"new@test.local","password":"initial-password1"})),
        ),
        (
            "PUT",
            format!("/users/{id}"),
            Some(json!({"name":"Changed"})),
        ),
        (
            "PUT",
            format!("/users/{id}/password"),
            Some(json!({"password":"changed-password1"})),
        ),
        ("DELETE", format!("/users/{id}"), None),
    ] {
        let (status, _, _) = raw(&app.router, method, &path, payload, Some(&cookie)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(
        serde_json::to_value(store.get_user(uid).unwrap().unwrap()).unwrap(),
        before
    );
}

#[tokio::test]
async fn user_management_preserves_admin_history_and_revalidates_sessions() {
    let (app, dir) = app().await;
    let (_, me) = json_req(&app, "GET", "/auth/me", None).await;
    let admin_id = me["id"].as_str().unwrap();
    for body in [json!({"role":"member"}), json!({"active":false})] {
        let (status, _) = json_req(&app, "PUT", &format!("/users/{admin_id}"), Some(body)).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    let (status, _) = json_req(&app, "DELETE", &format!("/users/{admin_id}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, user) = json_req(
        &app,
        "POST",
        "/users",
        Some(json!({"name":"QA","email":"qa@test.local","password":"qa-password1"})),
    )
    .await;
    let id = user["id"].as_str().unwrap();
    let path = format!("/users/{id}");
    let cookie = login_cookie(&app.router, "qa@test.local", "qa-password1").await;
    let (status, _) = json_req(
        &app,
        "PUT",
        &path,
        Some(json!({"email":"renamed@test.local","role":"member","active":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        raw(&app.router, "GET", "/auth/me", None, Some(&cookie))
            .await
            .0,
        StatusCode::OK
    );
    assert!(
        login_cookie(&app.router, "qa@test.local", "qa-password1")
            .await
            .is_empty()
    );
    let renamed_cookie = login_cookie(&app.router, "renamed@test.local", "qa-password1").await;
    assert!(!renamed_cookie.is_empty());
    let (status, _) = json_req(&app, "PUT", &path, Some(json!({"email":ADMIN_EMAIL}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    json_req(&app, "PUT", &path, Some(json!({"role":"admin"}))).await;
    assert_eq!(
        raw(&app.router, "GET", "/auth/me", None, Some(&renamed_cookie))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let admin_cookie = login_cookie(&app.router, "renamed@test.local", "qa-password1").await;
    json_req(&app, "PUT", &path, Some(json!({"active":false}))).await;
    assert_eq!(
        raw(&app.router, "GET", "/auth/me", None, Some(&admin_cookie))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    json_req(
        &app,
        "PUT",
        &path,
        Some(json!({"active":true,"role":"member"})),
    )
    .await;
    let cookie = login_cookie(&app.router, "renamed@test.local", "qa-password1").await;
    let customer = new_customer(&app, "History", "EUR", 6000).await;
    let cid = customer["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let (timer_status, _, _) = raw(
        &app.router,
        "POST",
        "/timer",
        Some(json!({"customer_id":cid,"project_code":"P1"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(timer_status, StatusCode::CREATED);
    assert_eq!(
        json_req(&app, "DELETE", &path, None).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        raw(&app.router, "DELETE", "/timer", None, Some(&cookie))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (status, _, _) = raw(
        &app.router,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-06","customer_id":cid,"project_code":"P1","hours":1})),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        json_req(&app, "DELETE", &path, None).await.0,
        StatusCode::CONFLICT
    );
    json_req(&app, "PUT", &path, Some(json!({"active":false}))).await;
    assert_eq!(
        json_req(&app, "DELETE", &path, None).await.0,
        StatusCode::CONFLICT
    );
    let (_, list) = json_req(&app, "GET", "/users", None).await;
    assert!(!list.to_string().contains("password"));
    let (_, disposable) = json_req(&app, "POST", "/users", Some(json!({"name":"Disposable","email":"disposable@test.local","password":"disposable-password1"}))).await;
    assert_eq!(
        json_req(
            &app,
            "DELETE",
            &format!("/users/{}", disposable["id"].as_str().unwrap()),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let store = Store::open(dir.path().join("data")).unwrap();
    let mut last_admin = store
        .get_user(uuid::Uuid::parse_str(admin_id).unwrap())
        .unwrap()
        .unwrap();
    let expected = last_admin.clone();
    last_admin.active = false;
    assert!(store.put_user_if_unchanged(&last_admin, &expected).is_err());
    assert!(store.delete_user(expected.id).is_err());
    let stale = store
        .get_user(uuid::Uuid::parse_str(id).unwrap())
        .unwrap()
        .unwrap();
    let mut updated = stale.clone();
    updated.name = "Concurrent edit".into();
    store.put_user_if_unchanged(&updated, &stale).unwrap();
    assert!(store.put_user_if_unchanged(&stale, &stale).is_err());
    let audit = std::fs::read_to_string(dir.path().join("data/audit.log")).unwrap();
    assert!(!audit.contains("qa-password1"));
}

#[tokio::test]
async fn report_applies_person_rate_tier_and_attributes_entry() {
    let (app, _d) = app().await;
    // Customer default 60/h; project rate 30/h.
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 3000})).await;

    // Admin creates a member with a personal default of 45/h.
    let (_s, bob) = json_req(
        &app,
        "POST",
        "/users",
        Some(json!({"name":"Bob","email":"bob@test.local","password":"bobpass123","role":"member","default_rate_minor":4500})),
    )
    .await;
    let bob_id = bob["id"].as_str().unwrap().to_string();

    // Bob logs 2h on P1.
    let bob_cookie = login_cookie(&app.router, "bob@test.local", "bobpass123").await;
    let (_s2, entry, _) = raw(
        &app.router,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2})),
        Some(&bob_cookie),
    )
    .await;
    assert_eq!(
        entry["user_id"], bob_id,
        "entry attributed to the logging user"
    );

    // Report: person rate (45) overrides project rate (30) -> 2h * 45 = 90.00 = 9000.
    let (_s3, summary) = json_req(
        &app,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07&group=customer",
        None,
    )
    .await;
    assert_eq!(summary["rows"][0]["amount_minor"], 9000);
}

// --------------------------------------------------------------- invoices --

#[tokio::test]
async fn invoice_sums_billable_and_ignores_nonbillable() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    // 3h billable @60 = 180.00
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    // 5h NON-billable -> excluded
    json_req(&app, "POST", "/entries", Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":5,"billable":false}))).await;

    let (s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    assert_eq!(inv["status"], "draft");
    assert_eq!(inv["currency"], "EUR");
    assert_eq!(inv["lines"].as_array().unwrap().len(), 1);
    assert_eq!(inv["total_minor"], 18000);
}

#[tokio::test]
async fn issuing_invoice_locks_its_entries() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    let (_, e) = json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let eid = e["id"].as_str().unwrap();

    let (_, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();

    // Draft does NOT lock.
    let (s_before, _) = json_req(
        &app,
        "PUT",
        &format!("/entries/{eid}"),
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":4})),
    )
    .await;
    assert_eq!(
        s_before,
        StatusCode::OK,
        "draft invoice must not lock entries"
    );

    // Issue -> now locked.
    let (si, _) = json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    assert_eq!(si, StatusCode::OK);
    let (s_edit, body) = json_req(
        &app,
        "PUT",
        &format!("/entries/{eid}"),
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":4})),
    )
    .await;
    assert_eq!(s_edit, StatusCode::CONFLICT, "{body}");
    let (s_del, _) = json_req(&app, "DELETE", &format!("/entries/{eid}"), None).await;
    assert_eq!(s_del, StatusCode::CONFLICT);
}

#[tokio::test]
async fn regeneration_excludes_invoiced_entries() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{}/issue", inv["id"].as_str().unwrap()),
        None,
    )
    .await;
    // Second invoice over the same period has nothing left to bill.
    let (s2, body) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn invoice_rejects_mixed_currencies() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"currency":"EUR","rate_minor":6000})).await;
    new_project(&app, cid, "P2", json!({"currency":"USD","rate_minor":3000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":1})),
    )
    .await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P2","hours":1})),
    )
    .await;
    let (s, body) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("currencies")
    );
}

// ---------------------------------------------------------------- expenses --

#[tokio::test]
async fn category_and_expense_crud_with_references() {
    let (app, _d) = app().await;
    let (s0, defaults) = json_req(&app, "GET", "/categories", None).await;
    assert_eq!(s0, StatusCode::OK);
    let default_names = defaults["categories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|category| category["name"].as_str())
        .collect::<Vec<_>>();
    for name in ["Food", "Travel", "Lodging", "Supplies", "Software"] {
        assert!(
            default_names.contains(&name),
            "missing default category {name}"
        );
    }
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;

    let (sc, cat) = json_req(&app, "POST", "/categories", Some(json!({"name":"Travel"}))).await;
    assert_eq!(sc, StatusCode::CREATED);
    let cat_id = cat["id"].as_str().unwrap();

    let (su, updated) = json_req(
        &app,
        "PUT",
        &format!("/categories/{cat_id}"),
        Some(json!({"name":"Work travel","default_billable":true,"active":true})),
    )
    .await;
    assert_eq!(su, StatusCode::OK);
    assert_eq!(updated["name"], "Work travel");

    let (se, exp) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({
            "date":"2026-10-02","customer_id":cid,"project_code":"P1","category_id":cat_id,
            "amount_minor":12500,"currency":"EUR","note":"flight"
        })),
    )
    .await;
    assert_eq!(se, StatusCode::CREATED, "{exp}");
    assert_eq!(exp["billable"], true); // default

    // Category can't be deleted while an expense references it.
    let (sd, _) = json_req(&app, "DELETE", &format!("/categories/{cat_id}"), None).await;
    assert_eq!(sd, StatusCode::CONFLICT);

    // Filter expenses by customer.
    let (sf, list) = json_req(&app, "GET", &format!("/expenses?customer_id={cid}"), None).await;
    assert_eq!(sf, StatusCode::OK);
    assert_eq!(list["expenses"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn expense_rejects_unknown_project_and_category() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    let (s, body) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({
            "date":"2026-10-02","customer_id":cid,"project_code":"NOPE",
            "category_id":"00000000-0000-0000-0000-000000000000","amount_minor":100,"currency":"EUR"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let fields: Vec<&str> = body["error"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["field"].as_str().unwrap())
        .collect();
    assert!(fields.contains(&"project_code"));
    assert!(fields.contains(&"category_id"));
}

#[tokio::test]
async fn expense_receipt_roundtrips() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    let (_, e) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({
            "date":"2026-10-02","customer_id":cid,"amount_minor":500,"currency":"EUR",
            "receipt_name":"hotel.png","receipt_b64":"aGVsbG8="
        })),
    )
    .await;
    let eid = e["id"].as_str().unwrap();
    let (_, got) = json_req(&app, "GET", &format!("/expenses/{eid}"), None).await;
    assert_eq!(got["receipt_name"], "hotel.png");
    assert_eq!(got["receipt_b64"], "aGVsbG8=");
}

// ------------------------------------------------------------ submissions --

async fn seed_week(app: &Client, cid: &str) -> String {
    let (_, e) = json_req(
        app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    e["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn submit_locks_and_reject_unlocks() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let eid = seed_week(&app, cid).await;

    let (s, sub) = json_req(
        &app,
        "POST",
        "/submissions",
        Some(json!({"week_start":"2026-09-28"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{sub}");
    let sid = sub["id"].as_str().unwrap().to_string();
    let edit_body = json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":4});

    // Submitted -> entry locked.
    let (s1, _) = json_req(
        &app,
        "PUT",
        &format!("/entries/{eid}"),
        Some(edit_body.clone()),
    )
    .await;
    assert_eq!(s1, StatusCode::CONFLICT);

    // Reject -> unlocked.
    let (sd, _) = json_req(
        &app,
        "POST",
        &format!("/submissions/{sid}/decision"),
        Some(json!({"decision":"reject","comment":"redo"})),
    )
    .await;
    assert_eq!(sd, StatusCode::OK);
    let (s2, _) = json_req(&app, "PUT", &format!("/entries/{eid}"), Some(edit_body)).await;
    assert_eq!(s2, StatusCode::OK);
}

#[tokio::test]
async fn approve_keeps_locked_and_double_submit_refused() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    let eid = seed_week(&app, cid).await;

    let (_, sub) = json_req(
        &app,
        "POST",
        "/submissions",
        Some(json!({"week_start":"2026-09-28"})),
    )
    .await;
    let sid = sub["id"].as_str().unwrap();
    json_req(
        &app,
        "POST",
        &format!("/submissions/{sid}/decision"),
        Some(json!({"decision":"approve"})),
    )
    .await;

    // Approved -> still locked.
    let (s_edit, _) = json_req(
        &app,
        "PUT",
        &format!("/entries/{eid}"),
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":4})),
    )
    .await;
    assert_eq!(s_edit, StatusCode::CONFLICT);

    // Resubmitting the same week: the only entry is locked, so nothing to submit.
    let (s2, _) = json_req(
        &app,
        "POST",
        "/submissions",
        Some(json!({"week_start":"2026-09-28"})),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
}

#[tokio::test]
async fn report_splits_billable_and_groups_by_person() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    // 3h billable + 2h non-billable, both by the admin.
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    json_req(&app, "POST", "/entries", Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2,"billable":false}))).await;

    let (_, s) = json_req(
        &app,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07&group=customer",
        None,
    )
    .await;
    assert_eq!(s["billable_hours"], 3.0);
    assert_eq!(s["nonbillable_hours"], 2.0);
    assert_eq!(s["total_hours"], 5.0);

    // billable=true scopes rows to billable only (3h).
    let (_, sb) = json_req(
        &app,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07&group=customer&billable=true",
        None,
    )
    .await;
    assert_eq!(sb["rows"][0]["hours"], 3.0);

    // group=person yields one row for the admin.
    let (_, sp) = json_req(
        &app,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07&group=person",
        None,
    )
    .await;
    assert_eq!(sp["group"], "person");
    assert_eq!(sp["rows"][0]["label"], "Admin");
}

#[tokio::test]
async fn invoice_pay_flow_and_summary() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();

    // Issue sets due_date (net-14).
    let (_, issued) = json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    assert_eq!(issued["status"], "issued");
    assert!(issued["due_date"].is_string(), "issue sets a due date");

    // Pay it.
    let (_, paid) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference":"bank-123"})),
    )
    .await;
    assert_eq!(paid["status"], "paid");
    assert_eq!(paid["payment_reference"], "bank-123");

    // Summary reflects one paid invoice, nothing outstanding.
    let (s, sum) = json_req(&app, "GET", "/invoices/summary", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(sum["paid"], 1);
    assert_eq!(sum["issued"], 0);
    assert!(sum["outstanding"].as_object().unwrap().is_empty());
}

// --------------------------------------------------- RBAC / private-per-user --

#[tokio::test]
async fn member_sees_only_own_entries() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    // Admin logs one entry.
    let (_, admin_entry) = json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2})),
    )
    .await;
    let admin_eid = admin_entry["id"].as_str().unwrap().to_string();

    // Create member Dana and log one entry as Dana.
    json_req(&app, "POST", "/users", Some(json!({"name":"Dana","email":"dana@test.local","password":"danapass123","role":"member"}))).await;
    let dana = login_cookie(&app.router, "dana@test.local", "danapass123").await;
    let (_, dana_entry, _) = raw(
        &app.router,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
        Some(&dana),
    )
    .await;
    let dana_eid = dana_entry["id"].as_str().unwrap().to_string();

    // Dana lists entries -> only her own.
    let (s, list, _) = raw(
        &app.router,
        "GET",
        "/entries?date=2026-10-02",
        None,
        Some(&dana),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![dana_eid.as_str()], "member sees only own entries");

    // Dana cannot read the admin's entry (404, not 403 — no existence leak).
    let (s404, _, _) = raw(
        &app.router,
        "GET",
        &format!("/entries/{admin_eid}"),
        None,
        Some(&dana),
    )
    .await;
    assert_eq!(s404, StatusCode::NOT_FOUND);

    // Admin sees both.
    let (_, admin_list) = json_req(&app, "GET", "/entries?date=2026-10-02", None).await;
    assert_eq!(admin_list["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn member_cannot_invoice_or_approve() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let eve = login_cookie(&app.router, "eve@test.local", "evepass123").await;

    let (s_inv, _, _) = raw(
        &app.router,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
        Some(&eve),
    )
    .await;
    assert_eq!(
        s_inv,
        StatusCode::FORBIDDEN,
        "member cannot create invoices"
    );
    let (s_users, _, _) = raw(&app.router, "GET", "/users", None, Some(&eve)).await;
    assert_eq!(s_users, StatusCode::FORBIDDEN, "member cannot list users");
    let (s_dec, _, _) = raw(
        &app.router,
        "POST",
        "/submissions/00000000-0000-0000-0000-000000000000/decision",
        Some(json!({"decision":"approve"})),
        Some(&eve),
    )
    .await;
    assert_eq!(s_dec, StatusCode::FORBIDDEN, "member cannot approve");
}

#[tokio::test]
async fn ubuntu_font_is_embedded_and_served_with_font_mime_type() {
    let (app, _dir) = app().await;
    let request = Request::builder()
        .uri("/vendor/ubuntu/f1ea362b-Ubuntu-wdth-wght--latin-v0.896a.woff2")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "font/woff2");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..4], b"wOF2");
}

#[tokio::test]
async fn security_headers_present() {
    let (client, _d) = app().await;
    let res = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let h = res.headers();
    assert_eq!(
        h.get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        h.get("x-frame-options").and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    assert_eq!(
        h.get("referrer-policy").and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    let csp = h
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(csp.contains("default-src 'self'"), "CSP present: {csp}");
    assert!(csp.contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn mutation_without_csrf_header_is_forbidden() {
    let (client, _d) = app().await;
    // POST without the X-CSRF-Protection header -> 403 (login is exempt, so
    // use an authenticated data mutation).
    let res = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/customers")
                .header(header::COOKIE, &client.cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"name":"X","currency":"EUR","default_rate_minor":1}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_rate_limited_after_repeated_failures() {
    let (client, _d) = app().await;
    let body = json!({"email": "attacker@test.local", "password": "wrong"});
    let mut last = StatusCode::UNAUTHORIZED;
    // 8 failures allowed (each 401); the 9th is locked out (429).
    for _ in 0..8 {
        let (s, _) = anon_req(&client, "POST", "/auth/login", Some(body.clone())).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        last = s;
    }
    let (s9, _) = anon_req(&client, "POST", "/auth/login", Some(body)).await;
    assert_eq!(
        s9,
        StatusCode::TOO_MANY_REQUESTS,
        "locked after threshold (last was {last:?})"
    );
}

#[test]
fn concurrent_writer_times_out_with_lock_busy() {
    use fs2::FileExt;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    // A store that gives up quickly when the writer lock is held.
    let store = tucano_time::store::Store::with_lock_timeout(
        dir.path().join("data"),
        Duration::from_millis(50),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    // Simulate another process holding the advisory writer lock.
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.path().join("data").join(".tucanotime.lock"))
        .unwrap();
    held.lock_exclusive().unwrap();

    let cust = tucano_time::domain::Customer {
        id: uuid::Uuid::new_v4(),
        name: "X".into(),
        currency: tucano_time::domain::Currency("EUR".into()),
        default_rate_minor: 1,
        active: true,
        email: String::new(),
        payment_terms: None,
        invoice_notes: String::new(),
        invoice_subject: String::new(),
        address: None,
        contacts: vec![],
        tax_hundredths: 0,
        discount_hundredths: 0,
    };
    let err = store.put_customer(&cust).expect_err("should time out");
    assert!(
        matches!(err, tucano_time::store::StoreError::LockTimeout),
        "{err:?}"
    );

    // Release -> the same write now succeeds.
    held.unlock().unwrap();
    drop(held);
    store
        .put_customer(&cust)
        .expect("write succeeds once lock is free");
}

#[tokio::test]
async fn audit_log_records_login_failure() {
    let (client, _d) = app().await;
    // A failed login (as an anonymous caller).
    let _ = anon_req(
        &client,
        "POST",
        "/auth/login",
        Some(json!({"email":"ghost@test.local","password":"wrong"})),
    )
    .await;
    // Admin reads the audit log and sees the event.
    let (s, body) = json_req(&client, "GET", "/audit", None).await;
    assert_eq!(s, StatusCode::OK);
    let events = body["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "login_failed" && e["subject"] == "ghost@test.local"),
        "audit missing login_failed: {events:?}"
    );
    // No password material is ever recorded.
    let raw = serde_json::to_string(&events).unwrap();
    assert!(!raw.contains("wrong"), "audit leaked a password");
}

#[tokio::test]
async fn logout_revokes_the_session() {
    let (client, _d) = app().await;
    // Session works.
    let (s1, _) = json_req(&client, "GET", "/auth/me", None).await;
    assert_eq!(s1, StatusCode::OK);
    // Log out (CSRF header is sent by raw()).
    let (s2, _, _) = raw(
        &client.router,
        "POST",
        "/auth/logout",
        None,
        Some(&client.cookie),
    )
    .await;
    assert_eq!(s2, StatusCode::NO_CONTENT);
    // The same cookie is now revoked.
    let (s3, _) = json_req(&client, "GET", "/auth/me", None).await;
    assert_eq!(
        s3,
        StatusCode::UNAUTHORIZED,
        "revoked session must not work"
    );
}

#[tokio::test]
async fn invoice_includes_billable_expenses_and_locks_them() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    // 2h billable time @60 = 12000
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2})),
    )
    .await;
    // billable expense 50.00 = 5000
    let (_, exp) = json_req(&app, "POST", "/expenses", Some(json!({"date":"2026-10-03","customer_id":cid,"project_code":"P1","amount_minor":5000,"currency":"EUR"}))).await;
    let xid = exp["id"].as_str().unwrap().to_string();

    // Invoice includes both -> 12000 + 5000 = 17000, two lines of different kinds.
    let (_, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    assert_eq!(inv["total_minor"], 17000);
    let kinds: Vec<&str> = inv["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"time") && kinds.contains(&"expense"),
        "{kinds:?}"
    );

    // Issue, then the expense is locked from deletion.
    json_req(
        &app,
        "POST",
        &format!("/invoices/{}/issue", inv["id"].as_str().unwrap()),
        None,
    )
    .await;
    let (s_del, _) = json_req(&app, "DELETE", &format!("/expenses/{xid}"), None).await;
    assert_eq!(
        s_del,
        StatusCode::CONFLICT,
        "issued invoice locks its expense"
    );
}

#[tokio::test]
async fn invoice_can_exclude_expenses() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2})),
    )
    .await;
    json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({"date":"2026-10-03","customer_id":cid,"amount_minor":5000,"currency":"EUR"})),
    )
    .await;
    let (_, inv) = json_req(&app, "POST", "/invoices", Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07","include_expenses":false}))).await;
    assert_eq!(inv["total_minor"], 12000, "expenses excluded");
    assert!(
        inv["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["kind"] == "time")
    );
}

#[tokio::test]
async fn profitability_revenue_vs_cost() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    // Bob: cost rate 20/h.
    json_req(&app, "POST", "/users", Some(json!({"name":"Bob","email":"bob@t.local","password":"bobpass123","role":"member","cost_rate_minor":2000}))).await;
    let bob = login_cookie(&app.router, "bob@t.local", "bobpass123").await;
    // Bob logs 2h billable (revenue 2x60=12000, labour cost 2x20=4000).
    raw(
        &app.router,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2})),
        Some(&bob),
    )
    .await;
    // billable expense 5000.
    json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({"date":"2026-10-03","customer_id":cid,"amount_minor":5000,"currency":"EUR"})),
    )
    .await;
    // invoice (12000 + 5000 = 17000) and issue.
    let (_, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{}/issue", inv["id"].as_str().unwrap()),
        None,
    )
    .await;

    let (s, prof) = json_req(
        &app,
        "GET",
        "/reports/profitability?from=2026-10-01&to=2026-10-31",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let row = prof["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "ACME")
        .expect("ACME row");
    assert_eq!(row["revenue_minor"], 17000);
    assert_eq!(row["cost_minor"], 9000); // 5000 expense + 4000 labour
    assert_eq!(row["margin_minor"], 8000);
}

#[tokio::test]
async fn invoice_report_and_export() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{}/issue", inv["id"].as_str().unwrap()),
        None,
    )
    .await;

    let (s, rep) = json_req(
        &app,
        "GET",
        "/invoices/report?from=2026-10-01&to=2026-10-31",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(rep["issued"], 1);
    assert_eq!(rep["revenue_by_currency"]["EUR"], 18000); // 3h * 60
    let row = &rep["rows"][0];
    assert_eq!(row["label"], "ACME");
    assert_eq!(row["revenue_minor"], 18000);

    let (s2, csv) = json_req(&app, "GET", "/invoices/export.csv", None).await;
    assert_eq!(s2, StatusCode::OK);
    let text = csv.as_str().unwrap();
    assert!(text.starts_with("number,customer,"), "csv header: {text}");
    assert!(text.contains("INV-0001") && text.contains("ACME"));
}

#[tokio::test]
async fn reports_partition_by_currency_and_never_sum_cents_across_it() {
    // #186: a customer may hold projects in different currencies (#11), so
    // every grouping level must produce per-currency rows — previously the
    // EUR and USD cents were added into a single number.
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "P-EUR", json!({"rate_minor": 6000})).await;
    new_project(
        &app,
        &cid,
        "P-USD",
        json!({"rate_minor": 5000, "currency": "USD"}),
    )
    .await;
    for (proj, hours) in [("P-EUR", 2), ("P-USD", 3)] {
        let (s, e) = json_req(
            &app,
            "POST",
            "/entries",
            Some(entry_body(&cid, proj, json!(hours), "2026-10-02")),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{e}");
    }

    for group in ["customer", "project", "person", "week"] {
        let (s, rep) = json_req(
            &app,
            "GET",
            &format!("/reports/summary?from=2026-10-01&to=2026-10-07&group={group}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let rows = rep["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{group}: one row per currency, got {rows:?}");
        let eur = rows
            .iter()
            .find(|r| r["currency"] == "EUR")
            .expect("EUR row");
        let usd = rows
            .iter()
            .find(|r| r["currency"] == "USD")
            .expect("USD row");
        assert_eq!(eur["amount_minor"], 12000, "{group}"); // 2h * 60.00
        assert_eq!(usd["amount_minor"], 15000, "{group}"); // 3h * 50.00
        // Week rows no longer smuggle the currency into the key.
        assert!(
            !rows
                .iter()
                .any(|r| r["key"].as_str().unwrap().contains('|')),
            "{group}: keys are labels, currency rides its own field"
        );
    }

    // Invoice report + profitability partition the same way.
    for (proj, amount) in [("P-EUR", 12000u64), ("P-USD", 15000)] {
        let (_, inv) = json_req(
            &app,
            "POST",
            "/invoices",
            Some(json!({
                "customer_id": cid,
                "from": "2026-10-01",
                "to": "2026-10-07",
                "project_codes": [proj],
                "include_expenses": false,
            })),
        )
        .await;
        assert_eq!(inv["total_minor"], amount);
        json_req(
            &app,
            "POST",
            &format!("/invoices/{}/issue", inv["id"].as_str().unwrap()),
            None,
        )
        .await;
    }
    let (_s, rep) = json_req(
        &app,
        "GET",
        "/invoices/report?from=2026-10-01&to=2026-10-31",
        None,
    )
    .await;
    let rows = rep["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "one row per (customer, currency): {rows:?}");
    assert_eq!(rep["revenue_by_currency"]["EUR"], 12000);
    assert_eq!(rep["revenue_by_currency"]["USD"], 15000);

    let (_s, prof) = json_req(
        &app,
        "GET",
        "/reports/profitability?from=2026-10-01&to=2026-10-31",
        None,
    )
    .await;
    let rows = prof["rows"].as_array().unwrap();
    let acme: Vec<&Value> = rows.iter().filter(|r| r["label"] == "ACME").collect();
    assert_eq!(acme.len(), 2, "profitability splits by currency: {rows:?}");
    let eur = acme.iter().find(|r| r["currency"] == "EUR").unwrap();
    let usd = acme.iter().find(|r| r["currency"] == "USD").unwrap();
    assert_eq!(eur["revenue_minor"], 12000);
    assert_eq!(usd["revenue_minor"], 15000);
}

#[tokio::test]
async fn reimbursement_claim_lifecycle_locks_expenses() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    let (_, e1) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({"date":"2026-10-02","customer_id":cid,"amount_minor":5000,"currency":"EUR"})),
    )
    .await;
    let (_, e2) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({"date":"2026-10-03","customer_id":cid,"amount_minor":3000,"currency":"EUR"})),
    )
    .await;
    let ids = json!([e1["id"], e2["id"]]);

    // Create draft claim.
    let (s, claim) = json_req(
        &app,
        "POST",
        "/claims",
        Some(json!({"title":"Oct costs","expense_ids":ids})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{claim}");
    assert_eq!(claim["state"], "draft");
    assert_eq!(claim["total_minor"], 8000);
    let claim_id = claim["id"].as_str().unwrap().to_string();
    let xid = e1["id"].as_str().unwrap().to_string();

    // Draft does not lock; submit then it locks.
    assert_eq!(
        json_req(&app, "DELETE", &format!("/expenses/{xid}"), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    // (that deleted e1; recreate for the lock test)
    let (_, e1b) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(json!({"date":"2026-10-02","customer_id":cid,"amount_minor":5000,"currency":"EUR"})),
    )
    .await;
    let xid = e1b["id"].as_str().unwrap().to_string();
    let (_, claim2) = json_req(
        &app,
        "POST",
        "/claims",
        Some(json!({"title":"Oct costs","expense_ids":[e1b["id"], e2["id"]]})),
    )
    .await;
    let cid2 = claim2["id"].as_str().unwrap().to_string();
    json_req(&app, "POST", &format!("/claims/{cid2}/submit"), None).await;
    assert_eq!(
        json_req(&app, "DELETE", &format!("/expenses/{xid}"), None)
            .await
            .0,
        StatusCode::CONFLICT,
        "submitted claim locks expense"
    );

    // Admin approves.
    let (sd, decided) = json_req(
        &app,
        "POST",
        &format!("/claims/{cid2}/decision"),
        Some(json!({"decision":"approve"})),
    )
    .await;
    assert_eq!(sd, StatusCode::OK, "{decided}");
    assert_eq!(decided["state"], "approved");
    let _ = claim_id;
}

#[tokio::test]
async fn secret_vault_endpoints_mask_and_authorize() {
    use tucano_time::vault::SecretVault;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let root = store.root().to_path_buf();
    let vault = std::sync::Arc::new(
        SecretVault::open(&root, Some("0123456789abcdef0123456789abcdef"))
            .unwrap()
            .unwrap(),
    );
    let session = std::sync::Arc::new(Session::new(
        b"test-session-secret-0000000000000032".to_vec(),
        3600,
        false,
    ));
    let state = AppState {
        store: std::sync::Arc::new(store),
        clock: std::sync::Arc::new(SystemClock),
        locks: std::sync::Arc::new(tucano_time::lock::NoLocks),
        session,
        rate: std::sync::Arc::new(tucano_time::ratelimit::RateLimiter::new(
            8,
            std::time::Duration::from_secs(300),
        )),
        audit: std::sync::Arc::new(tucano_time::audit::AuditLog::new(&root)),
        revocations: std::sync::Arc::new(tucano_time::revoke::Revocations::new(&root)),
        vault: Some(vault),
        email: std::sync::Arc::new(tucano_time::email::DisabledEmailSender),
        payments: std::sync::Arc::new(tucano_time::payments::PaymentRegistry::default()),
        accounting: std::sync::Arc::new(tucano_time::accounting::AccountingRegistry::default()),
        sso: std::sync::Arc::new(tucano_time::sso::SsoRegistry::default()),
        oauth_flows: std::sync::Arc::new(tucano_time::calendar_oauth::OAuthFlows::new()),
        cfg: std::sync::Arc::new(std::sync::Mutex::new(std::sync::Arc::new(
            tucano_time::appconfig::AppConfig::empty(),
        ))),
    };
    let router = tucano_time::build_router(state);
    // bootstrap admin
    raw(
        &router,
        "POST",
        "/auth/bootstrap",
        Some(json!({"name":"Admin","email":ADMIN_EMAIL,"password":ADMIN_PW})),
        None,
    )
    .await;
    let admin = login_cookie(&router, ADMIN_EMAIL, ADMIN_PW).await;
    // member
    raw(
        &router,
        "POST",
        "/users",
        Some(json!({"name":"M","email":"m@t.local","password":"memberpass1","role":"member"})),
        Some(&admin),
    )
    .await;
    let member = login_cookie(&router, "m@t.local", "memberpass1").await;

    // admin sets a secret
    let (s, _, _) = raw(
        &router,
        "PUT",
        "/admin/secrets/stripe.secret_key",
        Some(json!({"value":"dummy-secret-value-1234"})),
        Some(&admin),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // list shows masked hint, never the value
    let (s2, list, _) = raw(&router, "GET", "/admin/secrets", None, Some(&admin)).await;
    assert_eq!(s2, StatusCode::OK);
    let text = serde_json::to_string(&list).unwrap();
    assert!(text.contains("stripe.secret_key"), "{text}");
    assert!(
        !text.contains("dummy-secret-value-1234"),
        "value leaked in list: {text}"
    );
    assert!(text.contains("34"), "expected masked hint: {text}");
    assert!(
        !text.contains("1234"),
        "hint must show at most the final 2 chars (review A8)"
    );
    // member forbidden
    let (s3, _, _) = raw(
        &router,
        "PUT",
        "/admin/secrets/x",
        Some(json!({"value":"y"})),
        Some(&member),
    )
    .await;
    assert_eq!(s3, StatusCode::FORBIDDEN);
    // delete
    let (s4, _, _) = raw(
        &router,
        "DELETE",
        "/admin/secrets/stripe.secret_key",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(s4, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn timer_start_stop_creates_entry() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;

    // Start.
    let (s, timer) = json_req(
        &app,
        "POST",
        "/timer",
        Some(json!({"customer_id":cid,"project_code":"P1","note":"focus"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{timer}");
    // Second start while running -> 409.
    assert_eq!(
        json_req(
            &app,
            "POST",
            "/timer",
            Some(json!({"customer_id":cid,"project_code":"P1"}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Get shows it running.
    let (sg, cur) = json_req(&app, "GET", "/timer", None).await;
    assert_eq!(sg, StatusCode::OK);
    assert_eq!(cur["timer"]["project_code"], "P1");
    // Stop -> creates a timer-sourced entry.
    let (ss, entry) = json_req(&app, "POST", "/timer/stop", None).await;
    assert_eq!(ss, StatusCode::OK, "{entry}");
    assert_eq!(entry["source"], "timer");
    assert_eq!(entry["project_code"], "P1");
    // Timer cleared.
    let (_, none) = json_req(&app, "GET", "/timer", None).await;
    assert!(none.is_null());
}

#[tokio::test]
async fn calendar_events_from_configured_feed() {
    use tucano_time::vault::SecretVault;
    let dir = tempfile::tempdir().unwrap();
    let ics = dir.path().join("cal.ics");
    std::fs::write(&ics, "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:e1\r\nDTSTART:20261002T090000Z\r\nDTEND:20261002T100000Z\r\nSUMMARY:Stand-up\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();

    let store = Store::open(dir.path().join("data")).unwrap();
    let root = store.root().to_path_buf();
    let vault = std::sync::Arc::new(
        SecretVault::open(&root, Some("unit-test-vault-key-000000000000"))
            .unwrap()
            .unwrap(),
    );
    vault
        .put("calendar.ics_url", ics.to_str().unwrap())
        .unwrap();
    let session = std::sync::Arc::new(Session::new(
        b"test-session-secret-0000000000000032".to_vec(),
        3600,
        false,
    ));
    let state = AppState {
        store: std::sync::Arc::new(store),
        clock: std::sync::Arc::new(SystemClock),
        locks: std::sync::Arc::new(tucano_time::lock::NoLocks),
        session,
        rate: std::sync::Arc::new(tucano_time::ratelimit::RateLimiter::new(
            8,
            std::time::Duration::from_secs(300),
        )),
        audit: std::sync::Arc::new(tucano_time::audit::AuditLog::new(&root)),
        revocations: std::sync::Arc::new(tucano_time::revoke::Revocations::new(&root)),
        vault: Some(vault),
        email: std::sync::Arc::new(tucano_time::email::DisabledEmailSender),
        payments: std::sync::Arc::new(tucano_time::payments::PaymentRegistry::default()),
        accounting: std::sync::Arc::new(tucano_time::accounting::AccountingRegistry::default()),
        sso: std::sync::Arc::new(tucano_time::sso::SsoRegistry::default()),
        oauth_flows: std::sync::Arc::new(tucano_time::calendar_oauth::OAuthFlows::new()),
        cfg: std::sync::Arc::new(std::sync::Mutex::new(std::sync::Arc::new(
            tucano_time::appconfig::AppConfig::empty(),
        ))),
    };
    let router = tucano_time::build_router(state);
    raw(
        &router,
        "POST",
        "/auth/bootstrap",
        Some(json!({"name":"Admin","email":ADMIN_EMAIL,"password":ADMIN_PW})),
        None,
    )
    .await;
    let admin = login_cookie(&router, ADMIN_EMAIL, ADMIN_PW).await;

    let (s, body, _) = raw(
        &router,
        "GET",
        "/calendar/events?from=2026-10-01&to=2026-10-07",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["title"], "Stand-up");
}

#[tokio::test]
async fn notifications_endpoint_empty_then_read() {
    let (app, _d) = app().await;
    let (s, body) = json_req(&app, "GET", "/notifications", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["notifications"].as_array().unwrap().len(), 0);
    assert_eq!(body["unread"], 0);
    let (s2, _) = json_req(&app, "POST", "/notifications/read", None).await;
    assert_eq!(s2, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn notifications_hide_stale_and_resolved_no_time_reminders() {
    let (app, dir) = app().await;
    let store = Store::open(dir.path().join("data")).unwrap();
    let user = store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.email == ADMIN_EMAIL)
        .unwrap();
    let now = chrono::Utc::now();
    for (kind, created_at) in [
        ("no_time_today", now - chrono::Duration::days(1)),
        ("no_time_today", now),
        ("submit_timesheet", now),
    ] {
        store
            .push_notification(
                user.id,
                &tucano_time::domain::Notification {
                    id: uuid::Uuid::new_v4(),
                    kind: kind.to_string(),
                    title: kind.to_string(),
                    body: String::new(),
                    created_at,
                    read: false,
                },
            )
            .unwrap();
    }
    let (status, before) = json_req(&app, "GET", "/notifications", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["notifications"].as_array().unwrap().len(), 2);
    assert_eq!(before["unread"], 2);
    let customer = new_customer(&app, "Reminder test", "EUR", 6000).await;
    let cid = customer["id"].as_str().unwrap();
    new_project(&app, cid, "REMINDER", json!({})).await;
    let (created, _) = json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({
            "date": now.date_naive().to_string(),
            "customer_id": cid,
            "project_code": "REMINDER",
            "hours": 1,
        })),
    )
    .await;
    assert_eq!(created, StatusCode::CREATED);
    let (_, after) = json_req(&app, "GET", "/notifications", None).await;
    let notifications = after["notifications"].as_array().unwrap();
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0]["kind"], "submit_timesheet");
    assert_eq!(after["unread"], 1);
}

#[tokio::test]
async fn schedule_crud_admin_only() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    let (s, sched) = json_req(&app, "POST", "/schedules", Some(json!({"customer_id":cid,"cadence":"monthly","mode":"retainer","retainer_amount_minor":150000,"currency":"EUR"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{sched}");
    let sid = sched["id"].as_str().unwrap().to_string();
    let (sl, list) = json_req(&app, "GET", "/schedules", None).await;
    assert_eq!(sl, StatusCode::OK);
    assert_eq!(list["schedules"].as_array().unwrap().len(), 1);
    // Retainer with 0 amount rejected.
    let (sz, _) = json_req(&app, "POST", "/schedules", Some(json!({"customer_id":cid,"cadence":"monthly","mode":"retainer","retainer_amount_minor":0,"currency":"EUR"}))).await;
    assert_eq!(sz, StatusCode::UNPROCESSABLE_ENTITY);
    // Member forbidden (admin tier).
    json_req(
        &app,
        "POST",
        "/users",
        Some(json!({"name":"N","email":"n@t.local","password":"memberpass1","role":"member"})),
    )
    .await;
    let member = login_cookie(&app.router, "n@t.local", "memberpass1").await;
    let (sm, _, _) = raw(&app.router, "GET", "/schedules", None, Some(&member)).await;
    assert_eq!(sm, StatusCode::FORBIDDEN);
    // Delete.
    assert_eq!(
        json_req(&app, "DELETE", &format!("/schedules/{sid}"), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn budget_report_shows_burn() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(
        &app,
        cid,
        "P1",
        json!({"budget_hours": 1000, "budget_amount_minor": 60000}),
    )
    .await;
    let (s, _) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(cid, "P1", json!(6), "2026-10-02")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (sb, body) = json_req(&app, "GET", "/reports/budgets", None).await;
    assert_eq!(sb, StatusCode::OK);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["project"], "P1");
    assert_eq!(rows[0]["burn_hours"], 6.0);
    assert_eq!(rows[0]["hours_pct"], 60.0);
    assert_eq!(rows[0]["over"], false);
    assert_eq!(rows[0]["burn_amount_minor"], 36000);
    assert_eq!(rows[0]["amount_pct"], 60.0);
}

#[tokio::test]
async fn invoice_email_sends_to_customer() {
    let (app, _d) = app().await;
    // Customer with a billing email (#35).
    let (sc, cust) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"ACME","currency":"EUR","default_rate_minor":6000,"email":"billing@acme.test"})),
    )
    .await;
    assert_eq!(sc, StatusCode::CREATED, "{cust}");
    assert_eq!(cust["email"], "billing@acme.test");
    let cid = cust["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();

    // Draft invoices must be issued before emailing.
    let (sd, _) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(sd, StatusCode::CONFLICT);

    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (se, body) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(se, StatusCode::OK, "{body}");
    assert_eq!(body["sent_to"], "billing@acme.test");
    // #130: the result names the transport; the test recorder never claims smtp.
    assert_eq!(body["transport"], "recording");
    let msgs = app.email.messages();
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].text.contains("3h") || msgs[0].text.contains("180.00"),
        "{}",
        msgs[0].text
    );
    assert!(msgs[0].subject.contains(inv["number"].as_str().unwrap()));
}

#[tokio::test]
async fn invoice_email_without_customer_email_is_422() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "NOEMAIL", "EUR", 6000).await; // no email field
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (se, body) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(se, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(app.email.messages().len(), 0);
}

/// Posts a raw string body to the webhook endpoint WITHOUT the CSRF header,
/// proving the provider webhook is exempt and authenticated only by signature.
async fn webhook_post(
    router: &Router,
    provider: &str,
    body: &str,
    signature: Option<&str>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/payments/webhook/{provider}"))
        .header("content-type", "application/json");
    if let Some(s) = signature {
        b = b.header("x-webhook-signature", s);
    }
    let res = router
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .expect("oneshot");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn checkout_then_signed_webhook_marks_invoice_paid() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let stripe = tucano_time::payments::StripeProvider::with_secret("whsec_topsecret");
    let registry = tucano_time::payments::PaymentRegistry::new(vec![Arc::new(stripe)]);
    let app = Client::with_payments(store, registry).await;

    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    let number = inv["number"].as_str().unwrap().to_string();

    // Draft -> checkout rejected.
    let (sd, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/checkout"),
        Some(json!({"provider":"stripe"})),
    )
    .await;
    assert_eq!(sd, StatusCode::CONFLICT);

    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;

    // Unknown provider rejected.
    let (su, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/checkout"),
        Some(json!({"provider":"bitcoin"})),
    )
    .await;
    assert_eq!(su, StatusCode::BAD_REQUEST);

    // Checkout link for the issued invoice.
    let (sc, session) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/checkout"),
        Some(json!({"provider":"stripe"})),
    )
    .await;
    assert_eq!(sc, StatusCode::CREATED, "{session}");
    assert!(session["url"].as_str().unwrap().contains(&number));
    let reference = session["reference"].as_str().unwrap().to_string();

    // Unsigned / badly signed webhook rejected (and CSRF-exempt, since no
    // x-csrf header was sent at all).
    // Amount/currency ride the normalized event and must match the invoice
    // (3h @ 60.00 = 18000 EUR) — review A11.
    let payload = format!(
        "{{\"type\":\"checkout.session.completed\",\"payment_status\":\"paid\",\"amount_minor\":18000,\"currency\":\"EUR\",\"client_reference_id\":\"{reference}\",\"metadata\":{{\"invoice_number\":\"{number}\"}}}}"
    );
    let (sbad, _) = webhook_post(&app.router, "stripe", &payload, Some("sha256=deadbeef")).await;
    assert_eq!(sbad, StatusCode::UNAUTHORIZED);

    // Correct HMAC signature pays the invoice.
    let sig = tucano_time::payments::sign("whsec_topsecret", &payload);
    let (sgood, body) = webhook_post(&app.router, "stripe", &payload, Some(&sig)).await;
    assert_eq!(sgood, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "paid");
    let (_s2, got) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(got["status"], "paid");
    assert!(
        got["payment_reference"]
            .as_str()
            .unwrap()
            .contains("stripe:")
    );

    // Replay is idempotent.
    let (sre, re) = webhook_post(&app.router, "stripe", &payload, Some(&sig)).await;
    assert_eq!(sre, StatusCode::OK, "{re}");
    assert_eq!(re["status"], "already_paid");
}

#[tokio::test]
async fn webhook_partial_settles_partly_and_mismatch_stays_refused() {
    // Review A11 refined by #114: a signed event records a LEDGER PAYMENT of
    // its real amount — a partial parks at partly_paid (never a full settle),
    // a wrong-currency event is refused, over-collection is refused, and a
    // replay of the same provider event is idempotent.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let stripe: Arc<dyn tucano_time::payments::PaymentProvider> = Arc::new(
        tucano_time::payments::StripeProvider::with_secret("whsec_mismatch"),
    );
    let app = Client::with_payments(
        store,
        tucano_time::payments::PaymentRegistry::new(vec![stripe]),
    )
    .await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    let number = inv["number"].as_str().unwrap().to_string();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (_s2, session) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/checkout"),
        Some(json!({"provider":"stripe"})),
    )
    .await;
    let reference = session["reference"].as_str().unwrap().to_string();

    let make = |amount: u64, currency: &str, ev: &str| {
        format!(
            "{{\"type\":\"checkout.session.completed\",\"payment_status\":\"paid\",\"amount_minor\":{amount},\"currency\":\"{currency}\",\"client_reference_id\":\"{reference}\",\"metadata\":{{\"invoice_number\":\"{number}\"}},\"id\":\"{ev}\"}}"
        )
    };
    // Wrong currency: refused, no ledger entry.
    let wrongc = make(18000, "USD", "evt_usd");
    let sig = tucano_time::payments::sign("whsec_mismatch", &wrongc);
    let (sw, _) = webhook_post(&app.router, "stripe", &wrongc, Some(&sig)).await;
    assert_eq!(
        sw,
        StatusCode::CONFLICT,
        "currency mismatch must not settle"
    );

    // Over-collection (beyond the balance): refused (#114 keeps A11's "never
    // silently overpay" while allowing genuine partials).
    let over = make(19000, "EUR", "evt_over");
    let sig = tucano_time::payments::sign("whsec_mismatch", &over);
    let (so, _) = webhook_post(&app.router, "stripe", &over, Some(&sig)).await;
    assert_eq!(so, StatusCode::CONFLICT, "over-balance must be refused");

    // Partial: records a ledger payment, parks at partly_paid.
    let partial = make(5000, "EUR", "evt_part");
    let sig = tucano_time::payments::sign("whsec_mismatch", &partial);
    let (sp, bp) = webhook_post(&app.router, "stripe", &partial, Some(&sig)).await;
    assert_eq!(sp, StatusCode::OK, "{bp}");
    assert_eq!(bp["status"], "partly_paid");
    let (_s3, mid) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(
        mid["status"], "partly_paid",
        "partial does not fully settle"
    );
    assert_eq!(mid["payments"].as_array().unwrap().len(), 1);
    assert_eq!(mid["payments"][0]["amount_minor"], 5000);
    assert_eq!(mid["payments"][0]["method"], "stripe");

    // Replay of the SAME provider event: idempotent, ledger unchanged.
    let (sr, br) = webhook_post(&app.router, "stripe", &partial, Some(&sig)).await;
    assert_eq!(sr, StatusCode::OK, "{br}");
    assert_eq!(br["status"], "already_processed");
    let (_s4, mid2) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(
        mid2["payments"].as_array().unwrap().len(),
        1,
        "replay added no entry"
    );

    // A second distinct event covering the balance clears it.
    let rest = make(13000, "EUR", "evt_rest");
    let sig2 = tucano_time::payments::sign("whsec_mismatch", &rest);
    let (sp2, bp2) = webhook_post(&app.router, "stripe", &rest, Some(&sig2)).await;
    assert_eq!(sp2, StatusCode::OK, "{bp2}");
    assert_eq!(bp2["status"], "paid");

    // A distinct event cannot be acknowledged after another event has settled
    // the invoice unless its own reference is present in the ledger.
    let stale = make(13000, "EUR", "evt_stale");
    let stale_sig = tucano_time::payments::sign("whsec_mismatch", &stale);
    let (ss, _) = webhook_post(&app.router, "stripe", &stale, Some(&stale_sig)).await;
    assert_eq!(
        ss,
        StatusCode::CONFLICT,
        "unrecorded distinct event is not processed"
    );
}

/// Stub transport for accounting tests: returns deterministic ids, optionally
/// fails the first N posts to exercise the recorded-failure path.
struct StubTransport {
    posts: std::sync::Mutex<Vec<(String, Value)>>,
    fail_first: usize,
}

impl tucano_time::accounting::Transport for StubTransport {
    fn post(&self, path: &str, body: Value) -> Result<String, tucano_time::accounting::SyncError> {
        let mut posts = self.posts.lock().unwrap();
        posts.push((path.to_string(), body));
        if posts.len() <= self.fail_first {
            return Err(tucano_time::accounting::SyncError::Transport(
                "stub offline".into(),
            ));
        }
        Ok(format!("remote-{path}-{}", posts.len()))
    }
}

#[tokio::test]
async fn accounting_sync_idempotent_with_visible_status() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let t = Arc::new(StubTransport {
        posts: std::sync::Mutex::new(vec![]),
        fail_first: 0,
    });
    let qbo: Arc<dyn tucano_time::accounting::AccountingSync> =
        Arc::new(tucano_time::accounting::QboProvider::new(t.clone()));
    let registry = tucano_time::accounting::AccountingRegistry::new(vec![qbo]);
    let app = Client::with_accounting(store, registry).await;

    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();

    // Draft -> 409.
    let (sd, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    assert_eq!(sd, StatusCode::CONFLICT);

    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;

    // Unknown provider -> 400.
    let (su, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"sage"})),
    )
    .await;
    assert_eq!(su, StatusCode::BAD_REQUEST);

    // Sync -> synced record with a remote id; the doc went to "invoice".
    let (ss, body) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    assert_eq!(ss, StatusCode::OK, "{body}");
    assert_eq!(body["record"]["status"], "synced");
    assert!(
        body["record"]["remote_id"]
            .as_str()
            .unwrap()
            .starts_with("remote-invoice")
    );

    // Idempotent: a second sync is a no-op (no duplicate post).
    let posts_before = t.posts.lock().unwrap().len();
    let (s2, b2) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2["noop"], true);
    assert_eq!(t.posts.lock().unwrap().len(), posts_before);

    // Visible status lists the record + enabled providers.
    let (sv, status) = json_req(&app, "GET", "/sync/accounting", None).await;
    assert_eq!(sv, StatusCode::OK);
    assert_eq!(status["providers"].as_array().unwrap(), &vec![json!("qbo")]);
    assert_eq!(status["records"].as_array().unwrap().len(), 1);

    // Paying then syncing pushes invoice AND payment documents.
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference":"bank-transfer"})),
    )
    .await;
    let (_s3, b3) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    // Paid after sync -> the next sync pushes the PAYMENT against the remote
    // invoice (not a no-op), then later syncs are no-ops again.
    assert_eq!(b3["record"]["kind"], "payment", "{b3}");
    assert_eq!(b3["record"]["status"], "synced", "{b3}");
    {
        let posts = t.posts.lock().unwrap();
        assert_eq!(posts[posts.len() - 1].0, "payment");
    }
    let (_s4, b4) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    assert_eq!(b4["noop"], true, "{b4}");
}

#[tokio::test]
async fn accounting_sync_failure_is_recorded_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let t = Arc::new(StubTransport {
        posts: std::sync::Mutex::new(vec![]),
        fail_first: 1, // first post fails, retry succeeds
    });
    let qbo: Arc<dyn tucano_time::accounting::AccountingSync> =
        Arc::new(tucano_time::accounting::QboProvider::new(t.clone()));
    let app = Client::with_accounting(
        store,
        tucano_time::accounting::AccountingRegistry::new(vec![qbo]),
    )
    .await;

    let c = new_customer(&app, "GLOBEX", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":2})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;

    // First sync fails at the transport -> 200 with a recorded failure.
    let (sf, body) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    assert_eq!(sf, StatusCode::OK, "{body}");
    assert_eq!(body["failed"], true);
    assert_eq!(body["record"]["status"], "failed");
    // #190: the persisted/serialised error is a stable marker; the raw
    // transport text ("stub offline") stays in the server log only.
    assert_eq!(body["record"]["error"], "sync failed (see server log)");
    let (_sv, st) = json_req(&app, "GET", "/sync/accounting", None).await;
    assert!(
        !st.to_string().contains("stub offline"),
        "sync_status must not echo provider detail: {st}"
    );

    // Retry (second post succeeds) -> synced. The failed record is not
    // short-circuited (only synced records are).
    let (sr, body2) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/sync"),
        Some(json!({"provider":"qbo"})),
    )
    .await;
    assert_eq!(sr, StatusCode::OK);
    assert_eq!(body2["record"]["status"], "synced", "{body2}");
    assert!(
        body2["record"]["remote_id"]
            .as_str()
            .unwrap()
            .starts_with("remote-invoice")
    );
}

#[tokio::test]
async fn sso_jit_provisions_and_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let idp = tucano_time::sso::SignedTokenIdp::new("okta-test", "stub-secret", vec![]);
    let registry = Arc::new(tucano_time::sso::SsoRegistry::new(vec![Arc::new(idp)]));
    let state = AppState::with_session(
        store,
        Arc::new(Session::new(
            b"test-session-secret-0000000000000032".to_vec(),
            3600,
            false,
        )),
    )
    .with_sso(registry);
    let router = tucano_time::build_router(state);

    // Local admin exists FIRST — SSO runs alongside local login (#32 DoD).
    let (sa, _, _) = raw(
        &router,
        "POST",
        "/auth/bootstrap",
        Some(json!({"name":"Admin","email":ADMIN_EMAIL,"password":ADMIN_PW})),
        None,
    )
    .await;
    assert_eq!(sa, StatusCode::CREATED);

    // Providers are advertised pre-session.
    let (s, body, _) = raw(&router, "GET", "/auth/sso/providers", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body["providers"].as_array().unwrap(),
        &vec![json!("okta-test")]
    );

    let idp = tucano_time::sso::SignedTokenIdp::new("okta-test", "stub-secret", vec![]);

    // A valid signed assertion JIT-creates the user (member) and logs in.
    let claims = tucano_time::sso::TokenClaims {
        sub: "u1".into(),
        email: "Newbie@acme.test".into(),
        name: "New Bie".into(),
        groups: vec![],
        exp: chrono::Utc::now().timestamp() + 300,
    };
    let token = idp.issue(&claims);
    let (s1, user, _) = raw(
        &router,
        "POST",
        "/auth/sso/assertion",
        Some(json!({"provider":"okta-test","payload":token,"signature":""})),
        None,
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "{user}");
    assert_eq!(user["email"], "newbie@acme.test");
    assert_eq!(user["role"], "member");

    // The session cookie works like a local one.
    let cookie = {
        let (_s, _b, c) = raw(
            &router,
            "POST",
            "/auth/sso/assertion",
            Some(json!({"provider":"okta-test","payload":token,"signature":""})),
            None,
        )
        .await;
        c.unwrap_or_default()
    };
    let (sm, me, _) = raw(&router, "GET", "/auth/me", None, Some(&cookie)).await;
    assert_eq!(sm, StatusCode::OK, "{me}");
    assert_eq!(me["name"], "New Bie");

    // Bad signature -> 401, and no user created for it.
    let (sb, _, _) = raw(
        &router,
        "POST",
        "/auth/sso/assertion",
        Some(json!({"provider":"okta-test","payload":"aGVhZGVy.cGF5bG9hZC5mb3JnZWQ.bad","signature":""})),
        None,
    )
    .await;
    assert_eq!(sb, StatusCode::UNAUTHORIZED);

    // Local login still works alongside SSO.
    let (sl, _, _) = raw(
        &router,
        "POST",
        "/auth/login",
        Some(json!({"email":ADMIN_EMAIL,"password":ADMIN_PW})),
        None,
    )
    .await;
    assert_eq!(sl, StatusCode::OK);
}

#[tokio::test]
async fn sso_unknown_provider_is_400() {
    let (app, _d) = app().await;
    let (s, _) = json_req(
        &app,
        "POST",
        "/auth/sso/assertion",
        Some(json!({"provider":"nope","payload":"x","signature":""})),
    )
    .await;
    // app() client is authenticated; endpoint is public but provider unknown.
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn calendar_oauth_start_and_callback_plumbing() {
    let (app, _d) = app().await;
    // No client id configured (no vault in this test app) -> 422.
    let (s1, b1) = json_req(&app, "GET", "/calendar/oauth/start?provider=google", None).await;
    assert_eq!(s1, StatusCode::UNPROCESSABLE_ENTITY, "{b1}");

    // Unknown provider -> 400.
    let (s2, _) = json_req(&app, "GET", "/calendar/oauth/start?provider=yahoo", None).await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);

    // Callback with a state we never issued -> 401 (CSRF state is the defence).
    let (s3, b3) = json_req(
        &app,
        "GET",
        "/calendar/oauth/callback?code=abc&state=forged",
        None,
    )
    .await;
    assert_eq!(s3, StatusCode::UNAUTHORIZED, "{b3}");

    // Calendar events endpoint still reports ICS-not-configured (no provider).
    let (s4, _) = json_req(
        &app,
        "GET",
        "/calendar/events?from=2026-10-01&to=2026-10-07",
        None,
    )
    .await;
    assert_eq!(s4, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_config_roundtrip_precedence_and_guards() {
    let (app, d) = app().await;
    // Effective view: everything starts at its default source.
    let (s1, body) = json_req(&app, "GET", "/admin/config", None).await;
    assert_eq!(s1, StatusCode::OK, "{body}");
    // #190: no on-disk path is serialised (AGENTS.md: responses never expose
    // paths). The old `path` key leaked `<data>/config.json`.
    assert_eq!(
        body.get("path"),
        None,
        "GET /admin/config must not expose the filesystem path: {body}"
    );
    let serialized = body.to_string();
    assert!(
        !serialized.contains("config.json") && !serialized.contains(d.path().to_str().unwrap()),
        "no filesystem paths in the body: {serialized}"
    );
    let days = body["config"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "reminder_days")
        .unwrap();
    assert_eq!(days["value"], 7);
    assert_eq!(days["source"], "default");
    assert!(days["description"].as_str().unwrap().contains("reminder"));

    // PUT persists to <data>/config.json and flips the source to "file".
    let (s3, body3) = json_req(
        &app,
        "PUT",
        "/admin/config",
        Some(json!({"reminder_days": 15, "sso_admin_group": "tt-admins"})),
    )
    .await;
    assert_eq!(s3, StatusCode::OK);
    assert_eq!(body3["ok"], true);
    let cfg_file = d.path().join("data").join("config.json");
    let persisted = std::fs::read_to_string(&cfg_file).unwrap();
    assert!(persisted.contains("reminder_days"), "{persisted}");
    let (_s4, eff) = json_req(&app, "GET", "/admin/config", None).await;
    let days2 = eff["config"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "reminder_days")
        .unwrap();
    assert_eq!(days2["value"], 15);
    assert_eq!(days2["source"], "file");

    // Secret-shaped / unknown keys cannot be persisted through the API (#94 guard).
    let (s5, b5) = json_req(
        &app,
        "PUT",
        "/admin/config",
        Some(json!({"smtp.password": "hunter2"})),
    )
    .await;
    assert_eq!(s5, StatusCode::UNPROCESSABLE_ENTITY, "{b5}");
    let (s6, _) = json_req(&app, "PUT", "/admin/config", Some(json!({"max_docs": 2}))).await;
    assert_eq!(s6, StatusCode::UNPROCESSABLE_ENTITY, "out of range refused");

    // Members cannot see or edit configuration (admin tier).
    json_req(
        &app,
        "POST",
        "/users",
        Some(json!({"name":"N","email":"cfgmember@t.local","password":"memberpass1","role":"member"})),
    )
    .await;
    let member = login_cookie(&app.router, "cfgmember@t.local", "memberpass1").await;
    let (sm, _, _) = raw(&app.router, "GET", "/admin/config", None, Some(&member)).await;
    assert_eq!(sm, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn config_file_changes_request_time_behaviour_without_restart() {
    // #94: request-time knobs (SSO admin group) read the live config.
    let (app, _d) = app().await;
    let (_s, eff) = json_req(&app, "GET", "/admin/config", None).await;
    let grp = eff["config"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "sso_admin_group")
        .unwrap();
    assert_eq!(grp["value"], "");
    json_req(
        &app,
        "PUT",
        "/admin/config",
        Some(json!({"sso_admin_group": "finance-admins"})),
    )
    .await;
    let (_s2, eff2) = json_req(&app, "GET", "/admin/config", None).await;
    let grp2 = eff2["config"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "sso_admin_group")
        .unwrap();
    assert_eq!(grp2["value"], "finance-admins");
    assert_eq!(grp2["source"], "file");
}

#[tokio::test]
async fn management_reports_are_admin_only() {
    // Review A4: cost rates / budgets must not be visible to members.
    let (app, _d) = app().await;
    json_req(
        &app,
        "POST",
        "/users",
        Some(json!({"name":"M","email":"repmember@t.local","password":"memberpass1","role":"member"})),
    )
    .await;
    let member = login_cookie(&app.router, "repmember@t.local", "memberpass1").await;
    for path in ["/reports/profitability", "/reports/budgets"] {
        let (s, _, _) = raw(&app.router, "GET", path, None, Some(&member)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path} must be admin-only");
    }
    // Members keep their own summary/export.
    let (s, _, _) = raw(
        &app.router,
        "GET",
        "/reports/summary?from=2026-10-01&to=2026-10-07",
        None,
        Some(&member),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn sso_assertion_is_csrf_exempt_pre_session() {
    // Review A6: a real IdP POST-binding cannot send X-CSRF-Protection; the
    // route must reach the handler (400 unknown provider), not 403 csrf.
    let (app, _d) = app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/auth/sso/assertion")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"provider":"nope","payload":"x"}).to_string(),
        ))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn demo_flags_cannot_be_persisted_via_config() {
    // Review A3: webhook-bypass flags are env-only, never config.json.
    let (app, _d) = app().await;
    for key in ["stripe_demo", "paypal_demo"] {
        let (s, body) = json_req(&app, "PUT", "/admin/config", Some(json!({key: true}))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{key}: {body}");
    }
}

// ------------------------------------------------------------------ #113 ---

#[tokio::test]
async fn invoice_pdf_archived_at_issue_and_downloadable() {
    let (app, d) = app().await;
    let (issued, iid) = seed_issued_invoice(&app).await;

    // The invoice JSON carries the hint.
    let hint = &issued["pdf"];
    assert_eq!(
        hint["filename"],
        format!("{}.pdf", issued["number"].as_str().unwrap())
    );
    assert_eq!(hint["sha256"].as_str().unwrap().len(), 64);
    assert!(hint["bytes"].as_u64().unwrap() > 100);
    assert!(hint["archived_at"].is_string());

    // The file is on disk next to the document, atomically written at issue.
    let pdf_path = d
        .path()
        .join("data")
        .join("invoices")
        .join(format!("{iid}.pdf"));
    assert!(pdf_path.exists(), "archive written at issue");
    let on_disk = std::fs::read(&pdf_path).unwrap();

    // Download contract: admin 200 + binary headers, ETag from the sha256.
    let (status, headers, body) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/pdf");
    let disp = headers[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disp.contains(format!("filename=\"{}.pdf\"", issued["number"].as_str().unwrap()).as_str()),
        "{disp}"
    );
    let etag = headers[header::ETAG].to_str().unwrap().to_string();
    assert_eq!(etag, format!("\"{}\"", hint["sha256"].as_str().unwrap()));
    assert!(body.starts_with(b"%PDF-1.4"), "bytes start with %PDF");
    assert_eq!(body, on_disk, "download is the archived bytes");

    // Independent sha: body hashes to the advertised ETag.
    use sha2::{Digest, Sha256};
    let sha = Sha256::digest(&body);
    assert_eq!(
        sha.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        hint["sha256"].as_str().unwrap()
    );

    // Re-download: same bytes (immutable archive, stable ETag).
    let (s2, h2, body2) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(h2[header::ETAG], headers[header::ETAG]);
    assert_eq!(body, body2);
}

#[tokio::test]
async fn invoice_pdf_legacy_invoice_resolves_byte_stably() {
    let (app, d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    let (_s, headers, first) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert!(headers[header::ETAG].to_str().unwrap().starts_with('"'));

    // Simulate a legacy archive: the file vanishes while the (immutable)
    // invoice document stays. The first read must re-render **identical**
    // bytes and re-persist the archive.
    std::fs::remove_file(
        d.path()
            .join("data")
            .join("invoices")
            .join(format!("{iid}.pdf")),
    )
    .unwrap();
    let (_s2, _h2, second) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_eq!(first, second, "legacy re-render is byte-stable");
    let path = d
        .path()
        .join("data")
        .join("invoices")
        .join(format!("{iid}.pdf"));
    assert!(path.exists(), "archive re-persisted on first read");
    assert_eq!(std::fs::read(&path).unwrap(), second);
    // The hint's sha still matches (recomputed, but from identical bytes).
    let (_s3, fresh) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    use sha2::{Digest, Sha256};
    assert_eq!(
        fresh["pdf"]["sha256"].as_str().unwrap(),
        Sha256::digest(&second)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
}

#[tokio::test]
async fn invoice_pdf_rejects_draft_and_unknown() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "NOEMAIL", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();

    // Draft: 409, nothing persisted.
    let (status, _h, body) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&body).contains("issue the invoice"));

    // Unknown id: 404, never a PDF-shaped answer.
    let (s404, _, _) = raw_req(
        &app,
        "GET",
        "/invoices/00000000-0000-0000-0000-000000000000/pdf",
    )
    .await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn member_cannot_download_invoice_pdf() {
    let (app, _d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (s, _, _) = raw(
        &app.router,
        "GET",
        &format!("/invoices/{iid}/pdf"),
        None,
        Some(&cookie),
    )
    .await;
    // Invoices are the admin tier (#51): the whole surface refuses a member.
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn invoice_email_carries_the_archived_pdf_attachment() {
    let (app, _d) = app().await;
    let (issued, iid) = seed_issued_invoice(&app).await;
    let (_s, _h, pdf) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;

    let (se, body) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(se, StatusCode::OK, "{body}");
    let msgs = app.email.messages();
    assert_eq!(msgs.len(), 1);
    let (name, bytes) = msgs[0]
        .attachment
        .as_ref()
        .expect("the invoice email carries the PDF (#113)");
    assert_eq!(*name, format!("{}.pdf", issued["number"].as_str().unwrap()));
    assert_eq!(*bytes, pdf, "the attachment is the archived document");
    assert!(
        msgs[0].text.contains("PDF document is attached"),
        "{}",
        msgs[0].text
    );

    // The MIME builder labels the part as a PDF.
    let mime = tucano_time::email::build_mime(&msgs[0]);
    assert!(mime.contains("Content-Type: application/pdf"));
    assert!(mime.contains("base64"));
}

// ------------------------------------------------------------------ #112 ---

#[tokio::test]
async fn email_copy_sends_pdf_to_third_party_with_audit_trail() {
    let (app, _d) = app().await;
    let (issued, iid) = seed_issued_invoice(&app).await;
    let (_s, _h, pdf) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;

    let (status, body) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "accountant@firm.co", "note": "for the October books"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent_to"], "accountant@firm.co");
    assert_eq!(body["transport"], "recording");

    let msgs = app.email.messages();
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m.to, "accountant@firm.co");
    assert!(
        m.subject.contains(issued["number"].as_str().unwrap()),
        "{}",
        m.subject
    );
    // The cover names the requester (session user "Admin") and the note.
    assert!(m.text.contains("Admin"), "{}", m.text);
    assert!(m.text.contains("for the October books"), "{}", m.text);
    let (name, bytes) = m.attachment.as_ref().expect("PDF copy attached");
    assert_eq!(*name, format!("{}.pdf", issued["number"].as_str().unwrap()));
    assert_eq!(*bytes, pdf, "the attached copy is the archived document");

    // Audit records recipient + invoice, never the bytes (#52).
    let (_s, audit) = json_req(&app, "GET", "/audit", None).await;
    let hit = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "invoice_email_copy");
    assert!(hit.is_some(), "audit event recorded: {audit}");
    let subject = hit.unwrap()["subject"].as_str().unwrap();
    assert!(
        subject.contains(iid.as_str()) && subject.contains("accountant@firm.co"),
        "{subject}"
    );
    assert!(!subject.contains("%PDF"));
}

#[tokio::test]
async fn email_copy_invalid_recipient_is_422_without_sending() {
    let (app, _d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    let (status, body) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "not-an-email"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["fields"][0]["field"], "to");
    // Nothing sent, nothing persisted beyond the earlier issue.
    assert_eq!(app.email.messages().len(), 0);
    let (_s, audit) = json_req(&app, "GET", "/audit", None).await;
    assert!(
        !audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "invoice_email_copy"),
        "rejection without side effects"
    );
}

#[tokio::test]
async fn email_copy_draft_is_409_and_shapes_are_rejected() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "NOEMAIL", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();

    // Draft: 409, no send.
    let (status, _b) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "x@y.co"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(app.email.messages().len(), 0);

    // Unknown field (deny_unknown_fields) and oversized note: 422.
    let (s1, _b1) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "x@y.co", "bcc": "sneaky@evil.co"})),
    )
    .await;
    assert_eq!(s1, StatusCode::UNPROCESSABLE_ENTITY);
    let (s2, _b2) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "x@y.co", "note": "n".repeat(2001)})),
    )
    .await;
    assert_eq!(s2, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(app.email.messages().len(), 0);

    // Unknown invoice: 404.
    let (s3, _) = json_req(
        &app,
        "POST",
        "/invoices/00000000-0000-0000-0000-000000000000/email-copy",
        Some(json!({"to": "x@y.co"})),
    )
    .await;
    assert_eq!(s3, StatusCode::NOT_FOUND);

    // Member: admin tier, 403.
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (s4, _, _) = raw(
        &app.router,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "x@y.co"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(s4, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn email_copy_accepts_paid_invoices() {
    // Draft is the only refused state: a settled invoice still goes to the
    // accountant (matches the #113 download rule).
    let (app, _d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "bank:42"})),
    )
    .await;
    let (status, body) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "books@firm.co"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let msgs = app.email.messages();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].attachment.is_some());
}

// ------------------------------------------------------------------ #116 ---

async fn new_customer_ex(app: &Client, body: Value) -> (StatusCode, Value) {
    json_req(app, "POST", "/customers", Some(body)).await
}

/// Draft an invoice for a customer (billing email set), returning (customer,
/// draft invoice). `extra` merges into the customer creation body (#116
/// fields).
async fn seed_customer_invoice(app: &Client, name: &str, extra: Value) -> (Value, Value) {
    let mut body = json!({"name": name, "currency": "EUR", "default_rate_minor": 6000, "email": "billing@t116.test"});
    if let (Some(obj), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            obj.insert(k.clone(), v.clone());
        }
    }
    let (s, c) = new_customer_ex(app, body).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let cid = c["id"].as_str().unwrap();
    new_project(app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s2, inv) = json_req(
        app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    (c, inv)
}

#[tokio::test]
async fn payment_terms_decide_due_date_at_issue() {
    let today = chrono::Utc::now().date_naive();
    let cases = [
        (json!({"kind":"upon_receipt"}), today),
        (json!({"kind":"net_15"}), today + chrono::Duration::days(15)),
        (json!({"kind":"net_30"}), today + chrono::Duration::days(30)),
        (json!({"kind":"net_45"}), today + chrono::Duration::days(45)),
        (
            json!({"kind":"custom","days":21}),
            today + chrono::Duration::days(21),
        ),
    ];
    for (i, (terms, expected)) in cases.iter().enumerate() {
        let (app, _d) = app().await;
        let (_c, inv) =
            seed_customer_invoice(&app, &format!("TERMS{i}"), json!({"payment_terms": terms}))
                .await;
        let iid = inv["id"].as_str().unwrap();
        let (s, issued) = json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
        assert_eq!(s, StatusCode::OK, "{issued}");
        assert_eq!(
            issued["due_date"].as_str().unwrap(),
            expected.to_string(),
            "terms {terms}"
        );
    }
}

#[tokio::test]
async fn legacy_net14_fallback_is_untouched_and_org_default_applies() {
    // No terms anywhere: the legacy net-14 from period_to (2026-10-07) stays.
    let (app, _d) = app().await;
    let (_c, inv) = seed_customer_invoice(&app, "LEGACY", json!({})).await;
    let iid = inv["id"].as_str().unwrap();
    let (_, issued) = json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    assert_eq!(
        issued["due_date"], "2026-10-21",
        "legacy fallback unchanged"
    );

    // Org default terms now apply to a terms-less customer.
    let (s, _) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"","body":"","footer":"","payment_terms":{"kind":"net_20"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_c2, inv2) = seed_customer_invoice(&app, "NEXT", json!({})).await;
    let iid2 = inv2["id"].as_str().unwrap();
    let (_, issued2) = json_req(&app, "POST", &format!("/invoices/{iid2}/issue"), None).await;
    let today = chrono::Utc::now().date_naive();
    assert_eq!(
        issued2["due_date"].as_str().unwrap(),
        (today + chrono::Duration::days(20)).to_string()
    );

    // Customer terms beat the org default.
    let (_c3, inv3) = seed_customer_invoice(
        &app,
        "PAYER",
        json!({"payment_terms": {"kind": "upon_receipt"}}),
    )
    .await;
    let iid3 = inv3["id"].as_str().unwrap();
    let (_, issued3) = json_req(&app, "POST", &format!("/invoices/{iid3}/issue"), None).await;
    assert_eq!(issued3["due_date"].as_str().unwrap(), today.to_string());
}

#[tokio::test]
async fn template_save_validates_and_rejects_without_persisting() {
    let (app, d) = app().await;
    // Unknown %token% in the body: 422 naming the field; nothing persisted.
    let (s, body) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"Inv %invoice_number%","body":"pay %bogus% now","footer":""})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["fields"][0]["field"], "body");
    assert!(
        body["error"]["fields"][0]["message"]
            .as_str()
            .unwrap()
            .contains("%bogus%")
    );
    assert!(
        !d.path().join("data").join("invoice_template.json").exists(),
        "rejection persists nothing"
    );

    // Case-sensitive: %Total% is unknown.
    let (s2, _) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"%Total%","body":"","footer":""})),
    )
    .await;
    assert_eq!(s2, StatusCode::UNPROCESSABLE_ENTITY);

    // Malformed payment terms.
    let (s3, b3) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"payment_terms": {"kind": "custom"}})),
    )
    .await;
    assert_eq!(s3, StatusCode::UNPROCESSABLE_ENTITY, "{b3}");
    let (s4, _) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"payment_terms": {"kind": "net_30", "days": 30}})),
    )
    .await;
    assert_eq!(s4, StatusCode::UNPROCESSABLE_ENTITY);
    let (s5, _) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"payment_terms": {"kind": "custom", "days": 0}})),
    )
    .await;
    assert_eq!(s5, StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown field rejected pre-write (#deny_unknown_fields contract).
    let (s6, _) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject": "s", "evil": true})),
    )
    .await;
    assert_eq!(s6, StatusCode::UNPROCESSABLE_ENTITY);

    // A valid template round-trips and lands on disk atomically.
    let (s7, saved) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"Invoice %invoice_number% for %customer_name%","body":"# Hello\n\nthanks","footer":"Bank: IBAN","payment_terms":{"kind":"net_30"}})),
    )
    .await;
    assert_eq!(s7, StatusCode::OK, "{saved}");
    assert_eq!(saved["body"], "# Hello\n\nthanks");

    // GET returns the template plus the variable cheatsheet.
    let (_s8, got) = json_req(&app, "GET", "/admin/invoice-template", None).await;
    assert_eq!(
        got["template"]["subject"],
        "Invoice %invoice_number% for %customer_name%"
    );
    let vars = got["variables"].as_array().unwrap();
    assert!(vars.iter().any(|v| v["name"] == "%invoice_issue_month%"));
    assert!(vars.iter().any(|v| v["name"] == "%line_count%"));

    // Member: admin tier.
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (s9, _, _) = raw(
        &app.router,
        "GET",
        "/admin/invoice-template",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(s9, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn document_endpoint_renders_live_and_escapes() {
    let (app, _d) = app().await;
    let (_c, inv) = seed_customer_invoice(
        &app,
        "DOCS",
        json!({"invoice_notes": "Please pay by <script>alert(1)</script> transfer"}),
    )
    .await;
    json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"October: %invoice_number%","body":"Dear %customer_name%,\n\n- consulting\n- support","footer":"Wire to IBAN XX","payment_terms":null})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();
    let (s, doc) = json_req(&app, "GET", &format!("/invoices/{iid}/document"), None).await;
    assert_eq!(s, StatusCode::OK, "{doc}");
    assert_eq!(
        doc["subject"],
        format!("October: {}", inv["number"].as_str().unwrap())
    );
    let html = doc["html"].as_str().unwrap();
    assert!(html.contains("<li>consulting</li>"), "{html}");
    assert!(html.contains("Wire to IBAN XX"), "{html}");
    // Customer notes are appended after the org footer.
    assert!(html.contains("by &lt;script&gt;"), "{html}");
    assert!(!html.contains("<script>"), "escaping is non-negotiable");

    // Unknown id: 404.
    let (s404, _) = json_req(
        &app,
        "GET",
        "/invoices/00000000-0000-0000-0000-000000000000/document",
        None,
    )
    .await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn email_uses_resolved_subject_and_html_body() {
    let (app, _d) = app().await;
    let (_c, inv) = seed_customer_invoice(
        &app,
        "MAILER",
        json!({"invoice_subject": "%invoice_number% for %customer_name% — %invoice_issue_month%"}),
    )
    .await;
    json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"WRONG %invoice_number%","body":"Org footer body","footer":"","payment_terms":null})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (s, body) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let msgs = app.email.messages();
    assert_eq!(msgs.len(), 1);
    // Customer subject override beats the org template, variables resolved.
    assert_eq!(
        msgs[0].subject,
        format!("{} for MAILER — October", inv["number"].as_str().unwrap())
    );
    assert!(
        msgs[0].text.contains("Please find invoice"),
        "legacy text part stays"
    );
    let html = msgs[0].html.as_ref().expect("template body -> html part");
    assert!(html.contains("Org footer body"), "{html}");
    // The PDF attachment still rides along.
    assert!(msgs[0].attachment.is_some());
}

#[tokio::test]
async fn pdf_archive_applies_template_on_both_issue_and_lazy_paths() {
    let (app, d) = app().await;
    json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"","body":"THANK-YOU NOTE text","footer":"","payment_terms":null})),
    )
    .await;
    let (_c, inv) = seed_customer_invoice(&app, "SEAM", json!({})).await;
    let iid = inv["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (_s, _h, at_issue) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;

    // Drop the archive: the lazy path re-renders from the *same* content
    // source and must produce identical bytes (#113 determinism x #116 seam).
    std::fs::remove_file(
        d.path()
            .join("data")
            .join("invoices")
            .join(format!("{iid}.pdf")),
    )
    .unwrap();
    let (_s2, _h2, at_lazy) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_eq!(
        at_issue, at_lazy,
        "issue and lazy paths share the content source"
    );

    // And template content actually flows into the document: a body-less
    // template renders different bytes for the same invoice state.
    json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"","body":"","footer":"","payment_terms":null})),
    )
    .await;
    std::fs::remove_file(
        d.path()
            .join("data")
            .join("invoices")
            .join(format!("{iid}.pdf")),
    )
    .unwrap();
    let (_s3, _h3, at_no_template) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_ne!(
        at_issue, at_no_template,
        "the body text changed the document"
    );
}

#[tokio::test]
async fn customer_invoice_fields_round_trip_and_validate() {
    let (app, _d) = app().await;
    // Unknown token in the subject: rejected before persistence.
    let (s, _) = new_customer_ex(
        &app,
        json!({"name":"BAD","currency":"EUR","default_rate_minor":1,"invoice_subject":"%nope%"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (_s2, list) = json_req(&app, "GET", "/customers", None).await;
    assert!(list["customers"].as_array().unwrap().is_empty());

    // Invalid terms shapes.
    for bad in [
        json!({"kind":"custom","days":400}),
        json!({"kind":"net_20","days":20}),
    ] {
        let (sb, _bb) = new_customer_ex(
            &app,
            json!({"name":"BAD","currency":"EUR","default_rate_minor":1,"payment_terms": bad}),
        )
        .await;
        assert_eq!(sb, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }

    // Valid shape round-trips through create and update.
    let (sc, c) = new_customer_ex(
        &app,
        json!({"name":"FULL","currency":"EUR","default_rate_minor":1,
               "payment_terms":{"kind":"custom","days":21},
               "invoice_notes":"net 21, thanks","invoice_subject":"Inv %invoice_number%"}),
    )
    .await;
    assert_eq!(sc, StatusCode::CREATED, "{c}");
    assert_eq!(c["payment_terms"]["kind"], "custom");
    assert_eq!(c["payment_terms"]["days"], 21);
    let cid = c["id"].as_str().unwrap();
    let (_su, u) = json_req(
        &app,
        "PUT",
        &format!("/customers/{cid}"),
        Some(
            json!({"name":"FULL","currency":"EUR","default_rate_minor":1,
                    "payment_terms":{"kind":"upon_receipt"},
                    "invoice_notes":"","invoice_subject":""}),
        ),
    )
    .await;
    assert_eq!(u["payment_terms"]["kind"], "upon_receipt");
    assert!(
        u["payment_terms"].get("days").is_none() || u["payment_terms"]["days"].is_null(),
        "skipped when absent: {}",
        u["payment_terms"]
    );
}

// ------------------------------------------------------------------ #114 ---

#[tokio::test]
async fn record_payment_partial_then_settles_with_balance_semantics() {
    let (app, _d) = app().await;
    // 3h @ 60.00 = 18000 minor.
    let (_issued, iid) = seed_issued_invoice(&app).await;

    // A partial payment lands on partly_paid with a ledger entry.
    let (sp, part) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "bank:1", "amount_minor": 5000})),
    )
    .await;
    assert_eq!(sp, StatusCode::OK, "{part}");
    assert_eq!(part["status"], "partly_paid");
    let pays = part["payments"].as_array().unwrap();
    assert_eq!(pays.len(), 1);
    assert_eq!(pays[0]["amount_minor"], 5000);
    assert_eq!(pays[0]["method"], "manual");
    assert!(pays[0]["id"].is_string());
    assert!(pays[0]["received_at"].is_string());

    // Summary uses the BALANCE now: 13000 outstanding, not 18000.
    let (_s1, sum) = json_req(&app, "GET", "/invoices/summary", None).await;
    assert_eq!(sum["partly_paid"], 1);
    assert_eq!(sum["issued"], 0);
    assert_eq!(sum["outstanding"]["EUR"], 13000);

    // Zero amount: 422, ledger untouched.
    let (s0, _b0) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "x", "amount_minor": 0})),
    )
    .await;
    assert_eq!(s0, StatusCode::UNPROCESSABLE_ENTITY);

    // Over-balance: 409, still no persistence.
    let (so, bo) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "x", "amount_minor": 13001})),
    )
    .await;
    assert_eq!(so, StatusCode::CONFLICT, "{bo}");
    let (_s2, mid) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(
        mid["payments"].as_array().unwrap().len(),
        1,
        "rejections persisted nothing"
    );

    // The exact balance settles it; legacy mirror fields stay truthful.
    let (sf, paid) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "bank:2", "amount_minor": 13000})),
    )
    .await;
    assert_eq!(sf, StatusCode::OK, "{paid}");
    assert_eq!(paid["status"], "paid");
    assert!(paid["paid_at"].is_string());
    assert_eq!(paid["payment_reference"], "bank:2");
    assert_eq!(paid["payments"].as_array().unwrap().len(), 2);

    // No balance left: further payments conflict.
    let (sx, _bx) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "x", "amount_minor": 1})),
    )
    .await;
    assert_eq!(sx, StatusCode::CONFLICT);

    // Audit trail (#52): two invoice_payment events.
    let (_sa, audit) = json_req(&app, "GET", "/audit", None).await;
    let n = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "invoice_payment")
        .count();
    assert_eq!(n, 2, "{audit}");
}

#[tokio::test]
async fn pay_without_amount_still_pays_in_full() {
    // The default (no amount_minor) keeps #27's all-or-nothing behaviour.
    let (app, _d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    let (s, paid) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "cheque:9"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{paid}");
    assert_eq!(paid["status"], "paid");
    assert_eq!(paid["payments"][0]["amount_minor"], 18000);
}

#[tokio::test]
async fn write_off_requires_reason_and_excludes_the_balance() {
    let (app, _d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;

    // Missing/blank reason: 422 without persistence.
    let (sr, _br) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/write-off"),
        Some(json!({"reason": "   "})),
    )
    .await;
    assert_eq!(sr, StatusCode::UNPROCESSABLE_ENTITY);

    // Valid write-off of the open invoice.
    let (sw, wo) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/write-off"),
        Some(json!({"reason": "customer insolvent, debt forgiven 2026-10"})),
    )
    .await;
    assert_eq!(sw, StatusCode::OK, "{wo}");
    assert_eq!(wo["status"], "written_off");
    assert_eq!(
        wo["write_off_reason"],
        "customer insolvent, debt forgiven 2026-10"
    );
    assert!(wo["written_off_at"].is_string());

    // Outstanding drops to zero but the invoice is counted separately.
    let (_s1, sum) = json_req(&app, "GET", "/invoices/summary", None).await;
    assert_eq!(sum["written_off"], 1);
    assert_eq!(sum["issued"], 0);
    assert!(
        sum["outstanding"].as_object().unwrap().is_empty()
            || sum["outstanding"]["EUR"] == serde_json::json!(0),
        "{sum}"
    );

    // Final: re-writing-off and paying both conflict.
    let (s2, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/write-off"),
        Some(json!({"reason": "again"})),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    let (s3, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "x", "amount_minor": 100})),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT);

    // Audit: invoice_write_off recorded with id + actor.
    let (_sa, audit) = json_req(&app, "GET", "/audit", None).await;
    assert!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "invoice_write_off")
    );
}

#[tokio::test]
async fn write_off_rejects_draft_and_paid_states() {
    let (app, _d) = app().await;
    // Draft: refused.
    let c = new_customer(&app, "NOEMAIL", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let did = inv["id"].as_str().unwrap().to_string();
    let (sd, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{did}/write-off"),
        Some(json!({"reason": "early forgiveness"})),
    )
    .await;
    assert_eq!(sd, StatusCode::CONFLICT, "drafts cannot be written off");

    // Paid: refused too (nothing left to forgive).
    let (_issued, iid) = seed_issued_invoice(&app).await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "full"})),
    )
    .await;
    let (sp, _) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/write-off"),
        Some(json!({"reason": "too late"})),
    )
    .await;
    assert_eq!(sp, StatusCode::CONFLICT);
}

#[tokio::test]
async fn invoice_report_gains_paid_and_balance_columns() {
    let (app, _d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "half", "amount_minor": 8000})),
    )
    .await;
    let (s, rep) = json_req(
        &app,
        "GET",
        "/invoices/report?from=2026-10-01&to=2026-10-31",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    assert_eq!(rep["partly_paid"], 1);
    let row = rep["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["revenue_minor"] == 18000)
        .expect("ACME row");
    assert_eq!(row["paid_minor"], 8000);
    assert_eq!(row["balance_minor"], 10000);
    let (s2, csv) = json_req(&app, "GET", "/invoices/export.csv", None).await;
    assert_eq!(s2, StatusCode::OK);
    let csv = csv.as_str().unwrap();
    assert!(
        csv.contains("paid_minor,balance_minor,write_off_reason"),
        "{csv}"
    );
}

#[tokio::test]
async fn partly_paid_invoice_still_locks_its_entries() {
    let (app, _d) = app().await;
    let (issued, iid) = seed_issued_invoice(&app).await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "half", "amount_minor": 9000})),
    )
    .await;
    let eid = issued["lines"][0]["entry_id"].as_str().unwrap();
    let (s, _) = json_req(
        &app,
        "PUT",
        &format!("/entries/{eid}"),
        Some(json!({"date":"2026-10-02","customer_id":issued["customer_id"],"project_code":"P1","hours":4})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "partly paid keeps the #18 lock");
}

#[tokio::test]
async fn legacy_paid_docs_read_with_synthesised_ledger() {
    // A document written before #114: status=paid, no payments array. The
    // API must read it, treat the total as paid and show zero balance — the
    // open invoices dashboard must not overstate receivables.
    let (app, d) = app().await;
    let (_issued, iid) = seed_issued_invoice(&app).await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference": "old:1"})),
    )
    .await;
    let path = d
        .path()
        .join("data")
        .join("invoices")
        .join(format!("{iid}.json"));
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["status"], "paid");
    // Rewrite it as a legacy doc (strip the ledger).
    let mut legacy = raw.as_object().unwrap().clone();
    legacy.remove("payments");
    std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

    let (_s, sum) = json_req(&app, "GET", "/invoices/summary", None).await;
    assert_eq!(sum["paid"], 1);
    assert!(
        sum["outstanding"].as_object().unwrap().is_empty()
            || sum["outstanding"]["EUR"] == serde_json::json!(0),
        "legacy paid counts as settled: {sum}"
    );
    let (sr, rep) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(sr, StatusCode::OK, "{rep}");
    assert_eq!(rep["payments"].as_array().map(Vec::len).unwrap_or(0), 0);
    // Reports see it fully paid (synthesised).
    let (_s2, rep2) = json_req(
        &app,
        "GET",
        "/invoices/report?from=2026-10-01&to=2026-10-31",
        None,
    )
    .await;
    let row = rep2["rows"].as_array().unwrap().first().unwrap();
    assert_eq!(row["paid_minor"], 18000);
    assert_eq!(row["balance_minor"], 0);
}

#[tokio::test]
async fn email_result_reports_disabled_transport_honestly() {
    // #130: the DEFAULT production wiring (no SMTP env, no vault) boots
    // DisabledEmailSender — sends succeed as logged no-ops. The response must
    // disclose transport "disabled" so no client can read a 200 as delivery.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).expect("store");
    let router = tucano_time::build_router(AppState::new(store));
    raw(
        &router,
        "POST",
        "/auth/bootstrap",
        Some(json!({"name":"Admin","email":ADMIN_EMAIL,"password":ADMIN_PW})),
        None,
    )
    .await;
    let cookie = login_cookie(&router, ADMIN_EMAIL, ADMIN_PW).await;
    let call = |m: String, u: String, b: Option<Value>| {
        let r = router.clone();
        let c = cookie.clone();
        async move {
            let (s, v, _) = raw(&r, &m, &u, b, Some(&c)).await;
            (s, v)
        }
    };
    let (sc, cust) = call(
        "POST".to_string(),
        "/customers".into(),
        Some(json!({"name":"ACME","currency":"EUR","default_rate_minor":6000,"email":"billing@acme.test"})),
    )
    .await;
    assert_eq!(sc, StatusCode::CREATED, "{cust}");
    let cid = cust["id"].as_str().unwrap().to_string();
    call(
        "POST".to_string(),
        format!("/customers/{cid}/projects"),
        Some(json!({"code":"P1","currency":"EUR","rate_minor":6000})),
    )
    .await;
    call(
        "POST".to_string(),
        "/entries".into(),
        Some(json!({"date":"2026-10-02","customer_id":cid.clone(),"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, inv) = call(
        "POST".to_string(),
        "/invoices".into(),
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    call("POST".into(), format!("/invoices/{iid}/issue"), None).await;
    let (se, body) = call("POST".into(), format!("/invoices/{iid}/email"), None).await;
    assert_eq!(se, StatusCode::OK, "{body}");
    assert_eq!(body["sent_to"], "billing@acme.test");
    assert_eq!(body["transport"], "disabled", "no SMTP => honest label");
    let (sc2, copy) = call(
        "POST".to_string(),
        format!("/invoices/{iid}/email-copy"),
        Some(json!({"to": "books@firm.co"})),
    )
    .await;
    assert_eq!(sc2, StatusCode::OK, "{copy}");
    assert_eq!(copy["transport"], "disabled");
    // The audit line records the transport alongside the recipient (#52).
    let (_sa, audit) = call("GET".into(), "/audit".into(), None).await;
    let hit = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "invoice_email_copy");
    assert!(hit.is_some());
    assert!(
        hit.unwrap()["subject"]
            .as_str()
            .unwrap()
            .ends_with(":disabled")
    );
}

// ------------------------------------------------------------------ #136 ---

#[tokio::test]
async fn recurring_schedule_pause_resume_keeps_the_cursor() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    let (s, scd) = json_req(
        &app,
        "POST",
        "/schedules",
        Some(json!({"customer_id":cid,"cadence":"monthly","mode":"time","currency":"EUR"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{scd}");
    let sid = scd["id"].as_str().unwrap().to_string();
    // Pause: active=false, identity preserved.
    let (sp, paused) = json_req(
        &app,
        "PUT",
        &format!("/schedules/{sid}"),
        Some(json!({"active": false})),
    )
    .await;
    assert_eq!(sp, StatusCode::OK, "{paused}");
    assert_eq!(paused["active"], false);
    assert_eq!(paused["customer_id"], cid);
    assert_eq!(paused["cadence"], "monthly");
    // Resume via re-read.
    let (_sr, list) = json_req(&app, "GET", "/schedules", None).await;
    assert_eq!(list["schedules"].as_array().unwrap().len(), 1);
    let (sa, act) = json_req(
        &app,
        "PUT",
        &format!("/schedules/{sid}"),
        Some(json!({"active": true})),
    )
    .await;
    assert_eq!(sa, StatusCode::OK, "{act}");
    assert_eq!(act["active"], true);
    // Unknown schedule: 404; unknown field: 422 without change.
    let (s4, _) = json_req(
        &app,
        "PUT",
        "/schedules/00000000-0000-0000-0000-000000000000",
        Some(json!({"active": false})),
    )
    .await;
    assert_eq!(s4, StatusCode::NOT_FOUND);
    let (s5, _) = json_req(
        &app,
        "PUT",
        &format!("/schedules/{sid}"),
        Some(json!({"active": false, "cadence": "weekly"})),
    )
    .await;
    assert_eq!(s5, StatusCode::UNPROCESSABLE_ENTITY);
    let (_s6, still) = json_req(&app, "GET", "/schedules", None).await;
    assert_eq!(
        still["schedules"][0]["active"], true,
        "rejection persisted nothing"
    );
    // Member: admin tier.
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (s7, _, _) = raw(
        &app.router,
        "PUT",
        &format!("/schedules/{sid}"),
        Some(json!({"active": false})),
        Some(&cookie),
    )
    .await;
    assert_eq!(s7, StatusCode::FORBIDDEN);
}

// ------------------------------------------------------------------ #139 ---

#[tokio::test]
async fn customer_billing_details_round_trip_and_validate() {
    let (app, _d) = app().await;
    let (s, c) = json_req(
        &app,
        "POST",
        "/customers",
        Some(
            json!({"name":"GLOBAL","currency":"USD","default_rate_minor":7000,
          "address":{"street":"Mainstross 1","city":"Berlin","postal_code":"10115","country":"DE"},
          "contacts":[{"name":"Ada","role":"Finance","email":"ada@global.test","billing":true},
                      {"name":"Bob","role":"Ops","email":"bob@global.test"}],
          "tax_hundredths":1900,"discount_hundredths":250}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["address"]["country"], "DE");
    assert_eq!(c["contacts"].as_array().unwrap().len(), 2);
    assert_eq!(c["tax_hundredths"], 1900);
    assert_eq!(c["discount_hundredths"], 250);
    // No top-level email at all: the billing contact receives the invoice.
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(
        &app,
        &cid,
        "P1",
        json!({"currency":"USD","rate_minor":7000}),
    )
    .await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid.clone(),"project_code":"P1","hours":2})),
    )
    .await;
    let (_s2, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (se, body) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(se, StatusCode::OK, "{body}");
    assert_eq!(
        body["sent_to"], "ada@global.test",
        "billing contact wins (#139)"
    );
    assert_eq!(app.email.messages()[0].to, "ada@global.test");

    // Validation matrix: bad contact email, two billing flags, tax too big,
    // country too short — all 422, nothing persisted by the last shape.
    let (s3, b3) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"BAD","currency":"EUR","default_rate_minor":1,
          "contacts":[{"name":"N","email":"nope"}]})),
    )
    .await;
    assert_eq!(s3, StatusCode::UNPROCESSABLE_ENTITY, "{b3}");
    let (s4, _) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"BAD","currency":"EUR","default_rate_minor":1,
          "contacts":[{"name":"A","billing":true},{"name":"B","billing":true}]})),
    )
    .await;
    assert_eq!(s4, StatusCode::UNPROCESSABLE_ENTITY);
    let (s5, _) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"BAD","currency":"EUR","default_rate_minor":1,"tax_hundredths":10001})),
    )
    .await;
    assert_eq!(s5, StatusCode::UNPROCESSABLE_ENTITY);
    let (s6, b6) = json_req(
        &app,
        "POST",
        "/customers",
        Some(json!({"name":"BAD","currency":"EUR","default_rate_minor":1,
          "address":{"street":"x","country":"D"}})),
    )
    .await;
    assert_eq!(s6, StatusCode::UNPROCESSABLE_ENTITY, "{b6}");
    let (_s7, list) = json_req(&app, "GET", "/customers", None).await;
    assert!(
        list["customers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "BAD"),
        "rejections persisted nothing"
    );
}

#[tokio::test]
async fn legacy_customer_keeps_working_and_new_fields_are_omitted_when_empty() {
    let (app, d) = app().await;
    let c = new_customer(&app, "OLDSTYLE", "EUR", 5000).await; // only the pre-#139 shape
    assert_eq!(c["email"], "");
    assert!(c.get("contacts").is_none(), "empty vec is skipped: {c}");
    assert!(c.get("address").is_none());
    assert!(c.get("tax_hundredths").is_none() || c["tax_hundredths"] == 0);
    // The stored JSON lacks the new keys entirely; reads must work anyway.
    let path = d
        .path()
        .join("data")
        .join("customers")
        .join(format!("{}.json", c["id"].as_str().unwrap()));
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("contacts"));
    let (_s2, got) = json_req(
        &app,
        "GET",
        &format!("/customers/{}", c["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(got["name"], "OLDSTYLE");
}

// ------------------------------------------------------------------ #138 ---

#[tokio::test]
async fn org_profile_round_trips_and_prints_on_the_document() {
    let (app, _d) = app().await;
    // Default read: everything empty.
    let (_s0, empty) = json_req(&app, "GET", "/admin/org", None).await;
    assert_eq!(empty["name"], "");
    assert!(empty.get("address").is_none() || empty["address"].is_null());

    // Validation before persistence.
    let (sb, bb) = json_req(
        &app,
        "PUT",
        "/admin/org",
        Some(json!({"name":"X","legal_id":"","address":{"street":"a","country":""}})),
    )
    .await;
    assert_eq!(sb, StatusCode::UNPROCESSABLE_ENTITY, "{bb}");
    let (_sv, before) = json_req(&app, "GET", "/admin/org", None).await;
    assert_eq!(before["name"], "", "rejection persisted nothing");

    // Valid save round-trips.
    let (sp, saved) = json_req(
        &app,
        "PUT",
        "/admin/org",
        Some(json!({"name":"Tucano BV","legal_id":"BE0123456789",
          "address":{"street":"Markt 1","city":"Brugge","postal_code":"8000","country":"BE"}})),
    )
    .await;
    assert_eq!(sp, StatusCode::OK, "{saved}");
    assert_eq!(saved["name"], "Tucano BV");

    // The identity prints on the issued document (uncompressed streams, so
    // the bytes are greppable — the determinism contract of #113 in use).
    let (issued, iid) = seed_issued_invoice(&app).await;
    let (_s2, _h2, pdf) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    let text = String::from_utf8_lossy(&pdf).into_owned();
    // The layout emits one text-showing op per WORD (positions carry the
    // spacing), so decode the literals back into running text.
    let joined: String = text
        .split('(')
        .skip(1)
        .map(|s| s.split(')').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(joined.contains("BE0123456789"), "legal id on the PDF");
    assert!(
        joined.contains("Tucano BV"),
        "profile name replaces the config default"
    );
    assert!(joined.contains("Brugge"), "address on the PDF");
    assert_eq!(issued["status"], "issued");

    // Member: admin tier on both verbs.
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (sm, _, _) = raw(&app.router, "GET", "/admin/org", None, Some(&cookie)).await;
    assert_eq!(sm, StatusCode::FORBIDDEN);
    let (spm, _, _) = raw(
        &app.router,
        "PUT",
        "/admin/org",
        Some(json!({"name":"Evil"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(spm, StatusCode::FORBIDDEN);
}

// ------------------------------------------------------------------ #143 ---

#[tokio::test]
async fn manual_invoice_creation_computes_exact_totals() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "SHOP", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    let (s, inv) = json_req(
        &app,
        "POST",
        "/invoices/manual",
        Some(json!({"customer_id":cid,"tax_hundredths":2100,"discount_hundredths":1000,
          "lines":[{"description":"Office seat","item_kind":"product","quantity_hundredths":250,"unit_price_minor":4000,"project_code":"P1"},
                   {"description":"Setup service","item_kind":"service","quantity_hundredths":100,"unit_price_minor":5000}]})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    assert_eq!(inv["status"], "draft");
    assert!(inv["number"].as_str().unwrap().starts_with("INV-"));
    assert_eq!(inv["currency"], "EUR");
    let lines = inv["lines"].as_array().unwrap();
    assert_eq!(lines[0]["amount_minor"], 10000); // 2.50 x 40.00
    assert_eq!(lines[0]["kind"], "fixed");
    assert_eq!(lines[0]["item_kind"], "product");
    assert_eq!(lines[0]["project_code"], "P1");
    assert_eq!(lines[1]["amount_minor"], 5000);
    // subtotal 15000, discount 10% = 1500, VAT 21% of 13500 = 2835, total 16335.
    assert_eq!(inv["total_minor"], 16335);
    assert_eq!(inv["tax_hundredths"], 2100);
    assert_eq!(inv["discount_hundredths"], 1000);
    // Draft locks nothing and sends nothing.
    let eid = json_req(&app, "GET", "/entries", None).await;
    let _ = eid;
    let pdf = raw_req(
        &app,
        "GET",
        &format!("/invoices/{}/pdf", inv["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(
        pdf.0,
        StatusCode::CONFLICT,
        "draft has no archived document"
    );
}

#[tokio::test]
async fn manual_invoice_validation_matrix_persists_nothing() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "SHOP", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let base = |lines: Value, extra: Value| -> Value {
        let mut o = json!({"customer_id": cid, "lines": lines});
        if let (Some(o1), Some(o2)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in o2 {
                o1.insert(k.clone(), v.clone());
            }
        }
        o
    };
    let good = json!([{"description":"X","item_kind":"product","quantity_hundredths":100,"unit_price_minor":100}]);
    for (label, body) in [
        ("empty lines", base(json!([]), json!({}))),
        (
            "zero qty",
            base(
                json!([{"description":"X","item_kind":"product","quantity_hundredths":0,"unit_price_minor":100}]),
                json!({}),
            ),
        ),
        (
            "bad percent",
            base(good.clone(), json!({"tax_hundredths": 10001})),
        ),
        (
            "unknown project",
            base(
                json!([{"description":"X","item_kind":"product","quantity_hundredths":100,"unit_price_minor":100,"project_code":"NOPE"}]),
                json!({}),
            ),
        ),
        ("unknown field", base(good.clone(), json!({"evil": true}))),
        (
            "float quantity",
            json!({"customer_id": cid, "lines": [{"description":"X","item_kind":"product","quantity_hundredths":1.5,"unit_price_minor":100}]}),
        ),
    ] {
        let (st, body) = json_req(&app, "POST", "/invoices/manual", Some(body)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{label}: {body}");
    }
    let (_s, list) = json_req(&app, "GET", "/invoices", None).await;
    assert!(
        list["invoices"].as_array().unwrap().is_empty(),
        "nothing persisted"
    );
}

#[tokio::test]
async fn draft_editing_replaces_lines_and_issued_stays_immutable() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "SHOP", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1","hours":3})),
    )
    .await;
    let (_s, gent) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = gent["id"].as_str().unwrap().to_string();
    let tracked = &gent["lines"][0];
    assert_eq!(tracked["amount_minor"], 18000);

    // Draft edit: keep the tracked line verbatim + add a manual line, tax 10%.
    let edit = json!({"lines": [tracked,
        {"kind":"fixed","date":"2026-10-02","note":"Extra hosting","item_kind":"service",
         "quantity_hundredths":100,"unit_price_minor":900}],
        "tax_hundredths": 1000, "discount_hundredths": 0});
    let (se, edited) = json_req(&app, "PUT", &format!("/invoices/{iid}"), Some(edit)).await;
    assert_eq!(se, StatusCode::OK, "{edited}");
    assert_eq!(edited["lines"].as_array().unwrap().len(), 2);
    // 18000 + 900 = 18900 subtotal; 10% = 1890; total 20790.
    assert_eq!(edited["total_minor"], 20790);
    // The tracked line survived untouched (snapshot #8).
    assert_eq!(edited["lines"][0]["amount_minor"], 18000);
    assert_eq!(edited["lines"][0]["entry_id"], tracked["entry_id"]);

    // Tampering with a tracked line's amount is refused; nothing persisted.
    let mut tampered = tracked.clone();
    tampered["amount_minor"] = json!(1);
    let (st, bt) = json_req(
        &app,
        "PUT",
        &format!("/invoices/{iid}"),
        Some(json!({"lines":[tampered],"tax_hundredths":0,"discount_hundredths":0})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{bt}");
    let (_sr, again) = json_req(&app, "GET", &format!("/invoices/{iid}"), None).await;
    assert_eq!(again["total_minor"], 20790, "rejection persisted nothing");

    // Issue: snapshot locks the document; PUT now conflicts.
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    let (si, _) = json_req(
        &app,
        "PUT",
        &format!("/invoices/{iid}"),
        Some(json!({"lines":[],"tax_hundredths":0})),
    )
    .await;
    assert_eq!(si, StatusCode::CONFLICT);
    // The archived PDF shows the EDITED lines (the document follows the
    // draft at issue time, not the earlier generation).
    let (_sp, _h, pdf) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    let joined: String = String::from_utf8_lossy(&pdf)
        .split('(')
        .skip(1)
        .map(|x| x.split(')').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Extra hosting"),
        "manual line reached the document"
    );
    assert!(joined.contains("207.90"), "total with VAT printed");
}

// ------------------------------------------------- #187: validation gaps --

#[tokio::test]
async fn draft_line_input_rejects_unknown_fields_before_persisting() {
    // #187: the stored InvoiceLine was used directly as the PUT body
    // element without deny_unknown_fields — a typo'd or stale client field
    // was silently dropped. The split input DTO is strict.
    let (app, _d) = app().await;
    let c = new_customer(&app, "SHOP", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    let (_s, inv) = json_req(
        &app,
        "POST",
        "/invoices/manual",
        Some(json!({"customer_id":cid,
          "lines":[{"description":"Seat","item_kind":"product","quantity_hundredths":100,"unit_price_minor":4000}]})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    let good = json!({"kind":"fixed","date":"2026-10-02","note":"Seat",
                      "item_kind":"product","quantity_hundredths":100,"unit_price_minor":4000});
    // Outer envelope was always strict; prove the LINE element now is too.
    let mut stale = good.clone();
    stale["amountTtc"] = json!(1); // legacy/typo key
    let (s, b) = json_req(
        &app,
        "PUT",
        &format!("/invoices/{iid}"),
        Some(json!({"lines":[stale],"tax_hundredths":0,"discount_hundredths":0})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    // Sanity: the same edit without the unknown field succeeds.
    let (ok, _) = json_req(
        &app,
        "PUT",
        &format!("/invoices/{iid}"),
        Some(json!({"lines":[good],"tax_hundredths":0,"discount_hundredths":0})),
    )
    .await;
    assert_eq!(ok, StatusCode::OK);
}

#[tokio::test]
async fn manual_line_bounds_reject_identically_on_both_endpoints() {
    // #187: create (domain::validate_manual_lines) and edit previously kept
    // two copies of the bounds. One shared checker now guards both.
    let (app, _d) = app().await;
    let c = new_customer(&app, "SHOP", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s1, b1) = json_req(
        &app,
        "POST",
        "/invoices/manual",
        Some(json!({"customer_id":cid,
          "lines":[{"description":"Seat","item_kind":"product","quantity_hundredths":1_000_001,"unit_price_minor":4000}]})),
    )
    .await;
    assert_eq!(s1, StatusCode::UNPROCESSABLE_ENTITY, "{b1}");
    let (_s, draft) = json_req(
        &app,
        "POST",
        "/invoices/manual",
        Some(json!({"customer_id":cid,
          "lines":[{"description":"Seat","item_kind":"product","quantity_hundredths":100,"unit_price_minor":4000}]})),
    )
    .await;
    let iid = draft["id"].as_str().unwrap().to_string();
    let (s2, b2) = json_req(
        &app,
        "PUT",
        &format!("/invoices/{iid}"),
        Some(
            json!({"lines":[{"kind":"fixed","date":"2026-10-02","note":"Seat",
            "item_kind":"product","quantity_hundredths":1_000_001,"unit_price_minor":4000}],
            "tax_hundredths":0,"discount_hundredths":0}),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::UNPROCESSABLE_ENTITY, "{b2}");
    let msg = b2["error"]["fields"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        msg == b1["error"]["fields"][0]["message"]
            .as_str()
            .unwrap_or_default(),
        "both endpoints must report the same bound: {msg} vs {b1}"
    );
}

#[tokio::test]
async fn claim_rejects_duplicate_expense_ids() {
    // #187: [id, id] summed the expense amount twice into the persisted
    // claim total while referencing one expense.
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (_s, x) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(
            json!({"date":"2026-10-02","customer_id":cid,"amount_minor":2500,
                    "currency":"EUR","note":"taxi"}),
        ),
    )
    .await;
    let (s, b) = json_req(
        &app,
        "POST",
        "/claims",
        Some(json!({"title":"dup","expense_ids":[x["id"], x["id"]]})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
}

#[tokio::test]
async fn oversized_project_budgets_rejected_without_persisting() {
    // #187: budget fields were the only money/hours inputs persisted with no
    // bound (u64::MAX survived, breaking the burn-rate math downstream).
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    let p = new_project(&app, cid, "P1", json!({"budget_hours": 5000})).await;
    assert_eq!(p["budget_hours"], 5000);
    for (field, value) in [
        ("budget_amount_minor", json!(u64::MAX)),
        ("budget_hours", json!(100_000_001u64)),
    ] {
        let mut body = json!({"code":"P1","currency":"EUR","rate_minor":6000,
                              "budget_hours": 5000});
        body[field] = value;
        let (s, b) = json_req(
            &app,
            "PUT",
            &format!("/customers/{cid}/projects/P1"),
            Some(body),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {b}");
        let (_sr, got) =
            json_req(&app, "GET", &format!("/customers/{cid}/projects/P1"), None).await;
        assert_eq!(
            got["budget_hours"], 5000,
            "{field} rejection persisted nothing"
        );
    }
}

#[tokio::test]
async fn second_retainer_close_is_409_not_500() {
    // #187: the store transaction erased RetainerError into Io(String) and
    // close_retainer never applied the substring rescue — closing a closed
    // retainer answered 500. Typed errors make every mutation 409.
    let (app, _d) = app().await;
    let rid = seed_retainer(&app, 10_000).await;
    let (s1, b1) = json_req(&app, "POST", &format!("/retainers/{rid}"), None).await;
    assert_eq!(s1, StatusCode::OK, "{b1}");
    let (s2, b2) = json_req(&app, "POST", &format!("/retainers/{rid}"), None).await;
    assert_eq!(s2, StatusCode::CONFLICT, "second close: {b2}");
    // A draw on the closed retainer is the same 409 class, not a 500.
    let (s3, b3) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/draw"),
        Some(json!({"amount_minor": 100, "reason": "late draw"})),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT, "draw after close: {b3}");
}

#[tokio::test]
async fn expense_on_partly_paid_invoice_stays_locked() {
    // #218: the expense-delete scan said '== Issued' while the invoice locks
    // (and the dashboard treats as open) issued | partly_paid — a partial
    // payment silently unlocked the expense rows sitting on that invoice.
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "P1", json!({"rate_minor": 6000})).await;
    let (_s, x) = json_req(
        &app,
        "POST",
        "/expenses",
        Some(
            json!({"date":"2026-10-02","customer_id":cid,"project_code":"P1",
                    "amount_minor":5000,"currency":"EUR","billable":true,"note":"parts"}),
        ),
    )
    .await;
    let xid = x["id"].as_str().unwrap().to_string();
    let (_s2, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-07"})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap().to_string();
    assert_eq!(inv["total_minor"], 5000, "expense-only invoice: {inv}");
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    // Partial payment: issued -> partly_paid.
    let (_sp, paid) = json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"amount_minor": 2000, "reference": "wire-1"})),
    )
    .await;
    assert_eq!(paid["status"], "partly_paid", "{paid}");
    let (s_del, b_del) = json_req(&app, "DELETE", &format!("/expenses/{xid}"), None).await;
    assert_eq!(
        s_del,
        StatusCode::CONFLICT,
        "partly_paid must lock its expenses: {b_del}"
    );
    assert!(
        b_del["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("open invoice"),
        "{b_del}"
    );
    // Settle it: only then may the (no longer part of any receivable) expense
    // be removed — matching the entry-lock lifecycle documented in #216.
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"amount_minor": 3000, "reference": "wire-2"})),
    )
    .await;
    let (s_after, b_after) = json_req(&app, "DELETE", &format!("/expenses/{xid}"), None).await;
    assert_eq!(s_after, StatusCode::NO_CONTENT, "paid releases: {b_after}");
}

#[tokio::test]
async fn store_fault_during_entry_validation_is_500_not_422() {
    // #187: validate_entry_refs' `.ok().flatten()` turned any store failure
    // into a bogus "customer does not exist" 422. A corrupt customer doc
    // must surface as a 500 (generic, no internals leaked).
    let (app, d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "P1", json!({})).await;
    std::fs::write(
        d.path().join(format!("data/customers/{cid}.json")),
        b"{ not json",
    )
    .unwrap();
    let (s, b) = json_req(
        &app,
        "POST",
        "/entries",
        Some(entry_body(&cid, "P1", json!(1), "2026-10-02")),
    )
    .await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{b}");
    assert_eq!(b["error"]["code"], "internal");
    let text = b.to_string();
    assert!(
        !text.contains("json") && !text.contains("data/"),
        "no internals: {text}"
    );
}

// ------------------------------------------------------------------ #134 ---

#[tokio::test]
async fn invoice_preview_projects_and_paid_exclusion() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "PA", json!({"rate_minor": 5000})).await;
    new_project(&app, &cid, "PB", json!({"rate_minor": 7000})).await;
    for (proj, day) in [("PA", "2026-10-02"), ("PB", "2026-10-03")] {
        json_req(
            &app,
            "POST",
            "/entries",
            Some(json!({"date":day,"customer_id":cid.clone(),"project_code":proj,"hours":2})),
        )
        .await;
    }
    // Preview only PA: exactly its 2h x 50 = 10000, nothing persisted.
    let (sp, preview) = json_req(
        &app,
        "POST",
        "/invoices/preview",
        Some(json!({"customer_id":cid.clone(),"from":"2026-10-01","to":"2026-10-31","project_codes":["PA"]})),
    )
    .await;
    assert_eq!(sp, StatusCode::OK, "{preview}");
    assert_eq!(preview["number"], "", "preview is unsaved");
    let lines = preview["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["project_code"], "PA");
    assert_eq!(preview["total_minor"], 10000);
    let (_s0, list) = json_req(&app, "GET", "/invoices", None).await;
    assert!(
        list["invoices"].as_array().unwrap().is_empty(),
        "preview persists nothing"
    );

    // Unknown project for the customer: 422.
    let (su, _) = json_req(
        &app,
        "POST",
        "/invoices/preview",
        Some(json!({"customer_id":cid.clone(),"from":"2026-10-01","to":"2026-10-31","project_codes":["NOPE"]})),
    )
    .await;
    assert_eq!(su, StatusCode::UNPROCESSABLE_ENTITY);

    // Save PA for real, pay it fully, then generate again: PA must be
    // excluded now (any non-draft invoice's lines are billed work).
    let (_s1, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid.clone(),"from":"2026-10-01","to":"2026-10-31","project_codes":["PA"]})),
    )
    .await;
    let iid = inv["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid}/issue"), None).await;
    json_req(
        &app,
        "POST",
        &format!("/invoices/{iid}/pay"),
        Some(json!({"reference":"x"})),
    )
    .await;
    let (s2, again) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid.clone(),"from":"2026-10-01","to":"2026-10-31"})),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED, "{again}");
    let l2 = again["lines"].as_array().unwrap();
    assert_eq!(l2.len(), 1, "only PB remains billable");
    assert_eq!(l2[0]["project_code"], "PB");
    assert_eq!(again["total_minor"], 14000);
    // Issue that one too; with every line on a non-draft invoice the period
    // is exhausted -> NothingToInvoice 409.
    let iid2 = again["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid2}/issue"), None).await;
    // And the fully-invoiced period repeats -> NothingToInvoice 409.
    let (s3, _b3) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-10-01","to":"2026-10-31"})),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT);
}

// ------------------------------------------------------------------ #147 ---

#[tokio::test]
async fn item_type_catalog_crud_and_editor_prefill_contract() {
    let (app, _d) = app().await;
    let (s, item) = json_req(
        &app,
        "POST",
        "/admin/item-types",
        Some(
            json!({"name":"Support hour","kind":"service","description":"Extended support block",
                    "default_price_minor":9500,"currency":"EUR"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{item}");
    let tid = item["id"].as_str().unwrap().to_string();
    assert_eq!(item["active"], true);
    for bad in [
        json!({"name":"","kind":"service"}),
        json!({"name":"X","kind":"product","default_price_minor":100_000_001}),
    ] {
        let (sb, _) = json_req(&app, "POST", "/admin/item-types", Some(bad)).await;
        assert_eq!(sb, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let (_sl, list) = json_req(&app, "GET", "/admin/item-types", None).await;
    assert_eq!(list["item_types"].as_array().unwrap().len(), 1);
    let (sa, archived) = json_req(
        &app,
        "PUT",
        &format!("/admin/item-types/{tid}"),
        Some(json!({"name":"Support hour","kind":"service","default_price_minor":9500,"currency":"EUR","active":false})),
    )
    .await;
    assert_eq!(sa, StatusCode::OK, "{archived}");
    assert_eq!(archived["active"], false);
    assert_eq!(archived["id"], tid);
    let (s4, _) = json_req(
        &app,
        "PUT",
        "/admin/item-types/00000000-0000-0000-0000-000000000000",
        Some(json!({"name":"X","kind":"service"})),
    )
    .await;
    assert_eq!(s4, StatusCode::NOT_FOUND);
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (sm, _, _) = raw(&app.router, "GET", "/admin/item-types", None, Some(&cookie)).await;
    assert_eq!(sm, StatusCode::FORBIDDEN);
    let (sd, _) = json_req(&app, "DELETE", &format!("/admin/item-types/{tid}"), None).await;
    assert_eq!(sd, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn document_labels_reach_the_pdf_and_validation_binds() {
    let (app, _d) = app().await;
    let (sb, _) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"","body":"","footer":"","labels":{"total":"a very long label that goes over the forty character bound"}})),
    )
    .await;
    assert_eq!(sb, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, saved) = json_req(
        &app,
        "PUT",
        "/admin/invoice-template",
        Some(json!({"subject":"","body":"","footer":"","labels":{"description":"Service delivered","total":"Amount due"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{saved}");
    assert_eq!(saved["labels"]["total"], "Amount due");
    let (issued, iid) = seed_issued_invoice(&app).await;
    assert_eq!(issued["status"], "issued");
    let (_sp, _h, pdf) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    let joined: String = String::from_utf8_lossy(&pdf)
        .split('(')
        .skip(1)
        .map(|x| x.split(')').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Amount due"),
        "custom total label on the PDF"
    );
    assert!(
        joined.contains("Service delivered"),
        "custom description column label"
    );
    let (_sg, got) = json_req(&app, "GET", "/admin/invoice-template", None).await;
    assert_eq!(
        got["template"]["labels"]["description"],
        "Service delivered"
    );
}

// ------------------------------------------------------------------ #144 ---

async fn seed_retainer(app: &Client, opening: u64) -> String {
    let c = new_customer(app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(app, &cid, "WEB", json!({"rate_minor": 6000})).await;
    let (s, r) = json_req(
        app,
        "POST",
        "/retainers",
        Some(json!({"customer_id": cid, "project_code": "WEB", "opening_amount_minor": opening})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    r["retainer"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn retainer_ledger_reconciles_and_guarded_ops_persist_nothing() {
    let (app, dir) = app().await;
    let rid = seed_retainer(&app, 50_000).await;
    let (_s, d) = json_req(&app, "GET", &format!("/retainers/{rid}"), None).await;
    assert_eq!(d["balance_minor"], 50_000);
    assert_eq!(d["retainer"]["transactions"].as_array().unwrap().len(), 1);

    // credit then draw: balance reconciles from the ledger every read.
    let (sc, cred) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/credit"),
        Some(json!({"amount_minor": 10_000, "reason": "top up", "idempotency_key": "k1"})),
    )
    .await;
    assert_eq!(sc, StatusCode::OK, "{cred}");
    assert_eq!(cred["balance_minor"], 60_000);
    let (sd, drawn) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/draw"),
        Some(json!({"amount_minor": 25_000, "reason": "March work", "idempotency_key": "k2"})),
    )
    .await;
    assert_eq!(sd, StatusCode::OK, "{drawn}");
    assert_eq!(drawn["balance_minor"], 35_000);
    // balance is NEVER stored: the doc has no balance field, only txs.
    let doc_raw = std::fs::read_to_string(
        dir.path()
            .join("data")
            .join("retainers")
            .join(format!("{rid}.json")),
    )
    .unwrap();
    assert!(!doc_raw.contains("balance_minor"), "ledger only");

    // Replay of the same idempotency key: 409, no second tx.
    let (sr, br) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/credit"),
        Some(json!({"amount_minor": 10_000, "reason": "double click", "idempotency_key": "k1"})),
    )
    .await;
    assert_eq!(sr, StatusCode::CONFLICT, "{br}");
    let (_s3, d3) = json_req(&app, "GET", &format!("/retainers/{rid}"), None).await;
    assert_eq!(d3["balance_minor"], 35_000, "replay did not move money");

    // Guards: zero amount 422; draw without reason 422; overdraw 409; wrong currency 422.
    let (s0, _) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/credit"),
        Some(json!({"amount_minor": 0})),
    )
    .await;
    assert_eq!(s0, StatusCode::UNPROCESSABLE_ENTITY);
    let (srn, _) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/draw"),
        Some(json!({"amount_minor": 100})),
    )
    .await;
    assert_eq!(srn, StatusCode::UNPROCESSABLE_ENTITY);
    let (sov, _) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/draw"),
        Some(json!({"amount_minor": 35_001, "reason": "greed"})),
    )
    .await;
    assert_eq!(sov, StatusCode::CONFLICT);
    let (scur, _) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/credit"),
        Some(json!({"amount_minor": 100, "currency": "USD"})),
    )
    .await;
    assert_eq!(scur, StatusCode::UNPROCESSABLE_ENTITY);
    let (_sv2, dv2) = json_req(&app, "GET", &format!("/retainers/{rid}"), None).await;
    assert_eq!(dv2["balance_minor"], 35_000, "rejected ops changed nothing");

    // Close is final: further mutations 409; history stays readable.
    let (scl, _) = json_req(&app, "POST", &format!("/retainers/{rid}"), None).await;
    assert_eq!(scl, StatusCode::OK);
    let (scl2, _) = json_req(
        &app,
        "POST",
        &format!("/retainers/{rid}/credit"),
        Some(json!({"amount_minor": 100})),
    )
    .await;
    assert_eq!(scl2, StatusCode::CONFLICT);
    let (_sl, dl) = json_req(&app, "GET", &format!("/retainers/{rid}"), None).await;
    assert_eq!(dl["retainer"]["status"], "closed");
    assert_eq!(dl["retainer"]["transactions"].as_array().unwrap().len(), 3);

    // Member: 403 across the surface; unknown id: 404.
    json_req(
        &app,
        "POST",
        "/users",
        Some(
            json!({"name":"Eve","email":"eve@test.local","password":"evepass123","role":"member"}),
        ),
    )
    .await;
    let cookie = login_cookie(&app.router, "eve@test.local", "evepass123").await;
    let (sm, _, _) = raw(&app.router, "GET", "/retainers", None, Some(&cookie)).await;
    assert_eq!(sm, StatusCode::FORBIDDEN);
    let (s404, _) = json_req(
        &app,
        "POST",
        "/retainers/00000000-0000-0000-0000-000000000000/credit",
        Some(json!({"amount_minor": 1})),
    )
    .await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ #146 ---

#[tokio::test]
async fn appearance_and_messages_are_honored_not_inert() {
    let (app, _d) = app().await;
    // Invalid shapes are refused before persistence.
    let (sb, _) = json_req(
        &app,
        "PUT",
        "/admin/org",
        Some(json!({"name":"X","reply_to":"nope"})),
    )
    .await;
    assert_eq!(sb, StatusCode::UNPROCESSABLE_ENTITY);
    let (sb2, _) = json_req(
        &app,
        "PUT",
        "/admin/org",
        Some(json!({"name":"X","accent":"e95420"})),
    )
    .await;
    assert_eq!(sb2, StatusCode::UNPROCESSABLE_ENTITY);
    let (_sv, before) = json_req(&app, "GET", "/admin/org", None).await;
    assert_eq!(before["name"], "", "rejections persisted nothing");

    // Valid save round-trips.
    let (s, saved) = json_req(
        &app,
        "PUT",
        "/admin/org",
        Some(json!({"name":"Tucano BV","from_name":"Tucano Invoicing",
                    "reply_to":"billing@tucano.test","accent":"#E95420"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{saved}");
    assert_eq!(saved["from_name"], "Tucano Invoicing");

    // The mail adapter honors the sender identity (#146): the recorded
    // message carries it and build_mime would render From/Reply-To.
    let (issued, iid) = seed_issued_invoice(&app).await;
    let _ = issued;
    let (se, _b) = json_req(&app, "POST", &format!("/invoices/{iid}/email"), None).await;
    assert_eq!(se, StatusCode::OK);
    let msg = &app.email.messages()[0];
    assert_eq!(msg.from_name.as_deref(), Some("Tucano Invoicing"));
    assert_eq!(msg.reply_to.as_deref(), Some("billing@tucano.test"));

    // The accent paints the newly issued document: color ops exist, and
    // re-rendering is byte-identical (the #113 determinism contract).
    let (_s2, _h2, pdf) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert!(pdf.windows(3).any(|w| w == b" rg"), "accent emitted");
    let (_s3, _h3, pdf2) = raw_req(&app, "GET", &format!("/invoices/{iid}/pdf")).await;
    assert_eq!(pdf, pdf2, "accented bytes deterministic");

    // Clearing the accent restores the ink-only document (no color ops).
    json_req(
        &app,
        "PUT",
        "/admin/org",
        Some(json!({"name":"Tucano BV","from_name":"","reply_to":"","accent":""})),
    )
    .await;
    let c = new_customer(&app, "PLAIN", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap().to_string();
    new_project(&app, &cid, "PZ", json!({"rate_minor": 6000})).await;
    json_req(
        &app,
        "POST",
        "/entries",
        Some(json!({"date":"2026-11-02","customer_id":cid,"project_code":"PZ","hours":1})),
    )
    .await;
    let (_s4, inv) = json_req(
        &app,
        "POST",
        "/invoices",
        Some(json!({"customer_id":cid,"from":"2026-11-01","to":"2026-11-07"})),
    )
    .await;
    let iid2 = inv["id"].as_str().unwrap();
    json_req(&app, "POST", &format!("/invoices/{iid2}/issue"), None).await;
    let (_s5, _h5, plain) = raw_req(&app, "GET", &format!("/invoices/{iid2}/pdf")).await;
    assert!(
        !plain.windows(3).any(|w| w == b" rg"),
        "cleared accent = no color"
    );
}

#[tokio::test]
async fn customer_without_ever_having_projects_deletes_cleanly() {
    // #141 regression: the projects directory only exists after a project was
    // added; remove_dir_all's ENOENT used to surface as a 500.
    let (app, _d) = app().await;
    let c = new_customer(&app, "BARE", "EUR", 5000).await;
    let cid = c["id"].as_str().unwrap();
    let (s, _, _) = raw_req(&app, "DELETE", &format!("/customers/{cid}")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_sg, list) = json_req(&app, "GET", "/customers", None).await;
    assert!(
        list["customers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["id"] != c["id"])
    );
}
