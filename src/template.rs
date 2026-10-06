//! Invoice document templates (#116): `%variable%` interpolation plus a
//! deliberately tiny markdown subset.
//!
//! Pure module — no I/O, no clock beyond dates passed in, no randomness.
//! The markdown parses **once** into the `pdf::Block` model (#113's seam)
//! and two emitters consume it: `pdf::render_invoice_pdf` lays the blocks
//! out into the document, `to_html` renders them for `EmailMessage.html`.
//! HTML output escapes all input first — the GUI's "textContent, never
//! innerHTML" rule has no reason to be weaker server-side. Tables, links,
//! images and raw HTML are out of the subset on purpose: anything else is
//! literal text.
//!
//! Variable resolution is **fail-loud at save time** (`validate_*` rejects
//! unknown `%tokens%` with the standard 422 `FieldError` shape, so typos
//! never persist) and **fail-safe at render time** (an unknown token that
//! somehow reaches the renderer is left literal and logged — it never
//! panics and never guesses).
//!
//! Money comes from persisted minor units through `api::money_for_email`;
//! month names come from a static table. Floats and locale crates stay out.

use std::collections::BTreeMap;

use chrono::Datelike;

use crate::domain::{Currency, FieldError, Invoice};
use crate::pdf::{Block, Doc, Span};

/// The closed set of `%variable%` names. Case-sensitive by contract.
pub const VARS: &[&str] = &[
    "invoice_number",
    "invoice_issue_day",
    "invoice_issue_month",
    "invoice_issue_month_number",
    "invoice_issue_year",
    "invoice_due_date",
    "period_from",
    "period_to",
    "customer_name",
    "currency",
    "total",
    "line_count",
];

/// Cheatsheet rows (name, example) — the GUI renders this table, so the
/// documentation and the validation share one source of truth.
pub const VAR_HELP: &[(&str, &str)] = &[
    ("%invoice_number%", "INV-0007"),
    ("%invoice_issue_day%", "5"),
    ("%invoice_issue_month%", "October"),
    ("%invoice_issue_month_number%", "10"),
    ("%invoice_issue_year%", "2026"),
    ("%invoice_due_date%", "2026-11-04"),
    ("%period_from%", "2026-10-01"),
    ("%period_to%", "2026-10-31"),
    ("%customer_name%", "Capybara Solutions"),
    ("%currency%", "EUR"),
    ("%total%", "1,200.00"),
    ("%line_count%", "14"),
];

/// Caps (validated at save; #50 bounds on free text).
pub const SUBJECT_MAX: usize = 120;
pub const BODY_MAX: usize = 8000;
pub const FOOTER_MAX: usize = 8000;
pub const NOTES_MAX: usize = 4000;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn is_token_char(c: char) -> bool {
    // Upper case is a *candidate* too: tokens are case-sensitive, so
    // `%Total%` reads as an unknown variable (rejected at save, left literal
    // at render) instead of vanishing like stray prose percents.
    c.is_ascii_alphanumeric() || c == '_'
}

/// Scan `%token%` runs. A `%...%` is a *token candidate* only when its body
/// is non-empty and made solely of `[A-Za-z0-9_]` — stray percent signs in
/// ordinary prose (e.g. "100% paid") never form a token.
/// At `chars[i]`, recognise a `%token%`: returns the token and the number of
/// chars consumed, or `None` when this is not a token candidate.
fn token_at(chars: &[char], i: usize) -> Option<(String, usize)> {
    if chars.get(i) != Some(&'%') {
        return None;
    }
    let end = chars[i + 1..].iter().position(|&c| c == '%')?;
    let body: String = chars[i + 1..i + 1 + end].iter().collect();
    if body.is_empty() || !body.chars().all(is_token_char) {
        return None;
    }
    Some((body, end + 2))
}

fn candidates(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some((token, len)) = token_at(&chars, i) {
            out.push(token);
            i += len;
        } else {
            i += 1;
        }
    }
    out
}

/// Tokens used in `text` that are not in the closed set — the save-time
/// rejection list.
pub fn unknown_tokens(text: &str) -> Vec<String> {
    candidates(text)
        .into_iter()
        .filter(|t| !VARS.contains(&t.as_str()))
        .collect()
}

fn token_error(field: &str, token: &str) -> FieldError {
    FieldError::new(
        field,
        format!("unknown variable %{token}% (see the documented %variable% list)"),
    )
}

