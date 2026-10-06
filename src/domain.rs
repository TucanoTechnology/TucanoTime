// Domain model and validation for TucanoTime.
//
// Money is stored in minor units (cents) as integers; hours are stored as
// hundredths of an hour. Floats never touch persisted values, so totals are
// exact and a future invoice snapshot cannot drift.

use crate::auth::User;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Number;
use std::collections::BTreeMap;
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
    /// Optional billing email for invoice delivery (#35).
    #[serde(default)]
    pub email: String,
    /// Payment terms used for `due_date` at issue (#116). `None` defers to
    /// the org template's default, then the legacy net-14-from-period-to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_terms: Option<PaymentTerms>,
    /// Markdown-subset notes appended to this customer's invoice documents
    /// after the org footer (#116). Cap enforced by validation.
    #[serde(default)]
    pub invoice_notes: String,
    /// Per-customer subject override; `%variable%` placeholders allowed
    /// (#116). Empty defers to the org template, then the legacy subject.
    #[serde(default)]
    pub invoice_subject: String,
    /// Postal address for invoice documents and e-invoicing (#139).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
    /// Named contacts; the one flagged `billing` receives invoice emails
    /// when set (#139). Legacy customers have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<Contact>,
    /// Default VAT percent in HUNDREDTHS of a percent (2100 = 21.00%).
    /// Integer, never a float; #143's line model will consume it.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub tax_hundredths: u16,
    /// Default discount percent, same hundredths encoding, applied by #143.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub discount_hundredths: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// A postal address; every part is free text with validated bounds (#139).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub postal_code: String,
    #[serde(default)]
    pub country: String,
}

/// A named person at a customer (#139). `billing` marks the invoice
/// recipient; at most one contact may carry it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub billing: bool,
}

impl Customer {
    /// Where invoice emails go: the flagged billing contact's address when
    /// present, else the legacy top-level email (#139 keeps both truthful).
    #[must_use]
    pub fn billing_email(&self) -> &str {
        self.contacts
            .iter()
            .find(|c| c.billing && !c.email.trim().is_empty())
            .map(|c| c.email.trim())
            .unwrap_or(self.email.trim())
    }
}

pub const MAX_CONTACTS: usize = 10;

/// Validate the #139 additions; appends FieldErrors like every other input.
pub fn validate_customer_billing_fields(
    address: &Option<Address>,
    contacts: &[Contact],
    tax_hundredths: u16,
    discount_hundredths: u16,
    errors: &mut Vec<FieldError>,
) {
    if let Some(a) = address {
        for (field, val, max) in [
            ("address.street", &a.street, 120usize),
            ("address.city", &a.city, 80),
            ("address.postal_code", &a.postal_code, 16),
            ("address.country", &a.country, 56),
        ] {
            if val.chars().count() > max {
                errors.push(FieldError::new(field, format!("too long (max {max})")));
            }
        }
        let any = [
            a.street.trim(),
            a.city.trim(),
            a.postal_code.trim(),
            a.country.trim(),
        ]
        .iter()
        .any(|v| !v.is_empty());
        if any && a.country.trim().len() < 2 {
            errors.push(FieldError::new(
                "address.country",
                "required when addressing an invoice",
            ));
        }
    }
    if contacts.len() > MAX_CONTACTS {
        errors.push(FieldError::new(
            "contacts",
            format!("at most {MAX_CONTACTS} contacts"),
        ));
    }
    for (i, c) in contacts.iter().enumerate() {
        let name = format!("contacts[{i}].name");
        if c.name.trim().is_empty() || c.name.chars().count() > 120 {
            errors.push(FieldError::new(name, "required, at most 120 characters"));
        }
        if c.role.chars().count() > 60 {
            errors.push(FieldError::new(
                format!("contacts[{i}].role"),
                "at most 60 characters",
            ));
        }
        let email = c.email.trim();
        if !email.is_empty() && !crate::email::valid_address(email) {
            errors.push(FieldError::new(
                format!("contacts[{i}].email"),
                "not a valid email address",
            ));
        }
    }
    if contacts.iter().filter(|c| c.billing).count() > 1 {
        errors.push(FieldError::new(
            "contacts",
            "only one contact can be the billing contact",
        ));
    }
    for (field, v) in [
        ("tax_hundredths", tax_hundredths),
        ("discount_hundredths", discount_hundredths),
    ] {
        if v > 10_000 {
            errors.push(FieldError::new(field, "at most 100.00 percent"));
        }
    }
}

/// Payment terms that decide an invoice's `due_date` at issue (#116).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct PaymentTerms {
    pub kind: TermsKind,
    /// Only meaningful for `custom`; `1..=365`. Other kinds must omit it
    /// (validation rejects it otherwise, serde defaults it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermsKind {
    /// Due the day the invoice is issued.
    #[serde(rename = "upon_receipt")]
    UponReceipt,
    #[serde(rename = "net_15")]
    Net15,
    #[serde(rename = "net_20")]
    Net20,
    #[serde(rename = "net_30")]
    Net30,
    #[serde(rename = "net_45")]
    Net45,
    /// `days` required, 1..=365.
    #[serde(rename = "custom")]
    Custom,
}

impl PaymentTerms {
    /// The fixed-day kinds carry their days implicitly; `custom` and
    /// `upon_receipt` do not.
    pub fn fixed_days(&self) -> Option<u16> {
        match self.kind {
            TermsKind::UponReceipt => Some(0),
            TermsKind::Net15 => Some(15),
            TermsKind::Net20 => Some(20),
            TermsKind::Net30 => Some(30),
            TermsKind::Net45 => Some(45),
            TermsKind::Custom => self.days,
        }
    }
}

