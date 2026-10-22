use std::collections::{BTreeMap, HashSet};

use chrono::NaiveDate;
use serde::Serialize;

use crate::domain::{Currency, Hours};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_ROWS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Time,
    Expense,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub record: usize,
    pub line: u64,
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub record: usize,
    pub line: u64,
    pub end_line: u64,
    pub fields: BTreeMap<String, String>,
    pub date: Option<NaiveDate>,
    pub client: String,
    pub project: String,
    pub project_code: String,
    pub person: String,
    pub employee_id: String,
    pub task: String,
    pub category: String,
    pub note: String,
    pub currency: Option<Currency>,
    pub billable: Option<bool>,
    pub invoiced: Option<bool>,
    pub approved: Option<bool>,
    pub hours_hundredths: Option<i64>,
    pub amount_minor: Option<u64>,
    pub billable_rate_minor: Option<u64>,
    pub billable_amount_minor: Option<i64>,
    pub cost_rate_minor: Option<u64>,
    pub cost_amount_minor: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorrectionPair {
    pub positive_record: usize,
    pub negative_record: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub kind: ReportKind,
    pub rows: Vec<Row>,
    pub issues: Vec<Issue>,
    pub correction_pairs: Vec<CorrectionPair>,
}

fn issue(record: usize, line: u64, field: &str, message: &str) -> Issue {
    Issue {
        record,
        line,
        field: field.into(),
        message: message.into(),
    }
}

fn header(value: &str) -> String {
    value.trim().trim_end_matches('?').to_ascii_lowercase()
}

fn decimal(value: &str, grouped: bool) -> Option<i64> {
    let value = value.trim();
    let (negative, value) = match value.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, value),
    };
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || value.ends_with('.')
        || fraction.len() > 2
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let parts: Vec<_> = whole.split(',').collect();
    if parts.len() > 1
        && (!grouped
            || parts[0].is_empty()
            || parts[0].len() > 3
            || parts[1..].iter().any(|part| part.len() != 3))
    {
        return None;
    }
    let mut amount = 0_i64;
    for part in parts {
        if part.is_empty() {
            return None;
        }
        for byte in part.bytes() {
            if !byte.is_ascii_digit() {
                return None;
            }
            amount = amount
                .checked_mul(10)?
                .checked_add(i64::from(byte - b'0'))?;
        }
    }
    amount = amount.checked_mul(100)?;
    for (index, byte) in fraction.bytes().enumerate() {
        amount = amount.checked_add(i64::from(byte - b'0') * if index == 0 { 10 } else { 1 })?;
    }
    if negative {
        amount.checked_neg()
    } else {
        Some(amount)
    }
}

fn valid_quotes(text: &str) -> bool {
    #[derive(Clone, Copy)]
    enum State {
        Start,
        Unquoted,
        Quoted,
        Closed,
    }
    let mut state = State::Start;
    for byte in text.bytes() {
        state = match (state, byte) {
            (State::Start, b'"') => State::Quoted,
            (State::Start | State::Unquoted | State::Closed, b',' | b'\r' | b'\n') => State::Start,
            (State::Unquoted, b'"') => return false,
            (State::Start | State::Unquoted, _) => State::Unquoted,
            (State::Quoted, b'"') => State::Closed,
            (State::Quoted, _) => State::Quoted,
            (State::Closed, b'"') => State::Quoted,
            (State::Closed, _) => return false,
        };
    }
    !matches!(state, State::Quoted)
}

