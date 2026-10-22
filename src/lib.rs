//! Library surface: everything `main.rs` wires up, exposed so integration
//! tests can build the router against a temp data dir without spawning a
//! process.

use rust_embed::RustEmbed;

pub mod accounting;
pub mod api;
pub mod appconfig;
mod assets;
pub mod audit;
pub mod auth;
pub mod backup;
pub mod budgets;
pub mod buildinfo;
pub mod calendar;
pub mod calendar_oauth;
pub mod clock;
pub mod domain;
pub mod email;
pub mod email_reminders;
pub mod error;
pub mod fx;
pub mod harvest;
pub mod lock;
pub mod pdf;
pub mod providers;
pub mod ratelimit;
pub mod recurring;
pub mod reminders;
pub mod report;
pub mod retainer;
pub mod revoke;
mod router;
pub mod scheduler;
pub mod sso;
pub mod store;
pub mod template;
pub mod vault;

pub use assets::spawn_web_watcher;
pub(crate) use assets::{dev_reload, dev_reload_script, docs, openapi, static_assets};
pub use router::build_router;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

const OPENAPI: &str = include_str!("../openapi.json");

/// Lowercase hex encoding of `bytes` in one pre-sized allocation, instead of a
/// `format!("{b:02x}")` + `String` per byte (#256) — the ETag/hash path can
/// touch multi-MB PDFs.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The mounting tier a route lives under, mirroring the `meta` / `public` /
/// `protected` / `admin` sub-routers assembled in [`build_router`].
///
/// `Open` covers the root meta routes (`/healthz`, `/openapi.json`, `/docs`) and
/// the `public` sub-router (no auth middleware); a request without a session is
/// answered by the method router (405 on a method it does not serve).
/// `Member` and `Admin` are the `protected`/`admin` sub-routers, whose
/// `route_layer` guard answers 401 before method routing when no session cookie
/// is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Open,
    Member,
    Admin,
}

