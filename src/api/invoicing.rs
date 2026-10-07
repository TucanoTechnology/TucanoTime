//! Invoicing: draft/issue/pay lifecycle (#8, #27), email delivery (#35),
//! online payments (#34) and accounting sync (#33).

use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceInput {
    pub customer_id: Uuid,
    pub from: String,
    pub to: String,
    /// Include billable expenses in the period (default true, #25).
    #[serde(default = "default_active_true")]
    pub include_expenses: bool,
    /// Restrict billing to these projects of the customer (#134); absent or
    /// empty = every project with eligible work in the period.
    #[serde(default)]
    pub project_codes: Vec<crate::domain::ProjectCode>,
}

/// Shared selection pipeline for POST /invoices and /invoices/preview:
/// gather sources, honour project filtering (#134), build the draft.
#[allow(clippy::too_many_arguments)]
fn build_draft(
    app: &AppState,
    customer: &crate::domain::Customer,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    include_expenses: bool,
    project_codes: &[crate::domain::ProjectCode],
) -> Result<Invoice, ApiError> {
    if !project_codes.is_empty() {
        let owned = app.store.list_projects(customer.id)?;
        for code in project_codes {
            if !owned.iter().any(|p| &p.code == code) {
                return Err(ApiError::validation(vec![FieldError::new(
                    "project_codes",
                    format!("unknown project {} for this customer", code.0),
                )]));
            }
        }
    }
    let projects: Vec<_> = {
        let all = app.store.list_projects(customer.id)?;
        if project_codes.is_empty() {
            all
        } else {
            all.into_iter()
                .filter(|p| project_codes.contains(&p.code))
                .collect()
        }
    };
    let users = app.store.list_users()?;
    let entries = app.store.list_range(from, to)?;
    let expenses = app.store.list_expenses()?;
    let invoices = app.store.list_invoices()?;
    let issued: Vec<&Invoice> = invoices
        .iter()
        .filter(|i| i.status != InvoiceStatus::Draft)
        .collect();
    let excluded_entries: Vec<Uuid> = issued
        .iter()
        .flat_map(|i| i.lines.iter())
        .filter_map(|l| l.entry_id)
        .collect();
    let excluded_expenses: Vec<Uuid> = issued
        .iter()
        .flat_map(|i| i.lines.iter())
        .filter_map(|l| l.expense_id)
        .collect();
    let sources = crate::domain::InvoiceSources {
        projects: &projects,
        users: &users,
        entries: &entries,
        expenses: &expenses,
        excluded_entries: &excluded_entries,
        excluded_expenses: &excluded_expenses,
        include_expenses,
        restrict_projects: !project_codes.is_empty(),
    };
    generate_invoice(String::new(), customer, &sources, from, to, app.clock.now())
        .map_err(invoice_error)
}

/// `POST /invoices/preview` (#134): run the SAME generation as
/// `POST /invoices` but without persisting — the staged wizard reviews exact
/// lines, then saves the reviewed selection. A draft is created only by the
/// real POST; nothing here locks, numbers or stores.
pub async fn preview_invoice(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<InvoiceInput>,
) -> ApiResult {
    let customer = get_customer(&app.store, input.customer_id)?;
    let from = parse_date(&input.from)?;
    let to = parse_date(&input.to)?;
    if from > to {
        return Err(ApiError::bad_request("'from' must not be after 'to'"));
    }
    let draft = build_draft(
        &app,
        &customer,
        from,
        to,
        input.include_expenses,
        &input.project_codes,
    )?;
    Ok(Json(draft).into_response())
}

pub async fn list_invoices(State(app): State<AppState>) -> ApiResult {
    let invoices = app.store.list_invoices()?;
    Ok(Json(serde_json::json!({ "invoices": invoices })).into_response())
}

/// Generate a draft invoice from the billable, not-yet-invoiced work in a period.
pub async fn create_invoice(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<InvoiceInput>,
) -> ApiResult {
    let customer = get_customer(&app.store, input.customer_id)?;
    let from = parse_date(&input.from)?;
    let to = parse_date(&input.to)?;
    if from > to {
        return Err(ApiError::bad_request("'from' must not be after 'to'"));
    }
    let draft = build_draft(
        &app,
        &customer,
        from,
        to,
        input.include_expenses,
        &input.project_codes,
    )?;
    let invoice = app.store.create_invoice(draft)?;
    Ok((StatusCode::CREATED, Json(invoice)).into_response())
}

fn invoice_error(e: InvoiceError) -> ApiError {
    match e {
        InvoiceError::NothingToInvoice => {
            ApiError::conflict("no billable, not-yet-invoiced work in this period")
        }
        InvoiceError::MixedCurrency { a, b } => ApiError::conflict(format!(
            "the period mixes currencies ({a} and {b}); an invoice is single-currency"
        )),
    }
}

/// The load-or-404 every invoice route starts with (#100: this block existed
/// seven times). Reads only; state checks stay with each endpoint.
pub(crate) fn get_invoice_or_404(app: &AppState, id: Uuid) -> Result<Invoice, ApiError> {
    app.store
        .get_invoice(id)?
        .ok_or_else(|| ApiError::not_found("invoice"))
}

pub async fn get_invoice_handler(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    Ok(Json(get_invoice_or_404(&app, id)?).into_response())
}

/// The org identity singleton (`org_profile.json`, #138); absent = defaults.
pub(crate) fn load_org(app: &AppState) -> Result<crate::domain::OrgProfile, ApiError> {
    Ok(app
        .store
        .read_json_rel::<crate::domain::OrgProfile>("org_profile.json")?
        .unwrap_or_default())
}

/// The issuing organisation for invoice documents and emails, resolved ONCE
/// here so there is a single source of truth (#138): `org_profile.name` when
/// set, else the boot-time `org_name` config key (#94), else its default.
/// Legal id and address come from the profile only.
pub(crate) fn org_for(app: &AppState) -> Result<crate::pdf::Org, ApiError> {
    // #187: a corrupt org_profile.json must surface (500), not silently
    // render invoices with the fallback identity via `unwrap_or_default()`.
    let profile = load_org(app)?;
    let name = if profile.name.trim().is_empty() {
        app.cfg()
            .get_str("org_name", &crate::appconfig::process_env)
    } else {
        profile.name.trim().to_string()
    };
    Ok(crate::pdf::Org {
        name,
        legal_id: profile.legal_id.clone(),
        address: profile.address.clone(),
        accent: profile.accent.clone(),
    })
}

