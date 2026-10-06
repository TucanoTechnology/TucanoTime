//! Retainers (#144): advance funds held for a customer/project, tracked as
//! an append-only transaction ledger. The balance is ALWAYS recomputed from
//! the ledger — a derived number is never persisted, so the two can never
//! disagree. Money is integer minor units throughout (AGENTS invariant).
//!
//! A draw is an administrative reservation against the retainer; it is
//! deliberately NOT an invoice payment (#114 owns payments) and this module
//! never touches invoice or entry records. Distinct from the recurring
//! engine's `RecurMode::Retainer` (#26/#136), which auto-generates a fixed
//! draft invoice each period: THIS is the money held; that is a billing
//! cadence. The two never share state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{Currency, ProjectCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainerStatus {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxKind {
    /// Initial funding at creation.
    Opening,
    /// Add funds.
    Credit,
    /// Apply funds (e.g. to cover work); requires a reason.
    Draw,
    /// Signed fix for a mis-recorded amount.
    Correction,
}

/// One ledger entry. `amount_minor` is non-negative; direction comes from
/// `kind` (Correction accepts a leading sign in `reason`… no — corrections
/// carry their own sign via `signed_amount()` below).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetainerTx {
    pub id: Uuid,
    pub kind: TxKind,
    pub amount_minor: u64,
    pub reason: String,
    /// Optional client-generated key; a repeat with the same key is
    /// idempotent (#144: double-click safety), never a second transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub recorded_by: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Retainer {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub project_code: ProjectCode,
    pub currency: Currency,
    pub status: RetainerStatus,
    pub transactions: Vec<RetainerTx>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

impl TxKind {
    /// Signed ledger effect in minor units.
    #[must_use]
    pub fn signed(self, amount_minor: u64) -> i64 {
        let v = i64::try_from(amount_minor).unwrap_or(i64::MAX);
        match self {
            TxKind::Draw => -v,
            TxKind::Opening | TxKind::Credit | TxKind::Correction => v,
        }
    }
}

/// One ledger operation request (#144). Bundled so the aggregate keeps a
/// narrow, order-independent interface at every call site.
#[derive(Debug)]
pub struct TxRequest<'a> {
    pub kind: TxKind,
    pub amount_minor: u64,
    pub reason: String,
    pub idempotency_key: Option<String>,
    pub actor: Uuid,
    pub now: DateTime<Utc>,
    pub currency_claim: Option<&'a str>,
}

/// Why a ledger operation was refused; the API maps each to a status code.
#[derive(Debug, thiserror::Error)]
pub enum RetainerError {
    #[error("retainer is closed")]
    Closed,
    #[error("draw would overdraw the balance")]
    Overdraw,
    #[error("amount must be positive")]
    ZeroAmount,
    #[error("duplicate operation (already applied)")]
    Duplicate,
    #[error("currency mismatch: retainer holds {0}")]
    Currency(String),
    #[error("draw requires a reason")]
    ReasonRequired,
}

impl Retainer {
    /// Reconciled balance from the append-only ledger (never stored).
    #[must_use]
    pub fn balance_minor(&self) -> i64 {
        self.transactions
            .iter()
            .map(|t| t.kind.signed(t.amount_minor))
            .sum()
    }

