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

use hmac::{Hmac, Mac};
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
    pub invoice_number: String,
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

    pub fn from_sources(
        vault: Option<&crate::vault::SecretVault>,
        cfg: &crate::appconfig::AppConfig,
    ) -> Option<Self> {
        let secret = vault
            .and_then(|v| v.get("stripe.webhook_secret"))
            .or_else(|| std::env::var("TUCANO_STRIPE_WEBHOOK_SECRET").ok());
        let fake = cfg.get_bool_flag("stripe_demo", &crate::appconfig::process_env);
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

impl PayPalProvider {
    pub fn from_sources(
        vault: Option<&crate::vault::SecretVault>,
        cfg: &crate::appconfig::AppConfig,
    ) -> Option<Self> {
        let secret = vault
            .and_then(|v| v.get("paypal.webhook_secret"))
            .or_else(|| std::env::var("TUCANO_PAYPAL_WEBHOOK_SECRET").ok());
        let fake = cfg.get_bool_flag("paypal_demo", &crate::appconfig::process_env);
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
pub fn registry_from_vault(
    vault: Option<&crate::vault::SecretVault>,
    cfg: &crate::appconfig::AppConfig,
) -> Arc<PaymentRegistry> {
    let mut providers: Vec<Arc<dyn PaymentProvider>> = Vec::new();
    if let Some(p) = StripeProvider::from_sources(vault, cfg) {
        providers.push(Arc::new(p));
    }
    if let Some(p) = PayPalProvider::from_sources(vault, cfg) {
        providers.push(Arc::new(p));
    }
    Arc::new(PaymentRegistry::new(providers))
}

/// Provider registry consulted by the endpoints.
#[derive(Default)]
pub struct PaymentRegistry {
    providers: Vec<Arc<dyn PaymentProvider>>,
}

impl PaymentRegistry {
    pub fn new(providers: Vec<Arc<dyn PaymentProvider>>) -> Self {
        Self { providers }
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn PaymentProvider>> {
        self.providers.iter().find(|p| p.name() == name).cloned()
    }
    pub fn enabled(&self) -> bool {
        !self.providers.is_empty()
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
    let want = sign(secret, body);
    // Constant-ish comparison over equal-length hex digests.
    if got.len() != want.len() || !got.bytes().zip(want.bytes()).all(|(a, b)| a == b) {
        return Err(PaymentError::BadSignature);
    }
    Ok(())
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
    // Accept "<something>.checkout.completed" and Stripe's
    // "checkout.session.completed" (which has no leading prefix).
    let completed = event == "checkout.session.completed" || event.ends_with(".checkout.completed");
    if !completed {
        return Ok(None); // unrelated but valid
    }
    let status = v
        .get("payment_status")
        .and_then(|s| s.as_str())
        .unwrap_or("");
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
    Ok(Some(WebhookPayment {
        provider: provider.into(),
        reference,
        invoice_number,
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
            "{{\"type\":\"checkout.session.completed\",\"payment_status\":\"paid\",\"client_reference_id\":\"{refid}\",\"metadata\":{{\"invoice_number\":\"{inv}\"}}}}"
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
                invoice_number: "INV-0042".into(),
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
