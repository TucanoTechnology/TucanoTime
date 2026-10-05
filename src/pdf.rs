//! Deterministic invoice PDF rendering (#113).
//!
//! Architecture (ADR on #113, "route B"): `pdf-writer` (the Typst emitter)
//! guarantees valid PDF *syntax*; the invoice *layout* is ours. Content is a
//! block document model — `Doc` — built by the pure `doc_for(&Invoice,
//! &Customer, &Org)` from the persisted snapshot; `render_invoice_pdf(&Doc)`
//! only lays out and emits bytes. #116 (templates) replaces `doc_for`'s
//! hard-coded strings with `%variable%`-interpolated markdown, feeding the
//! **same** `Doc` — the emitter never sees HTML and never needs to change.
//!
//! Determinism contract: `render_invoice_pdf` is pure — no I/O, no clock, no
//! randomness. The same `Doc` in produces byte-identical output, which is
//! what makes the archived `invoices/<id>.pdf` verifiable (`sha256` hint,
//! `ETag`) and lets a legacy invoice re-render to the exact same bytes on
//! first download. There is deliberately no `/Info` dictionary and no file
//! ID, because those would carry timestamps.
//!
//! Money discipline: amounts are formatted from persisted minor units via
//! `api::money_for_email` (integer division), and the totals section prints
//! `invoice.total_minor` directly — layout arithmetic never re-sums printed
//! line amounts, and floats never enter the money path. Layout coordinates
//! use integer point arithmetic; f32 appears only at the pdf-writer boundary.
//!
//! Fonts: base-14 Type1 (`Helvetica` family) with `WinAnsiEncoding` — no
//! font asset, no parser. Scope line: Latin text only; characters outside
//! WinAnsi render as `?` (the agreed trigger to revisit is a non-Latin-1
//! customer name).

use chrono::{DateTime, Utc};

use crate::domain::{Currency, Customer, Invoice, InvoiceLine, LineKind};

// =============================================================== doc model ==

/// The issuing organisation. `doc_for` is the only place that reads it, and
/// the API layer resolves it from `config.json` (`org_name`, #94).
#[derive(Debug, Clone, PartialEq)]
pub struct Org {
    pub name: String,
}

/// Inline text with style flags. #116's markdown `**`/`*` map 1:1 onto these
/// via the base-14 `Helvetica-Bold` / `Helvetica-Oblique` faces.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

impl Span {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: false,
            italic: false,
        }
    }
    pub fn bold(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: true,
            italic: false,
        }
    }
    pub fn italic(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: false,
            italic: true,
        }
    }
}

/// A block of flowing content. Deliberately the same vocabulary #116's
/// markdown subset renders to; tables/links/images are out of scope.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    Bullet(Vec<Span>),
    Numbered {
        number: u32,
        spans: Vec<Span>,
    },
    /// A `label: value` line (header fields, totals).
    KeyVal {
        key: String,
        spans: Vec<Span>,
    },
}

/// One row of the line-item table. Every string is pre-formatted; the
/// renderer never recomputes amounts.
#[derive(Debug, Clone, PartialEq)]
pub struct LineRow {
    pub date: String,
    pub description: String,
    /// `""` when the line has no hours (expense/fixed).
    pub hours: String,
    pub rate: String,
    pub amount: String,
}

/// The invoice as a document — the #113/#116 seam. `header`/`footer` frame
/// the page, `blocks` is free content (customer notes from #116), `lines` is
/// the table, `totals` the money summary (always present, printed from
/// `total_minor`).
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub title: String,
    pub header: Vec<Block>,
    pub blocks: Vec<Block>,
    pub lines: Vec<LineRow>,
    pub totals: Vec<Block>,
    pub footer: Vec<Block>,
}

/// The date a document was issued, for display: the issue timestamp when set,
/// otherwise the creation timestamp.
fn issue_date(created_at: DateTime<Utc>, issued_at: Option<DateTime<Utc>>) -> String {
    (issued_at.unwrap_or(created_at)).date_naive().to_string()
}

/// A plain-text (no styles) key/val block.
fn kv(key: impl Into<String>, value: impl Into<String>) -> Block {
    Block::KeyVal {
        key: key.into(),
        spans: vec![Span::text(value.into())],
    }
}