/// `GET /admin/org` — company identity (#138).
pub async fn org_get(State(app): State<AppState>) -> ApiResult {
    Ok(Json(load_org(&app)?).into_response())
}

/// Validate + `PUT /admin/org` (#138). Invalid values fail before anything
/// is persisted; empty name legitimately means "fall back to config".
pub async fn org_put(
    State(app): State<AppState>,
    ValidJson(profile): ValidJson<crate::domain::OrgProfile>,
) -> ApiResult {
    let mut errors = Vec::new();
    if profile.name.chars().count() > 120 {
        errors.push(FieldError::new("name", "at most 120 characters"));
    }
    if profile.legal_id.chars().count() > 60 {
        errors.push(FieldError::new("legal_id", "at most 60 characters"));
    }
    // #146 presentation knobs, validated here so nothing inert is stored.
    if profile.from_name.chars().count() > 120 {
        errors.push(FieldError::new("from_name", "at most 120 characters"));
    }
    if !profile.reply_to.trim().is_empty() && !crate::email::valid_address(profile.reply_to.trim())
    {
        errors.push(FieldError::new("reply_to", "not a valid email address"));
    }
    if !profile.accent.is_empty() && crate::pdf::parse_accent(&profile.accent).is_none() {
        errors.push(FieldError::new("accent", "expected a #rrggbb hex color"));
    }
    let mut billing_errors = Vec::new();
    crate::domain::validate_customer_billing_fields(
        &profile.address,
        &[],
        0,
        0,
        &mut billing_errors,
    );
    errors.append(&mut billing_errors);
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    app.store.write_json_rel("org_profile.json", &profile)?;
    Ok(Json(profile).into_response())
}

/// Issue a draft invoice: this locks its entries from edits/deletes (#18 seam)
/// and archives its PDF for the first time (#113). The document is rendered
/// from the exact state the store is about to persist (same clock, same due
/// date), so issuing and archiving commit or fail as one transaction.
pub async fn issue_invoice(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    // Draft check + transition happen inside one store lock (review B4):
    // an issue racing a pay (or another issue) cannot double-transition.
    let invoice = get_invoice_or_404(&app, id)?;
    let customer = get_customer(&app.store, invoice.customer_id)?;
    let template = load_template(&app)?;
    // Payment terms (#116): customer -> org template -> legacy net-14 from
    // period_to (the fallback keeps its exact original arithmetic).
    // #190: capture the clock ONCE so the snapshot's issued_at and the value
    // the store persists are identical (three separate now() calls could
    // straddle a tick — visible at midnight boundaries and it breaks the
    // "re-render yields identical bytes" determinism claim).
    let now = app.clock.now();
    let due = resolve_due_date(&invoice, &customer, &template, now);
    // Render against the to-be-issued snapshot: the PDF is the issue-time
    // document, and bytes are pure in this state (legacy re-renders match).
    let mut snapshot = invoice;
    snapshot.status = crate::domain::InvoiceStatus::Issued;
    snapshot.issued_at = Some(now);
    snapshot.due_date = Some(due);
    let mut doc =
        crate::pdf::doc_for_labeled(&snapshot, &customer, &org_for(&app)?, &template.labels);
    let content = content_for(&snapshot, &customer, &template);
    crate::template::apply_doc_content(&mut doc, &content);
    let pdf = crate::pdf::render_invoice_pdf(&doc);
    let issued = app.store.issue_invoice(id, now, due, &pdf)?;
    Ok(Json(issued).into_response())
}

/// Download the archived invoice PDF (#113). Admin tier only (the whole
/// `/invoices` surface is, #51). A legacy issued invoice with no archive yet
/// gets it rendered lazily from the snapshot-locked document — byte-stable by
/// the renderer's determinism contract — and persisted on first read.
pub async fn invoice_pdf(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    if invoice.status == crate::domain::InvoiceStatus::Draft {
        return Err(ApiError::conflict(
            "issue the invoice before downloading its PDF",
        ));
    }
    let (bytes, filename) = match app.store.invoice_pdf_bytes(id)? {
        Some(b) => (
            b,
            format!("{}.pdf", crate::domain::safe_filename(&invoice.number)),
        ),
        None => {
            let b = render_pdf_now(&app, &invoice)?;
            // Persist the archive + hint; a failure here is a plain 500 and
            // costs nothing (the invoice JSON remains the record of truth).
            let attached = app.store.attach_invoice_pdf(id, &b, app.clock.now())?;
            let name = attached.pdf.map(|h| h.filename).unwrap_or_else(|| {
                format!("{}.pdf", crate::domain::safe_filename(&invoice.number))
            });
            (b, name)
        }
    };
    use sha2::{Digest, Sha256};
    let sha = Sha256::digest(&bytes);
    let etag = sha.iter().map(|b| format!("{b:02x}")).collect::<String>();
    tracing::debug!(invoice = %invoice.number, bytes = bytes.len(), "invoice pdf download");
    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/pdf".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (axum::http::header::ETAG, format!("\"{etag}\"")),
        ],
        bytes,
    )
        .into_response())
}

/// Download names come from `Invoice::number` (minted server-side), sanitised
/// defensively — never from user input (#50).

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayInput {
    #[serde(default)]
    pub reference: String,
    /// Minor units; absent = the full remaining balance (today's behaviour,
    /// #114). Zero and over-balance are rejected without persisting.
    #[serde(default)]
    pub amount_minor: Option<u64>,
}

/// Body for `POST /invoices/{id}/write-off` (#114).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteOffInput {
    pub reason: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutInput {
    pub provider: String,
}

/// Record a payment on an issued invoice (#27, #114): full balance by
/// default, or a partial amount. A zero amount is a 422 (bad shape), an
/// over-payment a 409 (bad state arithmetic) — both without persistence.
pub async fn pay_invoice(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<PayInput>,
) -> ApiResult {
    // State check + transition under one lock (review B4).
    get_invoice_or_404(&app, id)?;
    if input.amount_minor == Some(0) {
        return Err(ApiError::validation(vec![FieldError::new(
            "amount_minor",
            "must be greater than zero",
        )]));
    }
    let invoice = match app.store.record_payment(
        id,
        input.amount_minor,
        input.reference,
        "manual".into(),
        app.clock.now(),
    ) {
        Ok(inv) => inv,
        // #220: the old 'payment of ' string-prefix arm was dead weight —
        // From<StoreError> already maps every Conflict to 409 with the same
        // (message-safe) body. Match on structure, not prose.
        Err(e) => return Err(e.into()),
    };
    let paid = invoice
        .payments
        .last()
        .ok_or_else(|| ApiError::internal("payment ledger empty after record".into()))?;
    app.audit.record(
        "invoice_payment",
        &format!(
            "{}:{}:{}:{}",
            id, paid.amount_minor, actor.0.id, paid.method
        ),
        app.clock.now(),
    );
    Ok(Json(invoice).into_response())
}

