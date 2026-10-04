//! Calendar integration (#15): read an ICS feed and expose events so time can
//! be logged from them. The feed source is configured (never user-supplied per
//! request, avoiding SSRF) via the credential vault key `calendar.ics_url` or
//! the `TUCANO_CALENDAR_ICS` env var — either an http(s) URL or a local file
//! path (handy for tests / a synced file).
//!
//! MVP scope (per the locked decision): single (non-recurring) events only;
//! events with an `RRULE` are skipped. Parsing is a pure function so it is
//! deterministic under test with a fixture.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalendarEvent {
    pub uid: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub title: String,
    pub location: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CalendarError {
    #[error("no calendar feed is configured")]
    NotConfigured,
    #[error("could not fetch the calendar feed")]
    Fetch,
}

/// Unfold RFC 5545 continuation lines (a line starting with space/tab continues
/// the previous logical line).
fn unfold(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            let cont = &line[1..];
            if let Some(last) = out.last_mut() {
                last.push_str(cont);
            }
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// Parse an ICS value (RFC3339, basic date-time `YYYYMMDDTHHMMSS[Z]`, or
/// all-day date `YYYYMMDD`) into a UTC datetime.
fn parse_ics_dt(value: &str) -> Option<DateTime<Utc>> {
    let v = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    if v.len() >= 15 && v.as_bytes()[8] == b'T' {
        // YYYYMMDDTHHMMSS[Z]
        let (date, time) = (&v[..8], &v[9..15]);
        let n = chrono::NaiveDateTime::parse_from_str(&format!("{date}T{time}"), "%Y%m%dT%H%M%S")
            .ok()?;
        return Some(if v.ends_with('Z') {
            Utc.from_utc_datetime(&n)
        } else {
            // Treat naive local as UTC for MVP.
            Utc.from_utc_datetime(&n)
        });
    }
    if v.len() == 8 {
        // All-day date -> midnight UTC.
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    None
}

fn value_of(line: &str, prop: &str) -> Option<String> {
    // Lines look like `DTSTART;VALUE=DATE:20261002` or `SUMMARY:Title`.
    let (head, val) = line.split_once(':')?;
    let name = head.split(';').next()?.trim_end();
    (name.eq_ignore_ascii_case(prop)).then(|| val.trim().to_string())
}

/// Parse ICS text into events overlapping `[from, to]` (inclusive by day).
/// Recurring events (with RRULE) are skipped.
pub fn parse_ics(text: &str, from: NaiveDate, to: NaiveDate) -> Vec<CalendarEvent> {
    let lines = unfold(text);
    let mut events = Vec::new();
    let mut in_event = false;
    let mut uid = String::new();
    let mut start: Option<DateTime<Utc>> = None;
    let mut end: Option<DateTime<Utc>> = None;
    let mut title = String::new();
    let mut location = String::new();
    let mut recurring = false;
    for line in &lines {
        let upper = line.to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            in_event = true;
            uid.clear();
            start = None;
            end = None;
            title.clear();
            location.clear();
            recurring = false;
        } else if upper == "END:VEVENT" {
            in_event = false;
            if let (Some(s), Some(e)) = (start, end)
                && !recurring
                && e.date_naive() >= from
                && s.date_naive() <= to
            {
                events.push(CalendarEvent {
                    uid: uid.clone(),
                    start: s,
                    end: e,
                    title: title.clone(),
                    location: location.clone(),
                });
            }
        } else if in_event {
            if let Some(v) = value_of(line, "UID") {
                uid = v;
            } else if let Some(v) = value_of(line, "DTSTART") {
                start = parse_ics_dt(&v);
            } else if let Some(v) = value_of(line, "DTEND") {
                end = parse_ics_dt(&v);
            } else if let Some(v) = value_of(line, "SUMMARY") {
                title = unescape_ics(&v);
            } else if let Some(v) = value_of(line, "LOCATION") {
                location = unescape_ics(&v);
            } else if line.to_ascii_uppercase().starts_with("RRULE") {
                recurring = true;
            }
        }
    }
    events.sort_by_key(|e| e.start);
    events
}

fn unescape_ics(v: &str) -> String {
    v.replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\n", "\n")
        .replace("\\\\", "\\")
}

/// Fetch the feed text from a URL or local path.
pub fn fetch_ics(source: &str) -> Result<String, CalendarError> {
    if source.starts_with("http://") || source.starts_with("https://") {
        ureq::get(source)
            .call()
            .map_err(|_| CalendarError::Fetch)?
            .body_mut()
            .read_to_string()
            .map_err(|_| CalendarError::Fetch)
    } else {
        std::fs::read_to_string(source).map_err(|_| CalendarError::Fetch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:1@x\r\nDTSTART:20261002T090000Z\r\nDTEND:20261002T100000Z\r\nSUMMARY:Stand-up\r\nLOCATION:Room\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:2@x\r\nDTSTART:20261005T140000Z\r\nDTEND:20261005T150000Z\r\nSUMMARY:Client, sync\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:3@x\r\nDTSTART:20261003T090000Z\r\nDTEND:20261003T100000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly (skipped)\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn parses_and_filters_and_skips_recurring() {
        let all = parse_ics(ICS, d("2026-10-01"), d("2026-10-31"));
        assert_eq!(all.len(), 2, "recurring event skipped");
        assert_eq!(all[0].title, "Stand-up");
        assert_eq!(all[1].title, "Client, sync", "escaped comma unescaped");
        // Only the first event is in the narrow range.
        let narrow = parse_ics(ICS, d("2026-10-02"), d("2026-10-02"));
        assert_eq!(narrow.len(), 1);
        assert_eq!(narrow[0].uid, "1@x");
    }

    #[test]
    fn fetch_reads_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cal.ics");
        std::fs::write(&path, ICS).unwrap();
        let text = fetch_ics(path.to_str().unwrap()).unwrap();
        assert!(text.contains("VEVENT"));
    }
}