/// Organization legal identity for invoice documents and emails (#138),
/// stored as the singleton `org_profile.json`. `name` empty means "fall back
/// to the boot-time `org_name` config key" — a documented resolution order
/// (org_profile > config org_name > built-in default), never parallel truth:
/// every reader goes through `api::invoicing::org_for`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgProfile {
    /// Display name on documents (falls back to config `org_name`).
    #[serde(default)]
    pub name: String,
    /// VAT / tax / company registration identifier printed on documents.
    #[serde(default)]
    pub legal_id: String,
    #[serde(default)]
    pub address: Option<Address>,
    /// Sender display name for invoice emails (#146); empty = built-in
    /// "TucanoTime". The SMTP password/username stay in the vault — this is
    /// presentation only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from_name: String,
    /// Reply-To address for invoice emails (#146); empty omits the header.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reply_to: String,
    /// Invoice document accent, '#rrggbb' (#146); empty keeps the built-in
    /// ink-only rendering. Applied to the totals rule + amount text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accent: String,
}

/// Display-label overrides for invoice documents (#147). Empty = built-in
/// label; only DISPLAY changes — API/JSON field names are untouched.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelOverrides {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quantity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit_price: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtotal: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub discount: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tax: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub total: String,
}

impl LabelOverrides {
    /// Resolve a label, falling back to the built-in (trimmed).
    #[must_use]
    pub fn label<'a>(&'a self, value: &'a str, fallback: &'a str) -> &'a str {
        let v = value.trim();
        if v.is_empty() { fallback } else { v }
    }

    pub fn validate(&self, errors: &mut Vec<FieldError>) {
        for (field, v) in [
            ("labels.description", &self.description),
            ("labels.quantity", &self.quantity),
            ("labels.unit_price", &self.unit_price),
            ("labels.subtotal", &self.subtotal),
            ("labels.discount", &self.discount),
            ("labels.tax", &self.tax),
            ("labels.total", &self.total),
        ] {
            if v.chars().count() > 40 {
                errors.push(FieldError::new(field, "label too long (max 40 characters)"));
            }
        }
    }
}

/// A reusable Product/Service line type (#147), stored `items/<id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemType {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: LineItemKind,
    /// Default unit price in minor units (0 = ask at invoice time).
    pub default_price_minor: u64,
    #[serde(default = "default_currency_eur")]
    pub currency: Currency,
    #[serde(default = "default_active")]
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

fn default_currency_eur() -> Currency {
    Currency("EUR".to_string())
}

/// Create/update payload for item types (#147).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemTypeInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: LineItemKind,
    #[serde(default)]
    pub default_price_minor: u64,
    #[serde(default)]
    pub currency: Option<Currency>,
    #[serde(default = "default_active")]
    pub active: bool,
}

/// The org-wide invoice document template (#116), stored as the singleton
/// `invoice_template.json` next to `scheduler.json`. Every field is optional:
/// an unset template keeps the hard-coded legacy behaviour byte-for-byte.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceTemplate {
    /// Subject line; `%variable%` placeholders resolved at render/issue.
    #[serde(default)]
    pub subject: String,
    /// Markdown-subset body shown before the line table.
    #[serde(default)]
    pub body: String,
    /// Markdown-subset footer (e.g. bank details), after body + notes.
    #[serde(default)]
    pub footer: String,
    /// Default terms for customers that carry none.
    #[serde(default)]
    pub payment_terms: Option<PaymentTerms>,
    /// Document display labels (#147); empty = built-in.
    #[serde(default)]
    pub labels: LabelOverrides,
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
    /// Optional total budget in hours (hundredths) (#30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_hours: Option<u32>,
    /// Optional total budget in minor units (#30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_amount_minor: Option<u64>,
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
    #[serde(default)]
    budget_hours: Option<u32>,
    #[serde(default)]
    budget_amount_minor: Option<u64>,
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
            budget_hours: self.budget_hours,
            budget_amount_minor: self.budget_amount_minor,
        }
    }
}

/// Parse a stored project document, filling any pre-#11 missing currency/rate
/// from the owning customer.
pub fn project_from_bytes(bytes: &[u8], customer: &Customer) -> Result<Project, serde_json::Error> {
    let doc: ProjectDoc = serde_json::from_slice(bytes)?;
    Ok(doc.resolve(customer))
}

/// An optional work level under a project (#38), without billing overrides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    pub code: ProjectCode,
    pub name: String,
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

// ---------------------------------------------------------------- invoices --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvoiceStatus {
    Draft,
    Issued,
    /// Some recorded payments, balance still open (#114).
    #[serde(rename = "partly_paid")]
    PartlyPaid,
    Paid,
    /// Remaining balance forgiven: final, excluded from outstanding (#114).
    #[serde(rename = "written_off")]
    WrittenOff,
}

