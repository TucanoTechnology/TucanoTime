// Domain model and validation for TucanoTime.
//
// Money is stored in minor units (cents) as integers; hours are stored as
// hundredths of an hour. Floats never touch persisted values, so totals are
// exact and a future invoice snapshot cannot drift.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Number;
use uuid::Uuid;

/// Hours as hundredths (1 = 0.01 h, 2400 = 24.00 h).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hours(pub u32);

impl Hours {
    pub const MIN: u32 = 1;
    pub const MAX: u32 = 2400;

    /// Exact `hours * rate_minor / 100`, rounded half up to a minor unit.
    pub fn amount_minor(self, rate_minor: u64) -> u64 {
        let numerator = u128::from(self.0) * u128::from(rate_minor);
        u64::try_from((numerator + 50) / 100).unwrap_or(u64::MAX)
    }

    fn parse(s: &str) -> Option<u32> {
        let s = s.trim();
        let (int, frac) = match s.split_once('.') {
            Some((i, f)) => {
                if f.is_empty() || f.len() > 2 || !f.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (
                    i,
                    if f.len() == 1 {
                        format!("{f}0")
                    } else {
                        f.to_owned()
                    },
                )
            }
            None => (s, "00".to_owned()),
        };
        if int.is_empty() || int.len() > 2 || !int.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let hundredths: u32 = int.parse::<u32>().ok()? * 100 + frac.parse::<u32>().ok()?;
        if (Self::MIN..=Self::MAX).contains(&hundredths) {
            Some(hundredths)
        } else {
            None
        }
    }
}

impl<'de> Deserialize<'de> for Hours {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = Number::deserialize(d)?;
        // Number::to_string() is the shortest round-trip decimal, so it is an
        // exact textual form of the JSON token; parsing that as a decimal
        // avoids any binary float involvement.
        Self::parse(&n.to_string())
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("hours must be 0.01 to 24.00, two decimals"))
    }
}

impl Serialize for Hours {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let whole = self.0 / 100;
        let frac = self.0 % 100;
        let text = if frac == 0 {
            format!("{whole}")
        } else if frac.is_multiple_of(10) {
            format!("{whole}.{}", frac / 10)
        } else {
            format!("{whole}.{frac:02}")
        };
        use std::str::FromStr;
        Number::from_str(&text)
            .map_err(serde::ser::Error::custom)
            .and_then(|n| n.serialize(s))
    }
}

/// ISO-4217 alpha code, normalised to upper case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Currency(pub String);

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw)
            .ok_or_else(|| serde::de::Error::custom("currency must be a 3-letter ISO-4217 code"))
    }
}

impl Currency {
    pub fn parse(raw: &str) -> Option<Self> {
        let up = raw.trim().to_uppercase();
        if up.len() == 3 && up.bytes().all(|b| b.is_ascii_alphabetic()) {
            Some(Self(up))
        } else {
            None
        }
    }
}

/// Project code: letters, digits, dash and underscore, normalised to upper
/// case, must start with a letter or digit, at most 24 characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ProjectCode(pub String);

impl<'de> Deserialize<'de> for ProjectCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::custom("invalid project code"))
    }
}

impl ProjectCode {
    pub fn parse(raw: &str) -> Option<Self> {
        let up = raw.trim().to_uppercase();
        let ok = !up.is_empty()
            && up.len() <= 24
            && up
                .bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_alphanumeric() || (i > 0 && (b == b'-' || b == b'_')));
        if ok { Some(Self(up)) } else { None }
    }
}

fn validate_name(raw: &str) -> Option<&str> {
    let t = raw.trim();
    if t.is_empty() || t.chars().count() > 120 {
        None
    } else {
        Some(t)
    }
}