/// Forgive the remaining balance of an open invoice (#114). Requires a
/// non-empty reason; 409 on draft/paid/written-off states.
pub async fn write_off_invoice(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<WriteOffInput>,
) -> ApiResult {
    get_invoice_or_404(&app, id)?;
    if input.reason.trim().is_empty() {
        return Err(ApiError::validation(vec![FieldError::new(
            "reason",
            "a write-off reason is required",
        )]));
    }
    if input.reason.chars().count() > 500 {
        return Err(ApiError::validation(vec![FieldError::new(
            "reason",
            "reason too long (max 500 characters)",
        )]));
    }
    let invoice = app
        .store
        .write_off_invoice(id, input.reason, app.clock.now())?;
    app.audit.record(
        "invoice_write_off",
        &format!("{}:{}", id, actor.0.id),
        app.clock.now(),
    );
    Ok(Json(invoice).into_response())
}

pub async fn invoice_summary(State(app): State<AppState>) -> ApiResult {
    let invoices = app.store.list_invoices()?;
    let summary = crate::domain::summarise_invoices(&invoices, app.clock.today());
    Ok(Json(summary).into_response())
}

/// Formats minor units for an email body, e.g. `120000` + `EUR` -> `1,200.00 EUR`.
pub fn money_for_email(minor: u64, currency: &str) -> String {
    let whole = minor / 100;
    let frac = minor % 100;
    let grouped = whole
        .to_string()
        .chars()
        .rev()
        .enumerate()
        .fold(String::new(), |mut acc, (i, c)| {
            if i > 0 && i % 3 == 0 {
                acc.push(',');
            }
            acc.push(c);
            acc
        })
        .chars()
        .rev()
        .collect::<String>();
    format!("{grouped}.{:02} {currency}", frac)
}

/// Resolve the invoice PDF for delivery: the archived bytes when present,
/// otherwise rendered on demand from the snapshot-locked invoice and archived
/// (#113: "fall back to rendering it on demand if the archive is missing").
pub(crate) fn invoice_pdf_for_delivery(
    app: &AppState,
    invoice: &Invoice,
) -> Result<(String, Vec<u8>), ApiError> {
    if let Some(bytes) = app.store.invoice_pdf_bytes(invoice.id)? {
        let name = invoice
            .pdf
            .as_ref()
            .map(|h| h.filename.clone())
            .unwrap_or_else(|| format!("{}.pdf", crate::domain::safe_filename(&invoice.number)));
        return Ok((name, bytes));
    }
    let bytes = render_pdf_now(app, invoice)?;
    let attached = app
        .store
        .attach_invoice_pdf(invoice.id, &bytes, app.clock.now())?;
    let name = attached
        .pdf
        .map(|h| h.filename)
        .unwrap_or_else(|| format!("{}.pdf", crate::domain::safe_filename(&invoice.number)));
    Ok((name, bytes))
}

/// Emails an issued invoice to the customer's billing address (#35). Admin
/// only (invoices are admin surface). Non-blocking: the transport runs on a
/// blocking thread so a slow relay never stalls the request. The archived
/// PDF rides along as an `application/pdf` attachment (#113).
pub async fn send_invoice_email(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    if invoice.status == crate::domain::InvoiceStatus::Draft {
        return Err(ApiError::conflict("issue the invoice before emailing it"));
    }
    let customer = get_customer(&app.store, invoice.customer_id)?;
    // #139: the flagged billing contact receives it; the legacy top-level
    // email still works for customers without contacts.
    let billing_email = customer.billing_email().to_string();
    if billing_email.is_empty() {
        return Err(ApiError::validation(vec![FieldError::new(
            "email",
            "customer has no billing email on file",
        )]));
    }
    let (filename, pdf) = invoice_pdf_for_delivery(&app, &invoice)?;
    let amount = money_for_email(invoice.total_minor, &invoice.currency.0);
    let due = invoice.due_date.map(|d| d.to_string());
    let org = org_for(&app)?;
    let template = load_template(&app)?;
    let content = content_for(&invoice, &customer, &template);
    let text = crate::email::render_invoice_email(
        &customer.name,
        &invoice.number,
        &amount,
        due.as_deref(),
        &org.name,
        true,
    );
    // #116: the template subject wins when configured (customer override →
    // org template → legacy); the HTML letter is set only when there is
    // template content, keeping unset templates byte-identical.
    let subject = content
        .subject
        .clone()
        .unwrap_or_else(|| crate::email::invoice_subject(&invoice.number, &org.name));
    let html = crate::template::email_html(&content);
    let profile = load_org(&app)?;
    let msg = crate::email::EmailMessage {
        to: billing_email,
        subject,
        text,
        html,
        attachment: Some((filename, pdf)),
        from_name: sender_field(&profile.from_name),
        reply_to: sender_field(&profile.reply_to),
    };
    let sent_to = msg.to.clone();
    let sender = app.email.clone();
    // A slow relay must not stall the async request thread (#35).
    blocking("email send", move || {
        sender.send(&msg).map_err(|e| {
            ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                "email_failed",
                e.to_string(),
            )
        })
    })
    .await?;
    // Honest delivery state (#130): with no SMTP configured the transport
    // succeeds silently — clients must not read a 200 as "the customer got
    // an email". `transport` is "smtp" only when a relay accepted the send.
    Ok(
        Json(serde_json::json!({ "sent_to": sent_to, "transport": app.email.transport() }))
            .into_response(),
    )
}

/// Body for `POST /invoices/{id}/email-copy` (#112).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailCopyInput {
    pub to: String,
    #[serde(default)]
    pub note: String,
}

/// Cap on the free-form note so an email body cannot be padded (#50).
const COPY_NOTE_MAX_CHARS: usize = 2000;

