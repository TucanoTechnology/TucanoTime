//! OAuth calendar providers (#36) — an upgrade path for the ICS feed (#15).
//!
//! A `CalendarSource` port sits behind the seam from #18/#15: the ICS source
//! remains the default; Google Calendar and Microsoft Graph adapters map their
//! event JSON onto the same `CalendarEvent` shape, so `/calendar/events` and
//! the GUI are provider-agnostic. Recurring events are skipped exactly like
//! the #15 rule (single events only).
//!
//! Tokens live in the secret vault (#77) — `calendar.provider` selects the
//! adapter, `calendar.<provider>.access_token` (+ refresh credentials) drive
//! it. Live HTTP goes through a small `HttpFetch` transport trait so all
//! parsing/mapping logic is unit-tested against provider-shaped fixtures with
//! no network calls, matching the payments/accounting pattern.

use base64::Engine as _;
use serde_json::Value;

use crate::calendar::{CalendarError, CalendarEvent};

/// Minimal fetch abstraction over the provider HTTP APIs.
pub trait HttpFetch: Send + Sync {
    fn get_json(&self, url: &str) -> Result<Value, CalendarError>;
}

/// A source of calendar events for a date range (inclusive `from`..=`to`).
pub trait CalendarSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn fetch(
        &self,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<CalendarEvent>, CalendarError>;
}

// ------------------------------------------------------------------ google --

pub struct GoogleCalendar {
    http: Arc<dyn HttpFetch>,
    calendar_id: String,
}

/// Microsoft Graph (work/school personal calendar).
pub struct GraphCalendar {
    http: Arc<dyn HttpFetch>,
}

use std::sync::Arc;

impl GoogleCalendar {
    #[must_use]
    pub fn new(http: Arc<dyn HttpFetch>, calendar_id: impl Into<String>) -> Self {
        Self {
            http,
            calendar_id: calendar_id.into(),
        }
    }
}

impl GraphCalendar {
    #[must_use]
    pub fn new(http: Arc<dyn HttpFetch>) -> Self {
        Self { http }
    }
}

/// Google timestamps: `{"dateTime": "RFC3339", "timeZone": ...}` or
/// `{"date": "YYYY-MM-DD"}` for all-day events.
fn google_dt(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(s) = v.get("dateTime").and_then(|d| d.as_str()) {
        return chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&chrono::Utc))
            .ok();
    }
    v.get("date").and_then(|d| d.as_str()).and_then(|d| {
        chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .ok()
            .map(|n| n.and_time(chrono::NaiveTime::MIN).and_utc())
    })
}