fn validate_note(raw: &str) -> Option<&str> {
    if raw.chars().count() <= 500 {
        Some(raw)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub currency: Currency,
    pub default_rate_minor: u64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub customer_id: Uuid,
    pub code: ProjectCode,
    pub name: String,
    /// The project's own currency (required). Billing reads this directly.
    pub currency: Currency,
    /// The project's own hourly rate in minor units (required).
    pub rate_minor: u64,
    pub active: bool,
}

/// On-disk shape used only for reading. Pre-#11 project documents may omit
/// `currency`/`rate_minor`; `resolve` fills them from the owning customer so a
/// legacy document is never rejected (self-heals on the next write).
#[derive(Debug, Clone, Deserialize)]
struct ProjectDoc {
    customer_id: Uuid,
    code: ProjectCode,
    #[serde(default)]
    name: String,
    #[serde(default)]
    currency: Option<Currency>,
    #[serde(default)]
    rate_minor: Option<u64>,
    #[serde(default = "default_active")]
    active: bool,
}

impl ProjectDoc {
    fn resolve(self, customer: &Customer) -> Project {
        let name = if self.name.trim().is_empty() {
            self.code.0.clone()
        } else {
            self.name
        };
        Project {
            customer_id: self.customer_id,
            code: self.code,
            name,
            currency: self.currency.unwrap_or_else(|| customer.currency.clone()),
            rate_minor: self.rate_minor.unwrap_or(customer.default_rate_minor),
            active: self.active,
        }
    }
}

/// Parse a stored project document, filling any pre-#11 missing currency/rate
/// from the owning customer.
pub fn project_from_bytes(bytes: &[u8], customer: &Customer) -> Result<Project, serde_json::Error> {
    let doc: ProjectDoc = serde_json::from_slice(bytes)?;
    Ok(doc.resolve(customer))
}

/// An optional work level under a project (#38). A task may override the
/// project's currency/rate; otherwise it inherits the project's values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    pub code: ProjectCode,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub currency: Option<Currency>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rate_minor: Option<u64>,
    pub active: bool,
}

/// Where a time entry came from. Server-set; clients create `manual` entries.
/// Timer (#14) and calendar import (#15/#36) write their own value so imported
/// time is always traceable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    #[default]
    Manual,
    Timer,
    CalendarImport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub date: NaiveDate,
    pub customer_id: Uuid,
    /// The user who logged the entry (#21 attribution; None for legacy docs).
    #[serde(default)]
    pub user_id: Option<Uuid>,
    pub project_code: ProjectCode,
    /// Optional task within the project (#38). Must belong to `project_code`.
    #[serde(default)]
    pub task_code: Option<ProjectCode>,
    pub hours: Hours,
    pub note: String,
    /// Billable vs non-billable. Always serialised; defaults to true on read so
    /// pre-#20 documents load as billable (self-heals on next write).
    #[serde(default = "default_true")]
    pub billable: bool,
    /// Provenance. Defaults to `manual` so pre-#18 documents load unchanged.
    #[serde(default)]
    pub source: Source,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Effective (currency, rate) of an entry. Rate precedence (#21):
/// **task → person → project → customer default**. Currency precedence:
/// task → project → customer. `user_rate` is the logging person's default
/// (Some only when > 0), kept as a plain number so `domain` stays independent
/// of the `auth` module.
pub fn effective_rates(
    _entry: &Entry,
    customer: &Customer,
    project: Option<&Project>,
    task: Option<&Task>,
    user_rate: Option<u64>,
) -> (Currency, u64) {
    let currency = task
        .and_then(|t| t.currency.clone())
        .or_else(|| project.map(|p| p.currency.clone()))
        .unwrap_or_else(|| customer.currency.clone());
    let rate = task
        .and_then(|t| t.rate_minor)
        .or(user_rate.filter(|r| *r > 0))
        .or_else(|| project.map(|p| p.rate_minor))
        .unwrap_or(customer.default_rate_minor);
    (currency, rate)
}

// ---------------------------------------------------------------- inputs ---

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomerInput {
    pub name: String,
    pub currency: Currency,
    pub default_rate_minor: u64,
    #[serde(default = "default_active")]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInput {
    pub code: ProjectCode,
    #[serde(default)]
    pub name: String,
    /// Required: the project's own currency (prefilled from the customer in the GUI).
    pub currency: Currency,
    /// Required: the project's own hourly rate in minor units.
    pub rate_minor: u64,
    #[serde(default = "default_active")]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub code: ProjectCode,
    #[serde(default)]
    pub name: String,
    /// Optional override of the project currency.
    #[serde(default)]
    pub currency: Option<Currency>,
    /// Optional override of the project rate (minor units per hour).
    #[serde(default)]
    pub rate_minor: Option<u64>,
    #[serde(default = "default_active")]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryInput {
    pub date: String,
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    #[serde(default)]
    pub task_code: Option<ProjectCode>,
    pub hours: Hours,
    #[serde(default)]
    pub note: String,
    #[serde(default = "default_true")]
    pub billable: bool,
}

fn default_active() -> bool {
    true
}

fn default_true() -> bool {
    true
}

/// Field-level problem; carried into the structured 422 error shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Validation for a rate: 0 is allowed (unbilled work), absurd values are not.
pub fn validate_rate_minor(rate: u64, field: &str, errors: &mut Vec<FieldError>) {
    if rate > 100_000_000 {
        errors.push(FieldError::new(
            field,
            "rate in minor units must be at most 100000000",
        ));
    }
}

pub fn validate_customer_input(input: &CustomerInput) -> Result<CustomerDraft, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = validate_name(&input.name).map(str::to_owned);
    if name.is_none() {
        errors.push(FieldError::new("name", "required, at most 120 characters"));
    }
    validate_rate_minor(input.default_rate_minor, "default_rate_minor", &mut errors);
    if errors.is_empty() {
        Ok(CustomerDraft {
            name: name.unwrap(),
            currency: input.currency.0.clone(),
            default_rate_minor: input.default_rate_minor,
            active: input.active,
        })
    } else {
        Err(errors)
    }
}