/// Sends a PDF copy of an (issued) invoice to an arbitrary recipient — the
/// company accountant, a partner, whoever (#112). Admin tier, drafts are not
/// shared, the recipient is validated before anything is sent or persisted,
/// and the send lands in the audit log (#52) as recipient + invoice id —
/// never the PDF bytes.
pub async fn send_invoice_email_copy(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<EmailCopyInput>,
) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    // Fail-loud validation before any side effect (no partial persistence):
    let to = input.to.trim().to_string();
    if !crate::email::valid_address(&to) {
        return Err(ApiError::validation(vec![FieldError::new(
            "to",
            "not a valid email address",
        )]));
    }
    if input.note.chars().count() > COPY_NOTE_MAX_CHARS {
        return Err(ApiError::validation(vec![FieldError::new(
            "note",
            "note too long (max 2000 characters)",
        )]));
    }
    if invoice.status == crate::domain::InvoiceStatus::Draft {
        return Err(ApiError::conflict(
            "issue the invoice before sharing a copy",
        ));
    }
    let customer = get_customer(&app.store, invoice.customer_id)?;
    let (filename, pdf) = invoice_pdf_for_delivery(&app, &invoice)?;
    let org = org_for(&app)?;
    let subject = format!("Copy of invoice {} for {}", invoice.number, customer.name);
    let text = crate::email::render_copy_email(
        &invoice.number,
        &customer.name,
        &actor.0.name,
        Some(&input.note),
        &org.name,
    );
    let profile = load_org(&app)?;
    let msg = crate::email::EmailMessage {
        to: to.clone(),
        subject,
        text,
        html: None,
        attachment: Some((filename, pdf)),
        // Copy email carries the same configured sender identity (#146).
        from_name: sender_field(&profile.from_name),
        reply_to: sender_field(&profile.reply_to),
    };
    let sender = app.email.clone();
    let result = blocking("email copy send", move || {
        sender.send(&msg).map_err(|e| {
            ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                "email_failed",
                e.to_string(),
            )
        })
    })
    .await;
    match result {
        Ok(()) => {
            // #52: recipient + invoice, never the document bytes.
            app.audit.record(
                "invoice_email_copy",
                &format!("{id}:{to}:{}", app.email.transport()),
                app.clock.now(),
            );
            Ok(
                Json(serde_json::json!({ "sent_to": to, "transport": app.email.transport() }))
                    .into_response(),
            )
        }
        Err(e) => Err(e),
    }
}

// ------------------------------------------------------------- payments #34 --

/// Creates a hosted checkout link for an issued invoice (#34). The provider
/// adapter builds the URL and a client reference that the webhook echoes back
/// so the payment can be matched to the invoice.
pub async fn create_checkout(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<CheckoutInput>,
) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    match invoice.status {
        crate::domain::InvoiceStatus::Draft => {
            return Err(ApiError::conflict(
                "issue the invoice before taking payment",
            ));
        }
        crate::domain::InvoiceStatus::Paid => {
            return Err(ApiError::conflict("this invoice is already paid"));
        }
        crate::domain::InvoiceStatus::WrittenOff => {
            return Err(ApiError::conflict("this invoice is written off"));
        }
        crate::domain::InvoiceStatus::Issued | crate::domain::InvoiceStatus::PartlyPaid => {}
    }
    // A partly-paid checkout charges the BALANCE, never the total (#114).
    let amount = invoice.balance_minor();
    if amount == 0 {
        return Err(ApiError::conflict("this invoice has no balance to collect"));
    }
    let provider = provider_or_400(app.payments.get(&input.provider), "payment")?;
    let session = provider
        .create_checkout(invoice.id, &invoice.number, amount, &invoice.currency.0)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    app.audit.record(
        "checkout_created",
        &format!("{}:{id}", session.provider),
        app.clock.now(),
    );
    Ok((StatusCode::CREATED, Json(session)).into_response())
}

/// Provider webhook receiver (#34, public route — authenticated by the
/// provider signature, hence the CSRF exemption). A completed payment flips
/// the invoice to `paid` through the #27 status model; unrelated or replayed
/// events are accepted without effect so providers stop retrying.
pub async fn payment_webhook(
    State(app): State<AppState>,
    Path(provider): Path<String>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    let Some(p) = app.payments.get(&provider) else {
        return Err(ApiError::bad_request("unknown payment provider"));
    };
    let signature = headers
        .get("x-webhook-signature")
        .or_else(|| headers.get("stripe-signature"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let event = p.parse_webhook(&body, signature).map_err(|e| match e {
        crate::payments::PaymentError::BadSignature => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "bad_signature",
            "webhook signature verification failed",
        ),
        // Review A12: provider-facing messages stay generic; details are logged.
        other => {
            tracing::warn!(error = %other, "webhook payload rejected");
            ApiError::bad_request("invalid webhook payload")
        }
    })?;
    let Some(event) = event else {
        return Ok(Json(serde_json::json!({ "ignored": true })).into_response());
    };
    let Some(invoice) = app.store.find_invoice_by_number(&event.invoice_number)? else {
        return Err(ApiError::not_found("invoice"));
    };
    let reference = if event.event_id.is_empty() {
        format!("{}:{}", event.provider, event.reference)
    } else {
        format!("{}:evt-{}", event.provider, event.event_id)
    };
    match invoice.status {
        crate::domain::InvoiceStatus::Paid => {
            if invoice.payments.iter().any(|p| p.reference == reference) {
                return Ok(Json(serde_json::json!({ "status": "already_paid" })).into_response());
            }
            return Err(ApiError::conflict(
                "payment event was not recorded; invoice has already been settled",
            ));
        }
        crate::domain::InvoiceStatus::WrittenOff => {
            // Unexpected money on a forgiven invoice: accepted so providers
            // stop retrying, but it never reopens the document (#114).
            app.audit
                .record("payment_on_written_off", &invoice.number, app.clock.now());
            return Ok(Json(serde_json::json!({ "ignored": "written_off" })).into_response());
        }
        crate::domain::InvoiceStatus::Draft => {
            return Err(ApiError::conflict("invoice is not issued"));
        }
        crate::domain::InvoiceStatus::Issued | crate::domain::InvoiceStatus::PartlyPaid => {}
    }
    // Review A11: a signed event must carry what was ACTUALLY collected and
    // match the invoice, so a refund/other-currency event never settles it.
    if event.currency != invoice.currency.0.to_ascii_uppercase() {
        app.audit.record(
            "payment_currency_mismatch",
            &format!("{}:{}", invoice.number, event.currency),
            app.clock.now(),
        );
        return Err(ApiError::conflict("payment currency mismatch"));
    }
    // #114: the event records a LEDGER PAYMENT of its real amount. A partial
    // lands on partly_paid; over-collection is refused (never a silent
    // overpayment), and a replay of the same provider event is idempotent.
    // #185: the replay/idempotency check no longer runs on this unlocked
    // snapshot — it happens inside `record_payment_once`'s write lock, so two
    // concurrent deliveries of the same event cannot both post.
    let balance = invoice.balance_minor();
    if event.amount_minor > balance {
        app.audit.record(
            "payment_over_balance",
            &format!("{}:{}", invoice.number, event.amount_minor),
            app.clock.now(),
        );
        return Err(ApiError::conflict(
            "payment amount exceeds the remaining invoice balance",
        ));
    }
    match app.store.record_payment_once(
        invoice.id,
        Some(event.amount_minor),
        reference.clone(),
        event.provider.clone(),
        app.clock.now(),
    ) {
        Ok(None) => Ok(Json(
            serde_json::json!({ "status": "already_processed", "invoice": invoice.number }),
        )
        .into_response()),
        Ok(Some(settled)) => {
            app.audit
                .record("payment_received", &settled.number, app.clock.now());
            let status = if settled.status == crate::domain::InvoiceStatus::Paid {
                "paid"
            } else {
                "partly_paid"
            };
            Ok(
                Json(serde_json::json!({ "status": status, "invoice": settled.number }))
                    .into_response(),
            )
        }
        // A competing event may have changed the balance after the unlocked
        // preflight. Acknowledge only if this exact event is in the ledger.
        Err(crate::store::StoreError::Conflict(_)) => {
            let fresh = app.store.get_invoice(invoice.id)?;
            match fresh {
                Some(i) if i.payments.iter().any(|p| p.reference == reference) => Ok(Json(
                    serde_json::json!({ "status": "already_processed", "invoice": i.number }),
                )
                .into_response()),
                Some(_) => Err(ApiError::conflict(
                    "payment event was not recorded; retry after reconciling invoice balance",
                )),
                None => Err(ApiError::conflict("invoice is not issued")),
            }
        }
        Err(other) => Err(other.into()),
    }
}

/// Body for `POST /invoices/{id}/sync`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncInput {
    pub provider: String,
}