pub fn parse(kind: ReportKind, bytes: &[u8]) -> Result<Report, Vec<Issue>> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(vec![issue(0, 1, "file", "CSV exceeds the 1 MiB limit")]);
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| vec![issue(0, 1, "file", "CSV must be UTF-8")])?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.contains('\0') || !valid_quotes(text) {
        return Err(vec![issue(
            0,
            1,
            "file",
            "CSV contains invalid quoting or NUL bytes",
        )]);
    }
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let raw_headers = reader
        .headers()
        .map_err(|_| vec![issue(0, 1, "headers", "CSV header could not be read")])?;
    if raw_headers.len() > 64 || raw_headers.iter().any(|name| name.len() > 80) {
        return Err(vec![issue(
            0,
            1,
            "headers",
            "at most 64 columns with names up to 80 bytes",
        )]);
    }
    let headers = raw_headers.iter().map(header).collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    for name in &headers {
        if name.is_empty() || !seen.insert(name.clone()) {
            errors.push(issue(
                0,
                1,
                "headers",
                "column names must be nonempty and unique",
            ));
        }
    }
    let mut required = vec![
        "date",
        "client",
        "project",
        "project code",
        "notes",
        "billable",
        "invoiced",
        "first name",
        "last name",
        "currency",
    ];
    required.extend(match kind {
        ReportKind::Time => vec![
            "task",
            "hours",
            "billable rate",
            "billable amount",
            "cost rate",
            "cost amount",
        ],
        ReportKind::Expense => vec!["category", "amount", "approved"],
    });
    for name in required {
        if !seen.contains(name) {
            errors.push(issue(0, 1, name, "required Harvest column is missing"));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut report = Report {
        kind,
        rows: Vec::new(),
        issues: Vec::new(),
        correction_pairs: Vec::new(),
    };
    let mut record = csv::StringRecord::new();
    loop {
        let present = reader.read_record(&mut record).map_err(|error| {
            vec![issue(
                report.rows.len() + 1,
                error.position().map_or(1, csv::Position::line),
                "record",
                "CSV record has an invalid number of fields",
            )]
        })?;
        if !present {
            break;
        }
        if report.rows.len() == MAX_ROWS {
            return Err(vec![issue(
                MAX_ROWS + 1,
                reader.position().line(),
                "file",
                "CSV exceeds the 10000 row limit",
            )]);
        }
        let start = record.position().expect("CSV record position");
        let end = reader.position();
        let terminated = text
            .as_bytes()
            .get(end.byte().saturating_sub(1) as usize)
            .is_some_and(|byte| matches!(byte, b'\r' | b'\n'));
        let fields = headers
            .iter()
            .cloned()
            .zip(record.iter().map(str::to_owned))
            .collect();
        let mut row = Row {
            record: report.rows.len() + 1,
            line: start.line(),
            end_line: end.line().saturating_sub(u64::from(terminated)),
            fields,
            date: None,
            client: String::new(),
            project: String::new(),
            project_code: String::new(),
            person: String::new(),
            employee_id: String::new(),
            task: String::new(),
            category: String::new(),
            note: String::new(),
            currency: None,
            billable: None,
            invoiced: None,
            approved: None,
            hours_hundredths: None,
            amount_minor: None,
            billable_rate_minor: None,
            billable_amount_minor: None,
            cost_rate_minor: None,
            cost_amount_minor: None,
        };
        normalize_row(kind, &mut row, &mut report.issues);
        report.rows.push(row);
    }
    pair_corrections(&mut report);
    Ok(report)
}

fn value<'a>(row: &'a Row, field: &str) -> &'a str {
    row.fields
        .get(field)
        .map(String::as_str)
        .unwrap_or_default()
}

fn boolean(row: &Row, field: &str, errors: &mut Vec<Issue>) -> Option<bool> {
    match value(row, field).trim().to_ascii_lowercase().as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => {
            errors.push(issue(row.record, row.line, field, "must be Yes or No"));
            None
        }
    }
}

fn number(
    row: &Row,
    field: &str,
    signed: bool,
    grouped: bool,
    errors: &mut Vec<Issue>,
) -> Option<i64> {
    let result = decimal(value(row, field), grouped).filter(|value| signed || *value >= 0);
    if result.is_none() {
        errors.push(issue(
            row.record,
            row.line,
            field,
            "must be an exact decimal with at most two places within range",
        ));
    }
    result
}