impl InvoiceStatus {
    /// Issued and not yet closed out: locks its entries (#18) and counts as
    /// receivable (#114).
    pub fn is_open(&self) -> bool {
        matches!(self, InvoiceStatus::Issued | InvoiceStatus::PartlyPaid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Time,
    Expense,
    Fixed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvoiceLine {
    pub kind: LineKind,
    pub date: NaiveDate,
    /// Time lines reference an entry; expense lines reference an expense.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expense_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_code: Option<ProjectCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_code: Option<ProjectCode>,
    /// Hours + rate apply to time lines only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<Hours>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_minor: Option<u64>,
    /// Manual lines are re-priced by the server from quantity x unit price,
    /// so an edit payload may omit it (#143); it is always stored.
    #[serde(default)]
    pub amount_minor: u64,
    #[serde(default)]
    pub note: String,
    /// Manual lines (#143): quantity in HUNDREDTHS (150 = 1.50 units) and the
    /// snapshot unit price, so `amount_minor` is always recomputable with
    /// integer math. Absent on tracked lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity_hundredths: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_price_minor: Option<u64>,
    /// Product vs Service on manual lines (#143/#147 catalog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_kind: Option<LineItemKind>,
}

/// What a manual line sells (#143). Tracked lines need no value (time and
/// expenses are inherently services/costs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineItemKind {
    Product,
    Service,
}

/// Integer money math for manual lines: hundredths x minor units rounded
/// half up to a minor unit — the same discipline as `Hours::amount_minor`.
#[must_use]
pub fn manual_amount_minor(quantity_hundredths: u32, unit_price_minor: u64) -> u64 {
    let numerator = u128::from(quantity_hundredths) * u128::from(unit_price_minor);
    u64::try_from((numerator + 50) / 100).unwrap_or(u64::MAX)
}

/// Percentages in hundredths of a percent applied to the integer subtotal.
#[must_use]
pub fn percent_of_minor(value_minor: u64, hundredths: u16) -> u64 {
    // hundredths-of-a-percent over 10_000, rounded half up to a minor unit.
    let numerator = u128::from(value_minor) * u128::from(hundredths);
    u64::try_from((numerator + 5_000) / 10_000).unwrap_or(u64::MAX)
}

/// Document money: discount off the subtotal, VAT on the net. Persisted
/// totals are always these values — never re-derived at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct InvoiceTotals {
    pub subtotal_minor: u64,
    pub discount_minor: u64,
    pub tax_minor: u64,
    pub total_minor: u64,
}

#[must_use]
pub fn invoice_totals(
    subtotal_minor: u64,
    tax_hundredths: u16,
    discount_hundredths: u16,
) -> InvoiceTotals {
    let discount_minor = percent_of_minor(subtotal_minor, discount_hundredths);
    let net = subtotal_minor.saturating_sub(discount_minor);
    let tax_minor = percent_of_minor(net, tax_hundredths);
    InvoiceTotals {
        subtotal_minor,
        discount_minor,
        tax_minor,
        total_minor: net + tax_minor,
    }
}

/// A manual line as submitted (#143); the server computes the amount.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualLineInput {
    pub description: String,
    pub item_kind: LineItemKind,
    /// Integer API: hundredths of a unit (150 = 1.50). No floats.
    pub quantity_hundredths: u32,
    pub unit_price_minor: u64,
    #[serde(default)]
    pub project_code: Option<ProjectCode>,
}

pub const MANUAL_LINE_MAX: usize = 100;

/// Validate + convert manual line input; errors carry `lines[i].field` names.
pub fn validate_manual_lines(
    lines: &[ManualLineInput],
    errors: &mut Vec<FieldError>,
) -> Vec<InvoiceLine> {
    if lines.len() > MANUAL_LINE_MAX {
        errors.push(FieldError::new(
            "lines",
            format!("at most {MANUAL_LINE_MAX} lines"),
        ));
        return Vec::new();
    }
    let today = chrono::Utc::now().date_naive();
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let desc = l.description.trim();
            if desc.is_empty() || desc.chars().count() > 500 {
                errors.push(FieldError::new(
                    format!("lines[{i}].description"),
                    "required, at most 500 characters",
                ));
            }
            if l.quantity_hundredths == 0 || l.quantity_hundredths > 1_000_000 {
                errors.push(FieldError::new(
                    format!("lines[{i}].quantity_hundredths"),
                    "must be between 0.01 and 10000.00 units",
                ));
            }
            if l.unit_price_minor > 100_000_000 {
                errors.push(FieldError::new(
                    format!("lines[{i}].unit_price_minor"),
                    "at most 100000000",
                ));
            }
            InvoiceLine {
                kind: LineKind::Fixed,
                date: today,
                entry_id: None,
                expense_id: None,
                project_code: l.project_code.clone(),
                task_code: None,
                hours: None,
                rate_minor: None,
                amount_minor: manual_amount_minor(l.quantity_hundredths, l.unit_price_minor),
                note: desc.to_string(),
                quantity_hundredths: Some(l.quantity_hundredths),
                unit_price_minor: Some(l.unit_price_minor),
                item_kind: Some(l.item_kind),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invoice {
    pub id: Uuid,
    pub number: String,
    pub customer_id: Uuid,
    pub currency: Currency,
    pub period_from: NaiveDate,
    pub period_to: NaiveDate,
    pub lines: Vec<InvoiceLine>,
    pub total_minor: u64,
    pub status: InvoiceStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at: Option<DateTime<Utc>>,
    /// Payment terms (net-14 from issue, #27). None until issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub payment_reference: String,
    /// Archived PDF document hint (#113): metadata about the immutable
    /// `invoices/<id>.pdf` written at issue time. `None` for drafts and for
    /// legacy issued invoices whose PDF has not been resolved yet (the first
    /// download renders it from the snapshot-locked invoice).
    /// VAT percent in hundredths (2100 = 21.00%) — manual/draft feature
    /// (#143); tracked invoices keep 0 and their sum-of-lines total.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub tax_hundredths: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub discount_hundredths: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfHint>,
    /// Recorded payments ledger (#114). Legacy documents default to empty;
    /// a legacy `Paid` invoice without entries reports its full total as
    /// paid (see `paid_minor`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payments: Vec<InvoicePayment>,
    /// Non-empty once written off: the reason for forgiving the balance (#114).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub write_off_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_off_at: Option<DateTime<Utc>>,
}

/// One recorded payment against an invoice (#114). Amounts are minor units;
/// `method` distinguishes manual/webhook/provider settlements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvoicePayment {
    pub id: Uuid,
    pub amount_minor: u64,
    pub received_at: DateTime<Utc>,
    pub reference: String,
    pub method: String,
}

/// Metadata for a PDF archived next to the invoice document (#113). Lets the
/// GUI say "document on file" without reading the file; `sha256` is the
/// download `ETag` and the archive's verification handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PdfHint {
    /// Sanitised download filename, e.g. `INV-0001.pdf` (derived from
    /// `Invoice::number`, never from user input).
    pub filename: String,
    /// Size of the archived file in bytes.
    pub bytes: u64,
    /// Lower-case hex SHA-256 of the archived bytes.
    pub sha256: String,
    /// When the archive was written (issue time, or lazy first-download for
    /// legacy invoices).
    pub archived_at: DateTime<Utc>,
}