/// Record one sync attempt from a provider push outcome, translating the
/// Ok/Err into Synced/Failed with the shared `SyncAttempt` shape (#189).
fn record_sync_from(
    app: &AppState,
    provider: &str,
    kind: &str,
    invoice: &Invoice,
    detail: String,
    outcome: Result<String, crate::accounting::SyncError>,
) -> Result<crate::accounting::SyncRecord, ApiError> {
    let (remote_id, status, error) = match outcome {
        Ok(remote) => (remote, crate::accounting::SyncStatus::Synced, String::new()),
        Err(e) => (
            String::new(),
            crate::accounting::SyncStatus::Failed,
            e.to_string(),
        ),
    };
    Ok(crate::accounting::record_sync(
        &app.store,
        crate::accounting::SyncAttempt {
            provider,
            kind,
            invoice,
            remote_id,
            status,
            error,
            now: app.clock.now(),
            detail,
        },
    )?)
}

/// Copies an issued invoice to the accounting provider (#33). Idempotent:
/// a previously synced invoice is a no-op, and failures are recorded rather
/// than thrown (sync must never block the invoice flow).
pub async fn sync_invoice(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<SyncInput>,
) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    if invoice.status == crate::domain::InvoiceStatus::Draft {
        return Err(ApiError::conflict("issue the invoice before syncing it"));
    }
    let provider = provider_or_400(app.accounting.get(&input.provider), "accounting")?;
    let customer = get_customer(&app.store, invoice.customer_id)?;
    let records = crate::accounting::list_records(&app.store)?;
    // A legacy `Paid` invoice predating the ledger (#114) is represented as
    // one payment of the total under the empty detail.
    let detail_of = |p: &crate::domain::InvoicePayment| p.id.to_string();
    let pending: Vec<(String, u64, String, chrono::DateTime<chrono::Utc>)> =
        if invoice.payments.is_empty() {
            if invoice.paid_minor() > 0 {
                vec![(
                    String::new(),
                    invoice.paid_minor(),
                    invoice.payment_reference.clone(),
                    invoice.paid_at.unwrap_or_else(|| app.clock.now()),
                )]
            } else {
                Vec::new()
            }
        } else {
            invoice
                .payments
                .iter()
                .filter(|p| {
                    !records.iter().any(|r| {
                        r.provider == provider.name()
                            && r.kind == "payment"
                            && r.invoice_id == invoice.id
                            && r.detail == detail_of(p)
                            && r.status == crate::accounting::SyncStatus::Synced
                    })
                })
                .map(|p| {
                    (
                        detail_of(p),
                        p.amount_minor,
                        p.reference.clone(),
                        p.received_at,
                    )
                })
                .collect()
        };
    if let Some(prev) = records
        .iter()
        .find(|r| {
            r.provider == provider.name() && r.kind == "invoice" && r.invoice_id == invoice.id
        })
        .cloned()
        .filter(|r| r.status == crate::accounting::SyncStatus::Synced)
    {
        // Invoice already synced: push every collected-but-unsynced payment
        // against the remote invoice (#33, #114), else a plain no-op.
        if pending.is_empty() {
            return Ok(Json(serde_json::json!({ "record": prev, "noop": true })).into_response());
        }
        let mut last: Option<crate::accounting::SyncRecord> = None;
        for pending in &pending {
            let outcome = provider.push_payment(
                &prev.remote_id,
                pending.1,
                &invoice.currency.0,
                &pending.2,
                pending.3,
            );
            let rec = record_sync_from(
                &app,
                provider.name(),
                "payment",
                &invoice,
                pending.0.clone(),
                outcome,
            )?;
            if rec.status == crate::accounting::SyncStatus::Synced {
                app.audit
                    .record("accounting_payment_sync", &invoice.number, app.clock.now());
            }
            last = Some(rec);
        }
        return Ok(Json(
            serde_json::json!({ "record": last, "invoice_synced": prev.remote_id, "payments": pending.len() }),
        )
        .into_response());
    }
    let key = crate::accounting::invoice_key(provider.name(), &invoice.number);
    let doc = crate::accounting::InvoiceDoc {
        invoice: &invoice,
        customer: &customer,
    };
    match provider.push_invoice(&doc, &key) {
        Ok(remote) => {
            // Synced invoices also carry their ledger, in real amounts.
            for pending in &pending {
                let outcome = provider.push_payment(
                    &remote,
                    pending.1,
                    &invoice.currency.0,
                    &pending.2,
                    pending.3,
                );
                if let Err(e) = &outcome {
                    tracing::warn!(error = %e, "payment sync failed (invoice synced anyway)");
                }
                record_sync_from(
                    &app,
                    provider.name(),
                    "payment",
                    &invoice,
                    pending.0.clone(),
                    outcome,
                )?;
            }
            let rec = record_sync_from(
                &app,
                provider.name(),
                "invoice",
                &invoice,
                String::new(),
                Ok(remote),
            )?;
            app.audit
                .record("accounting_sync", &invoice.number, app.clock.now());
            Ok(Json(serde_json::json!({ "record": rec })).into_response())
        }
        Err(e) => {
            let rec = record_sync_from(
                &app,
                provider.name(),
                "invoice",
                &invoice,
                String::new(),
                Err(e),
            )?;
            Ok(Json(serde_json::json!({ "record": rec, "failed": true })).into_response())
        }
    }
}

