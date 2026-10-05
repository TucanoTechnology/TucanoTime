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
    let projects = app.store.list_projects(customer.id)?;
    let mut tasks = Vec::new();
    for p in &projects {
        tasks.extend(app.store.list_tasks(customer.id, &p.code.0)?);
    }
    let users = app.store.list_users()?;
    let entries = app.store.list_range(from, to)?;
    let expenses = app.store.list_expenses()?;
    // Items already on an issued invoice are excluded from a new one.
    let invoices = app.store.list_invoices()?;
    let issued: Vec<&Invoice> = invoices
        .iter()
        .filter(|i| i.status == InvoiceStatus::Issued)
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
        tasks: &tasks,
        users: &users,
        entries: &entries,
        expenses: &expenses,
        excluded_entries: &excluded_entries,
        excluded_expenses: &excluded_expenses,
        include_expenses: input.include_expenses,
    };
    let invoice = generate_invoice(
        String::new(),
        &customer,
        &sources,
        from,
        to,
        app.clock.now(),
    )
    .map_err(invoice_error)?;
    let invoice = app.store.create_invoice(invoice)?;
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

/// The issuing organisation for invoice documents (#113), from
/// `config.json`/`TUCANO_ORG_NAME` (#94); the email surface shares it.
pub(crate) fn org_for(app: &AppState) -> crate::pdf::Org {
    crate::pdf::Org {
        name: app
            .cfg()
            .get_str("org_name", &crate::appconfig::process_env),
    }
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
    let due = resolve_due_date(&invoice, &customer, &template, app.clock.now());
    // Render against the to-be-issued snapshot: the PDF is the issue-time
    // document, and bytes are pure in this state (legacy re-renders match).
    let mut snapshot = invoice;
    snapshot.status = crate::domain::InvoiceStatus::Issued;
    snapshot.issued_at = Some(app.clock.now());
    snapshot.due_date = Some(due);
    let mut doc = crate::pdf::doc_for(&snapshot, &customer, &org_for(&app));
    let content = content_for(&snapshot, &customer, &template);
    crate::template::apply_doc_content(&mut doc, &content);
    let pdf = crate::pdf::render_invoice_pdf(&doc);
    let issued = app.store.issue_invoice(id, app.clock.now(), due, &pdf)?;
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
        Some(b) => (b, format!("{}.pdf", sanitize_number(&invoice.number))),
        None => {
            let b = render_pdf_now(&app, &invoice)?;
            // Persist the archive + hint; a failure here is a plain 500 and
            // costs nothing (the invoice JSON remains the record of truth).
            let attached = app.store.attach_invoice_pdf(id, &b, app.clock.now())?;
            let name = attached
                .pdf
                .map(|h| h.filename)
                .unwrap_or_else(|| format!("{}.pdf", sanitize_number(&invoice.number)));
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
fn sanitize_number(number: &str) -> String {
    number
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

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
        // Over-balance is arithmetic on stored state: 409, message safe.
        Err(crate::store::StoreError::Conflict(m)) if m.starts_with("payment of ") => {
            return Err(ApiError::conflict(m));
        }
        Err(e) => return Err(e.into()),
    };
    let paid = invoice.payments.last().expect("just recorded");
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
            .unwrap_or_else(|| format!("{}.pdf", sanitize_number(&invoice.number)));
        return Ok((name, bytes));
    }
    let bytes = render_pdf_now(app, invoice)?;
    let attached = app
        .store
        .attach_invoice_pdf(invoice.id, &bytes, app.clock.now())?;
    let name = attached
        .pdf
        .map(|h| h.filename)
        .unwrap_or_else(|| format!("{}.pdf", sanitize_number(&invoice.number)));
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
    if customer.email.trim().is_empty() {
        return Err(ApiError::validation(vec![FieldError::new(
            "email",
            "customer has no billing email on file",
        )]));
    }
    let (filename, pdf) = invoice_pdf_for_delivery(&app, &invoice)?;
    let amount = money_for_email(invoice.total_minor, &invoice.currency.0);
    let due = invoice.due_date.map(|d| d.to_string());
    let org = org_for(&app);
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
    let msg = crate::email::EmailMessage {
        to: customer.email.clone(),
        subject,
        text,
        html,
        attachment: Some((filename, pdf)),
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
    Ok(Json(serde_json::json!({ "sent_to": sent_to })).into_response())
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
    let org = org_for(&app);
    let subject = format!("Copy of invoice {} for {}", invoice.number, customer.name);
    let text = crate::email::render_copy_email(
        &invoice.number,
        &customer.name,
        &actor.0.name,
        Some(&input.note),
        &org.name,
    );
    let msg = crate::email::EmailMessage {
        to: to.clone(),
        subject,
        text,
        html: None,
        attachment: Some((filename, pdf)),
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
            app.audit
                .record("invoice_email_copy", &format!("{id}:{to}"), app.clock.now());
            Ok(Json(serde_json::json!({ "sent_to": to })).into_response())
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
    match invoice.status {
        crate::domain::InvoiceStatus::Paid => {
            // Replay-safe: the provider can deliver the same event more than once.
            return Ok(Json(serde_json::json!({ "status": "already_paid" })).into_response());
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
    let reference = if event.event_id.is_empty() {
        format!("{}:{}", event.provider, event.reference)
    } else {
        format!("{}:evt-{}", event.provider, event.event_id)
    };
    if invoice.payments.iter().any(|p| p.reference == reference) {
        return Ok(Json(
            serde_json::json!({ "status": "already_processed", "invoice": invoice.number }),
        )
        .into_response());
    }
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
    match app.store.record_payment(
        invoice.id,
        Some(event.amount_minor),
        reference,
        event.provider.clone(),
        app.clock.now(),
    ) {
        Ok(settled) => {
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
        // Concurrently settled or raced: re-read tells the provider which.
        Err(crate::store::StoreError::Conflict(_)) => {
            let fresh = app.store.get_invoice(invoice.id)?;
            match fresh.map(|i| i.status) {
                Some(crate::domain::InvoiceStatus::Paid) => {
                    Ok(Json(serde_json::json!({ "status": "already_paid" })).into_response())
                }
                Some(_) => {
                    Ok(Json(serde_json::json!({ "status": "already_processed" })).into_response())
                }
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
        for (detail, amount, reference, paid_at) in &pending {
            last = Some(
                match provider.push_payment(
                    &prev.remote_id,
                    *amount,
                    &invoice.currency.0,
                    reference,
                    *paid_at,
                ) {
                    Ok(remote) => {
                        app.audit.record(
                            "accounting_payment_sync",
                            &invoice.number,
                            app.clock.now(),
                        );
                        crate::accounting::record_sync(
                            &app.store,
                            crate::accounting::SyncAttempt {
                                provider: provider.name(),
                                kind: "payment",
                                invoice: &invoice,
                                remote_id: remote,
                                status: crate::accounting::SyncStatus::Synced,
                                error: String::new(),
                                now: app.clock.now(),
                                detail: detail.clone(),
                            },
                        )?
                    }
                    Err(e) => crate::accounting::record_sync(
                        &app.store,
                        crate::accounting::SyncAttempt {
                            provider: provider.name(),
                            kind: "payment",
                            invoice: &invoice,
                            remote_id: String::new(),
                            status: crate::accounting::SyncStatus::Failed,
                            error: e.to_string(),
                            now: app.clock.now(),
                            detail: detail.clone(),
                        },
                    )?,
                },
            );
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
            for (detail, amount, reference, paid_at) in &pending {
                if let Err(e) = provider.push_payment(
                    &remote,
                    *amount,
                    &invoice.currency.0,
                    reference,
                    *paid_at,
                ) {
                    crate::accounting::record_sync(
                        &app.store,
                        crate::accounting::SyncAttempt {
                            provider: provider.name(),
                            kind: "payment",
                            invoice: &invoice,
                            remote_id: String::new(),
                            status: crate::accounting::SyncStatus::Failed,
                            error: e.to_string(),
                            now: app.clock.now(),
                            detail: detail.clone(),
                        },
                    )?;
                    tracing::warn!(error = %e, "payment sync failed (invoice synced anyway)");
                } else {
                    crate::accounting::record_sync(
                        &app.store,
                        crate::accounting::SyncAttempt {
                            provider: provider.name(),
                            kind: "payment",
                            invoice: &invoice,
                            remote_id: String::new(),
                            status: crate::accounting::SyncStatus::Synced,
                            error: String::new(),
                            now: app.clock.now(),
                            detail: detail.clone(),
                        },
                    )?;
                }
            }
            let rec = crate::accounting::record_sync(
                &app.store,
                crate::accounting::SyncAttempt {
                    provider: provider.name(),
                    kind: "invoice",
                    invoice: &invoice,
                    remote_id: remote,
                    status: crate::accounting::SyncStatus::Synced,
                    error: String::new(),
                    now: app.clock.now(),
                    detail: String::new(),
                },
            )?;
            app.audit
                .record("accounting_sync", &invoice.number, app.clock.now());
            Ok(Json(serde_json::json!({ "record": rec })).into_response())
        }
        Err(e) => {
            let rec = crate::accounting::record_sync(
                &app.store,
                crate::accounting::SyncAttempt {
                    provider: provider.name(),
                    kind: "invoice",
                    invoice: &invoice,
                    remote_id: String::new(),
                    status: crate::accounting::SyncStatus::Failed,
                    error: e.to_string(),
                    now: app.clock.now(),
                    detail: String::new(),
                },
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
    let mut doc = crate::pdf::doc_for(invoice, &customer, &org_for(app));
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
    let org = org_for(&app);
    let subject = content
        .subject
        .clone()
        .or_else(|| Some(crate::email::invoice_subject(&invoice.number, &org.name)))
        .unwrap_or_default();
    let html = crate::template::email_html(&content).unwrap_or_default();
    Ok(Json(serde_json::json!({ "subject": subject, "html": html })).into_response())
}
