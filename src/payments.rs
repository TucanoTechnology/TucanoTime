//! Online payments (#34). Invoices get a hosted checkout link from a
//! `PaymentProvider` adapter behind this port (the #18 seam family), and a
//! public webhook feeds the #27 invoice status model (`issued -> paid`).
//!
//! No card data is ever handled here — PCI lives with the provider, so the
//! only secrets are an API key and a webhook signing secret, read from the
//! vault (#77) or env, never from the repo or API responses. The adapters in
//! this crate build provider-shaped URLs but do **not** call live endpoints;
//! they verify webhook signatures with real HMAC-SHA256 so the security path
//! is genuinely covered by tests. `TUCANO_STRIPE_FAKE` / `TUCANO_PAYPAL_FAKE`
//! exist for local demos only and make signature checks pass without a secret.

use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum PaymentError {
    #[error("unknown payment provider")]
    UnknownProvider,
    #[error("webhook signature verification failed")]
    BadSignature,
    #[error("invalid webhook payload")]
    BadPayload,
    #[error("only issued invoices can take payment (invoice {0})")]
    NotIssued(String),
}

/// A hosted checkout session for one invoice.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CheckoutSession {
    pub invoice_id: Uuid,
    pub provider: String,
    /// The reference echoed back by the webhook when payment completes.
    pub reference: String,
    pub url: String,
}

/// A completed payment reported by a provider webhook.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookPayment {
    pub provider: String,
    pub reference: String,
    /// The provider's own event id, when it sends one. Ledger entries key on
    /// this so two different events sharing a `client_reference_id` (e.g. a
    /// deposit then a final payment on one session) are distinct, while a
    /// replay of the SAME event is idempotent (#114).
    pub event_id: String,
    pub invoice_number: String,
    /// What the provider actually collected, in minor units — the handler
    /// refuses to settle when it disagrees with the invoice (review A11).
    pub amount_minor: u64,
    pub currency: String,
}

pub trait PaymentProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Builds the checkout link for an invoice. `amount` is in minor units.
    fn create_checkout(
        &self,
        invoice_id: Uuid,
        invoice_number: &str,
        amount_minor: u64,
        currency: &str,
    ) -> Result<CheckoutSession, PaymentError>;
    /// Verifies the webhook signature (HMAC-SHA256 over the raw body) and
    /// decodes a completed payment. `Ok(None)` = valid signature, unrelated
    /// event. Any signature failure is an error — never a silent accept.
    fn parse_webhook(
        &self,
        body: &str,
        signature: &str,
    ) -> Result<Option<WebhookPayment>, PaymentError>;
}

/// Stripe-shaped adapter. Real Stripe would add the API version + hosted
/// session creation here; the webhook secret verification below is the part
/// that carries security weight and is fully implemented.
#[derive(Clone)]
pub struct StripeProvider {
    signing_secret: Option<String>,
    /// Local demo mode: signature checks always pass. Never for production.
    fake: bool,
}

/// PayPal-shaped adapter (same shape, different URL + event vocabulary).
#[derive(Clone)]
pub struct PayPalProvider {
    signing_secret: Option<String>,
    fake: bool,
}

impl StripeProvider {
    /// Test/explicit constructor with a known webhook secret (fake mode off).
    #[must_use]
    pub fn with_secret(secret: impl Into<String>) -> Self {
        Self {
            signing_secret: Some(secret.into()),
            fake: false,
        }
    }

    pub fn from_sources(vault: Option<&crate::vault::SecretVault>) -> Option<Self> {
        let secret = crate::providers::resolve_secret(
            vault,
            "stripe.webhook_secret",
            "TUCANO_STRIPE_WEBHOOK_SECRET",
        );
        let fake = demo_flag("TUCANO_STRIPE_FAKE", secret.is_some());
        if fake || secret.is_some() {
            Some(Self {
                signing_secret: secret,
                fake,
            })
        } else {
            None
        }
    }
}

/// Demo-mode webhook verification bypass: **env-only** (never config.json, so
/// `PUT /admin/config` cannot turn it on remotely) and never honoured when a
/// real webhook secret is configured or `TUCANO_ENV=production` (review A3).
fn demo_flag(env: &str, has_real_secret: bool) -> bool {
    if std::env::var(env).is_err() {
        return false;
    }
    if has_real_secret || std::env::var("TUCANO_ENV").as_deref() == Ok("production") {
        tracing::error!(
            "{env} set while a real webhook secret exists / in production — demo bypass DISABLED"
        );
        return false;
    }
    tracing::warn!("{env}: webhook signature verification is DISABLED (demo mode, dev only)");
    true
}