/// Canonical `(method, path, tier)` registry of every route the mounted router
/// serves — the machine-readable shadow of [`build_router`], which is written by
/// hand and is not introspectable (axum exposes no route listing). It is the
/// single source of truth for the `(method, path)` surface: the
/// `router_routes_match_openapi_bidirectionally`/`contract_declares_producible_status_and_security`
/// contract tests diff it against `openapi.json` in both directions and, by
/// dispatching a `TRACE` probe per entry against the live router, assert it
/// still equals `build_router` (a removed/renamed/re-tiered route makes the
/// probe fail). Keep it in lockstep with [`build_router`]: update it whenever a
/// route is added, removed, renamed or moved between tiers (#253).
pub const API_ROUTES: &[(&str, &str, Tier)] = &[
    // ---- root meta + the `public` sub-router (Tier::Open) ----
    ("GET", "/healthz", Tier::Open),
    ("GET", "/openapi.json", Tier::Open),
    ("GET", "/docs", Tier::Open),
    ("POST", "/api/auth/bootstrap", Tier::Open),
    ("POST", "/api/auth/login", Tier::Open),
    ("POST", "/api/auth/logout", Tier::Open),
    ("GET", "/api/auth/status", Tier::Open),
    ("GET", "/api/auth/me", Tier::Open),
    ("GET", "/api/auth/sso/providers", Tier::Open),
    ("POST", "/api/auth/sso/assertion", Tier::Open),
    // ---- the `protected` sub-router (Tier::Member) ----
    ("GET", "/api/customers", Tier::Member),
    ("POST", "/api/customers", Tier::Member),
    ("GET", "/api/customers/{id}", Tier::Member),
    ("PUT", "/api/customers/{id}", Tier::Member),
    ("DELETE", "/api/customers/{id}", Tier::Member),
    ("GET", "/api/customers/{cid}/projects", Tier::Member),
    ("POST", "/api/customers/{cid}/projects", Tier::Member),
    ("GET", "/api/customers/{cid}/projects/{code}", Tier::Member),
    ("PUT", "/api/customers/{cid}/projects/{code}", Tier::Member),
    (
        "DELETE",
        "/api/customers/{cid}/projects/{code}",
        Tier::Member,
    ),
    (
        "GET",
        "/api/customers/{cid}/projects/{pcode}/tasks",
        Tier::Member,
    ),
    (
        "POST",
        "/api/customers/{cid}/projects/{pcode}/tasks",
        Tier::Member,
    ),
    (
        "GET",
        "/api/customers/{cid}/projects/{pcode}/tasks/{code}",
        Tier::Member,
    ),
    (
        "PUT",
        "/api/customers/{cid}/projects/{pcode}/tasks/{code}",
        Tier::Member,
    ),
    (
        "DELETE",
        "/api/customers/{cid}/projects/{pcode}/tasks/{code}",
        Tier::Member,
    ),
    ("GET", "/api/entries", Tier::Member),
    ("POST", "/api/entries", Tier::Member),
    ("GET", "/api/entries/{id}", Tier::Member),
    ("PUT", "/api/entries/{id}", Tier::Member),
    ("DELETE", "/api/entries/{id}", Tier::Member),
    ("GET", "/api/reports/summary", Tier::Member),
    ("GET", "/api/reports/export.csv", Tier::Member),
    ("GET", "/api/categories", Tier::Member),
    ("POST", "/api/categories", Tier::Member),
    ("PUT", "/api/categories/{id}", Tier::Member),
    ("DELETE", "/api/categories/{id}", Tier::Member),
    ("GET", "/api/expenses", Tier::Member),
    ("POST", "/api/expenses", Tier::Member),
    ("GET", "/api/expenses/{id}", Tier::Member),
    ("PUT", "/api/expenses/{id}", Tier::Member),
    ("DELETE", "/api/expenses/{id}", Tier::Member),
    ("GET", "/api/fx", Tier::Member),
    ("GET", "/api/submissions", Tier::Member),
    ("POST", "/api/submissions", Tier::Member),
    ("POST", "/api/submissions/{id}/decision", Tier::Member),
    ("GET", "/api/claims", Tier::Member),
    ("POST", "/api/claims", Tier::Member),
    ("POST", "/api/claims/{id}/submit", Tier::Member),
    ("GET", "/api/timer", Tier::Member),
    ("POST", "/api/timer", Tier::Member),
    ("DELETE", "/api/timer", Tier::Member),
    ("POST", "/api/timer/stop", Tier::Member),
    ("GET", "/api/calendar/events", Tier::Member),
    ("GET", "/api/notifications", Tier::Member),
    ("POST", "/api/notifications/read", Tier::Member),
    // ---- the `admin` sub-router (Tier::Admin) ----
    ("GET", "/api/users", Tier::Admin),
    ("POST", "/api/users", Tier::Admin),
    ("PUT", "/api/users/{id}", Tier::Admin),
    ("DELETE", "/api/users/{id}", Tier::Admin),
    ("PUT", "/api/users/{id}/password", Tier::Admin),
    ("GET", "/api/audit", Tier::Admin),
    ("GET", "/api/retainers", Tier::Admin),
    ("POST", "/api/retainers", Tier::Admin),
    ("GET", "/api/retainers/{id}", Tier::Admin),
    ("POST", "/api/retainers/{id}", Tier::Admin),
    ("POST", "/api/retainers/{id}/credit", Tier::Admin),
    ("POST", "/api/retainers/{id}/draw", Tier::Admin),
    ("GET", "/api/reports/profitability", Tier::Admin),
    ("GET", "/api/reports/budgets", Tier::Admin),
    ("GET", "/api/invoices", Tier::Admin),
    ("POST", "/api/invoices", Tier::Admin),
    ("POST", "/api/invoices/manual", Tier::Admin),
    ("POST", "/api/invoices/preview", Tier::Admin),
    ("GET", "/api/invoices/{id}", Tier::Admin),
    ("PUT", "/api/invoices/{id}", Tier::Admin),
    ("DELETE", "/api/invoices/{id}", Tier::Admin),
    ("POST", "/api/invoices/{id}/issue", Tier::Admin),
    ("GET", "/api/invoices/{id}/pdf", Tier::Admin),
    ("GET", "/api/invoices/{id}/pdf/preview", Tier::Admin),
    ("POST", "/api/invoices/{id}/pay", Tier::Admin),
    ("POST", "/api/invoices/{id}/write-off", Tier::Admin),
    ("POST", "/api/invoices/{id}/email", Tier::Admin),
    ("POST", "/api/invoices/{id}/email-copy", Tier::Admin),
    ("POST", "/api/invoices/{id}/sync", Tier::Admin),
    ("GET", "/api/sync/accounting", Tier::Admin),
    ("GET", "/api/admin/invoice-template", Tier::Admin),
    ("PUT", "/api/admin/invoice-template", Tier::Admin),
    ("POST", "/api/admin/invoice-template/preview", Tier::Admin),
    ("GET", "/api/admin/org", Tier::Admin),
    ("PUT", "/api/admin/org", Tier::Admin),
    ("GET", "/api/admin/item-types", Tier::Admin),
    ("POST", "/api/admin/item-types", Tier::Admin),
    ("PUT", "/api/admin/item-types/{id}", Tier::Admin),
    ("DELETE", "/api/admin/item-types/{id}", Tier::Admin),
    ("GET", "/api/invoices/{id}/document", Tier::Admin),
    ("GET", "/api/invoices/{id}/timeline", Tier::Admin),
    ("POST", "/api/invoices/{id}/sent", Tier::Admin),
    ("GET", "/api/invoices/summary", Tier::Admin),
    ("GET", "/api/invoices/report", Tier::Admin),
    ("GET", "/api/invoices/export.csv", Tier::Admin),
    ("POST", "/api/claims/{id}/decision", Tier::Admin),
    ("GET", "/api/admin/config", Tier::Admin),
    ("PUT", "/api/admin/config", Tier::Admin),
    ("GET", "/api/admin/email", Tier::Admin),
    ("PUT", "/api/admin/email", Tier::Admin),
    ("POST", "/api/admin/email/test", Tier::Admin),
    ("GET", "/api/admin/vault", Tier::Admin),
    ("POST", "/api/admin/vault/enable", Tier::Admin),
    ("POST", "/api/admin/vault/disable", Tier::Admin),
    ("POST", "/api/admin/vault/regenerate", Tier::Admin),
    ("GET", "/api/admin/secrets", Tier::Admin),
    ("PUT", "/api/admin/secrets/{key}", Tier::Admin),
    ("DELETE", "/api/admin/secrets/{key}", Tier::Admin),
    ("GET", "/api/schedules", Tier::Admin),
    ("POST", "/api/schedules", Tier::Admin),
    ("DELETE", "/api/schedules/{id}", Tier::Admin),
    ("PUT", "/api/schedules/{id}", Tier::Admin),
    ("GET", "/api/calendar/oauth/start", Tier::Admin),
    ("GET", "/api/calendar/oauth/callback", Tier::Admin),
];

