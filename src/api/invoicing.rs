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
    // net-14 terms (fallback = same day; only reachable at date extremes)
    let due = invoice
        .period_to
        .checked_add_days(chrono::Days::new(14))
        .unwrap_or(invoice.period_to);
    let customer = get_customer(&app.store, invoice.customer_id)?;
    // Render against the to-be-issued snapshot: the PDF is the issue-time
    // document, and bytes are pure in this state (legacy re-renders match).
    let mut snapshot = invoice;
    snapshot.status = crate::domain::InvoiceStatus::Issued;
    snapshot.issued_at = Some(app.clock.now());
    snapshot.due_date = Some(due);
    let doc = crate::pdf::doc_for(&snapshot, &customer, &org_for(&app));
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
            let customer = get_customer(&app.store, invoice.customer_id)?;
            let doc = crate::pdf::doc_for(&invoice, &customer, &org_for(&app));
            let b = crate::pdf::render_invoice_pdf(&doc);
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
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutInput {
    pub provider: String,
}

/// Mark an issued invoice as paid (#27).
pub async fn pay_invoice(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<PayInput>,
) -> ApiResult {
    // State check + transition under one lock (review B4).
    get_invoice_or_404(&app, id)?;
    let invoice = app
        .store
        .pay_invoice(id, app.clock.now(), input.reference)?;
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
    let customer = get_customer(&app.store, invoice.customer_id)?;
    let doc = crate::pdf::doc_for(invoice, &customer, &org_for(app));
    let bytes = crate::pdf::render_invoice_pdf(&doc);
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
    let text = crate::email::render_invoice_email(
        &customer.name,
        &invoice.number,
        &amount,
        due.as_deref(),
        &org.name,
        true,
    );
    let subject = crate::email::invoice_subject(&invoice.number, &org.name);
    let msg = crate::email::EmailMessage {
        to: customer.email.clone(),
        subject,
        text,
        html: None,
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
        crate::domain::InvoiceStatus::Issued => {}
    }
    let provider = provider_or_400(app.payments.get(&input.provider), "payment")?;
    let session = provider
        .create_checkout(
            invoice.id,
            &invoice.number,
            invoice.total_minor,
            &invoice.currency.0,
        )
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
    if invoice.status == crate::domain::InvoiceStatus::Paid {
        // Replay-safe: the provider can deliver the same event more than once.
        return Ok(Json(serde_json::json!({ "status": "already_paid" })).into_response());
    }
    // Review A11: a signed event must carry what was ACTUALLY collected and
    // match the invoice, or a partial/refund event could settle in full.
    if event.currency != invoice.currency.0.to_ascii_uppercase() {
        app.audit.record(
            "payment_currency_mismatch",
            &format!("{}:{}", invoice.number, event.currency),
            app.clock.now(),
        );
        return Err(ApiError::conflict("payment currency mismatch"));
    }
    if event.amount_minor < invoice.total_minor {
        app.audit.record(
            "payment_underpaid",
            &format!("{}:{}", invoice.number, event.amount_minor),
            app.clock.now(),
        );
        return Err(ApiError::conflict("payment amount is below invoice total"));
    }
    let reference = format!("{}:{}", event.provider, event.reference);
    match app
        .store
        .pay_invoice(invoice.id, app.clock.now(), reference)
    {
        Ok(paid) => {
            app.audit
                .record("payment_received", &paid.number, app.clock.now());
            Ok(
                Json(serde_json::json!({ "status": "paid", "invoice": paid.number }))
                    .into_response(),
            )
        }
        // Draft or concurrently settled: re-read tells the provider which.
        Err(crate::store::StoreError::Conflict(_)) => {
            let fresh = app.store.get_invoice(invoice.id)?;
            match fresh.map(|i| i.status) {
                Some(crate::domain::InvoiceStatus::Paid) => {
                    Ok(Json(serde_json::json!({ "status": "already_paid" })).into_response())
                }
                _ => Err(ApiError::conflict("invoice is not issued")),
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
    if let Some(prev) = records
        .iter()
        .find(|r| {
            r.provider == provider.name() && r.kind == "invoice" && r.invoice_id == invoice.id
        })
        .cloned()
        .filter(|r| r.status == crate::accounting::SyncStatus::Synced)
    {
        // Invoice already synced. If it has since been paid and no payment
        // record exists yet, push the payment against the remote invoice (#33).
        let has_payment = records.iter().any(|r| {
            r.provider == prev.provider && r.kind == "payment" && r.invoice_id == invoice.id
        });
        if invoice.status != crate::domain::InvoiceStatus::Paid || has_payment {
            return Ok(Json(serde_json::json!({ "record": prev, "noop": true })).into_response());
        }
        let paid_at = invoice.paid_at.unwrap_or_else(|| app.clock.now());
        return match provider.push_payment(
            &prev.remote_id,
            invoice.total_minor,
            &invoice.currency.0,
            &invoice.payment_reference,
            paid_at,
        ) {
            Ok(remote) => {
                let rec = crate::accounting::record_sync(
                    &app.store,
                    crate::accounting::SyncAttempt {
                        provider: provider.name(),
                        kind: "payment",
                        invoice: &invoice,
                        remote_id: remote,
                        status: crate::accounting::SyncStatus::Synced,
                        error: String::new(),
                        now: app.clock.now(),
                    },
                )?;
                app.audit
                    .record("accounting_payment_sync", &invoice.number, app.clock.now());
                Ok(
                    Json(serde_json::json!({ "record": rec, "invoice_synced": prev.remote_id }))
                        .into_response(),
                )
            }
            Err(e) => {
                let rec = crate::accounting::record_sync(
                    &app.store,
                    crate::accounting::SyncAttempt {
                        provider: provider.name(),
                        kind: "payment",
                        invoice: &invoice,
                        remote_id: String::new(),
                        status: crate::accounting::SyncStatus::Failed,
                        error: e.to_string(),
                        now: app.clock.now(),
                    },
                )?;
                Ok(Json(serde_json::json!({ "record": rec, "failed": true })).into_response())
            }
        };
    }
    let key = crate::accounting::invoice_key(provider.name(), &invoice.number);
    let doc = crate::accounting::InvoiceDoc {
        invoice: &invoice,
        customer: &customer,
    };
    match provider.push_invoice(&doc, &key) {
        Ok(remote) => {
            if invoice.status == crate::domain::InvoiceStatus::Paid
                && let Err(e) = provider.push_payment(
                    &remote,
                    invoice.total_minor,
                    &invoice.currency.0,
                    &invoice.payment_reference,
                    invoice.paid_at.unwrap_or_else(|| app.clock.now()),
                )
            {
                let prec = crate::accounting::record_sync(
                    &app.store,
                    crate::accounting::SyncAttempt {
                        provider: provider.name(),
                        kind: "payment",
                        invoice: &invoice,
                        remote_id: String::new(),
                        status: crate::accounting::SyncStatus::Failed,
                        error: e.to_string(),
                        now: app.clock.now(),
                    },
                )?;
                tracing::warn!(error = %e, "payment sync failed (invoice synced anyway)");
                return Ok(
                    Json(serde_json::json!({ "record": prec, "invoice_synced": remote }))
                        .into_response(),
                );
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