/// Validate one free-text template field: size cap + closed variable set.
/// Appends `FieldError`s to `errors`; renders nothing.
pub fn validate_field(field: &str, text: &str, max: usize, errors: &mut Vec<FieldError>) {
    if text.chars().count() > max {
        errors.push(FieldError::new(
            field,
            format!("too long (max {max} characters)"),
        ));
    }
    for token in unknown_tokens(text) {
        errors.push(token_error(field, &token));
    }
}

/// The resolved values every `%variable%` reads from. Built by `vars_for`.
pub type Vars = BTreeMap<String, String>;

/// Resolve the variable table for one (invoice, customer) pair. Dates are
/// passed by the caller (the clock lives in the API layer, not here).
pub fn vars_for(
    invoice: &Invoice,
    customer_name: &str,
    issue_day: Option<chrono::NaiveDate>,
) -> Vars {
    let mut v = Vars::new();
    v.insert("invoice_number".into(), invoice.number.clone());
    if let Some(d) = issue_day {
        v.insert("invoice_issue_day".into(), d.day().to_string());
        v.insert(
            "invoice_issue_month".into(),
            MONTHS[d.month0() as usize].into(),
        );
        v.insert("invoice_issue_month_number".into(), d.month().to_string());
        v.insert("invoice_issue_year".into(), d.year().to_string());
    }
    v.insert(
        "invoice_due_date".into(),
        invoice.due_date.map(|d| d.to_string()).unwrap_or_default(),
    );
    v.insert("period_from".into(), invoice.period_from.to_string());
    v.insert("period_to".into(), invoice.period_to.to_string());
    v.insert("customer_name".into(), customer_name.to_string());
    v.insert("currency".into(), invoice.currency.0.clone());
    v.insert(
        "total".into(),
        crate::api::money_for_email(invoice.total_minor, &invoice.currency.0),
    );
    v.insert("line_count".into(), invoice.lines.len().to_string());
    v
}

/// Replace known `%tokens%` with their values. Unknown token candidates are
/// left **literal** with a warning (fail-safe: never panics, never guesses).
#[must_use]
pub fn interpolate(text: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if let Some((token, len)) = token_at(&chars, i) {
            match vars.get(&token) {
                Some(value) => {
                    out.push_str(value);
                    i += len;
                    continue;
                }
                None => {
                    tracing::warn!(token = %token, "template met an unknown variable, leaving it literal");
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------- markdown -----

/// Parse the markdown subset into blocks. Line structure first, inline
/// styling after. Unterminated `**`/`*` markers degrade to literal text.
#[must_use]
pub fn parse_markdown(src: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let flush = |para: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(spans(&para.join("\n"))));
            para.clear();
        }
    };
    for line in src.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            flush(&mut para, &mut blocks);
            continue;
        }
        if let Some(rest) = strip_heading(t) {
            flush(&mut para, &mut blocks);
            let level = t.chars().take_while(|c| *c == '#').count() as u8;
            blocks.push(Block::Heading {
                level,
                spans: spans(rest),
            });
        } else if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "))
            && !rest.trim().is_empty()
        {
            // A bullet right after prose text starts a fresh list block.
            flush(&mut para, &mut blocks);
            blocks.push(Block::Bullet(spans(rest.trim())));
        } else if let Some((number, rest)) = strip_numbered(t) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Numbered {
                number,
                spans: spans(rest.trim()),
            });
        } else {
            para.push(t.to_string());
        }
    }
    flush(&mut para, &mut blocks);
    blocks
}

fn strip_heading(t: &str) -> Option<&str> {
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=3).contains(&hashes) {
        let rest = &t[hashes..];
        // A heading marker must be followed by a space to count.
        return rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t'));
    }
    None
}

fn strip_numbered(t: &str) -> Option<(u32, &str)> {
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &t[digits.len()..];
    let n: u32 = digits.parse().ok()?;
    rest.strip_prefix(". ").map(|r| (n, r))
}