fn iso(s: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    s.and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// All-day Google events report `end.date` as EXCLUSIVE; extend to end-of-day
/// so a same-day range filter keeps them.
fn all_day_end(d: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    d.get("date")
        .and_then(|x| x.as_str())
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .map(|n| {
            n.pred_opt()
                .unwrap_or(n)
                .and_time(chrono::NaiveTime::from_hms_opt(23, 59, 59).unwrap())
                .and_utc()
        })
}

impl CalendarSource for GoogleCalendar {
    fn name(&self) -> &'static str {
        "google"
    }
    fn fetch(
        &self,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<CalendarEvent>, CalendarError> {
        let base = "https://www.googleapis.com/calendar/v3/calendars/";
        let url = format!(
            "{base}{}?timeMin={}T00:00:00Z&timeMax={}T23:59:59Z&singleEvents=false",
            urlencoded(&self.calendar_id),
            from,
            to
        );
        let body = self.http.get_json(&url)?;
        let mut out = Vec::new();
        for ev in body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            // Skip recurring instances the same way the ICS path skips RRULE.
            if ev.get("recurringEventId").is_some() || ev.get("recurrence").is_some() {
                continue;
            }
            let (Some(start), Some(end)) = (
                google_dt(ev.get("start").unwrap_or(&Value::Null)),
                ev.get("end")
                    .and_then(all_day_end)
                    .or_else(|| google_dt(ev.get("end").unwrap_or(&Value::Null))),
            ) else {
                continue;
            };
            out.push(CalendarEvent {
                uid: ev
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                start,
                end,
                title: ev
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("(untitled)")
                    .to_string(),
                location: ev
                    .get("location")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(out)
    }
}

impl CalendarSource for GraphCalendar {
    fn name(&self) -> &'static str {
        "microsoft"
    }
    fn fetch(
        &self,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<CalendarEvent>, CalendarError> {
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/calendarView?startDateTime={from}T00:00:00Z&endDateTime={to}T23:59:59Z"
        );
        let body = self.http.get_json(&url)?;
        let mut out = Vec::new();
        for ev in body
            .get("value")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            // Any occurrence of a series has a seriesMasterId — skip (single
            // events only, the #15 rule).
            if ev.get("seriesMasterId").is_some_and(|v| !v.is_null()) {
                continue;
            }
            let tz = |k: &str| {
                ev.get(k)
                    .and_then(|d| iso(d.get("dateTime").and_then(Value::as_str)))
            };
            let (Some(start), Some(end)) = (tz("start"), tz("end")) else {
                continue;
            };
            out.push(CalendarEvent {
                uid: ev
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                start,
                end,
                title: ev
                    .get("subject")
                    .and_then(Value::as_str)
                    .unwrap_or("(untitled)")
                    .to_string(),
                location: ev
                    .get("location")
                    .and_then(|l| l.get("displayName"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(out)
    }
}

fn urlencoded(s: &str) -> String {
    // Percent-encode everything outside RFC 3986 unreserved chars.
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------- oauth dance --

/// Pending consent flows keyed by CSRF `state` (provider + expiry). A restart
/// just drops them — the user re-initiates.
#[derive(Default)]
pub struct OAuthFlows {
    pending: std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>,
}

impl OAuthFlows {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a new state (random, single-use, 10-minute lifetime).
    pub fn start(&self, provider: &str, now: i64) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, (_, exp)| *exp > now); // opportunistic GC
        pending.insert(state.clone(), (provider.to_string(), now + 600));
        state
    }

    /// Consumes and validates a state; returns the provider it belongs to.
    pub fn take(&self, state: &str, now: i64) -> Option<String> {
        let mut pending = self.pending.lock().unwrap();
        pending
            .remove(state)
            .filter(|(_, exp)| *exp > now)
            .map(|(provider, _)| provider)
    }
}

/// Builds the consent redirect URL for the OAuth code flow (#36). Pure and
/// table-tested; production values come from the vault so nothing secret is
/// logged.
#[must_use]
pub fn authorize_url(provider: &str, client_id: &str, redirect_uri: &str, state: &str) -> String {
    match provider {
        "google" => format!(
            "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.readonly&access_type=offline&prompt=consent&state={state}",
            urlencoded(client_id),
            urlencoded(redirect_uri),
        ),
        "microsoft" => format!(
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id={}&redirect_uri={}&response_type=code&scope=Calendars.Read%20offline_access&state={state}",
            urlencoded(client_id),
            urlencoded(redirect_uri),
        ),
        _ => String::new(),
    }
}

/// Exchanges an authorization code for tokens (real provider call; never
/// exercised in CI — the URL/state plumbing above is what's tested).
pub fn exchange_code(
    provider: &str,
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> Result<(String, String), CalendarError> {
    let endpoint = match provider {
        "google" => "https://oauth2.googleapis.com/token",
        "microsoft" => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        _ => return Err(CalendarError::NotConfigured),
    };
    let form = format!(
        "grant_type=authorization_code&code={}&client_id={}&client_secret={}&redirect_uri={}",
        urlencoded(code),
        urlencoded(client_id),
        urlencoded(client_secret),
        urlencoded(redirect_uri),
    );
    let mut res = ureq::post(endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .send(form.as_bytes())
        .map_err(|_| CalendarError::Fetch)?;
    let text = res
        .body_mut()
        .read_to_string()
        .map_err(|_| CalendarError::Fetch)?;
    let v: Value = serde_json::from_str(&text).map_err(|_| CalendarError::Fetch)?;
    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(CalendarError::Fetch)?
        .to_string();
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((access, refresh))
}

/// Picks the configured provider from vault/env; `None` = keep the ICS path
/// (#15 stays the default, per the locked decision).
#[must_use]
pub fn configured_provider(vault: Option<&crate::vault::SecretVault>) -> Option<String> {
    let get = |key: &str, env: &str| -> Option<String> {
        vault
            .and_then(|v| v.get(key))
            .or_else(|| std::env::var(env).ok())
    };
    get("calendar.provider", "TUCANO_CALENDAR_PROVIDER")
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| p == "google" || p == "microsoft")
}

/// Builds the OAuth source for the configured provider. The bearer token is
/// read at call time through the transport, which also transparently refreshes
/// it (a 401 triggers one refresh + retry) — tokens are never returned.
pub fn source_for(
    provider: &str,
    vault: Option<&crate::vault::SecretVault>,
) -> Option<Arc<dyn CalendarSource>> {
    let get = |key: &str, env: &str| -> Option<String> {
        vault
            .and_then(|v| v.get(key))
            .or_else(|| std::env::var(env).ok())
    };
    match provider {
        "google" => {
            let token = get("calendar.google.access_token", "TUCANO_GOOGLE_TOKEN")?;
            let cal = get("calendar.google.id", "TUCANO_GOOGLE_CALENDAR")
                .unwrap_or_else(|| "primary".into());
            let refresh = get(
                "calendar.google.refresh_token",
                "TUCANO_GOOGLE_REFRESH_TOKEN",
            );
            let client_id = get("calendar.google.client_id", "TUCANO_GOOGLE_CLIENT_ID");
            let client_secret = get(
                "calendar.google.client_secret",
                "TUCANO_GOOGLE_CLIENT_SECRET",
            );
            Some(Arc::new(GoogleCalendar::new(
                Arc::new(BearerFetch::new(
                    "google",
                    token,
                    refresh,
                    client_id,
                    client_secret,
                )),
                cal,
            )))
        }
        "microsoft" => {
            let token = get("calendar.microsoft.access_token", "TUCANO_MS_TOKEN")?;
            let refresh = get(
                "calendar.microsoft.refresh_token",
                "TUCANO_MS_REFRESH_TOKEN",
            );
            let client_id = get("calendar.microsoft.client_id", "TUCANO_MS_CLIENT_ID");
            let client_secret = get(
                "calendar.microsoft.client_secret",
                "TUCANO_MS_CLIENT_SECRET",
            );
            Some(Arc::new(GraphCalendar::new(Arc::new(BearerFetch::new(
                "microsoft",
                token,
                refresh,
                client_id,
                client_secret,
            )))))
        }
        _ => None,
    }
}

/// Bearer-token HTTP transport with transparent OAuth refresh on 401:
/// POSTs the refresh grant to the provider token endpoint and retries once.
/// Kept tiny and behind `HttpFetch` so the parsers never need the network.
pub struct BearerFetch {
    provider: String,
    access_token: std::sync::Mutex<String>,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

impl BearerFetch {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        access_token: String,
        refresh_token: Option<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            access_token: std::sync::Mutex::new(access_token),
            refresh_token,
            client_id,
            client_secret,
        }
    }

    fn token_endpoint(&self) -> &'static str {
        match self.provider.as_str() {
            "google" => "https://oauth2.googleapis.com/token",
            _ => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }
    }

    fn current_token(&self) -> String {
        self.access_token.lock().unwrap().clone()
    }

    fn do_get(&self, url: &str) -> Result<(u16, String), CalendarError> {
        let token = self.current_token();
        let res = ureq::get(url)
            .header("authorization", &format!("Bearer {token}"))
            .call();
        match res {
            Ok(mut r) => {
                let status: u16 = r.status().as_u16();
                let body = r
                    .body_mut()
                    .read_to_string()
                    .map_err(|_| CalendarError::Fetch)?;
                Ok((status, body))
            }
            Err(ureq::Error::StatusCode(code)) => Ok((code, String::new())),
            Err(_) => Err(CalendarError::Fetch),
        }
    }

    /// Attempts one OAuth refresh-token grant. Returns false when refresh
    /// credentials are absent or the grant fails.
    fn refresh(&self) -> bool {
        let (Some(rt), Some(cid), Some(cs)) =
            (&self.refresh_token, &self.client_id, &self.client_secret)
        else {
            return false;
        };
        let form = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
            rt, cid, cs
        );
        let Ok(mut res) = ureq::post(self.token_endpoint())
            .header("content-type", "application/x-www-form-urlencoded")
            .send(form.as_bytes())
        else {
            return false;
        };
        let Ok(text) = res.body_mut().read_to_string() else {
            return false;
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => {
                if let Some(t) = v.get("access_token").and_then(Value::as_str) {
                    *self.access_token.lock().unwrap() = t.to_string();
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        }
    }
}

impl HttpFetch for BearerFetch {
    fn get_json(&self, url: &str) -> Result<Value, CalendarError> {
        let (status, body) = self.do_get(url)?;
        if status == 401 && self.refresh() {
            // One transparent retry with the fresh token.
            let (status2, body2) = self.do_get(url)?;
            if !(200..300).contains(&status2) {
                return Err(CalendarError::Fetch);
            }
            return serde_json::from_str(&body2).map_err(|_| CalendarError::Fetch);
        }
        if !(200..300).contains(&status) {
            return Err(CalendarError::Fetch);
        }
        serde_json::from_str(&body).map_err(|_| CalendarError::Fetch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, Timelike};

    /// Fake transport returning a fixture by URL pattern; counts calls and
    /// can emulate a 401-then-refresh flow.
    struct FixtureFetch {
        body: Value,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl HttpFetch for FixtureFetch {
        fn get_json(&self, _url: &str) -> Result<Value, CalendarError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.body.clone())
        }
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn google_events_map_and_recurring_skipped() {
        let fixture = serde_json::json!({
            "items": [
                {"id":"g1","summary":"Stand-up","location":"Room",
                 "start":{"dateTime":"2026-10-05T09:00:00Z"},"end":{"dateTime":"2026-10-05T09:30:00Z"}},
                {"id":"g2","summary":"Weekly","recurrence":["RRULE:FREQ=WEEKLY"],
                 "start":{"dateTime":"2026-10-06T10:00:00Z"},"end":{"dateTime":"2026-10-06T11:00:00Z"}},
                {"id":"g3","summary":"All day","start":{"date":"2026-10-07"},"end":{"date":"2026-10-08"}}
            ]
        });
        let g = GoogleCalendar::new(
            Arc::new(FixtureFetch {
                body: fixture,
                calls: 0.into(),
            }),
            "primary",
        );
        let evs = g.fetch(d("2026-10-01"), d("2026-10-31")).unwrap();
        assert_eq!(evs.len(), 2, "recurring event skipped");
        assert_eq!(evs[0].title, "Stand-up");
        assert_eq!(evs[1].uid, "g3");
        assert!(evs[1].start.hour() == 0 && evs[1].end.hour() == 23);
    }

    #[test]
    fn graph_events_map_and_series_skipped() {
        let fixture = serde_json::json!({
            "value": [
                {"id":"m1","subject":"Sync","start":{"dateTime":"2026-10-05T14:00:00.000Z"},
                 "end":{"dateTime":"2026-10-05T15:00:00.000Z"},"location":{"displayName":"Teams"}},
                {"id":"m2","subject":"Occurrence","seriesMasterId":"s1",
                 "start":{"dateTime":"2026-10-06T09:00:00.000Z"},
                 "end":{"dateTime":"2026-10-06T09:30:00.000Z"}}
            ]
        });
        let g = GraphCalendar::new(Arc::new(FixtureFetch {
            body: fixture,
            calls: 0.into(),
        }));
        let evs = g.fetch(d("2026-10-01"), d("2026-10-31")).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].title, "Sync");
        assert_eq!(evs[0].location, "Teams");
    }

    #[test]
    fn provider_selection_defaults_to_none() {
        // No vault, no env -> ICS default stays in charge.
        assert!(configured_provider(None).is_none());
    }

    #[test]
    fn urlencoding_percent_encodes_reserved() {
        assert_eq!(urlencoded("a@b.co"), "a%40b.co");
        assert_eq!(
            urlencoded("https://x.test/cb?a=1"),
            "https%3A%2F%2Fx.test%2Fcb%3Fa%3D1"
        );
    }

    #[test]
    fn authorize_urls_carry_client_state_and_redirect() {
        let g = authorize_url("google", "cid123", "https://app.test/cb", "st-42");
        assert!(g.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(g.contains("client_id=cid123"));
        assert!(g.contains("redirect_uri=https%3A%2F%2Fapp.test%2Fcb"));
        assert!(g.contains("access_type=offline"));
        assert!(g.ends_with("&state=st-42"));
        let m = authorize_url("microsoft", "cid123", "https://app.test/cb", "st-42");
        assert!(m.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(m.contains("Calendars.Read%20offline_access"));
        assert_eq!(authorize_url("yahoo", "c", "r", "s"), "");
    }

    #[test]
    fn oauth_state_is_single_use_and_expires() {
        let flows = OAuthFlows::new();
        let state = flows.start("google", 1_000);
        assert_ne!(state, flows.start("google", 1_000)); // random per flow
        let fresh = flows.start("google", 1_000);
        assert_eq!(flows.take(&fresh, 1_010).as_deref(), Some("google"));
        // Consumed — replays are rejected.
        assert!(flows.take(&fresh, 1_020).is_none());
        // Expired states drop.
        let late = flows.start("microsoft", 1_000);
        assert!(flows.take(&late, 1_000 + 601).is_none());
    }
}