pub fn validate_project_input(input: &ProjectInput) -> Result<ProjectDraft, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = if input.name.trim().is_empty() {
        input.code.0.clone()
    } else if input.name.chars().count() <= 120 {
        input.name.trim().to_owned()
    } else {
        errors.push(FieldError::new("name", "at most 120 characters"));
        String::new()
    };
    validate_rate_minor(input.rate_minor, "rate_minor", &mut errors);
    if errors.is_empty() {
        Ok(ProjectDraft {
            code: input.code.0.clone(),
            name,
            currency: input.currency.0.clone(),
            rate_minor: input.rate_minor,
            active: input.active,
        })
    } else {
        Err(errors)
    }
}

pub fn validate_entry_input(input: &EntryInput) -> Result<EntryDraft, Vec<FieldError>> {
    let mut errors = Vec::new();
    let date = NaiveDate::parse_from_str(input.date.trim(), "%Y-%m-%d")
        .map_err(|_| {
            errors.push(FieldError::new(
                "date",
                "must be a calendar date as YYYY-MM-DD",
            ));
        })
        .ok();
    let note_ok = validate_note(&input.note).is_some();
    if !note_ok {
        errors.push(FieldError::new("note", "at most 500 characters"));
    }
    match (date, note_ok) {
        (Some(date), true) => Ok(EntryDraft {
            date,
            customer_id: input.customer_id,
            project_code: input.project_code.0.clone(),
            task_code: input.task_code.as_ref().map(|c| c.0.clone()),
            hours: input.hours,
            note: input.note.clone(),
            billable: input.billable,
        }),
        _ => Err(errors),
    }
}

/// Validated, ready-to-persist payloads (owned, normalised strings).
#[derive(Debug)]
pub struct CustomerDraft {
    pub name: String,
    pub currency: String,
    pub default_rate_minor: u64,
    pub active: bool,
}

#[derive(Debug)]
pub struct ProjectDraft {
    pub code: String,
    pub name: String,
    pub currency: String,
    pub rate_minor: u64,
    pub active: bool,
}

#[derive(Debug)]
pub struct EntryDraft {
    pub date: NaiveDate,
    pub customer_id: Uuid,
    pub project_code: String,
    pub task_code: Option<String>,
    pub hours: Hours,
    pub note: String,
    pub billable: bool,
}

#[derive(Debug)]
pub struct TaskDraft {
    pub code: String,
    pub name: String,
    pub currency: Option<String>,
    pub rate_minor: Option<u64>,
    pub active: bool,
}