/// The one place invoice strings get formatted (#116 replaces *content*
/// here, never layout). Prints `due_date` **as stored** — the terms
/// resolution order belongs to #116, and an issued invoice is never
/// recomputed.
#[must_use]
pub fn doc_for(invoice: &Invoice, customer: &Customer, org: &Org) -> Doc {
    let money = |minor: u64| {
        crate::api::money_for_email(minor, "")
            .trim_end()
            .to_string()
    };
    let mut header = vec![
        kv("From", org.name.clone()),
        kv("Bill to", customer.name.clone()),
        kv("Invoice number", invoice.number.clone()),
        kv(
            "Issue date",
            issue_date(invoice.created_at, invoice.issued_at),
        ),
        kv(
            "Period",
            format!("{} to {}", invoice.period_from, invoice.period_to),
        ),
    ];
    if let Some(due) = invoice.due_date {
        header.push(kv("Due date", due.to_string()));
    }
    let lines = invoice.lines.iter().map(|l| line_row(l, &money)).collect();
    let totals = vec![Block::KeyVal {
        key: "Total".into(),
        spans: vec![Span::bold(money_for_email_total(
            invoice.total_minor,
            &invoice.currency,
        ))],
    }];
    Doc {
        title: format!("Invoice {}", invoice.number),
        header,
        blocks: vec![],
        lines,
        totals,
        footer: vec![Block::Paragraph(vec![Span::italic(format!(
            "Generated by {} on TucanoTime.",
            org.name
        ))])],
    }
}

/// Amount formatting shared with the email surface — minor units in, grouped
/// decimal out, no floats (`money_for_email` with the currency stripped back
/// off, used for table columns).
fn money_for_email_total(minor: u64, currency: &Currency) -> String {
    crate::api::money_for_email(minor, &currency.0)
}

fn line_row(line: &InvoiceLine, money: &impl Fn(u64) -> String) -> LineRow {
    let mut desc: Vec<String> = Vec::new();
    if let Some(code) = &line.project_code {
        desc.push(code.0.clone());
    }
    if let Some(task) = &line.task_code {
        desc.push(task.0.clone());
    }
    if !line.note.trim().is_empty() {
        desc.push(line.note.trim().to_string());
    }
    if desc.is_empty() {
        desc.push(
            match line.kind {
                LineKind::Time => "Time",
                LineKind::Expense => "Expense",
                LineKind::Fixed => "Fixed charge",
            }
            .to_string(),
        );
    }
    let (hours, rate) = match line.kind {
        LineKind::Time => (
            line.hours.map(|h| hours_text(h.0)).unwrap_or_default(),
            line.rate_minor.map(money).unwrap_or_else(|| "-".into()),
        ),
        _ => (String::new(), "-".into()),
    };
    LineRow {
        date: line.date.to_string(),
        description: desc.join(" - "),
        hours,
        rate,
        amount: money(line.amount_minor),
    }
}

/// Hundredths to a fixed two-decimal string ("8.50"), integer arithmetic.
fn hours_text(hundredths: u32) -> String {
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

// ============================================================== renderer ===

// A4 at 72 dpi, integer points.
const PAGE_W: i32 = 595;
const PAGE_H: i32 = 842;
const MARGIN: i32 = 50;
const CONTENT_W: i32 = PAGE_W - 2 * MARGIN;
/// Content stops here; below is footer whitespace, above triggers a page break.
const BOTTOM: i32 = MARGIN + 40;

// Table columns (sum = CONTENT_W).
const COL_DATE: i32 = 70;
const COL_DESC: i32 = 195;
const COL_HOURS: i32 = 55;
const COL_RATE: i32 = 75;
const COL_AMOUNT: i32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Face {
    Regular,
    Bold,
    Oblique,
    BoldOblique,
}

impl Face {
    fn of(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => Face::Regular,
            (true, false) => Face::Bold,
            (false, true) => Face::Oblique,
            (true, true) => Face::BoldOblique,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Face::Regular => "F1",
            Face::Bold => "F2",
            Face::Oblique => "F3",
            Face::BoldOblique => "F4",
        }
    }
    fn base(self) -> &'static [u8] {
        match self {
            Face::Regular => b"Helvetica",
            Face::Bold => b"Helvetica-Bold",
            Face::Oblique => b"Helvetica-Oblique",
            Face::BoldOblique => b"Helvetica-BoldOblique",
        }
    }
}

/// A laid-out drawing operation in integer points, top-left-origin page
/// space; flipped to PDF's bottom-up space at emit time.
#[derive(Debug, PartialEq)]
enum Op {
    Text {
        face: Face,
        size: i32,
        x: i32,
        y: i32,
        bytes: Vec<u8>,
    },
    /// Right-aligned text (numeric table columns).
    TextRight {
        face: Face,
        size: i32,
        x_right: i32,
        y: i32,
        bytes: Vec<u8>,
    },
    Line {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        width: i32,
    },
}

