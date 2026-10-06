//! Retainer balances and add/draw workflow (#144). Admin tier like
//! invoices. Money is integer minor units; the balance is ALWAYS the
//! reconciled ledger, never a stored number.

use super::*;
use crate::retainer::{Retainer, RetainerStatus, TxKind};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainerCreate {
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    #[serde(default)]
    pub currency: Option<Currency>,
    #[serde(default)]
    pub opening_amount_minor: u64,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainerTxInput {
    pub amount_minor: u64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// A retainer plus its reconciled balance (the ledger stays the truth).
fn retainer_json(r: &Retainer) -> serde_json::Value {
    serde_json::json!({
        "retainer": r,
        "balance_minor": r.balance_minor(),
    })
}

pub async fn list_retainers(State(app): State<AppState>) -> ApiResult {
    let rs = app.store.list_retainers()?;
    Ok(Json(serde_json::json!({
        "retainers": rs.iter().map(|r| serde_json::json!({
            "id": r.id, "customer_id": r.customer_id, "project_code": r.project_code,
            "currency": r.currency, "status": r.status,
            "balance_minor": r.balance_minor(),
            "last_activity": r.transactions.last().map(|t| t.created_at),
        })).collect::<Vec<_>>(),
    }))
    .into_response())
}

pub async fn get_retainer(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    let r = app
        .store
        .get_retainer(id)?
        .ok_or_else(|| ApiError::not_found("retainer"))?;
    Ok(Json(retainer_json(&r)).into_response())
}

pub async fn create_retainer(
    State(app): State<AppState>,
    actor: AuthUser,
    ValidJson(input): ValidJson<RetainerCreate>,
) -> ApiResult {
    let customer = get_customer(&app.store, input.customer_id)?;
    let projects = app.store.list_projects(input.customer_id)?;
    if !projects.iter().any(|p| p.code == input.project_code) {
        return Err(ApiError::validation(vec![FieldError::new(
            "project_code",
            "unknown project for this customer",
        )]));
    }
    let currency = match input
        .currency
        .clone()
        .or_else(|| Some(customer.currency.clone()))
    {
        Some(c) => c,
        None => Currency("EUR".into()),
    };
    let mut r = Retainer {
        id: Uuid::new_v4(),
        customer_id: input.customer_id,
        project_code: input.project_code,
        currency,
        status: RetainerStatus::Open,
        transactions: vec![],
        created_at: app.clock.now(),
        closed_at: None,
    };
    if input.opening_amount_minor > 0 {
        r.apply(crate::retainer::TxRequest {
            kind: TxKind::Opening,
            amount_minor: input.opening_amount_minor,
            reason: "opening balance".into(),
            idempotency_key: input.idempotency_key,
            actor: actor.0.id,
            now: app.clock.now(),
            currency_claim: None,
        })
        .map_err(retainer_error)?;
    }
    app.store.put_retainer(&r)?;
    app.audit
        .record("retainer_created", &format!("{}", r.id), app.clock.now());
    Ok((StatusCode::CREATED, Json(retainer_json(&r))).into_response())
}

fn retainer_error(e: crate::retainer::RetainerError) -> ApiError {
    use crate::retainer::RetainerError::*;
    match e {
        Closed => ApiError::conflict(e.to_string()),
        Overdraw => ApiError::conflict(e.to_string()),
        Duplicate => ApiError::conflict(e.to_string()),
        Currency(_) => ApiError::validation(vec![FieldError::new("currency", e.to_string())]),
        ZeroAmount => ApiError::validation(vec![FieldError::new("amount_minor", e.to_string())]),
        ReasonRequired => ApiError::validation(vec![FieldError::new("reason", e.to_string())]),
    }
}

async fn ledger_op(
    app: &AppState,
    actor: &AuthUser,
    id: Uuid,
    kind: TxKind,
    input: RetainerTxInput,
) -> ApiResult {
    let reason_limit = 500;
    if input.reason.chars().count() > reason_limit {
        return Err(ApiError::validation(vec![FieldError::new(
            "reason",
            "at most 500 characters",
        )]));
    }
    let updated = app
        .store
        .update_retainer(id, |r| {
            r.apply(crate::retainer::TxRequest {
                kind,
                amount_minor: input.amount_minor,
                reason: input.reason,
                idempotency_key: input.idempotency_key,
                actor: actor.0.id,
                now: app.clock.now(),
                currency_claim: input.currency.as_deref(),
            })
        })
        .map_err(|e| match e {
            crate::store::StoreError::Io(msg) if msg.contains("closed") => ApiError::conflict(msg),
            crate::store::StoreError::Io(msg) if msg.contains("overdraw") => {
                ApiError::conflict(msg)
            }
            crate::store::StoreError::Io(msg) if msg.contains("duplicate") => {
                ApiError::conflict(msg)
            }
            crate::store::StoreError::Io(msg) if msg.contains("positive") => {
                ApiError::validation(vec![FieldError::new("amount_minor", msg)])
            }
            crate::store::StoreError::Io(msg) if msg.contains("reason") => {
                ApiError::validation(vec![FieldError::new("reason", msg)])
            }
            crate::store::StoreError::Io(msg) if msg.contains("currency") => {
                ApiError::validation(vec![FieldError::new("currency", msg)])
            }
            other => other.into(),
        })?;
    app.audit.record(
        match kind {
            TxKind::Draw => "retainer_draw",
            _ => "retainer_credit",
        },
        &format!("{}:{}:{}", id, kind.signed(input.amount_minor), actor.0.id),
        app.clock.now(),
    );
    Ok(Json(retainer_json(&updated)).into_response())
}

pub async fn credit_retainer(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<RetainerTxInput>,
) -> ApiResult {
    ledger_op(&app, &actor, id, TxKind::Credit, input).await
}

pub async fn draw_retainer(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<RetainerTxInput>,
) -> ApiResult {
    ledger_op(&app, &actor, id, TxKind::Draw, input).await
}

/// Close a retainer: final — history readable, mutations 409 (#144).
pub async fn close_retainer(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let updated = app.store.update_retainer(id, |r| {
        if r.status == RetainerStatus::Closed {
            return Err(crate::retainer::RetainerError::Closed);
        }
        r.status = RetainerStatus::Closed;
        r.closed_at = Some(app.clock.now());
        Ok(())
    })?;
    app.audit.record(
        "retainer_closed",
        &format!("{}:{}", id, actor.0.id),
        app.clock.now(),
    );
    Ok(Json(retainer_json(&updated)).into_response())
}