    /// Validate + append a transaction. Idempotency keys repeat-check first:
    /// a replay returns Ok with the EXISTING ledger untouched.
    pub fn apply(&mut self, req: TxRequest<'_>) -> Result<(), RetainerError> {
        let TxRequest {
            kind,
            amount_minor,
            reason,
            idempotency_key,
            actor,
            now,
            currency_claim,
        } = req;
        if self.status == RetainerStatus::Closed {
            return Err(RetainerError::Closed);
        }
        if let Some(key) = &idempotency_key
            && self
                .transactions
                .iter()
                .any(|t| t.idempotency_key.as_deref() == Some(key))
        {
            return Err(RetainerError::Duplicate);
        }
        if let Some(cur) = currency_claim
            && !cur.eq_ignore_ascii_case(&self.currency.0)
        {
            return Err(RetainerError::Currency(self.currency.0.clone()));
        }
        if amount_minor == 0 {
            return Err(RetainerError::ZeroAmount);
        }
        if kind == TxKind::Draw && reason.trim().is_empty() {
            return Err(RetainerError::ReasonRequired);
        }
        let after = self.balance_minor() + kind.signed(amount_minor);
        if kind == TxKind::Draw && after < 0 {
            return Err(RetainerError::Overdraw);
        }
        self.transactions.push(RetainerTx {
            id: Uuid::new_v4(),
            kind,
            amount_minor,
            reason: reason.trim().to_string(),
            idempotency_key,
            recorded_by: actor,
            created_at: now,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn retainer() -> Retainer {
        Retainer {
            id: Uuid::new_v4(),
            customer_id: Uuid::new_v4(),
            project_code: ProjectCode("WEB".into()),
            currency: Currency("EUR".into()),
            status: RetainerStatus::Open,
            transactions: vec![],
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            closed_at: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn ledger_reconciles_credits_and_draws() {
        let mut r = retainer();
        r.apply(TxRequest {
            kind: TxKind::Opening,
            amount_minor: 50_000,
            reason: String::new(),
            idempotency_key: None,
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        r.apply(TxRequest {
            kind: TxKind::Credit,
            amount_minor: 10_000,
            reason: "top up".into(),
            idempotency_key: None,
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        assert_eq!(r.balance_minor(), 60_000);
        r.apply(TxRequest {
            kind: TxKind::Draw,
            amount_minor: 25_000,
            reason: "cover March work".into(),
            idempotency_key: None,
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        assert_eq!(r.balance_minor(), 35_000);
        // exact-balance draw is allowed, one more cent is not
        r.apply(TxRequest {
            kind: TxKind::Draw,
            amount_minor: 35_000,
            reason: "clear".into(),
            idempotency_key: Some("k1".into()),
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        assert_eq!(r.balance_minor(), 0);
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Draw,
                amount_minor: 1,
                reason: "x".into(),
                idempotency_key: None,
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: None
            }),
            Err(RetainerError::Overdraw)
        ));
        // rejected ops left the ledger untouched (4 tx)
        assert_eq!(r.transactions.len(), 4);
    }

    #[test]
    fn guards_are_exact() {
        let mut r = retainer();
        r.apply(TxRequest {
            kind: TxKind::Opening,
            amount_minor: 100,
            reason: String::new(),
            idempotency_key: None,
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Credit,
                amount_minor: 0,
                reason: String::new(),
                idempotency_key: None,
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: None
            }),
            Err(RetainerError::ZeroAmount)
        ));
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Draw,
                amount_minor: 50,
                reason: "  ".into(),
                idempotency_key: None,
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: None
            }),
            Err(RetainerError::ReasonRequired)
        ));
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Credit,
                amount_minor: 50,
                reason: String::new(),
                idempotency_key: None,
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: Some("USD")
            }),
            Err(RetainerError::Currency(_))
        ));
        assert_eq!(r.transactions.len(), 1);
        // idempotency: same key replays as Duplicate, never a second entry
        r.apply(TxRequest {
            kind: TxKind::Credit,
            amount_minor: 50,
            reason: "once".into(),
            idempotency_key: Some("dup".into()),
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Credit,
                amount_minor: 50,
                reason: "twice".into(),
                idempotency_key: Some("dup".into()),
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: None
            }),
            Err(RetainerError::Duplicate)
        ));
        assert_eq!(r.balance_minor(), 150);
    }

    #[test]
    fn closed_retainers_reject_mutation() {
        let mut r = retainer();
        r.apply(TxRequest {
            kind: TxKind::Opening,
            amount_minor: 100,
            reason: String::new(),
            idempotency_key: None,
            actor: Uuid::new_v4(),
            now: now(),
            currency_claim: None,
        })
        .unwrap();
        r.status = RetainerStatus::Closed;
        r.closed_at = Some(now());
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Credit,
                amount_minor: 10,
                reason: String::new(),
                idempotency_key: None,
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: None
            }),
            Err(RetainerError::Closed)
        ));
        assert!(matches!(
            r.apply(TxRequest {
                kind: TxKind::Draw,
                amount_minor: 10,
                reason: "x".into(),
                idempotency_key: None,
                actor: Uuid::new_v4(),
                now: now(),
                currency_claim: None
            }),
            Err(RetainerError::Closed)
        ));
        assert_eq!(r.transactions.len(), 1, "closed = frozen history");
    }
}