/// Text in WinAnsi bytes (the base-14 encoding). UTF-8 is mapped code point
/// by code point; unrepresentable characters become `?`. Deterministic by
/// construction.
fn winansi(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let b = match ch {
            '€' => 0x80,
            '…' => 0x85,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '™' => 0x99,
            c if (c as u32) <= 0xFF && (c as u32) >= 0x20 || (c as u32) == 0x0A => c as u8,
            _ => b'?',
        };
        out.push(b);
    }
    out
}

/// Helvetica (and Bold) advance widths in 1/1000 units for the WinAnsi
/// codes 32..=255. Oblique faces share the roman widths (they are slants,
/// not re-metrics). Upper WinAnsi punctuation is exact where common; exotic
/// Latin-1 glyphs fall back to their ASCII base or 556.
fn width_units(face: Face, code: u8) -> u32 {
    if code < 32 {
        return 0;
    }
    let bold = matches!(face, Face::Bold | Face::BoldOblique);
    if code <= 126 {
        let idx = (code - 32) as usize;
        return if bold {
            BOLD_WIDTHS[idx]
        } else {
            REG_WIDTHS[idx]
        };
    }
    // WinAnsi 128..=159: only these occur after our mapping.
    if code <= 159 {
        return match code {
            0x85 | 0x89 | 0x97 => 1000, // ellipsis, per-mille, em dash
            0x95 => {
                if bold {
                    400
                } else {
                    350
                }
            } // bullet
            0x96 => 556,                // en dash
            0x91..=0x94 => {
                if bold {
                    333
                } else {
                    222
                }
            } // quotes
            0x80 => 556,                // euro
            0x99 => 1000,               // tm
            _ => 556,
        };
    }
    // Latin-1 supplement: width of the de-accented base, 556 fallback.
    match code {
        0xA0 | 0xAD => 278, // nbsp, soft hyphen
        0xA1 | 0xBF => {
            if bold {
                333
            } else {
                278
            }
        } // inverted !?
        0xA2..=0xA5 => 556, // cents/pounds/currency/yen
        0xA6 => 260,        // broken bar
        0xA7 | 0xB6 => 556, // section, pilcrow
        0xA8 | 0xAF | 0xB4 | 0xB8 => 333,
        0xA9 | 0xAE => 667, // (c), (r)
        0xB0 => 400,        // degree
        0xB1 => 584,        // plusminus
        0xB2 | 0xB3 | 0xB9 | 0xAA | 0xBA => 333,
        0xAB | 0xBB => 556, // guillemets
        0xAC | 0xD7 | 0xF7 => 584,
        0xB5 => 556, // micro
        // Accented letters inherit coarse class widths (metrics only steer
        // wrapping; viewers use the font's real built-in metrics).
        c if (0xC0..=0xDE).contains(&c) => 700,
        c if (0xE0..=0xFE).contains(&c) => 560,
        0xFF => 500,
        _ => 556,
    }
}