impl Invoice {
    /// Sum of the payment ledger (integer minor units). A legacy `Paid`
    /// document predating #114 carries no ledger: its total counts as paid
    /// so receivables are never overstated by the migration.
    #[must_use]
    pub fn paid_minor(&self) -> u64 {
        let ledger = self.payments.iter().map(|p| p.amount_minor).sum::<u64>();
        if ledger == 0 && self.status == InvoiceStatus::Paid {
            self.total_minor
        } else {
            ledger
        }
    }

    /// What remains collectable (#114). A written-off balance is forgiven:
    /// it is not outstanding, so this returns 0 for that state.
    #[must_use]
    pub fn balance_minor(&self) -> u64 {
        if self.status == InvoiceStatus::WrittenOff {
            return 0;
        }
        self.total_minor.saturating_sub(self.paid_minor())
    }
}

/// Aggregate of invoice states for the dashboard (#27, #114).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InvoiceSummary {
    pub draft: usize,
    pub issued: usize,
    pub overdue: usize,
    pub paid: usize,
    /// Issued with a partially settled balance (#114).
    pub partly_paid: usize,
    /// Forgiven invoices, excluded from outstanding (#114).
    pub written_off: usize,
    /// Outstanding balances (issued + partly paid) keyed by currency.
    pub outstanding: BTreeMap<String, u64>,
}

/// Roll up invoice states as of `today`; an issued invoice past its due date
/// counts as overdue and its total is outstanding.
pub fn summarise_invoices(invoices: &[Invoice], today: NaiveDate) -> InvoiceSummary {
    let mut s = InvoiceSummary {
        draft: 0,
        issued: 0,
        overdue: 0,
        paid: 0,
        partly_paid: 0,
        written_off: 0,
        outstanding: BTreeMap::new(),
    };
    for inv in invoices {
        match inv.status {
            InvoiceStatus::Draft => s.draft += 1,
            InvoiceStatus::Paid => s.paid += 1,
            InvoiceStatus::WrittenOff => s.written_off += 1,
            InvoiceStatus::Issued => s.issued += 1,
            InvoiceStatus::PartlyPaid => s.partly_paid += 1,
        }
        // Overdue + outstanding follow the BALANCE (#114): a settled or
        // forgiven invoice is neither, and a partial payment pulls only the
        // remainder into receivables.
        if inv.status.is_open() && inv.balance_minor() > 0 {
            if inv.due_date.is_some_and(|d| d < today) {
                s.overdue += 1;
            }
            *s.outstanding.entry(inv.currency.0.clone()).or_insert(0) += inv.balance_minor();
        }
    }
    s
}

/// Why invoice generation was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum InvoiceError {
    /// The period's billable work spans more than one currency; an invoice is
    /// single-currency, so the operator must resolve it (per ADR-001).
    MixedCurrency { a: String, b: String },
    /// No billable, not-yet-invoiced work in the period.
    NothingToInvoice,
}

// ---------------------------------------------------------------- expenses --

/// A cost category (org-wide), e.g. Travel, Software.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub id: Uuid,
    pub name: String,
    pub default_billable: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expense {
    pub id: Uuid,
    pub date: NaiveDate,
    pub customer_id: Uuid,
    /// The user who recorded the expense (#51 ownership; None for legacy docs).
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_code: Option<ProjectCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_id: Option<Uuid>,
    pub amount_minor: u64,
    pub currency: Currency,
    pub billable: bool,
    #[serde(default)]
    pub note: String,
    /// Optional receipt stored inline (base64) with its filename (#23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_b64: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryInput {
    pub name: String,
    #[serde(default = "default_true")]
    pub default_billable: bool,
    #[serde(default = "default_active")]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpenseInput {
    pub date: String,
    pub customer_id: Uuid,
    #[serde(default)]
    pub project_code: Option<ProjectCode>,
    #[serde(default)]
    pub category_id: Option<Uuid>,
    pub amount_minor: u64,
    pub currency: Currency,
    #[serde(default = "default_true")]
    pub billable: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub receipt_name: Option<String>,
    #[serde(default)]
    pub receipt_b64: Option<String>,
}

pub fn validate_category_input(input: &CategoryInput) -> Result<CategoryDraft, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = validate_name(&input.name).map(str::to_owned);
    if name.is_none() {
        errors.push(FieldError::new("name", "required, at most 120 characters"));
    }
    if errors.is_empty() {
        Ok(CategoryDraft {
            name: name.unwrap(),
            default_billable: input.default_billable,
            active: input.active,
        })
    } else {
        Err(errors)
    }
}

pub fn validate_expense_input(input: &ExpenseInput) -> Result<ExpenseDraft, Vec<FieldError>> {
    let mut errors = Vec::new();
    let date = NaiveDate::parse_from_str(input.date.trim(), "%Y-%m-%d")
        .map_err(|_| {
            errors.push(FieldError::new(
                "date",
                "must be a calendar date as YYYY-MM-DD",
            ))
        })
        .ok();
    if input.amount_minor > 100_000_000 {
        errors.push(FieldError::new("amount_minor", "at most 100000000"));
    }
    if validate_note(&input.note).is_none() {
        errors.push(FieldError::new("note", "at most 500 characters"));
    }
    if input
        .receipt_b64
        .as_deref()
        .is_some_and(|b| b.len() > 4_000_000)
    {
        errors.push(FieldError::new("receipt_b64", "receipt too large"));
    }
    match (date, errors.is_empty()) {
        (Some(date), true) => Ok(ExpenseDraft {
            date,
            customer_id: input.customer_id,
            project_code: input.project_code.as_ref().map(|c| c.0.clone()),
            category_id: input.category_id,
            amount_minor: input.amount_minor,
            currency: input.currency.0.clone(),
            billable: input.billable,
            note: input.note.clone(),
            receipt_name: input.receipt_name.clone(),
            receipt_b64: input.receipt_b64.clone(),
        }),
        _ => Err(errors),
    }
}