/// Visible sync status for the dashboard (#33): every provider record.
pub async fn sync_status(State(app): State<AppState>) -> ApiResult {
    let records = crate::accounting::list_records(&app.store)?;
    Ok(Json(serde_json::json!({
        "records": records,
        "providers": app.accounting.names(),
    }))
    .into_response())
}

pub async fn invoice_report_handler(
    State(app): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> ApiResult {
    let (from, to) = q.dates()?;
    let invoices = app.store.list_invoices()?;
    let customers = app.store.list_customers()?;
    let report = report::invoice_report(&invoices, &customers, from, to);
    Ok(Json(report).into_response())
}

pub async fn invoice_export_csv(State(app): State<AppState>) -> ApiResult {
    let invoices = app.store.list_invoices()?;
    let customers = app.store.list_customers()?;
    let csv = report::invoice_csv(&invoices, &customers);
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        csv,
    )
        .into_response())
}

pub async fn delete_invoice(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    if invoice.status != InvoiceStatus::Draft {
        return Err(ApiError::conflict("only a draft invoice can be deleted"));
    }
    app.store.delete_invoice(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ------------------------------------------------- templates & document --

/// The singleton invoice template (#116) as stored; absent means defaults.
pub(crate) fn load_template(app: &AppState) -> Result<crate::domain::InvoiceTemplate, ApiError> {
    Ok(app
        .store
        .read_json_rel::<crate::domain::InvoiceTemplate>("invoice_template.json")?
        .unwrap_or_default())
}

/// Resolved variables + rendered content for one invoice/customer/template.
pub(crate) fn content_for(
    invoice: &Invoice,
    customer: &crate::domain::Customer,
    template: &crate::domain::InvoiceTemplate,
) -> crate::template::DocContent {
    let issue_day = invoice
        .issued_at
        .or(Some(invoice.created_at))
        .map(|dt| dt.date_naive());
    let vars = crate::template::vars_for(invoice, &customer.name, issue_day);
    crate::template::doc_content(template, customer, &vars)
}

/// Render + archive the PDF from the invoice as stored today, using the
/// current template content (the legacy path of #113/#116). Deterministic in
/// (invoice, customer, template, org): a re-render while nothing was edited
/// yields identical bytes.
pub(crate) fn render_pdf_now(app: &AppState, invoice: &Invoice) -> Result<Vec<u8>, ApiError> {
    let customer = get_customer(&app.store, invoice.customer_id)?;
    let template = load_template(app)?;
    let mut doc = crate::pdf::doc_for_labeled(invoice, &customer, &org_for(app)?, &template.labels);
    let content = content_for(invoice, &customer, &template);
    crate::template::apply_doc_content(&mut doc, &content);
    Ok(crate::pdf::render_invoice_pdf(&doc))
}

/// `due_date` at issue (#116): customer terms → org default terms → the
/// legacy net-14-from-`period_to`. Terms count from the issue day; the legacy
/// fallback keeps its exact original arithmetic so nothing shifts.
pub(crate) fn resolve_due_date(
    invoice: &Invoice,
    customer: &crate::domain::Customer,
    template: &crate::domain::InvoiceTemplate,
    now: chrono::DateTime<chrono::Utc>,
) -> chrono::NaiveDate {
    let terms = customer
        .payment_terms
        .as_ref()
        .or(template.payment_terms.as_ref());
    if let Some(days) = terms.and_then(|t| t.fixed_days()) {
        let from = now.date_naive();
        return from
            .checked_add_days(chrono::Days::new(u64::from(days)))
            .unwrap_or(from);
    }
    invoice
        .period_to
        .checked_add_days(chrono::Days::new(14))
        .unwrap_or(invoice.period_to)
}

/// `GET /admin/invoice-template` — the stored template plus the variable
/// cheatsheet, so the GUI renders the same table the validator enforces.
pub async fn invoice_template_get(State(app): State<AppState>) -> ApiResult {
    let template = load_template(&app)?;
    Ok(Json(serde_json::json!({
        "template": template,
        "variables": crate::template::VAR_HELP
            .iter()
            .map(|(name, example)| serde_json::json!({"name": name, "example": example}))
            .collect::<Vec<_>>(),
    }))
    .into_response())
}

/// `PUT /admin/invoice-template` — validate fail-loud (unknown `%tokens%`,
/// oversized fields, malformed payment terms), render the pieces against a
/// synthetic invoice as an extra gauntlet, then persist atomically. A
/// rejection persists nothing.
pub async fn invoice_template_put(
    State(app): State<AppState>,
    ValidJson(template): ValidJson<crate::domain::InvoiceTemplate>,
) -> ApiResult {
    let mut errors = Vec::new();
    crate::template::validate_field(
        "subject",
        &template.subject,
        crate::template::SUBJECT_MAX,
        &mut errors,
    );
    crate::template::validate_field(
        "body",
        &template.body,
        crate::template::BODY_MAX,
        &mut errors,
    );
    crate::template::validate_field(
        "footer",
        &template.footer,
        crate::template::FOOTER_MAX,
        &mut errors,
    );
    crate::domain::validate_payment_terms(template.payment_terms.as_ref(), &mut errors);
    template.labels.validate(&mut errors);
    if errors.is_empty() {
        // Render against a synthetic sample: anything that fails here would
        // fail on a real invoice later. Never persists a broken template.
        let sample = Invoice {
            id: Uuid::nil(),
            number: "INV-0000".into(),
            customer_id: Uuid::nil(),
            currency: crate::domain::Currency("EUR".into()),
            period_from: chrono::NaiveDate::MIN + chrono::Duration::days(1),
            period_to: chrono::NaiveDate::MIN + chrono::Duration::days(2),
            lines: vec![],
            total_minor: 0,
            status: crate::domain::InvoiceStatus::Draft,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
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
        };
        let vars = crate::template::vars_for(&sample, "Sample Customer", None);
        // interpolate/parse cannot fail by design; the call is the assertion
        // that no panic escapes for this template.
        let _ = crate::template::doc_content(&template, &synthetic_customer(), &vars);
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    app.store
        .write_json_rel("invoice_template.json", &template)?;
    Ok(Json(template).into_response())
}

fn synthetic_customer() -> crate::domain::Customer {
    crate::domain::Customer {
        id: Uuid::nil(),
        name: "Sample Customer".into(),
        currency: crate::domain::Currency("EUR".into()),
        default_rate_minor: 0,
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

/// `GET /invoices/{id}/document` — the live `{subject, html}` preview:
/// exactly the content the PDF (#113) and the email (#35) render from,
/// resolved from the *current* template + customer, not a baked snapshot.
pub async fn invoice_document(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    let customer = get_customer(&app.store, invoice.customer_id)?;
    let template = load_template(&app)?;
    let content = content_for(&invoice, &customer, &template);
    let org = org_for(&app)?;
    let subject = content
        .subject
        .clone()
        .or_else(|| Some(crate::email::invoice_subject(&invoice.number, &org.name)))
        .unwrap_or_default();
    let html = crate::template::email_html(&content).unwrap_or_default();
    Ok(Json(serde_json::json!({ "subject": subject, "html": html })).into_response())
}

// ------------------------------------------------- manual lines & edit ----

/// Body for `POST /invoices/manual` (#143).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualInvoiceInput {
    pub customer_id: Uuid,
    /// Absent = the customer's currency.
    #[serde(default)]
    pub currency: Option<Currency>,
    #[serde(default)]
    pub tax_hundredths: u16,
    #[serde(default)]
    pub discount_hundredths: u16,
    pub lines: Vec<crate::domain::ManualLineInput>,
}

/// A draft line as submitted (#187). Mirrors `domain::InvoiceLine` but is an
/// INPUT type with `deny_unknown_fields`: the shared validation invariant
/// ("every payload rejected before any write") could not apply to the stored
/// type without also making every legacy invoice document strict at read
/// time (a future added-then-removed field would brick `list_invoices`, and
/// via #185's fail-closed locks, entry editing). Conversion is field-by-field;
/// `amount_minor` is always recomputed server-side for manual lines.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceLineInput {
    pub kind: crate::domain::LineKind,
    pub date: chrono::NaiveDate,
    #[serde(default)]
    pub entry_id: Option<Uuid>,
    #[serde(default)]
    pub expense_id: Option<Uuid>,
    #[serde(default)]
    pub project_code: Option<crate::domain::ProjectCode>,
    #[serde(default)]
    pub task_code: Option<crate::domain::ProjectCode>,
    #[serde(default)]
    pub hours: Option<crate::domain::Hours>,
    #[serde(default)]
    pub rate_minor: Option<u64>,
    #[serde(default)]
    pub amount_minor: u64,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub quantity_hundredths: Option<u32>,
    #[serde(default)]
    pub unit_price_minor: Option<u64>,
    #[serde(default)]
    pub item_kind: Option<crate::domain::LineItemKind>,
}

impl From<&InvoiceLineInput> for crate::domain::InvoiceLine {
    fn from(l: &InvoiceLineInput) -> Self {
        crate::domain::InvoiceLine {
            kind: l.kind,
            date: l.date,
            entry_id: l.entry_id,
            expense_id: l.expense_id,
            project_code: l.project_code.clone(),
            task_code: l.task_code.clone(),
            hours: l.hours,
            rate_minor: l.rate_minor,
            amount_minor: l.amount_minor,
            note: l.note.clone(),
            quantity_hundredths: l.quantity_hundredths,
            unit_price_minor: l.unit_price_minor,
            item_kind: l.item_kind,
        }
    }
}

/// Body for `PUT /invoices/{id}` (#143): draft-only full replacement of the
/// line set + percentages. Tracked lines must come back exactly as stored
/// (identity and amounts are enforced server-side); manual lines are free.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceEditInput {
    pub lines: Vec<InvoiceLineInput>,
    #[serde(default)]
    pub tax_hundredths: u16,
    #[serde(default)]
    pub discount_hundredths: u16,
}

fn percent_field(value: u16, field: &str, errors: &mut Vec<FieldError>) {
    if value > 10_000 {
        errors.push(FieldError::new(field, "at most 100.00 percent"));
    }
}

/// Create a draft invoice from manual Product/Service lines (#143). Money is
/// computed server-side with integer math; the draft locks nothing (#8).
pub async fn create_manual_invoice(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<ManualInvoiceInput>,
) -> ApiResult {
    let customer = get_customer(&app.store, input.customer_id)?;
    let mut errors = Vec::new();
    if input.lines.is_empty() {
        errors.push(FieldError::new("lines", "at least one line is required"));
    }
    percent_field(input.tax_hundredths, "tax_hundredths", &mut errors);
    percent_field(
        input.discount_hundredths,
        "discount_hundredths",
        &mut errors,
    );
    let lines = crate::domain::validate_manual_lines(&input.lines, &mut errors);
    validate_line_projects(&app, &customer.id, &lines, &mut errors)?;
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let subtotal = lines.iter().map(|l| l.amount_minor).sum();
    let totals =
        crate::domain::invoice_totals(subtotal, input.tax_hundredths, input.discount_hundredths);
    let today = app.clock.today();
    let draft = Invoice {
        id: Uuid::new_v4(),
        number: String::new(),
        customer_id: customer.id,
        currency: input
            .currency
            .clone()
            .unwrap_or_else(|| customer.currency.clone()),
        period_from: today,
        period_to: today,
        lines,
        total_minor: totals.total_minor,
        status: crate::domain::InvoiceStatus::Draft,
        created_at: app.clock.now(),
        issued_at: None,
        due_date: None,
        paid_at: None,
        payment_reference: String::new(),
        pdf: None,
        payments: vec![],
        write_off_reason: String::new(),
        written_off_at: None,
        tax_hundredths: input.tax_hundredths,
        discount_hundredths: input.discount_hundredths,
    };
    let created = app.store.create_invoice(draft)?;
    Ok((StatusCode::CREATED, Json(created)).into_response())
}

fn validate_line_projects(
    app: &AppState,
    customer_id: &Uuid,
    lines: &[crate::domain::InvoiceLine],
    errors: &mut Vec<FieldError>,
) -> Result<(), ApiError> {
    // #187: one store read per call (was one per line), and a store failure
    // surfaces as its own error instead of an `.unwrap_or(false)` that turned
    // an IO fault into a bogus "unknown project" 422.
    if lines.iter().all(|l| l.project_code.is_none()) {
        return Ok(());
    }
    let projects = app.store.list_projects(*customer_id)?;
    for (i, l) in lines.iter().enumerate() {
        if let Some(code) = &l.project_code
            && !projects.iter().any(|p| &p.code == code)
        {
            errors.push(FieldError::new(
                format!("lines[{i}].project_code"),
                "unknown project for this customer",
            ));
        }
    }
    Ok(())
}

/// Edit a draft invoice (#143): replace the line set and percentages under a
/// Draft-only guard. Tracked lines must be preserved verbatim; manual lines
/// are re-priced from their integer quantity x unit price. 409 for anything
/// that is not a draft; validation failures persist nothing.
pub async fn update_invoice_draft(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(edit): ValidJson<InvoiceEditInput>,
) -> ApiResult {
    let invoice = get_invoice_or_404(&app, id)?;
    if invoice.status != crate::domain::InvoiceStatus::Draft {
        return Err(ApiError::conflict("only a draft invoice can be edited"));
    }
    let mut errors = Vec::new();
    if edit.lines.is_empty() {
        errors.push(FieldError::new("lines", "at least one line is required"));
    }
    if edit.lines.len() > crate::domain::MANUAL_LINE_MAX {
        errors.push(FieldError::new(
            "lines",
            format!("at most {} lines", crate::domain::MANUAL_LINE_MAX),
        ));
    }
    percent_field(edit.tax_hundredths, "tax_hundredths", &mut errors);
    percent_field(edit.discount_hundredths, "discount_hundredths", &mut errors);
    let stored_tracked: Vec<&crate::domain::InvoiceLine> = invoice
        .lines
        .iter()
        .filter(|l| l.entry_id.is_some() || l.expense_id.is_some())
        .collect();
    let mut out_lines: Vec<crate::domain::InvoiceLine> = Vec::new();
    for (i, l) in edit.lines.iter().enumerate() {
        let is_tracked = l.entry_id.is_some() || l.expense_id.is_some();
        if is_tracked {
            // Tracked lines survive exactly as snapshotted (the rate-snapshot
            // guarantee of #8): identity + numbers must match the stored one.
            let Some(orig) = stored_tracked
                .iter()
                .find(|o| o.entry_id == l.entry_id && o.expense_id == l.expense_id)
            else {
                errors.push(FieldError::new(
                    format!("lines[{i}]"),
                    "tracked lines must come from this draft",
                ));
                continue;
            };
            let candidate: crate::domain::InvoiceLine = l.into();
            if &candidate != *orig {
                errors.push(FieldError::new(
                    format!("lines[{i}]"),
                    "tracked lines are immutable (rate snapshot #8)",
                ));
                continue;
            }
            out_lines.push((*orig).clone());
        } else {
            // Manual line: re-priced server-side. Uses the SAME bounds as
            // POST /invoices/manual (#187) — previously a divergent inline
            // copy that could drift.
            let qty = l.quantity_hundredths.unwrap_or(0);
            let price = l.unit_price_minor.unwrap_or(0);
            if l.kind != crate::domain::LineKind::Fixed || l.item_kind.is_none() {
                errors.push(FieldError::new(
                    format!("lines[{i}].kind"),
                    "manual lines are Fixed with an item kind",
                ));
                continue;
            }
            crate::domain::check_manual_line(
                &l.note,
                qty,
                price,
                &format!("lines[{i}].note"),
                &format!("lines[{i}].quantity_hundredths"),
                &format!("lines[{i}].unit_price_minor"),
                &mut errors,
            );
            let mut line: crate::domain::InvoiceLine = l.into();
            line.amount_minor = crate::domain::manual_amount_minor(qty, price);
            line.note = l.note.trim().to_string();
            out_lines.push(line);
        }
    }
    validate_line_projects(&app, &invoice.customer_id, &out_lines, &mut errors)?;
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let subtotal = out_lines.iter().map(|l| l.amount_minor).sum();
    let totals =
        crate::domain::invoice_totals(subtotal, edit.tax_hundredths, edit.discount_hundredths);
    let candidate = Invoice {
        lines: out_lines,
        total_minor: totals.total_minor,
        tax_hundredths: edit.tax_hundredths,
        discount_hundredths: edit.discount_hundredths,
        ..invoice.clone()
    };
    let updated = app.store.replace_invoice_draft(id, &candidate)?;
    Ok(Json(updated).into_response())
}

// ------------------------------------------------- product/service catalog --

fn validate_item_type(input: &crate::domain::ItemTypeInput) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        errors.push(FieldError::new("name", "required, at most 120 characters"));
    }
    if input.description.chars().count() > 500 {
        errors.push(FieldError::new("description", "at most 500 characters"));
    }
    if input.default_price_minor > 100_000_000 {
        errors.push(FieldError::new("default_price_minor", "at most 100000000"));
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    Ok(())
}

/// `GET /admin/item-types` — catalog incl. archived (#147).
pub async fn list_item_types(State(app): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::json!({ "item_types": app.store.list_item_types()? })).into_response())
}