fn normalize_row(kind: ReportKind, row: &mut Row, errors: &mut Vec<Issue>) {
    row.date = NaiveDate::parse_from_str(value(row, "date").trim(), "%Y-%m-%d").ok();
    if row.date.is_none() {
        errors.push(issue(
            row.record,
            row.line,
            "date",
            "must be a valid YYYY-MM-DD date",
        ));
    }
    row.client = value(row, "client").trim().into();
    row.project = value(row, "project").trim().into();
    row.project_code = value(row, "project code").trim().into();
    row.person = format!(
        "{} {}",
        value(row, "first name").trim(),
        value(row, "last name").trim()
    )
    .trim()
    .into();
    row.employee_id = value(row, "employee id").trim().into();
    row.task = value(row, "task").trim().into();
    row.category = value(row, "category").trim().into();
    row.note = value(row, "notes").into();
    for (field, text) in [
        ("client", &row.client),
        ("project", &row.project),
        ("person", &row.person),
    ] {
        if text.is_empty() || text.chars().count() > 120 {
            errors.push(issue(
                row.record,
                row.line,
                field,
                "must contain 1 to 120 characters",
            ));
        }
    }
    if row.note.chars().count() > 500 {
        errors.push(issue(
            row.record,
            row.line,
            "notes",
            "at most 500 characters; source text has not been truncated",
        ));
    }
    let currency = value(row, "currency").trim();
    row.currency = Currency::parse(
        currency
            .rsplit_once(" - ")
            .map_or(currency, |(_, code)| code),
    );
    if row.currency.is_none() {
        errors.push(issue(
            row.record,
            row.line,
            "currency",
            "must identify a three-letter currency code",
        ));
    }
    row.billable = boolean(row, "billable", errors);
    row.invoiced = boolean(row, "invoiced", errors);
    match kind {
        ReportKind::Time => {
            if row.task.is_empty() || row.task.chars().count() > 120 {
                errors.push(issue(
                    row.record,
                    row.line,
                    "task",
                    "must contain 1 to 120 characters",
                ));
            }
            row.hours_hundredths = number(row, "hours", true, false, errors);
            if row
                .hours_hundredths
                .is_some_and(|hours| hours == 0 || hours.unsigned_abs() > u64::from(Hours::MAX))
            {
                errors.push(issue(
                    row.record,
                    row.line,
                    "hours",
                    "magnitude must be 0.01 to 24.00 hours",
                ));
            }
            row.billable_rate_minor =
                number(row, "billable rate", false, true, errors).map(|value| value as u64);
            row.cost_rate_minor =
                number(row, "cost rate", false, true, errors).map(|value| value as u64);
            row.billable_amount_minor = number(row, "billable amount", true, true, errors);
            row.cost_amount_minor = number(row, "cost amount", true, true, errors);
            for (field, rate, amount) in [
                (
                    "billable amount",
                    row.billable_rate_minor,
                    row.billable_amount_minor,
                ),
                ("cost amount", row.cost_rate_minor, row.cost_amount_minor),
            ] {
                if let (Some(hours), Some(rate), Some(amount)) =
                    (row.hours_hundredths, rate, amount)
                {
                    let magnitude =
                        (u128::from(hours.unsigned_abs()) * u128::from(rate) + 50) / 100;
                    let expected = i64::try_from(magnitude).ok().and_then(|value| {
                        if hours < 0 {
                            value.checked_neg()
                        } else {
                            Some(value)
                        }
                    });
                    if expected != Some(amount) {
                        errors.push(issue(
                            row.record,
                            row.line,
                            field,
                            "does not reconcile with exported hours and rate",
                        ));
                    }
                }
            }
        }
        ReportKind::Expense => {
            if row.category.is_empty() || row.category.chars().count() > 120 {
                errors.push(issue(
                    row.record,
                    row.line,
                    "category",
                    "must contain 1 to 120 characters",
                ));
            }
            row.amount_minor = number(row, "amount", false, true, errors).map(|value| value as u64);
            row.approved = boolean(row, "approved", errors);
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct CorrectionKey<'a> {
    date: NaiveDate,
    client: &'a str,
    project: &'a str,
    project_code: &'a str,
    person: &'a str,
    employee_id: &'a str,
    task: &'a str,
    currency: &'a str,
    billable: bool,
    invoiced: bool,
    hours: u64,
    billable_rate: u64,
    cost_rate: u64,
    billable_amount: u64,
    cost_amount: u64,
}

fn correction_key(row: &Row) -> Option<CorrectionKey<'_>> {
    Some(CorrectionKey {
        date: row.date?,
        client: &row.client,
        project: &row.project,
        project_code: &row.project_code,
        person: &row.person,
        employee_id: &row.employee_id,
        task: &row.task,
        currency: &row.currency.as_ref()?.0,
        billable: row.billable?,
        invoiced: row.invoiced?,
        hours: row.hours_hundredths?.unsigned_abs(),
        billable_rate: row.billable_rate_minor?,
        cost_rate: row.cost_rate_minor?,
        billable_amount: row.billable_amount_minor?.unsigned_abs(),
        cost_amount: row.cost_amount_minor?.unsigned_abs(),
    })
}

fn pair_corrections(report: &mut Report) {
    let invalid: HashSet<_> = report.issues.iter().map(|issue| issue.record).collect();
    let mut groups: BTreeMap<CorrectionKey<'_>, Vec<&Row>> = BTreeMap::new();
    for row in report
        .rows
        .iter()
        .filter(|row| !invalid.contains(&row.record))
    {
        if let Some(key) = correction_key(row) {
            groups.entry(key).or_default().push(row);
        }
    }
    for rows in groups.values() {
        let positive: Vec<_> = rows
            .iter()
            .filter(|row| row.hours_hundredths.is_some_and(|hours| hours > 0))
            .collect();
        let negative: Vec<_> = rows
            .iter()
            .filter(|row| row.hours_hundredths.is_some_and(|hours| hours < 0))
            .collect();
        if positive.len() == 1 && negative.len() == 1 {
            report.correction_pairs.push(CorrectionPair {
                positive_record: positive[0].record,
                negative_record: negative[0].record,
            });
        } else {
            for row in negative {
                report.issues.push(issue(
                    row.record,
                    row.line,
                    "hours",
                    "negative hours require explicit review; no unique offsetting row",
                ));
            }
        }
    }
    report
        .correction_pairs
        .sort_by_key(|pair| pair.positive_record);
    report
        .issues
        .sort_by(|left, right| (left.record, &left.field).cmp(&(right.record, &right.field)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIME: &str = "Date,Client,Project,Project Code,Task,Notes,Hours,Billable?,Invoiced?,First Name,Last Name,Employee Id,Billable Rate,Billable Amount,Cost Rate,Cost Amount,Currency\n";
    const EXPENSE: &str = "Date,Client,Project,Project Code,Category,Notes,Amount,Units,Billable,Invoiced?,Approved?,First name,Last name,Employee Id,Currency\n";

    #[test]
    fn parses_multiline_time_without_float_conversion_or_date_inference() {
        let csv = format!(
            "{TIME}2025-01-06,Example Ltd,2026,FAPTIC,Internal,\"A comma, and\nsecond line\",0.75,Yes,No,Ada,Example,,65.0,48.75,60.0,45.0,United States Dollar - USD\n"
        );
        let report = parse(ReportKind::Time, csv.as_bytes()).unwrap();
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        let row = &report.rows[0];
        assert_eq!((row.line, row.end_line), (2, 3));
        assert_eq!(row.hours_hundredths, Some(75));
        assert_eq!(row.billable_rate_minor, Some(6500));
        assert_eq!(row.billable_amount_minor, Some(4875));
        assert_eq!(row.date.unwrap().to_string(), "2025-01-06");
        assert_eq!(row.project, "2026");
        assert_eq!(row.note, "A comma, and\nsecond line");
        assert_eq!(row.currency.as_ref().unwrap().0, "USD");
    }

    #[test]
    fn parses_expense_grouping_and_retains_units_and_approval() {
        let csv = format!(
            "\u{feff}{EXPENSE}2026-03-31,Example Ltd,2026,FAPTIC,Other,Bonus,\"16,200.0\",,Yes,Yes,No,Ada,Example,,Euro - EUR\r\n"
        );
        let report = parse(ReportKind::Expense, csv.as_bytes()).unwrap();
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(report.rows[0].amount_minor, Some(1_620_000));
        assert_eq!(report.rows[0].approved, Some(false));
        assert_eq!(report.rows[0].invoiced, Some(true));
        assert_eq!(report.rows[0].fields["units"], "");
    }

    #[test]
    fn identifies_only_unambiguous_corrections_and_retains_both_source_rows() {
        let positive = "2025-09-03,Example Ltd,2026,FAPTIC,Support,Original,5.0,No,Yes,Ada,Example,,0.0,0.0,60.0,300.0,Euro - EUR\n";
        let negative = "2025-09-03,Example Ltd,2026,FAPTIC,Support,Correction,-5.0,No,Yes,Ada,Example,,0.0,-0.0,60.0,-300.0,Euro - EUR\n";
        let report = parse(
            ReportKind::Time,
            format!("{TIME}{positive}{negative}").as_bytes(),
        )
        .unwrap();
        assert!(report.issues.is_empty());
        assert_eq!(report.rows.len(), 2);
        assert_eq!(
            report.correction_pairs,
            [CorrectionPair {
                positive_record: 1,
                negative_record: 2
            }]
        );
        let report = parse(
            ReportKind::Time,
            format!("{TIME}{positive}{positive}{negative}").as_bytes(),
        )
        .unwrap();
        assert!(report.correction_pairs.is_empty());
        assert_eq!(report.issues[0].record, 3);
        assert_eq!(report.issues[0].field, "hours");
    }

    #[test]
    fn rejects_bad_headers_and_malformed_csv() {
        for text in [
            "",
            "Date,Date\n",
            "Date,Client\n",
            "\"unterminated",
            "a\"b,c\n",
            "\"a\"b,c\n",
        ] {
            assert!(
                parse(ReportKind::Time, text.as_bytes()).is_err(),
                "{text:?}"
            );
        }
        assert!(parse(ReportKind::Expense, &[0xff]).is_err());
        assert!(parse(ReportKind::Time, "a\0b".as_bytes()).is_err());
    }

    #[test]
    fn decimal_parser_rejects_ambiguous_grouping_precision_and_overflow() {
        for text in [
            "",
            "1,20.00",
            "12,34,567",
            "1.234",
            "NaN",
            "inf",
            "1e3",
            "1.",
            ".5",
            "+5",
            "92233720368547759",
        ] {
            assert_eq!(decimal(text, true), None, "{text}");
        }
        assert_eq!(decimal("16,200.0", true), Some(1_620_000));
        assert_eq!(decimal("2.3", false), Some(230));
        assert_eq!(decimal("-0.0", true), Some(0));
        assert_eq!(decimal("1,000", false), None);
    }

    #[test]
    fn reports_row_errors_without_dropping_or_truncating_source_values() {
        let long_note = "x".repeat(501);
        let csv = format!(
            "{TIME}2025-02-30,Example Ltd,2026,FAPTIC,Internal,{long_note},25.0,Maybe,No,Ada,Example,,60.0,1.0,60.0,1500.0,EUR\n"
        );
        let report = parse(ReportKind::Time, csv.as_bytes()).unwrap();
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].note.len(), 501);
        for field in ["date", "notes", "hours", "billable", "billable amount"] {
            assert!(
                report
                    .issues
                    .iter()
                    .any(|error| error.field == field && error.record == 1 && error.line == 2),
                "{field}"
            );
        }
    }

    #[test]
    fn applies_limits_before_returning_a_report() {
        assert!(parse(ReportKind::Time, &vec![b'x'; MAX_FILE_BYTES + 1]).is_err());
        let row = "2026-01-01,C,P,,Other,,1,,No,No,No,A,B,,EUR\n";
        let full = format!("{EXPENSE}{}", row.repeat(MAX_ROWS));
        assert_eq!(
            parse(ReportKind::Expense, full.as_bytes())
                .unwrap()
                .rows
                .len(),
            MAX_ROWS
        );
        let oversized = format!("{full}{row}");
        let error = parse(ReportKind::Expense, oversized.as_bytes()).unwrap_err();
        assert_eq!(error[0].record, MAX_ROWS + 1);
        assert!(
            parse(
                ReportKind::Expense,
                format!("{}\n", "column,".repeat(65)).as_bytes()
            )
            .is_err()
        );
        assert!(
            parse(
                ReportKind::Expense,
                format!("{}\n", "x".repeat(81)).as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn accepts_header_variations_and_retains_unknown_metadata() {
        let text = EXPENSE
            .replace("First name", " FIRST NAME ")
            .replace("Currency\n", "Currency,Extra\n");
        let csv = format!(
            "{text}2026-01-01,C,P,,Other,\"Quote: \"\"hello\"\"\",59.0,2,Yes,No,No,A,B,,EUR,metadata"
        );
        let report = parse(ReportKind::Expense, csv.as_bytes()).unwrap();
        assert!(report.issues.is_empty());
        assert_eq!(report.rows[0].person, "A B");
        assert_eq!(report.rows[0].note, "Quote: \"hello\"");
        assert_eq!(report.rows[0].fields["extra"], "metadata");
        assert_eq!(report.rows[0].fields["units"], "2");
    }

    #[test]
    fn correction_matching_respects_identity_and_rejects_duplicate_negatives() {
        let positive = "2025-09-03,C,P,CODE,Support,,5.0,No,Yes,A,B,one,0,0,60,300,EUR\n";
        let negative = "2025-09-03,C,P,CODE,Support,,-5.0,No,Yes,A,B,two,0,-0,60,-300,EUR\n";
        let report = parse(
            ReportKind::Time,
            format!("{TIME}{positive}{negative}").as_bytes(),
        )
        .unwrap();
        assert!(report.correction_pairs.is_empty());
        assert_eq!(report.issues.len(), 1);
        let negative = negative.replace(",two,", ",one,");
        let report = parse(
            ReportKind::Time,
            format!("{TIME}{positive}{negative}{negative}").as_bytes(),
        )
        .unwrap();
        assert!(report.correction_pairs.is_empty());
        assert_eq!(report.issues.len(), 2);
    }

    #[test]
    fn malformed_record_width_rejects_the_entire_file() {
        let text =
            format!("{EXPENSE}2026-01-01,C,P,,Other,,1,,No,No,No,A,B,,EUR\nmissing,fields\n");
        let errors = parse(ReportKind::Expense, text.as_bytes()).unwrap_err();
        assert_eq!(errors[0].record, 2);
        assert_eq!(errors[0].line, 3);
    }

    #[test]
    #[ignore = "requires private exports supplied through HARVEST_TIME_CSV and HARVEST_EXPENSE_CSV"]
    fn private_exports_are_read_only_and_reconcile() {
        for (kind, variable) in [
            (ReportKind::Time, "HARVEST_TIME_CSV"),
            (ReportKind::Expense, "HARVEST_EXPENSE_CSV"),
        ] {
            let path =
                std::env::var(variable).expect("set the private export environment variable");
            let bytes = std::fs::read(path).expect("read private export");
            let report = parse(kind, &bytes).expect("valid Harvest CSV structure");
            eprintln!(
                "{kind:?}: {} records, {} correction pairs, {} issues",
                report.rows.len(),
                report.correction_pairs.len(),
                report.issues.len()
            );
            assert!(report.issues.is_empty(), "{:?}", report.issues);
            assert!(!report.rows.is_empty());
        }
    }
}