impl PayPalProvider {
    pub fn from_sources(vault: Option<&crate::vault::SecretVault>) -> Option<Self> {
        let secret = crate::providers::resolve_secret(
            vault,
            "paypal.webhook_secret",
            "TUCANO_PAYPAL_WEBHOOK_SECRET",
        );
        let fake = demo_flag("TUCANO_PAYPAL_FAKE", secret.is_some());
        if fake || secret.is_some() {
            Some(Self {
                signing_secret: secret,
                fake,
            })
        } else {
            None
        }
    }
}

/// Builds the registry from vault/env: a provider is enabled only when its
/// webhook secret (or explicit fake flag) is present.
pub fn registry_from_vault(vault: Option<&crate::vault::SecretVault>) -> Arc<PaymentRegistry> {
    let mut providers: Vec<Arc<dyn PaymentProvider>> = Vec::new();
    if let Some(p) = StripeProvider::from_sources(vault) {
        providers.push(Arc::new(p));
    }
    if let Some(p) = PayPalProvider::from_sources(vault) {
        providers.push(Arc::new(p));
    }
    Arc::new(PaymentRegistry::new(providers))
}

/// Provider registry consulted by the endpoints (#101: the shared generic
/// `Registry`, no longer a payments-only copy).
pub type PaymentRegistry = crate::providers::Registry<dyn PaymentProvider>;

impl crate::providers::NamedProvider for dyn PaymentProvider {
    fn name(&self) -> &str {
        PaymentProvider::name(self)
    }
}

/// HMAC-SHA256 of `body` keyed by `secret`, hex encoded.
pub fn sign(secret: &str, body: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac any key");
    m.update(body.as_bytes());
    m.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn verify(secret: Option<&String>, signature: &str, body: &str) -> Result<(), PaymentError> {
    let secret = secret.ok_or(PaymentError::BadSignature)?;
    let got = signature.strip_prefix("sha256=").unwrap_or(signature);
    let tag = hex_decode(got).ok_or(PaymentError::BadSignature)?;
    // hmac crate's verify_slice is the constant-time primitive (review A7).
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac any key");
    m.update(body.as_bytes());
    m.verify_slice(&tag).map_err(|_| PaymentError::BadSignature)
}

/// Strict lowercase-hex decoder (a malformed signature is a bad signature).
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push(digit(pair[0])? << 4 | digit(pair[1])?);
    }
    Some(out)
}

/// Shared webhook decoding: verify, then accept only paid checkout events.
fn decode(
    provider: &str,
    fake: bool,
    secret: Option<&String>,
    body: &str,
    signature: &str,
) -> Result<Option<WebhookPayment>, PaymentError> {
    if !fake {
        verify(secret, signature, body)?;
    }
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| PaymentError::BadPayload)?;
    let event = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
    // Exact terminal-event vocabulary per provider (review B10): a suffix
    // match lets unrelated "*\.checkout.completed" events settle invoices.
    let terminal = match provider {
        "stripe" => event == "checkout.session.completed",
        "paypal" => event == "paypal.checkout.completed",
        _ => false,
    };
    if !terminal {
        return Ok(None); // unrelated but valid
    }
    let status = v
        .get("payment_status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_ascii_lowercase(); // PayPal posts "COMPLETED"; Stripe "paid"
    if !status.is_empty() && status != "paid" && status != "completed" {
        return Ok(None);
    }
    let reference = v
        .get("client_reference_id")
        .or_else(|| v.get("reference"))
        .and_then(|r| r.as_str())
        .ok_or(PaymentError::BadPayload)?
        .to_string();
    let invoice_number = v
        .get("metadata")
        .and_then(|m| m.get("invoice_number"))
        .and_then(|n| n.as_str())
        .ok_or(PaymentError::BadPayload)?
        .to_string();
    let amount_minor = v
        .get("amount_minor")
        .and_then(|a| a.as_u64())
        .ok_or(PaymentError::BadPayload)?;
    let currency = v
        .get("currency")
        .and_then(|c| c.as_str())
        .filter(|c| (3..=10).contains(&c.len()))
        .ok_or(PaymentError::BadPayload)?
        .to_ascii_uppercase();
    let event_id = v
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(Some(WebhookPayment {
        provider: provider.into(),
        reference,
        event_id,
        invoice_number,
        amount_minor,
        currency,
    }))
}

macro_rules! impl_provider {
    ($ty:ty, $name:literal, $base:literal) => {
        impl PaymentProvider for $ty {
            fn name(&self) -> &'static str {
                $name
            }
            fn create_checkout(
                &self,
                invoice_id: Uuid,
                invoice_number: &str,
                amount_minor: u64,
                currency: &str,
            ) -> Result<CheckoutSession, PaymentError> {
                let _ = amount_minor;
                let reference = format!("{name}_{invoice_id}", name = $name);
                Ok(CheckoutSession {
                    invoice_id,
                    provider: $name.into(),
                    url: format!("{base}/{invoice_number}?currency={currency}", base = $base),
                    reference,
                })
            }
            fn parse_webhook(
                &self,
                body: &str,
                signature: &str,
            ) -> Result<Option<WebhookPayment>, PaymentError> {
                decode(
                    $name,
                    self.fake,
                    self.signing_secret.as_ref(),
                    body,
                    signature,
                )
            }
        }
    };
}