/// Single-instance guard (review B6): take an exclusive advisory lock on
/// `<root>/.server.lock`. A second server against the same data dir would
/// diverge on cached vault/revocation state and fire every scheduler job
/// twice, so the caller holds the returned handle for the process lifetime —
/// dropping it releases the lock. Extracted verbatim from `main` so the guard
/// is unit-testable (#262; no behaviour change).
pub fn acquire_server_lock(root: &std::path::Path) -> Result<std::fs::File, String> {
    use fs2::FileExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join(".server.lock"))
        .map_err(|e| e.to_string())?;
    match f.try_lock_exclusive() {
        Ok(()) => Ok(f),
        Err(_) => Err(
            "another tucano-time instance is running against this data dir \
             (stale .server.lock?) — refusing to start a second scheduler"
                .to_owned(),
        ),
    }
}

/// The GUI's bookmarkable page routes. Each maps to the same single-page
/// shell; the client router reads `location.pathname` and selects the matching
/// tab/segment/panel. Matched case-insensitively with any trailing slash
/// ignored.
///
/// This is the server half of a pair: the client mirror is `TAB_ROUTES` /
/// `KNOWN_ROUTES` in `web/js/route.js`, and
/// `tests::route_parity::gui_routes_match_spa_allowlist` fails the build if the
/// two lists drift (#264). `""`, `/timesheets` and `/settings` are server-only
/// entry points the client canonicalises to a concrete view.
const SPA_ROUTES: &[&str] = &[
    "",
    "/timesheets",
    "/timesheets/day",
    "/timesheets/week",
    "/timesheets/calendar",
    "/customers",
    "/projects",
    "/tasks",
    "/invoices",
    "/expenses",
    "/approvals",
    "/reports",
    "/settings",
    "/settings/users",
    "/settings/company",
    "/settings/invoice-template",
    "/settings/catalog",
    "/settings/expenses",
    "/settings/email",
    "/settings/security",
];

fn spa_page(path: &str) -> bool {
    let p = path.trim_end_matches('/').to_ascii_lowercase();
    SPA_ROUTES.contains(&p.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #264: pin the GUI route table to the server allowlist so the two can
    /// never drift apart silently. `web/js/route.js`'s `TAB_ROUTES` values name
    /// every view; `""`, `/timesheets` and `/settings` are the server-only
    /// aliases the client router canonicalises (`normaliseRoute`).
    mod route_parity {
        use super::SPA_ROUTES;

        #[test]
        fn gui_routes_match_spa_allowlist() {
            const GUI: &str = include_str!("../web/js/route.js");
            let start = GUI
                .find("const TAB_ROUTES")
                .expect("TAB_ROUTES missing from web/js/route.js");
            let end = GUI[start..]
                .find("const KNOWN_ROUTES")
                .expect("KNOWN_ROUTES missing from web/js/route.js")
                + start;
            // Pull every '/'-prefixed single-quoted literal out of the
            // TAB_ROUTES object (the keys are bare tab ids, so only the route
            // values match). This is deliberately dumb text scraping: it has no
            // JS parser to skew and fails loudly on any edit.
            let block = &GUI[start..end];
            let mut from_js: Vec<String> = Vec::new();
            let bytes = block.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'\'' => {
                        let Some(rel) = block[i + 1..].find('\'') else {
                            i += 1;
                            continue;
                        };
                        let lit = &block[i + 1..i + 1 + rel];
                        if lit.starts_with('/') {
                            from_js.push(lit.to_string());
                        }
                        i += 1 + rel + 1;
                    }
                    _ => i += 1,
                }
            }
            for alias in ["", "/timesheets", "/settings"] {
                from_js.push(alias.to_string());
            }
            from_js.sort();
            from_js.dedup();

            let mut server: Vec<String> = SPA_ROUTES.iter().map(|s| s.to_string()).collect();
            server.sort();
            server.dedup();

            assert_eq!(
                from_js, server,
                "TAB_ROUTES (web/js/route.js) drifted from spa_page (src/lib.rs)"
            );
        }
    }
}