#[derive(Debug)]
pub struct CategoryDraft {
    pub name: String,
    pub default_billable: bool,
    pub active: bool,
}

// ------------------------------------------------------------- reimbursements --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimState {
    Draft,
    Submitted,
    Approved,
    Rejected,
}

/// A reimbursement claim over a set of expenses (#24). While submitted or
/// approved, its expenses are locked from deletion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpenseClaim {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: String,
    pub expense_ids: Vec<Uuid>,
    pub total_minor: u64,
    pub currency: Currency,
    pub state: ClaimState,
    pub comment: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimInput {
    pub title: String,
    pub expense_ids: Vec<Uuid>,
}

// ------------------------------------------------------------------- timer --

/// A running timer for one user (#14). Persisted so it survives reloads and is
/// visible from any device (server-side state).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub user_id: Uuid,
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    #[serde(default)]
    pub task_code: Option<ProjectCode>,
    #[serde(default)]
    pub note: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartTimerInput {
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    #[serde(default)]
    pub task_code: Option<ProjectCode>,
    #[serde(default)]
    pub note: String,
}

// ------------------------------------------------------------- recurring --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cadence {
    Weekly,
    Monthly,
    Quarterly,
}

impl Cadence {
    pub fn interval_days(self) -> i64 {
        match self {
            Cadence::Weekly => 7,
            Cadence::Monthly => 30,
            Cadence::Quarterly => 91,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecurMode {
    /// Bill tracked time + expenses for the period.
    Time,
    /// Bill a fixed retainer amount each cycle.
    Retainer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecurringSchedule {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub cadence: Cadence,
    pub mode: RecurMode,
    /// Retainer amount in minor units (used when mode = Retainer).
    pub retainer_amount_minor: u64,
    pub currency: Currency,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_period_end: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
}

/// If a schedule is due as of `today`, return the period `(from, to)` to bill.
pub fn due_period(
    cadence: Cadence,
    last_period_end: Option<NaiveDate>,
    today: NaiveDate,
) -> Option<(NaiveDate, NaiveDate)> {
    let interval = cadence.interval_days();
    match last_period_end {
        None => {
            let from = today - chrono::Duration::days(interval - 1);
            Some((from, today))
        }
        Some(last) => {
            if (today - last).num_days() >= interval {
                Some((last + chrono::Duration::days(1), today))
            } else {
                None
            }
        }
    }
}

/// Elapsed hundredths-of-an-hour for a running timer, rounded half-up and
/// clamped to the valid entry range (0.01–24.00 h).
pub fn elapsed_hundredths(started_at: DateTime<Utc>, now: DateTime<Utc>) -> u32 {
    let secs = (now - started_at).num_seconds().max(0) as u64;
    let hundredths = (secs * 100 + 1800) / 3600; // round half up
    hundredths.clamp(1, 2400) as u32
}

// -------------------------------------------------------------- notifications --

/// A user-facing notification produced by reminders (#22) or alerts (#30).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: Uuid,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub read: bool,
}

// -------------------------------------------------------------- submissions --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionState {
    Draft,
    Submitted,
    Approved,
    Rejected,
}

/// A weekly timesheet submission for one user. While `submitted` or `approved`
/// its entries are locked (via the shared lock seam, #18); `rejected`/`draft`
/// release them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub id: Uuid,
    pub user_id: Uuid,
    pub week_start: NaiveDate,
    pub week_end: NaiveDate,
    pub state: SubmissionState,
    pub entry_ids: Vec<Uuid>,
    pub comment: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct ExpenseDraft {
    pub date: NaiveDate,
    pub customer_id: Uuid,
    pub project_code: Option<String>,
    pub category_id: Option<Uuid>,
    pub amount_minor: u64,
    pub currency: String,
    pub billable: bool,
    pub note: String,
    pub receipt_name: Option<String>,
    pub receipt_b64: Option<String>,
}

/// Read-only context for invoice generation, bundled to keep the signature small.
pub struct InvoiceSources<'a> {
    pub projects: &'a [Project],
    pub tasks: &'a [Task],
    pub users: &'a [User],
    pub entries: &'a [Entry],
    pub expenses: &'a [Expense],
    /// Entry ids already on an issued invoice (excluded).
    pub excluded_entries: &'a [Uuid],
    /// Expense ids already on an issued invoice (excluded).
    pub excluded_expenses: &'a [Uuid],
    pub include_expenses: bool,
    /// #134: when true the `projects` list is the billable SCOPE — entries
    /// whose project is absent were deliberately deselected by the staged
    /// wizard and must not bill via the customer-rate fallback.
    pub restrict_projects: bool,
}

/// Build a draft invoice from the billable entries (and, if enabled, billable
/// expenses) in `[from, to]` for one customer. Time-line rates are resolved per
/// entry (person > project > customer) and snapshotted, so a later rate
/// change never rewrites an issued invoice. Items already on an issued invoice
/// (`excluded_*`) are skipped.
pub fn generate_invoice(
    number: String,
    customer: &Customer,
    src: &InvoiceSources<'_>,
    from: NaiveDate,
    to: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Invoice, InvoiceError> {
    let InvoiceSources {
        projects,
        tasks,
        users,
        entries,
        expenses,
        excluded_entries,
        excluded_expenses,
        include_expenses,
        restrict_projects,
    } = src;
    // Review D4: index the lookups done per entry — linear finds made large
    // billing runs O(entries × records).
    let excluded_entry: std::collections::HashSet<Uuid> =
        excluded_entries.iter().copied().collect();
    let excluded_expense: std::collections::HashSet<Uuid> =
        excluded_expenses.iter().copied().collect();
    let project_by_code: std::collections::HashMap<&str, &Project> =
        projects.iter().map(|p| (p.code.0.as_str(), p)).collect();
    let task_by_key: std::collections::HashMap<(&str, &str), &Task> = tasks
        .iter()
        .map(|t| ((t.project_code.0.as_str(), t.code.0.as_str()), t))
        .collect();
    let user_by_id: std::collections::HashMap<Uuid, &User> =
        users.iter().map(|u| (u.id, u)).collect();
    let user_rate = |e: &Entry| -> Option<u64> {
        e.user_id
            .and_then(|uid| user_by_id.get(&uid).map(|u| u.default_rate_minor))
    };
    let mut lines: Vec<InvoiceLine> = Vec::new();
    let mut currency: Option<Currency> = None;
    let mut set_currency = |cur: &Currency| -> Result<(), InvoiceError> {
        match &currency {
            None => {
                currency = Some(cur.clone());
                Ok(())
            }
            Some(existing) if existing == cur => Ok(()),
            Some(existing) => Err(InvoiceError::MixedCurrency {
                a: existing.0.clone(),
                b: cur.0.clone(),
            }),
        }
    };
    for e in entries.iter() {
        if e.customer_id != customer.id || !e.billable || e.date < from || e.date > to {
            continue;
        }
        if excluded_entry.contains(&e.id) {
            continue; // already billed on an issued invoice
        }
        let project = project_by_code.get(e.project_code.0.as_str()).copied();
        if *restrict_projects && project.is_none() {
            continue; // deselected project (#134)
        }
        let task = e.task_code.as_ref().and_then(|tc| {
            task_by_key
                .get(&(e.project_code.0.as_str(), tc.0.as_str()))
                .copied()
        });
        let (cur, rate) = effective_rates(e, customer, project, task, user_rate(e));
        set_currency(&cur)?;
        lines.push(InvoiceLine {
            kind: LineKind::Time,
            date: e.date,
            entry_id: Some(e.id),
            expense_id: None,
            project_code: Some(e.project_code.clone()),
            task_code: e.task_code.clone(),
            hours: Some(e.hours),
            rate_minor: Some(rate),
            amount_minor: e.hours.amount_minor(rate),
            note: e.note.clone(),
            quantity_hundredths: None,
            unit_price_minor: None,
            item_kind: None,
        });
    }
    if *include_expenses {
        for x in expenses.iter() {
            if x.customer_id != customer.id || !x.billable || x.date < from || x.date > to {
                continue;
            }
            if excluded_expense.contains(&x.id) {
                continue;
            }
            if *restrict_projects
                && x.project_code
                    .as_ref()
                    .is_none_or(|pc| !project_by_code.contains_key(pc.0.as_str()))
            {
                continue; // deselected project (#134)
            }
            set_currency(&x.currency)?;
            lines.push(InvoiceLine {
                kind: LineKind::Expense,
                date: x.date,
                entry_id: None,
                expense_id: Some(x.id),
                project_code: x.project_code.clone(),
                task_code: None,
                hours: None,
                rate_minor: None,
                amount_minor: x.amount_minor,
                note: x.note.clone(),
                quantity_hundredths: None,
                unit_price_minor: None,
                item_kind: None,
            });
        }
    }
    if lines.is_empty() {
        return Err(InvoiceError::NothingToInvoice);
    }
    let total_minor = lines.iter().map(|l| l.amount_minor).sum();
    Ok(Invoice {
        id: Uuid::new_v4(),
        number,
        customer_id: customer.id,
        currency: currency.expect("non-empty lines set a currency"),
        period_from: from,
        period_to: to,
        lines,
        total_minor,
        status: InvoiceStatus::Draft,
        created_at: now,
        issued_at: None,
        due_date: None,
        paid_at: None,
        payment_reference: String::new(),
        pdf: None,
        payments: vec![],
        write_off_reason: String::new(),
        written_off_at: None,
        tax_hundredths: 0,
        discount_hundredths: 0,
    })
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
/// **person → project → customer default**. Currency precedence:
/// project → customer. `user_rate` is the logging person's default
/// (Some only when > 0), kept as a plain number so `domain` stays independent
/// of the `auth` module.
pub fn effective_rates(
    _entry: &Entry,
    customer: &Customer,
    project: Option<&Project>,
    _task: Option<&Task>,
    user_rate: Option<u64>,
) -> (Currency, u64) {
    let currency = project
        .map(|p| p.currency.clone())
        .unwrap_or_else(|| customer.currency.clone());
    let rate = user_rate
        .filter(|r| *r > 0)
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
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub payment_terms: Option<PaymentTerms>,
    #[serde(default)]
    pub invoice_notes: String,
    #[serde(default)]
    pub invoice_subject: String,
    #[serde(default)]
    pub address: Option<Address>,
    #[serde(default)]
    pub contacts: Vec<Contact>,
    #[serde(default)]
    pub tax_hundredths: u16,
    #[serde(default)]
    pub discount_hundredths: u16,
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
    #[serde(default)]
    pub budget_hours: Option<u32>,
    #[serde(default)]
    pub budget_amount_minor: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub code: ProjectCode,
    #[serde(default)]
    pub name: String,
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
    let email = input.email.trim();
    if !email.is_empty() && !crate::email::valid_address(email) {
        errors.push(FieldError::new("email", "not a valid email address"));
    }
    // Invoice document fields (#116): same gauntlet as the rest of the input,
    // including the template validators so unknown %tokens% never persist.
    validate_payment_terms(input.payment_terms.as_ref(), &mut errors);
    crate::template::validate_field(
        "invoice_notes",
        &input.invoice_notes,
        crate::template::NOTES_MAX,
        &mut errors,
    );
    crate::template::validate_field(
        "invoice_subject",
        &input.invoice_subject,
        crate::template::SUBJECT_MAX,
        &mut errors,
    );
    validate_customer_billing_fields(
        &input.address,
        &input.contacts,
        input.tax_hundredths,
        input.discount_hundredths,
        &mut errors,
    );
    if errors.is_empty() {
        Ok(CustomerDraft {
            name: name.unwrap(),
            currency: input.currency.0.clone(),
            default_rate_minor: input.default_rate_minor,
            active: input.active,
            email: email.to_string(),
            payment_terms: input.payment_terms,
            invoice_notes: input.invoice_notes.trim().to_string(),
            invoice_subject: input.invoice_subject.trim().to_string(),
            address: input.address.as_ref().map(|a| Address {
                street: a.street.trim().to_string(),
                city: a.city.trim().to_string(),
                postal_code: a.postal_code.trim().to_string(),
                country: a.country.trim().to_string(),
            }),
            contacts: input
                .contacts
                .iter()
                .map(|c| Contact {
                    name: c.name.trim().to_string(),
                    role: c.role.trim().to_string(),
                    email: c.email.trim().to_string(),
                    billing: c.billing,
                })
                .collect(),
            tax_hundredths: input.tax_hundredths,
            discount_hundredths: input.discount_hundredths,
        })
    } else {
        Err(errors)
    }
}

/// `custom` requires days in 1..=365; every other kind must omit them (#116).
pub fn validate_payment_terms(terms: Option<&PaymentTerms>, errors: &mut Vec<FieldError>) {
    let Some(t) = terms else { return };
    match t.kind {
        TermsKind::Custom => match t.days {
            Some(d) if (1..=365).contains(&d) => {}
            _ => errors.push(FieldError::new(
                "payment_terms",
                "custom terms require days in 1..=365",
            )),
        },
        _ => {
            if t.days.is_some() {
                errors.push(FieldError::new(
                    "payment_terms",
                    "only custom terms carry a day count",
                ));
            }
        }
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
            budget_hours: input.budget_hours,
            budget_amount_minor: input.budget_amount_minor,
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
    pub email: String,
    pub payment_terms: Option<PaymentTerms>,
    pub invoice_notes: String,
    pub invoice_subject: String,
    pub address: Option<Address>,
    pub contacts: Vec<Contact>,
    pub tax_hundredths: u16,
    pub discount_hundredths: u16,
}

#[derive(Debug)]
pub struct ProjectDraft {
    pub code: String,
    pub name: String,
    pub currency: String,
    pub rate_minor: u64,
    pub active: bool,
    pub budget_hours: Option<u32>,
    pub budget_amount_minor: Option<u64>,
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
    if errors.is_empty() {
        Ok(TaskDraft {
            code: input.code.0.clone(),
            name,
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
            email: String::new(),
            payment_terms: None,
            invoice_notes: String::new(),
            invoice_subject: String::new(),
            address: None,
            contacts: vec![],
            tax_hundredths: 0,
            discount_hundredths: 0,
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
    fn effective_rate_precedence_person_project_customer_ignores_legacy_tasks() {
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
            email: String::new(),
            payment_terms: None,
            invoice_notes: String::new(),
            invoice_subject: String::new(),
            address: None,
            contacts: vec![],
            tax_hundredths: 0,
            discount_hundredths: 0,
        };
        let project = Project {
            customer_id: customer.id,
            code: entry.project_code.clone(),
            name: "P1".into(),
            currency: Currency("USD".into()),
            rate_minor: 3000,
            active: true,
            budget_hours: None,
            budget_amount_minor: None,
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
        let task: Task = serde_json::from_value(serde_json::json!({
            "customer_id": customer.id,
            "project_code": entry.project_code,
            "code": "T1",
            "name": "T1",
            "currency": "GBP",
            "rate_minor": 5000,
            "active": true,
        }))
        .unwrap();
        assert_eq!(
            effective_rates(&entry, &customer, Some(&project), Some(&task), Some(4500)),
            (Currency("USD".into()), 4500)
        );
        assert_eq!(
            effective_rates(&entry, &customer, Some(&project), Some(&task), None),
            (Currency("USD".into()), 3000)
        );
        let serialized = serde_json::to_value(task).unwrap();
        assert!(serialized.get("currency").is_none());
        assert!(serialized.get("rate_minor").is_none());
    }

    fn inv(status: InvoiceStatus, total: u64, due: Option<NaiveDate>) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            number: "INV-1".into(),
            customer_id: Uuid::new_v4(),
            currency: Currency("EUR".into()),
            period_from: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            period_to: NaiveDate::from_ymd_opt(2026, 1, 7).unwrap(),
            lines: vec![],
            total_minor: total,
            status,
            created_at: Utc::now(),
            issued_at: None,
            due_date: due,
            paid_at: None,
            payment_reference: String::new(),
            pdf: None,
            payments: vec![],
            write_off_reason: String::new(),
            written_off_at: None,
            tax_hundredths: 0,
            discount_hundredths: 0,
        }
    }

    #[test]
    fn manual_money_math_is_exact_integers() {
        // 1.50 units x 99.00 = 148.50 -> 14850 minor.
        assert_eq!(manual_amount_minor(150, 9900), 14850);
        // half-up rounding on the last minor unit: 3 x 333 / 100 = 9.99 -> 999
        assert_eq!(manual_amount_minor(300, 333), 999);
        assert_eq!(manual_amount_minor(1, 1), 0); // 0.01 x 0.01 = 0.00005 -> 0
        assert_eq!(percent_of_minor(10_000, 2100), 2100); // 21% VAT
        let t = invoice_totals(15_000, 2100, 1000);
        assert_eq!(
            (
                t.subtotal_minor,
                t.discount_minor,
                t.tax_minor,
                t.total_minor
            ),
            (15_000, 1_500, 2_835, 16_335)
        );
        // Zero percents are exact pass-through: tracked totals stay stable.
        let z = invoice_totals(18_000, 0, 0);
        assert_eq!(z.total_minor, 18_000);
        assert_eq!(z.discount_minor + z.tax_minor, 0);
    }

    #[test]
    fn manual_line_validation_guards() {
        let mut errs = Vec::new();
        let ok = ManualLineInput {
            description: "Seat".into(),
            item_kind: LineItemKind::Product,
            quantity_hundredths: 400,
            unit_price_minor: 2500,
            project_code: None,
        };
        let lines = validate_manual_lines(std::slice::from_ref(&ok), &mut errs);
        assert!(errs.is_empty());
        assert_eq!(lines[0].amount_minor, 10_000);
        assert_eq!(lines[0].kind, LineKind::Fixed);
        let bad = ManualLineInput {
            quantity_hundredths: 0,
            ..ok.clone()
        };
        let mut e2 = Vec::new();
        validate_manual_lines(&[bad], &mut e2);
        assert_eq!(e2.len(), 1);
        // > MANUAL_LINE_MAX lines short-circuits.
        let many = vec![ok; MANUAL_LINE_MAX + 1];
        let mut e3 = Vec::new();
        assert!(validate_manual_lines(&many, &mut e3).is_empty());
        assert!(!e3.is_empty());
    }

    #[test]
    fn legacy_customer_docs_parse_with_new_invoice_fields_defaulted() {
        // #116 backward compatibility: documents written before payment
        // terms / notes / subject existed must still deserialize (and the
        // absent terms must not break due-date resolution).
        let legacy = r#"{
            "id": "11111111-1111-1111-1111-111111111111",
            "name": "Old Co",
            "currency": "EUR",
            "default_rate_minor": 5000,
            "active": true
        }"#;
        let c: Customer = serde_json::from_str(legacy).unwrap();
        assert_eq!(c.payment_terms, None);
        assert_eq!(c.invoice_notes, "");
        assert_eq!(c.invoice_subject, "");
        // Round-trips without serialising empty optionals back out.
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("payment_terms"), "{json}");
    }

    #[test]
    fn payment_terms_validation() {
        let mut errs = Vec::new();
        validate_payment_terms(
            Some(&PaymentTerms {
                kind: TermsKind::Custom,
                days: None,
            }),
            &mut errs,
        );
        assert_eq!(errs.len(), 1);
        validate_payment_terms(
            Some(&PaymentTerms {
                kind: TermsKind::Custom,
                days: Some(400),
            }),
            &mut errs,
        );
        assert_eq!(errs.len(), 2);
        validate_payment_terms(
            Some(&PaymentTerms {
                kind: TermsKind::Net30,
                days: Some(30),
            }),
            &mut errs,
        );
        assert_eq!(errs.len(), 3, "fixed kinds must omit days");
        validate_payment_terms(
            Some(&PaymentTerms {
                kind: TermsKind::Custom,
                days: Some(21),
            }),
            &mut errs,
        );
        validate_payment_terms(
            Some(&PaymentTerms {
                kind: TermsKind::UponReceipt,
                days: None,
            }),
            &mut errs,
        );
        assert_eq!(errs.len(), 3);
        assert_eq!(
            PaymentTerms {
                kind: TermsKind::UponReceipt,
                days: None
            }
            .fixed_days(),
            Some(0)
        );
        assert_eq!(
            PaymentTerms {
                kind: TermsKind::Net45,
                days: None
            }
            .fixed_days(),
            Some(45)
        );
        assert_eq!(
            PaymentTerms {
                kind: TermsKind::Custom,
                days: Some(9)
            }
            .fixed_days(),
            Some(9)
        );
    }

    #[test]
    fn summary_uses_balance_and_new_lifecycle_states() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let mk = |status: InvoiceStatus, total: u64, paid: u64| {
            let mut i = inv(
                status,
                total,
                Some(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
            );
            if paid > 0 {
                i.payments.push(InvoicePayment {
                    id: Uuid::new_v4(),
                    amount_minor: paid,
                    received_at: i.created_at,
                    reference: "r".into(),
                    method: "manual".into(),
                });
            }
            i
        };
        let invoices = vec![
            mk(InvoiceStatus::Issued, 10_000, 0),
            mk(InvoiceStatus::PartlyPaid, 10_000, 4_000),
            mk(InvoiceStatus::Paid, 10_000, 10_000),
            mk(InvoiceStatus::WrittenOff, 10_000, 2_000),
        ];
        let s = summarise_invoices(&invoices, today);
        assert_eq!(s.issued, 1);
        assert_eq!(s.partly_paid, 1);
        assert_eq!(s.paid, 1);
        assert_eq!(s.written_off, 1);
        // Outstanding = balances of open states only: 10000 + 6000.
        assert_eq!(s.outstanding.get("EUR").copied(), Some(16_000));
        // Both open invoices are past due; paid/written_off never overdue.
        assert_eq!(s.overdue, 2);
    }

    #[test]
    fn invoice_summary_counts_and_outstanding() {
        let today = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let invoices = vec![
            inv(InvoiceStatus::Draft, 100, None),
            inv(
                InvoiceStatus::Issued,
                500,
                Some(NaiveDate::from_ymd_opt(2026, 1, 20).unwrap()),
            ), // overdue
            inv(
                InvoiceStatus::Issued,
                300,
                Some(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
            ), // not yet due
            inv(InvoiceStatus::Paid, 999, None),
        ];
        let s = summarise_invoices(&invoices, today);
        assert_eq!((s.draft, s.issued, s.overdue, s.paid), (1, 2, 1, 1));
        assert_eq!(s.outstanding.get("EUR"), Some(&(800))); // both issued, unpaid
    }
}

#[cfg(test)]
mod timer_tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn elapsed_rounds_and_clamps() {
        let s = at(0);
        assert_eq!(elapsed_hundredths(s, at(3600)), 100); // 1.00h
        assert_eq!(elapsed_hundredths(s, at(1800)), 50); // 0.50h
        assert_eq!(elapsed_hundredths(s, at(18)), 1); // ~0.005 -> 0.01 (min)
        assert_eq!(elapsed_hundredths(s, at(0)), 1); // zero -> min 0.01
        assert_eq!(elapsed_hundredths(s, at(25 * 3600)), 2400); // clamp 24h
    }
}