/// Inline spans: `**bold**`, `*italic*`. No nesting (out of the subset);
/// unmatched markers stay literal.
#[must_use]
pub fn spans(text: &str) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let chars = text;
    let push_plain = |plain: &mut String, out: &mut Vec<Span>| {
        if !plain.is_empty() {
            out.push(Span::text(std::mem::take(plain)));
        }
    };
    while i < chars.len() {
        if bytes[i..].starts_with(b"**")
            && let Some(end) = chars[i + 2..].find("**")
        {
            push_plain(&mut plain, &mut out);
            out.push(Span::bold(&chars[i + 2..i + 2 + end]));
            i += end + 4;
            continue;
        }
        if bytes[i] == b'*'
            && let Some(end) = chars[i + 1..].find('*')
        {
            let inner = &chars[i + 1..i + 1 + end];
            if !inner.is_empty() && !inner.contains('*') {
                push_plain(&mut plain, &mut out);
                out.push(Span::italic(inner));
                i += end + 2;
                continue;
            }
        }
        // Copy one char (byte-wise is wrong for multibyte; find the next boundary).
        let ch = chars[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    push_plain(&mut plain, &mut out);
    if out.is_empty() {
        out.push(Span::text(String::new()));
    }
    out
}

// ------------------------------------------------------------- html ------

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn spans_html(spans: &[Span]) -> String {
    let mut out = String::new();
    for s in spans {
        let esc = escape(&s.text).replace('\n', "<br>");
        match (s.bold, s.italic) {
            (true, true) => out.push_str(&format!("<strong><em>{esc}</em></strong>")),
            (true, false) => out.push_str(&format!("<strong>{esc}</strong>")),
            (false, true) => out.push_str(&format!("<em>{esc}</em>")),
            (false, false) => out.push_str(&esc),
        }
    }
    out
}

/// Render blocks to the HTML email part. Input was escaped before wrapping,
/// so nothing a customer typed can inject markup.
#[must_use]
pub fn to_html(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            Block::Heading { level, spans } => {
                let l = (*level).clamp(1, 3);
                out.push_str(&format!("<h{l}>{}</h{l}>", spans_html(spans)));
                i += 1;
            }
            Block::Paragraph(spans) => {
                out.push_str(&format!("<p>{}</p>", spans_html(spans)));
                i += 1;
            }
            Block::KeyVal { key, spans } => {
                out.push_str(&format!(
                    "<p><strong>{}:</strong> {}</p>",
                    escape(key),
                    spans_html(spans)
                ));
                i += 1;
            }
            Block::Bullet(_) => {
                out.push_str("<ul>");
                while let Some(Block::Bullet(s)) = blocks.get(i) {
                    out.push_str(&format!("<li>{}</li>", spans_html(s)));
                    i += 1;
                }
                out.push_str("</ul>");
            }
            Block::Numbered { .. } => {
                out.push_str("<ol>");
                while let Some(Block::Numbered { number: _, spans }) = blocks.get(i) {
                    out.push_str(&format!("<li>{}</li>", spans_html(spans)));
                    i += 1;
                }
                out.push_str("</ol>");
            }
        }
    }
    out
}

// ------------------------------------------------------- document glue ---

/// Compose the content parts of the #116 document — org body, org footer,
/// customer notes — from the template and customer, interpolated against the
/// invoice. Each piece is `""`/unset when absent, and empty pieces produce
/// empty vecs, keeping the caller's legacy behaviour untouched.
#[must_use]
pub fn doc_content(
    template: &crate::domain::InvoiceTemplate,
    customer: &crate::domain::Customer,
    vars: &Vars,
) -> DocContent {
    let md = |src: &str| parse_markdown(&interpolate(src, vars));
    DocContent {
        body: md(&template.body),
        footer: md(&template.footer),
        notes: md(&customer.invoice_notes),
        subject: resolve_subject(template, customer, vars),
    }
}

/// Resolve the subject: customer override → org template → `None` (the
/// caller keeps its legacy subject).
fn resolve_subject(
    template: &crate::domain::InvoiceTemplate,
    customer: &crate::domain::Customer,
    vars: &Vars,
) -> Option<String> {
    let customer_subj = customer.invoice_subject.trim();
    if !customer_subj.is_empty() {
        return Some(interpolate(customer_subj, vars));
    }
    let org_subj = template.subject.trim();
    if !org_subj.is_empty() {
        return Some(interpolate(org_subj, vars));
    }
    None
}

pub struct DocContent {
    pub body: Vec<Block>,
    pub footer: Vec<Block>,
    pub notes: Vec<Block>,
    pub subject: Option<String>,
}

/// Merge template content into a `Doc` built by `pdf::doc_for`: body blocks
/// flow before the line table; the footer becomes generated-line → org
/// footer → customer notes. With no template and no notes the doc is
/// unchanged, which is what keeps unset templates byte-identical.
pub fn apply_doc_content(doc: &mut Doc, content: &DocContent) {
    if !content.body.is_empty() {
        let mut blocks = content.body.clone();
        blocks.append(&mut doc.blocks);
        doc.blocks = blocks;
    }
    let mut footer = content.footer.clone();
    footer.append(&mut doc.footer.clone());
    doc.footer = footer;
    if !content.notes.is_empty() {
        doc.footer.extend_from_slice(&content.notes);
    }
}