pub async fn create_item_type(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<crate::domain::ItemTypeInput>,
) -> ApiResult {
    validate_item_type(&input)?;
    let item = crate::domain::ItemType {
        id: Uuid::new_v4(),
        name: input.name.trim().to_string(),
        description: input.description.trim().to_string(),
        kind: input.kind,
        default_price_minor: input.default_price_minor,
        currency: input
            .currency
            .unwrap_or_else(|| crate::domain::Currency("EUR".into())),
        active: input.active,
        created_at: app.clock.now(),
    };
    app.store.put_item_type(&item)?;
    Ok((StatusCode::CREATED, Json(item)).into_response())
}

pub async fn update_item_type(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<crate::domain::ItemTypeInput>,
) -> ApiResult {
    let Some(existing) = app.store.get_item_type(id)? else {
        return Err(ApiError::not_found("item type"));
    };
    validate_item_type(&input)?;
    let updated = crate::domain::ItemType {
        id,
        name: input.name.trim().to_string(),
        description: input.description.trim().to_string(),
        kind: input.kind,
        default_price_minor: input.default_price_minor,
        currency: input.currency.unwrap_or(existing.currency),
        active: input.active,
        created_at: existing.created_at,
    };
    app.store.put_item_type(&updated)?;
    Ok(Json(updated).into_response())
}

/// Delete a catalog item; invoice lines keep their snapshots (#147).
pub async fn delete_item_type(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_item_type(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Empty string in the org profile means "omit" (#146).
fn sender_field(value: &str) -> Option<String> {
    let v = value.trim();
    (!v.is_empty()).then(|| v.to_string())
}