impl_provider!(
    StripeProvider,
    "stripe",
    "https://checkout.stripe.com/c/pay"
);
impl_provider!(
    PayPalProvider,
    "paypal",
    "https://www.sandbox.paypal.com/pay"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn stripe(secret: &str) -> StripeProvider {
        StripeProvider {
            signing_secret: Some(secret.into()),
            fake: false,
        }
    }

    fn payload(refid: &str, inv: &str) -> String {
        format!(
            "{{\"type\":\"checkout.session.completed\",\"payment_status\":\"paid\",\"amount_minor\":15000,\"currency\":\"EUR\",\"client_reference_id\":\"{refid}\",\"metadata\":{{\"invoice_number\":\"{inv}\"}}}}"
        )
    }

    #[test]
    fn webhook_valid_signature_decodes_payment() {
        let p = stripe("whsec_test_key");
        let body = payload("stripe_42", "INV-0042");
        let sig = sign("whsec_test_key", &body);
        let got = p.parse_webhook(&body, &format!("sha256={sig}")).unwrap();
        assert_eq!(
            got,
            Some(WebhookPayment {
                provider: "stripe".into(),
                reference: "stripe_42".into(),
                event_id: String::new(),
                invoice_number: "INV-0042".into(),
                amount_minor: 15000,
                currency: "EUR".into(),
            })
        );
    }

    #[test]
    fn webhook_rejects_tampered_payload_and_bad_signature() {
        let p = stripe("whsec_test_key");
        let body = payload("stripe_42", "INV-0042");
        let sig = sign("whsec_test_key", &body);
        let evil = payload("stripe_42", "INV-9999");
        assert!(matches!(
            p.parse_webhook(&evil, &sig),
            Err(PaymentError::BadSignature)
        ));
        assert!(matches!(
            p.parse_webhook(&body, "sha256=deadbeef"),
            Err(PaymentError::BadSignature)
        ));
    }

    #[test]
    fn suffix_spoof_events_are_not_terminal() {
        // Review B10: "*\.checkout.completed" suffixes must not settle.
        let p = stripe("whsec");
        let body = "{\"type\":\"evil.checkout.completed\",\"amount_minor\":1,\"currency\":\"EUR\",\"client_reference_id\":\"stripe_1\",\"metadata\":{\"invoice_number\":\"INV-1\"}}";
        let sig = sign("whsec", body);
        assert_eq!(p.parse_webhook(body, &sig).unwrap(), None);
        // PayPal's real terminal name differs from Stripe's.
        let pp = PayPalProvider {
            signing_secret: Some("psec".into()),
            fake: false,
        };
        let body2 = "{\"type\":\"paypal.checkout.completed\",\"payment_status\":\"COMPLETED\",\"amount_minor\":2,\"currency\":\"usd\",\"client_reference_id\":\"paypal_1\",\"metadata\":{\"invoice_number\":\"INV-2\"}}";
        let sig2 = sign("psec", body2);
        assert!(pp.parse_webhook(body2, &sig2).unwrap().is_some());
    }

    #[test]
    fn unrelated_events_are_ignored_but_valid() {
        let p = stripe("whsec");
        let body = "{\"type\":\"invoice.created\",\"data\":{}}";
        let sig = sign("whsec", body);
        assert_eq!(p.parse_webhook(body, &sig).unwrap(), None);
    }

    #[test]
    fn checkout_link_shape() {
        let id = Uuid::new_v4();
        let s = stripe("whsec")
            .create_checkout(id, "INV-7", 150000, "EUR")
            .unwrap();
        assert_eq!(
            s.url,
            "https://checkout.stripe.com/c/pay/INV-7?currency=EUR"
        );
        assert_eq!(s.reference, format!("stripe_{id}"));
        let pp = PayPalProvider {
            signing_secret: None,
            fake: true,
        };
        let s2 = pp.create_checkout(id, "INV-7", 1, "USD").unwrap();
        assert!(
            s2.url
                .starts_with("https://www.sandbox.paypal.com/pay/INV-7")
        );
        assert_eq!(s2.provider, "paypal");
    }

    #[test]
    fn fake_mode_skips_signature() {
        let f = StripeProvider {
            signing_secret: None,
            fake: true,
        };
        let body = payload("r1", "INV-1");
        assert!(f.parse_webhook(&body, "").unwrap().is_some());
    }
}