/// The email HTML letter: body, then footer, then notes. `None` when there
/// is no configured content (legacy: text-only emails).
#[must_use]
pub fn email_html(content: &DocContent) -> Option<String> {
    if content.body.is_empty() && content.footer.is_empty() && content.notes.is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    blocks.extend(content.body.iter().cloned());
    blocks.extend(content.footer.iter().cloned());
    blocks.extend(content.notes.iter().cloned());
    Some(to_html(&blocks))
}

/// Format helper so callers never rebuild this pair.
pub fn currency_minor(minor: u64, currency: &Currency) -> String {
    crate::api::money_for_email(minor, &currency.0)
}

// =========================================================== unit tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sample_invoice() -> Invoice {
        use crate::domain::{Currency, InvoiceStatus};
        Invoice {
            id: uuid::Uuid::new_v4(),
            number: "INV-0007".into(),
            customer_id: uuid::Uuid::new_v4(),
            currency: Currency("EUR".into()),
            period_from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
            lines: vec![],
            total_minor: 120000,
            status: InvoiceStatus::Draft,
            created_at: NaiveDate::from_ymd_opt(2026, 10, 5)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap()
                .and_utc(),
            issued_at: None,
            due_date: Some(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap()),
            paid_at: None,
            payment_reference: String::new(),
            pdf: None,
            payments: vec![],
            write_off_reason: String::new(),
            written_off_at: None,
        }
    }

    fn vars() -> Vars {
        vars_for(
            &sample_invoice(),
            "Capybara Solutions",
            Some(NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()),
        )
    }

    #[test]
    fn variable_table_resolves_documented_examples() {
        let issue = Some(NaiveDate::from_ymd_opt(2026, 10, 5).unwrap());
        let inv = sample_invoice();
        let built = vars_for(&inv, "Capybara Solutions", issue);
        assert_eq!(built["invoice_number"], "INV-0007");
        assert_eq!(built["invoice_issue_day"], "5");
        assert_eq!(built["invoice_issue_month"], "October");
        assert_eq!(built["invoice_issue_month_number"], "10");
        assert_eq!(built["invoice_issue_year"], "2026");
        assert_eq!(built["invoice_due_date"], "2026-11-04");
        assert_eq!(built["period_from"], "2026-10-01");
        assert_eq!(built["period_to"], "2026-10-31");
        assert_eq!(built["customer_name"], "Capybara Solutions");
        assert_eq!(built["currency"], "EUR");
        assert_eq!(built["total"], "1,200.00 EUR");
        assert_eq!(built["line_count"], "0");
    }

    #[test]
    fn interpolation_replaces_known_tokens() {
        let v = vars();
        let s = interpolate(
            "Invoice %invoice_number% for %customer_name%: %total% due %invoice_due_date%",
            &v,
        );
        assert_eq!(
            s,
            "Invoice INV-0007 for Capybara Solutions: 1,200.00 EUR due 2026-11-04"
        );
    }

    #[test]
    fn unknown_tokens_left_literal_at_render_and_flagged_at_save() {
        let v = vars();
        assert_eq!(interpolate("Pay %bogus% now", &v), "Pay %bogus% now");
        assert_eq!(unknown_tokens("Pay %bogus% and %total%"), vec!["bogus"]);
        let mut errors = Vec::new();
        validate_field("body", "Pay %bogus%", BODY_MAX, &mut errors);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "body");
        // Case-sensitive: %Total% is not a token candidate spelling we know.
        assert_eq!(unknown_tokens("%Total%"), vec!["Total"]);
    }

    #[test]
    fn stray_percent_never_forms_a_token() {
        assert!(candidates("100% paid, 50% deposit").is_empty());
        assert_eq!(interpolate("100% done", &vars()), "100% done");
    }

    #[test]
    fn markdown_subset_parses_blocks() {
        let md = "# Title\nIntro text\n\n- one\n- two\n\n1. first\n2. second\n\n### Small **bold** and *it*";
        let blocks = parse_markdown(md);
        assert!(matches!(&blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(&blocks[1], Block::Paragraph(_)));
        assert!(matches!(&blocks[2], Block::Bullet(_)));
        assert!(matches!(&blocks[3], Block::Bullet(_)));
        assert!(matches!(&blocks[4], Block::Numbered { number: 1, .. }));
        assert!(matches!(&blocks[5], Block::Numbered { number: 2, .. }));
        match &blocks[6] {
            Block::Heading { level: 3, spans } => {
                assert_eq!(spans[0].text, "Small ");
                assert!(spans[1].bold);
                assert_eq!(spans[2].text, " and ");
                assert!(spans[3].italic);
            }
            other => panic!("expected heading, got {other:?}"),
        }
    }

    #[test]
    fn markdown_escapes_html_and_blocks_unsupported_syntax() {
        let html = to_html(&parse_markdown(
            "<script>alert(1)</script> and |a|b| tables [x](y)",
        ));
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        // Tables/links/images are literal text, never markup.
        assert!(html.contains("[x](y)"), "{html}");
    }

    #[test]
    fn unmatched_style_markers_stay_literal() {
        let s = interpolate("50% off **today", &Vars::new());
        assert_eq!(s, "50% off **today");
        let blocks = parse_markdown("a ** b");
        match &blocks[0] {
            Block::Paragraph(spans) => assert!(spans.iter().all(|sp| !sp.bold)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn html_wraps_lists_and_breaks_lines() {
        let html = to_html(&parse_markdown("- a\n- b"));
        assert!(html.starts_with("<ul><li>a</li><li>b</li></ul>"), "{html}");
        let p = to_html(&parse_markdown("line one\nline two"));
        assert!(p.contains("line one<br>line two"), "{p}");
    }

    #[test]
    fn subject_precedence_customer_then_org_then_none() {
        use crate::domain::{Currency, Customer, InvoiceTemplate};
        let customer = Customer {
            id: uuid::Uuid::new_v4(),
            name: "C".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 1,
            active: true,
            email: String::new(),
            payment_terms: None,
            invoice_notes: String::new(),
            invoice_subject: "%invoice_number% for %customer_name%".into(),
            address: None,
            contacts: vec![],
            tax_hundredths: 0,
            discount_hundredths: 0,
        };
        let template = InvoiceTemplate {
            subject: "Invoice %invoice_number%".into(),
            ..Default::default()
        };
        let v = vars();
        assert_eq!(
            resolve_subject(&template, &customer, &v).as_deref(),
            Some("INV-0007 for Capybara Solutions")
        );
        let bare = Customer {
            invoice_subject: String::new(),
            ..customer
        };
        assert_eq!(
            resolve_subject(&template, &bare, &v).as_deref(),
            Some("Invoice INV-0007")
        );
        let empty = crate::domain::InvoiceTemplate::default();
        assert_eq!(resolve_subject(&empty, &bare, &v), None);
    }

    #[test]
    fn unset_template_leaves_the_doc_untouched() {
        use crate::domain::{Currency, Customer, InvoiceTemplate};
        let customer = Customer {
            id: uuid::Uuid::new_v4(),
            name: "C".into(),
            currency: Currency("EUR".into()),
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
        let template = InvoiceTemplate::default();
        let inv = sample_invoice();
        let mut doc = crate::pdf::doc_for(&inv, &customer, &crate::pdf::Org::named("Tucano"));
        let before = doc.clone();
        let content = doc_content(&template, &customer, &vars_for(&inv, &customer.name, None));
        apply_doc_content(&mut doc, &content);
        assert_eq!(doc.blocks, before.blocks);
        assert_eq!(doc.footer, before.footer);
        assert!(email_html(&content).is_none(), "unset template: text-only");
    }

    #[test]
    fn template_content_flows_into_the_doc_and_email() {
        use crate::domain::{Currency, Customer, InvoiceTemplate};
        let customer = Customer {
            id: uuid::Uuid::new_v4(),
            name: "Capybara Solutions".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 1,
            active: true,
            email: String::new(),
            payment_terms: None,
            invoice_notes: "Thanks for the swift payment!".into(),
            invoice_subject: String::new(),
            address: None,
            contacts: vec![],
            tax_hundredths: 0,
            discount_hundredths: 0,
        };
        let template = InvoiceTemplate {
            body: "## Work for %invoice_issue_month%\n- consultancy".into(),
            footer: "Bank: IBAN XX01".into(),
            ..Default::default()
        };
        let inv = sample_invoice();
        let mut doc = crate::pdf::doc_for(&inv, &customer, &crate::pdf::Org::named("Tucano"));
        let content = doc_content(
            &template,
            &customer,
            &vars_for(
                &inv,
                &customer.name,
                Some(NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()),
            ),
        );
        apply_doc_content(&mut doc, &content);
        assert!(
            matches!(&doc.blocks[0], Block::Heading { level: 2, spans } if spans[0].text == "Work for October")
        );
        // Generated line stays, org footer precedes notes.
        let last = doc.footer.last().unwrap();
        match last {
            Block::Paragraph(spans) => assert!(spans[0].text.contains("swift payment")),
            other => panic!("{other:?}"),
        }
        let html = email_html(&content).unwrap();
        assert!(html.contains("<h2>Work for October</h2>"), "{html}");
        assert!(html.contains("Bank: IBAN XX01"), "{html}");
        assert!(html.contains("swift payment"), "{html}");
    }
}