/// Advance widths for ASCII 32..=126, Helvetica (units / 1000).
static REG_WIDTHS: [u32; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// Advance widths for ASCII 32..=126, Helvetica-Bold (units / 1000).
static BOLD_WIDTHS: [u32; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// Width in integer points (round half up) of WinAnsi bytes at `size`.
fn measure(face: Face, size: i32, bytes: &[u8]) -> i32 {
    let total: u64 = bytes
        .iter()
        .map(|&b| u64::from(width_units(face, b)))
        .sum::<u64>()
        * size.max(0) as u64;
    ((total + 500) / 1000) as i32
}

/// Greedy word wrap over WinAnsi bytes at a fixed face/size. Whitespace is
/// collapsed; over-long words hard-split so nothing overflows the column.
fn wrap(face: Face, size: i32, bytes: &[u8], max_w: i32) -> Vec<Vec<u8>> {
    let space = measure(face, size, b" ");
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_w = 0;
    for word in bytes.split(|b| *b == b' ' || *b == b'\n') {
        if word.is_empty() {
            continue;
        }
        let mut w = measure(face, size, word);
        if w == 0 {
            continue;
        }
        let mut word = word;
        // Hard-split a word wider than the column.
        while w > max_w && word.len() > 1 {
            let mut n = word.len();
            while n > 1 && measure(face, size, &word[..n]) > max_w {
                n -= 1;
            }
            let (head, tail) = word.split_at(n);
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            lines.push(head.to_vec());
            word = tail;
            w = measure(face, size, word);
        }
        let add = if cur.is_empty() { w } else { w + space };
        if cur_w + add > max_w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if !cur.is_empty() {
            cur.push(b' ');
            cur_w += space;
        }
        cur.extend_from_slice(word);
        cur_w += w;
        if cur_w >= max_w {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Wrap then cap at `max_lines`, marking a truncated tail with ASCII "...".
fn wrap_limited(face: Face, size: i32, bytes: &[u8], max_w: i32, max_lines: usize) -> Vec<Vec<u8>> {
    let mut lines = wrap(face, size, bytes, max_w);
    if lines.len() > max_lines {
        let head = lines.remove(max_lines - 1);
        let dots = b"...";
        let mut cut = head;
        while cut.len() >= 3 && measure(face, size, &cut) + measure(face, size, dots) > max_w {
            cut.pop();
        }
        cut.extend_from_slice(dots);
        lines.truncate(max_lines - 1);
        lines.push(cut);
    }
    lines
}

struct Pager {
    pages: Vec<Vec<Op>>,
    cur: Vec<Op>,
    /// Baseline cursor of the next text line, in top-down page points.
    y: i32,
}

impl Pager {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            cur: Vec::new(),
            y: MARGIN,
        }
    }

    /// Ensure `height` points of room remain; page-break otherwise. Returns
    /// whether a break happened.
    fn need(&mut self, height: i32) -> bool {
        if self.y + height <= PAGE_H - BOTTOM {
            return false;
        }
        let page = std::mem::take(&mut self.cur);
        self.pages.push(page);
        self.y = MARGIN;
        true
    }

    /// Consume `height` vertical space.
    fn advance(&mut self, height: i32) {
        self.y += height;
    }

    fn finish(mut self) -> Vec<Vec<Op>> {
        self.pages.push(std::mem::take(&mut self.cur));
        self.pages
    }
}

/// Lay the `Doc` out into per-page operation lists.
fn layout(doc: &Doc) -> Vec<Vec<Op>> {
    let mut p = Pager::new();

    // Title.
    let title = winansi(&doc.title);
    p.cur.push(Op::Text {
        face: Face::Bold,
        size: 18,
        x: MARGIN,
        y: p.y + 18,
        bytes: title,
    });
    p.advance(18 + 16);
    p.cur.push(Op::Line {
        x1: MARGIN,
        y1: p.y,
        x2: MARGIN + CONTENT_W,
        y2: p.y,
        width: 1,
    });
    p.advance(10);

    // Header key/values, then free blocks.
    for b in &doc.header {
        emit_block(&mut p, b, 10, 0);
    }
    p.advance(8);
    for b in &doc.blocks {
        emit_block(&mut p, b, 10, 0);
    }
    p.advance(10);

    // Line table with a repeating header.
    if !doc.lines.is_empty() {
        emit_table_head(&mut p);
        for row in &doc.lines {
            emit_table_row(&mut p, row);
        }
        p.advance(6);
    }

    // Totals (bold rule above; prints total_minor, never a re-sum).
    for b in &doc.totals {
        emit_block(&mut p, b, 11, 0);
    }
    p.advance(14);

    // Footer.
    for b in &doc.footer {
        emit_block(&mut p, b, 9, 0);
    }

    p.finish()
}

fn emit_table_head(p: &mut Pager) {
    p.need(30);
    let y = p.y + 12;
    let cols = [
        ("Date", MARGIN, COL_DATE, false),
        ("Description", MARGIN + COL_DATE, COL_DESC, false),
        ("Hours", MARGIN + COL_DATE + COL_DESC, COL_HOURS, true),
        (
            "Rate",
            MARGIN + COL_DATE + COL_DESC + COL_HOURS,
            COL_RATE,
            true,
        ),
        (
            "Amount",
            MARGIN + COL_DATE + COL_DESC + COL_HOURS + COL_RATE,
            COL_AMOUNT,
            true,
        ),
    ];
    for (label, x, w, right) in cols {
        let bytes = winansi(label);
        p.cur.push(if right {
            Op::TextRight {
                face: Face::Bold,
                size: 9,
                x_right: x + w,
                y,
                bytes,
            }
        } else {
            Op::Text {
                face: Face::Bold,
                size: 9,
                x,
                y,
                bytes,
            }
        });
    }
    p.y = y + 4;
    p.cur.push(Op::Line {
        x1: MARGIN,
        y1: p.y,
        x2: MARGIN + CONTENT_W,
        y2: p.y,
        width: 1,
    });
    p.advance(6);
}

fn emit_table_row(p: &mut Pager, row: &LineRow) {
    // Description wraps to at most two lines in its column.
    let desc_bytes = winansi(&row.description);
    let desc_lines = wrap_limited(Face::Regular, 9, &desc_bytes, COL_DESC - 6, 2);
    let height = 12 * desc_lines.len() as i32 + 4;
    if p.need(height) {
        emit_table_head(p);
    }
    let base = p.y + 11;
    p.cur.push(Op::Text {
        face: Face::Regular,
        size: 9,
        x: MARGIN,
        y: base,
        bytes: winansi(&row.date),
    });
    for (i, l) in desc_lines.into_iter().enumerate() {
        p.cur.push(Op::Text {
            face: Face::Regular,
            size: 9,
            x: MARGIN + COL_DATE,
            y: base + 11 * i as i32,
            bytes: l,
        });
    }
    let mut num = |s: &str, x_right: i32| {
        if s.is_empty() {
            return;
        }
        p.cur.push(Op::TextRight {
            face: Face::Regular,
            size: 9,
            x_right,
            y: base,
            bytes: winansi(s),
        });
    };
    num(&row.hours, MARGIN + COL_DATE + COL_DESC + COL_HOURS - 4);
    num(
        &row.rate,
        MARGIN + COL_DATE + COL_DESC + COL_HOURS + COL_RATE - 4,
    );
    num(
        &row.amount,
        MARGIN + COL_DATE + COL_DESC + COL_HOURS + COL_RATE + COL_AMOUNT - 4,
    );
    p.advance(height);
}

/// Emit one flowing block at `size` points. `indent` nests bullets/lists.
fn emit_block(p: &mut Pager, block: &Block, size: i32, indent: i32) {
    match block {
        Block::Heading { level, spans } => {
            let h = match level {
                1 => 14,
                2 => 12,
                _ => 11,
            };
            emit_spans(p, spans, h, true, indent);
        }
        Block::Paragraph(spans) => emit_spans(p, spans, size, false, indent),
        Block::Bullet(spans) => emit_spans_with_prefix(p, spans, size, indent, b"\x95 "),
        Block::Numbered { number, spans } => {
            emit_spans_with_prefix(p, spans, size, indent, &winansi(&format!("{number}. ")))
        }
        Block::KeyVal { key, spans } => emit_keyval(p, key, spans, size, indent),
    }
}

/// A `Key: value` line: bold key in a fixed label column, value wrapped.
fn emit_keyval(p: &mut Pager, key: &str, spans: &[Span], size: i32, indent: i32) {
    const LABEL_W: i32 = 110;
    let key_bytes = winansi(&format!("{key}:"));
    let x0 = MARGIN + indent;
    let avail = CONTENT_W - indent - LABEL_W;
    let mut lines = wrap_spans(spans, size, avail, false);
    if lines.is_empty() {
        lines.push(vec![]);
    }
    let height = 13 * lines.len() as i32;
    p.need(height);
    let base = p.y + size;
    p.cur.push(Op::Text {
        face: Face::Bold,
        size,
        x: x0,
        y: base,
        bytes: key_bytes,
    });
    let xv = x0 + LABEL_W;
    for (i, line) in lines.into_iter().enumerate() {
        emit_line_ops(p, &line, size, xv, base + 13 * i as i32);
    }
    p.advance(height);
}

/// Styled paragraph: wrap across spans (each span keeps its own face), then
/// emit line by line with running x.
fn emit_spans(p: &mut Pager, spans: &[Span], size: i32, force_bold: bool, indent: i32) {
    let lines = wrap_spans(spans, size, CONTENT_W - indent, force_bold);
    let height = 13 * lines.len() as i32;
    p.need(height);
    let base = p.y + size;
    for (i, line) in lines.into_iter().enumerate() {
        emit_line_ops(p, &line, size, MARGIN + indent, base + 13 * i as i32);
    }
    p.advance(height);
}

fn emit_spans_with_prefix(p: &mut Pager, spans: &[Span], size: i32, indent: i32, prefix: &[u8]) {
    let px = measure(Face::Regular, size, prefix);
    let lines = wrap_spans(spans, size, CONTENT_W - indent - px, false);
    let height = 13 * lines.len() as i32;
    p.need(height);
    let base = p.y + size;
    p.cur.push(Op::Text {
        face: Face::Regular,
        size,
        x: MARGIN + indent,
        y: base,
        bytes: prefix.to_vec(),
    });
    for (i, line) in lines.into_iter().enumerate() {
        emit_line_ops(p, &line, size, MARGIN + indent + px, base + 13 * i as i32);
    }
    p.advance(height);
}

/// One laid-out word of a line: the span it came from (face) + its bytes.
type Word = (bool, bool, Vec<u8>);

/// Greedy multi-span wrap: words carry their span styling; measurement uses
/// the exact face per word.
fn wrap_spans(spans: &[Span], size: i32, max_w: i32, force_bold: bool) -> Vec<Vec<Word>> {
    let space = measure(Face::Regular, size, b" ");
    let mut lines: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    let mut cur_w = 0;
    for s in spans {
        if s.text.is_empty() {
            continue;
        }
        let bold = s.bold || force_bold;
        let bytes = winansi(&s.text);
        for word in bytes.split(|b| *b == b' ' || *b == b'\n') {
            if word.is_empty() {
                continue;
            }
            let face = Face::of(bold, s.italic);
            let mut w = measure(face, size, word);
            let mut word = word;
            while w > max_w && word.len() > 1 {
                let mut n = word.len();
                while n > 1 && measure(face, size, &word[..n]) > max_w {
                    n -= 1;
                }
                let (head, tail) = word.split_at(n);
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                lines.push(vec![(bold, s.italic, head.to_vec())]);
                word = tail;
                w = measure(face, size, word);
            }
            let add = if cur.is_empty() { w } else { w + space };
            if cur_w + add > max_w && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if !cur.is_empty() {
                cur_w += space;
            }
            cur.push((bold, s.italic, word.to_vec()));
            cur_w += w;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(vec![]);
    }
    lines
}

/// Emit one wrapped line of styled words at `(x, y)` with a running cursor.
fn emit_line_ops(p: &mut Pager, line: &[Word], size: i32, x0: i32, y: i32) {
    let space = measure(Face::Regular, size, b" ");
    let mut x = x0;
    for (i, (bold, italic, bytes)) in line.iter().enumerate() {
        if i > 0 {
            x += space;
        }
        let face = Face::of(*bold, *italic);
        let w = measure(face, size, bytes);
        p.cur.push(Op::Text {
            face,
            size,
            x,
            y,
            bytes: bytes.clone(),
        });
        x += w;
    }
}

/// Render the document to PDF bytes. Pure: same `Doc` in, byte-identical
/// output out — see the module determinism contract.
#[must_use]
pub fn render_invoice_pdf(doc: &Doc) -> Vec<u8> {
    let pages = layout(doc);
    emit(&pages)
}

/// Turn laid-out pages into a PDF 1.4 file. Object ids are fixed:
/// 1 catalog, 2 page tree (carrying the shared /Resources), 3 resources,
/// 4..7 fonts, 8+ page/content pairs. No /Info, no file ID — those carry
/// timestamps and would break byte determinism.
fn emit(pages: &[Vec<Op>]) -> Vec<u8> {
    use pdf_writer::{Content, Name, Pdf, Rect, Ref, writers::Resources};

    let catalog = Ref::new(1);
    let page_tree = Ref::new(2);
    let resources = Ref::new(3);
    let faces = [Face::Regular, Face::Bold, Face::Oblique, Face::BoldOblique];
    let font_ids: Vec<Ref> = (4..8).map(Ref::new).collect();
    let first_page = 8usize;

    let mut pdf = Pdf::new();
    pdf.set_version(1, 4);
    pdf.catalog(catalog).pages(page_tree);
    let page_refs: Vec<Ref> = (0..pages.len())
        .map(|i| Ref::new((first_page + 2 * i) as i32))
        .collect();
    {
        let mut pw = pdf.pages(page_tree);
        pw.kids(page_refs.iter().copied()).count(pages.len() as i32);
        // /Resources is inheritable from the page tree (PDF 1.4 §7.3.1), so
        // the font dict is written once for every page.
        pw.pair(Name(b"Resources"), resources);
    }

    {
        let mut res = pdf.indirect(resources).start::<Resources>();
        let mut fonts = res.fonts();
        for (id, face) in font_ids.iter().zip(faces) {
            fonts.pair(Name(face.name().as_bytes()), *id);
        }
    }

    for (id, face) in font_ids.iter().zip(faces) {
        let mut f = pdf.type1_font(*id);
        f.base_font(Name(face.base()))
            .encoding_predefined(Name(b"WinAnsiEncoding"))
            .first_char(32)
            .last_char(255)
            .widths((32..=255u8).map(|c| width_units(face, c) as f32));
    }

    for (i, ops) in pages.iter().enumerate() {
        let content_ref = Ref::new((first_page + 2 * i + 1) as i32);
        let mut content = Content::new();
        for op in ops {
            match op {
                Op::Text {
                    face,
                    size,
                    x,
                    y,
                    bytes,
                } => {
                    put_text(&mut content, *face, *size, *x, *y, bytes);
                }
                Op::TextRight {
                    face,
                    size,
                    x_right,
                    y,
                    bytes,
                } => {
                    let w = measure(*face, *size, bytes);
                    put_text(&mut content, *face, *size, *x_right - w, *y, bytes);
                }
                Op::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    width,
                } => {
                    content.set_line_width(*width as f32);
                    content.move_to(*x1 as f32, (PAGE_H - y1) as f32);
                    content.line_to(*x2 as f32, (PAGE_H - y2) as f32);
                    content.stroke();
                }
            }
        }
        let buf = content.finish();
        pdf.stream(content_ref, buf.as_slice());
        pdf.page(page_refs[i])
            .parent(page_tree)
            .media_box(Rect::new(0.0, 0.0, PAGE_W as f32, PAGE_H as f32))
            .contents(content_ref);
    }

    pdf.finish()
}

/// One positioned text draw; PDF's y axis points up, ours doesn't.
fn put_text(
    content: &mut pdf_writer::Content,
    face: Face,
    size: i32,
    x: i32,
    y: i32,
    bytes: &[u8],
) {
    use pdf_writer::{Name, Str};
    content.begin_text();
    content.set_font(Name(face.name().as_bytes()), size as f32);
    content.set_text_matrix([1.0, 0.0, 0.0, 1.0, x as f32, (PAGE_H - y) as f32]);
    content.show(Str(bytes));
    content.end_text();
}

// ============================================================ unit tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Currency, Customer, InvoiceLine, InvoiceStatus, LineKind};
    use chrono::TimeZone;

    fn sample_invoice() -> Invoice {
        Invoice {
            id: "11111111-1111-1111-1111-111111111111".parse().unwrap(),
            number: "INV-0001".into(),
            customer_id: "22222222-2222-2222-2222-222222222222".parse().unwrap(),
            currency: Currency("EUR".into()),
            period_from: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            period_to: chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
            lines: vec![
                InvoiceLine {
                    kind: LineKind::Time,
                    date: chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(),
                    entry_id: None,
                    expense_id: None,
                    project_code: Some(crate::domain::ProjectCode("WEB-1".into())),
                    task_code: None,
                    hours: Some(crate::domain::Hours(800)),
                    rate_minor: Some(9000),
                    amount_minor: 72000,
                    note: String::new(),
                },
                InvoiceLine {
                    kind: LineKind::Expense,
                    date: chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap(),
                    entry_id: None,
                    expense_id: None,
                    project_code: None,
                    task_code: None,
                    hours: None,
                    rate_minor: None,
                    amount_minor: 12500,
                    note: "Flight tickets".into(),
                },
            ],
            total_minor: 84500,
            status: InvoiceStatus::Issued,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            issued_at: Some(Utc.timestamp_opt(1_760_000_000, 0).unwrap()),
            due_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()),
            paid_at: None,
            payment_reference: String::new(),
            pdf: None,
        }
    }

    fn customer() -> Customer {
        Customer {
            id: "22222222-2222-2222-2222-222222222222".parse().unwrap(),
            name: "Capybara Solutions".into(),
            currency: Currency("EUR".into()),
            default_rate_minor: 9000,
            active: true,
            email: "billing@capybara.example".into(),
        }
    }

    fn org() -> Org {
        Org {
            name: "Tucano".into(),
        }
    }

    #[test]
    fn renderer_is_deterministic() {
        let doc = doc_for(&sample_invoice(), &customer(), &org());
        let a = render_invoice_pdf(&doc);
        let b = render_invoice_pdf(&doc);
        assert_eq!(a, b, "same Doc must produce byte-identical PDFs");
    }

    #[test]
    fn output_is_a_pdf_and_reparses() {
        let doc = doc_for(&sample_invoice(), &customer(), &org());
        let bytes = render_invoice_pdf(&doc);
        assert!(bytes.starts_with(b"%PDF-1.4"), "header");
        assert!(bytes.ends_with(b"%%EOF"), "trailer");
        // Independent validation (ADR guard 2): lopdf must parse it.
        let _ = lopdf::Document::load_mem(&bytes).expect("re-parse");
    }

    #[test]
    fn money_never_floats_and_totals_are_printed_not_summed() {
        let doc = doc_for(&sample_invoice(), &customer(), &org());
        let total = match &doc.totals[0] {
            Block::KeyVal { spans, .. } => &spans[0].text,
            _ => panic!("totals are a KeyVal"),
        };
        assert_eq!(total, "845.00 EUR");
        // Line amounts format from minor units only.
        assert_eq!(doc.lines[0].amount, "720.00");
        assert_eq!(doc.lines[0].hours, "8.00");
        assert_eq!(doc.lines[0].rate, "90.00");
    }

    #[test]
    fn due_date_prints_as_stored() {
        let mut inv = sample_invoice();
        inv.due_date = None; // draft: no due date line at all
        let doc = doc_for(&inv, &customer(), &org());
        assert!(
            !doc.header
                .iter()
                .any(|b| matches!(b, Block::KeyVal { key, .. } if key == "Due date")),
        );
    }

    #[test]
    fn expense_and_fixed_rows_have_no_hours_or_rate() {
        let doc = doc_for(&sample_invoice(), &customer(), &org());
        assert_eq!(doc.lines[1].hours, "");
        assert_eq!(doc.lines[1].rate, "-");
        assert_eq!(doc.lines[1].description, "Flight tickets");
    }

    #[test]
    fn empty_note_invoice_renders() {
        let mut inv = sample_invoice();
        inv.lines.clear();
        inv.total_minor = 0;
        let doc = doc_for(&inv, &customer(), &org());
        let bytes = render_invoice_pdf(&doc);
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn many_lines_flow_onto_more_pages() {
        let mut inv = sample_invoice();
        inv.lines = (0..80)
            .map(|i| InvoiceLine {
                kind: LineKind::Fixed,
                date: inv.period_from,
                entry_id: None,
                expense_id: None,
                project_code: None,
                task_code: None,
                hours: None,
                rate_minor: None,
                amount_minor: i as u64 * 100,
                note: format!("Recurring charge {i}"),
            })
            .collect();
        let doc = doc_for(&inv, &customer(), &org());
        let pages = layout(&doc);
        assert!(pages.len() > 1, "80 lines must flow across pages");
        let bytes = render_invoice_pdf(&doc);
        let parsed = lopdf::Document::load_mem(&bytes).expect("re-parse multi-page");
        assert_eq!(parsed.get_pages().len(), pages.len());
    }

    #[test]
    fn winansi_maps_and_substitutes() {
        assert_eq!(winansi("€5.00"), vec![0x80, b'5', b'.', b'0', b'0']);
        assert_eq!(
            winansi("naïve — ok"),
            vec![b'n', b'a', 0xEF, b'v', b'e', b' ', 0x97, b' ', b'o', b'k']
        );
        assert_eq!(winansi("日本語"), b"???".to_vec());
    }

    #[test]
    fn wrap_breaks_words_to_width() {
        let text = b"the quick brown fox jumps over the lazy dog".to_vec();
        let lines = wrap(Face::Regular, 10, &text, 70);
        assert!(lines.len() >= 3);
        for l in &lines {
            assert!(
                measure(Face::Regular, 10, l) <= 70,
                "{:?} wider than cap",
                String::from_utf8_lossy(l)
            );
        }
    }

    #[test]
    fn truncate_marks_with_dots() {
        let long = "x".repeat(300).into_bytes();
        let lines = wrap_limited(Face::Regular, 9, &long, COL_DESC - 6, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with(b"..."));
    }

    #[test]
    fn spans_carry_bold_and_italic_faces() {
        let doc = Doc {
            title: "T".into(),
            header: vec![],
            blocks: vec![Block::Paragraph(vec![
                Span::bold("B"),
                Span::text(" "),
                Span::italic("I"),
                Span::text(" plain"),
            ])],
            lines: vec![],
            totals: vec![],
            footer: vec![],
        };
        let pages = layout(&doc);
        let faces: Vec<Face> = pages[0]
            .iter()
            .filter_map(|op| match op {
                Op::Text { face, .. } | Op::TextRight { face, .. } => Some(*face),
                Op::Line { .. } => None,
            })
            .collect();
        assert!(faces.contains(&Face::Bold));
        assert!(faces.contains(&Face::Oblique));
        assert!(faces.contains(&Face::Regular));
    }
}
