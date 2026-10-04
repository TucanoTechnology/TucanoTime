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
        let session = Arc::new(Session::new(
            b"test-session-secret-0000000000000032".to_vec(),
            3600,
            false,
        ));
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
                }
            }
            None => AppState::with_session(store, session),
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
        Self { router, cookie }
    }
}

/// A test lock provider that freezes a single entry id.
struct LockOne(Option<uuid::Uuid>);
impl EntryLock for LockOne {
    fn entry_lock(&self, entry_id: uuid::Uuid) -> Option<LockReason> {
        if self.0 == Some(entry_id) {
            Some(LockReason::Invoiced { id: "INV-1".into() })
        } else {
            None
        }
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
async fn report_uses_task_rate_override() {
    let (app, _d) = app().await;
    let c = new_customer(&app, "ACME", "EUR", 6000).await; // 60/h default
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({"rate_minor": 6000})).await;
    new_task(&app, cid, "P1", "PREM", json!({"rate_minor": 9500})).await; // 95/h

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
    // 2h * 95 + 1h * 60 = 190 + 60 = 250.00 = 25000 minor.
    assert_eq!(summary["rows"][0]["amount_minor"], 25000);
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
    let c = new_customer(&app, "ACME", "EUR", 6000).await;
    let cid = c["id"].as_str().unwrap();
    new_project(&app, cid, "P1", json!({})).await;

    let (sc, cat) = json_req(&app, "POST", "/categories", Some(json!({"name":"Travel"}))).await;
    assert_eq!(sc, StatusCode::CREATED);
    let cat_id = cat["id"].as_str().unwrap();

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
    assert_eq!(rep["total_revenue_minor"], 18000); // 3h * 60
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
    assert!(text.contains("1234"), "expected masked hint: {text}");
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