pub fn validate_task_input(input: &TaskInput) -> Result<TaskDraft, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = if input.name.trim().is_empty() {
        input.code.0.clone()
    } else if input.name.chars().count() <= 120 {
        input.name.trim().to_owned()
    } else {
        errors.push(FieldError::new("name", "at most 120 characters"));
        String::new()
    };
    if let Some(rate) = input.rate_minor {
        validate_rate_minor(rate, "rate_minor", &mut errors);
    }
    if errors.is_empty() {
        Ok(TaskDraft {
            code: input.code.0.clone(),
            name,
            currency: input.currency.as_ref().map(|c| c.0.clone()),
            rate_minor: input.rate_minor,
            active: input.active,
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_hours_token(token: &str) -> Result<Hours, serde_json::Error> {
        serde_json::from_str::<Hours>(token)
    }

    #[test]
    fn hours_accepts_two_decimal_forms() {
        assert_eq!(parse_hours_token("7.5").unwrap().0, 750);
        assert_eq!(parse_hours_token("7.50").unwrap().0, 750);
        assert_eq!(parse_hours_token("7").unwrap().0, 700);
        assert_eq!(parse_hours_token("0.07").unwrap().0, 7);
        assert_eq!(parse_hours_token("24").unwrap().0, 2400);
        assert_eq!(parse_hours_token("0.01").unwrap().0, 1);
    }

    #[test]
    fn hours_rejects_out_of_range_and_precision() {
        assert!(parse_hours_token("0").is_err());
        assert!(parse_hours_token("0.001").is_err());
        assert!(parse_hours_token("24.01").is_err());
        assert!(parse_hours_token("100").is_err());
        assert!(parse_hours_token("-2").is_err());
    }

    #[test]
    fn hours_round_trips_exactly() {
        for token in ["7.5", "7.25", "8", "0.07", "24"] {
            let h = parse_hours_token(token).unwrap();
            let out = serde_json::to_string(&h).unwrap();
            let back = parse_hours_token(&out).unwrap();
            assert_eq!(h, back);
        }
    }

    #[test]
    fn amount_is_exact_minor_math() {
        // 7.50 h * 60.00 EUR/h = 450.00 EUR = 45000 cents.
        assert_eq!(Hours(750).amount_minor(6000), 45000);
        // 0.07 h * 33 cents/h = 2.31 -> 2 (half-up at the cent).
        assert_eq!(Hours(7).amount_minor(33), 2);
        assert_eq!(Hours(1).amount_minor(50), 1); // 0.5 -> 1
    }

    #[test]
    fn currency_and_code_normalise() {
        assert_eq!(Currency::parse(" eur").unwrap().0, "EUR");
        assert!(Currency::parse("EU").is_none());
        assert!(Currency::parse("EU1").is_none());
        assert_eq!(ProjectCode::parse(" acme-42 ").unwrap().0, "ACME-42");
        assert!(ProjectCode::parse("").is_none());
        assert!(ProjectCode::parse("-abc").is_none());
        assert!(ProjectCode::parse("a/b").is_none());
        assert!(ProjectCode::parse("../x").is_none());
    }

    fn customer_input(name: &str, currency: &str, rate: u64) -> CustomerInput {
        CustomerInput {
            name: name.into(),
            currency: Currency::parse(currency).unwrap(),
            default_rate_minor: rate,
            active: true,
        }
    }

    #[test]
    fn customer_validation_boundaries() {
        assert!(validate_customer_input(&customer_input("ACME", "EUR", 6000)).is_ok());
        let errs = validate_customer_input(&customer_input("  ", "EUR", 6000)).unwrap_err();
        assert_eq!(errs[0].field, "name");
        let errs = validate_customer_input(&customer_input("x", "EUR", 100_000_001)).unwrap_err();
        assert_eq!(errs[0].field, "default_rate_minor");
    }

    #[test]
    fn entry_validation_reports_every_problem() {
        let input = EntryInput {
            date: "2026-13-01".into(),
            customer_id: Uuid::new_v4(),
            project_code: ProjectCode::parse("P1").unwrap(),
            task_code: None,
            hours: Hours(100),
            note: "x".repeat(501),
            billable: true,
        };
        let errs = validate_entry_input(&input).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "date"));
        assert!(errs.iter().any(|e| e.field == "note"));
    }

    #[test]
    fn unknown_fields_are_rejected_by_dto_deser() {
        let err = serde_json::from_str::<CustomerInput>(
            r#"{"name":"A","currency":"EUR","default_rate_minor":1,"extra":true}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn effective_rate_precedence_task_person_project_customer() {
        let entry = Entry {
            id: Uuid::new_v4(),
            date: chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            customer_id: Uuid::new_v4(),
            user_id: None,
            project_code: ProjectCode::parse("P1").unwrap(),
            task_code: None,
            hours: Hours(100),
            note: String::new(),
            billable: true,
            source: Source::Manual,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let customer = Customer {
            id: entry.customer_id,
            name: "ACME".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 6000,
            active: true,
        };
        let project = Project {
            customer_id: customer.id,
            code: entry.project_code.clone(),
            name: "P1".into(),
            currency: Currency("USD".into()),
            rate_minor: 3000,
            active: true,
        };
        // Project value wins over customer default.
        assert_eq!(
            effective_rates(&entry, &customer, Some(&project), None, None),
            (Currency("USD".into()), 3000)
        );
        // Missing project falls to customer default.
        assert_eq!(
            effective_rates(&entry, &customer, None, None, None),
            (Currency("EUR".into()), 6000)
        );
        // Person rate beats the project rate, keeps the project currency.
        assert_eq!(
            effective_rates(&entry, &customer, Some(&project), None, Some(4500)),
            (Currency("USD".into()), 4500)
        );
        // A task override beats the person rate.
        let task = Task {
            customer_id: customer.id,
            project_code: entry.project_code.clone(),
            code: ProjectCode::parse("T1").unwrap(),
            name: "T1".into(),
            currency: Some(Currency("GBP".into())),
            rate_minor: Some(5000),
            active: true,
        };
        assert_eq!(
            effective_rates(&entry, &customer, Some(&project), Some(&task), Some(4500)),
            (Currency("GBP".into()), 5000)
        );
    }
}
