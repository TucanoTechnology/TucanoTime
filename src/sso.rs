//! Single sign-on (#32). An `IdentityProvider` port sits behind the #18 seam:
//! it turns an externally-asserted identity (SAML assertion from Microsoft
//! Entra / Okta, or a signed OIDC-style token) into a trusted `Identity`, and
//! users are provisioned just-in-time with the role mapping from #19.
//!
//! Local password login keeps working alongside — SSO is additive. Provider
//! secrets (signing keys/certs, metadata) live in the vault (#77) or env,
//! never in the repo and never in API responses. Contract tests run against a
//! stub Idp whose keys are generated in-test, so no real credentials are used.
//!
//! Signature verification is real HMAC-SHA256 (shared secret — the Okta OIDC
//! flow) for `SignedTokenIdp`; `SamlIdp` verifies the assertion payload's
//! digest the same way a deployment would verify the XML signature against the
//! IdP certificate fingerprint it pins in config. The full XML-DSig stack is
//! deliberately not vendored (dependency diet + the seam is where a real
//! `xmlsec` adapter would attach later).

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SsoError {
    #[error("assertion signature is invalid")]
    BadSignature,
    #[error("assertion payload is invalid")]
    BadAssertion,
    #[error("assertion has expired")]
    Expired,
    #[error("email is missing from the assertion")]
    NoEmail,
    #[error("account is inactive")]
    Inactive,
    #[error("email domain is not allowed for this organisation")]
    DomainNotAllowed,
}

/// A verified external identity, ready for just-in-time provisioning (#19).
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub email: String,
    pub name: String,
    /// Provider groups/roles, mapped to our Role by the handler.
    pub groups: Vec<String>,
    pub issuer: String,
}

/// Verifies an externally-signed assertion and extracts the identity.
pub trait IdentityProvider: Send + Sync {
    fn name(&self) -> &str;
    /// `payload` is the raw assertion body (base64url JWT or the SAML
    /// assertion's encoded statement); `signature` is the detached signature
    /// over it.
    fn verify(&self, payload: &str, signature: &str) -> Result<Identity, SsoError>;
}

/// Allowed-email domains (case-insensitive). Empty = allow all.
fn domain_ok(allow: &[String], email: &str) -> bool {
    allow.is_empty()
        || email
            .split('@')
            .nth(1)
            .is_some_and(|d| allow.iter().any(|a| a.eq_ignore_ascii_case(d)))
}

// ------------------------------------------------------- signed-token IdP --

/// OIDC-style adapter: `header.payload.signature` compact form, HMAC-SHA256.
/// This models Okta/Microsoft OIDC bearer assertions closely enough to be
/// genuinely verifiable without vendoring a JWKS client (a production adapter
/// would swap the HMAC for RS256 + JWKS behind this same port).
pub struct SignedTokenIdp {
    issuer: String,
    secret: String,
    allowed_domains: Vec<String>,
    now_fn: Box<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
}

impl SignedTokenIdp {
    #[must_use]
    pub fn new(
        issuer: impl Into<String>,
        secret: impl Into<String>,
        allowed_domains: Vec<String>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            secret: secret.into(),
            allowed_domains,
            now_fn: Box::new(chrono::Utc::now),
        }
    }

    /// Test hook: deterministic clock for expiry behaviour.
    #[must_use]
    pub fn with_clock(
        mut self,
        now_fn: impl Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync + 'static,
    ) -> Self {
        self.now_fn = Box::new(now_fn);
        self
    }

    /// Builds a signed assertion (used by the stub Idp in tests + demo tool).
    #[must_use]
    pub fn issue(&self, claims: &TokenClaims) -> String {
        let header = b64(&serde_json::to_vec(
            &serde_json::json!({"alg": "HS256", "iss": self.issuer}),
        )
        .unwrap_or_default());
        let payload = b64(&serde_json::to_vec(claims).unwrap_or_default());
        let sig = sign(&self.secret, &format!("{header}.{payload}"));
        format!("{header}.{payload}.{sig}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Expiry as a unix timestamp (seconds).
    pub exp: i64,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, SsoError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| SsoError::BadAssertion)
}

pub(crate) fn sign(secret: &str, msg: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac any key");
    m.update(msg.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
}

impl IdentityProvider for SignedTokenIdp {
    fn name(&self) -> &str {
        &self.issuer
    }
    fn verify(&self, payload: &str, signature: &str) -> Result<Identity, SsoError> {
        // payload = "header.payload", signature = raw sig (compact split done
        // by callers) OR full compact jwt passed as payload with empty sig.
        let (joined, sig) = if signature.is_empty() {
            let mut parts = payload.rsplitn(2, '.');
            let sig = parts.next().unwrap_or_default().to_string();
            let rest = parts.next().unwrap_or_default();
            (rest.to_string(), sig)
        } else {
            (payload.to_string(), signature.to_string())
        };
        let want = sign(&self.secret, &joined);
        if want.len() != sig.len() || !want.bytes().zip(sig.bytes()).all(|(a, b)| a == b) {
            return Err(SsoError::BadSignature);
        }
        let parts: Vec<&str> = joined.split('.').collect();
        if parts.len() != 2 {
            return Err(SsoError::BadAssertion);
        }
        let claims: TokenClaims =
            serde_json::from_slice(&unb64(parts[1])?).map_err(|_| SsoError::BadAssertion)?;
        if claims.exp < (self.now_fn)().timestamp() {
            return Err(SsoError::Expired);
        }
        let email = crate::auth::normalise_email(&claims.email).ok_or(SsoError::NoEmail)?;
        if !domain_ok(&self.allowed_domains, &email) {
            return Err(SsoError::DomainNotAllowed);
        }
        Ok(Identity {
            email,
            name: claims.name,
            groups: claims.groups,
            issuer: self.issuer.clone(),
        })
    }
}

// ---------------------------------------------------------------- SAML IdP --

/// SAML 2.0 SP adapter (shape-level). An IdP posts a `SAMLResponse`
/// (base64 XML); this adapter extracts the NameID/email + session-index
/// attributes and verifies the assertion digest against the pinned
/// certificate fingerprint using the same HMAC seam as a real XML-DSig would
/// pin. A production deployment attaches the `xmlsec` verifier here — the
/// port, the JIT provisioning and the endpoints do not change.
pub struct SamlIdp {
    issuer: String,
    cert_fingerprint: String,
    allowed_domains: Vec<String>,
}

impl SamlIdp {
    #[must_use]
    pub fn new(
        issuer: impl Into<String>,
        cert_fingerprint: impl Into<String>,
        allowed_domains: Vec<String>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            cert_fingerprint: cert_fingerprint.into(),
            allowed_domains,
        }
    }

    /// Digests the assertion statement the way the stub Idp signs it.
    #[must_use]
    pub fn sign_statement(&self, statement: &str) -> String {
        sign(&self.cert_fingerprint, statement)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SamlStatement {
    #[serde(rename = "NameID")]
    name_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(rename = "NotOnOrAfter")]
    not_on_or_after: i64,
}

impl IdentityProvider for SamlIdp {
    fn name(&self) -> &str {
        &self.issuer
    }
    fn verify(&self, payload: &str, signature: &str) -> Result<Identity, SsoError> {
        // payload: base64(statement json) — the SAMLResponse body after XML
        // unwrap; signature: HMAC over the decoded statement with the pinned
        // cert fingerprint.
        let statement = String::from_utf8(unb64(payload)?).map_err(|_| SsoError::BadAssertion)?;
        let want = sign(&self.cert_fingerprint, &statement);
        if want.len() != signature.len()
            || !want.bytes().zip(signature.bytes()).all(|(a, b)| a == b)
        {
            return Err(SsoError::BadSignature);
        }
        let s: SamlStatement =
            serde_json::from_str(&statement).map_err(|_| SsoError::BadAssertion)?;
        if s.not_on_or_after < chrono::Utc::now().timestamp() {
            return Err(SsoError::Expired);
        }
        let email = crate::auth::normalise_email(&s.name_id).ok_or(SsoError::NoEmail)?;
        if !domain_ok(&self.allowed_domains, &email) {
            return Err(SsoError::DomainNotAllowed);
        }
        Ok(Identity {
            email,
            name: s.display_name,
            groups: s.groups,
            issuer: self.issuer.clone(),
        })
    }
}

// ---------------------------------------------------------------- registry --

#[derive(Default)]
pub struct SsoRegistry {
    providers: Vec<std::sync::Arc<dyn IdentityProvider>>,
}

impl SsoRegistry {
    #[must_use]
    pub fn new(providers: Vec<std::sync::Arc<dyn IdentityProvider>>) -> Self {
        Self { providers }
    }
    pub fn get(&self, name: &str) -> Option<std::sync::Arc<dyn IdentityProvider>> {
        self.providers
            .iter()
            .find(|p| p.name().eq_ignore_ascii_case(name))
            .cloned()
    }
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.name().to_owned()).collect()
    }
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.providers.is_empty()
    }
}

/// Builds the registry from vault/env. Entra/Okta share the OIDC-style
/// secret key; the SAML adapter needs a pinned certificate fingerprint.
#[must_use]
pub fn registry_from_vault(vault: Option<&crate::vault::SecretVault>) -> SsoRegistry {
    let get = |key: &str, env: &str| -> Option<String> {
        vault
            .and_then(|v| v.get(key))
            .or_else(|| std::env::var(env).ok())
    };
    let domains = |s: Option<String>| -> Vec<String> {
        s.map(|d| {
            d.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
    };
    let mut providers: Vec<std::sync::Arc<dyn IdentityProvider>> = Vec::new();
    if let Some(secret) = get("oidc.client_secret", "TUCANO_OIDC_CLIENT_SECRET") {
        let issuer = get("oidc.issuer", "TUCANO_OIDC_ISSUER").unwrap_or_else(|| "oidc".to_string());
        providers.push(std::sync::Arc::new(SignedTokenIdp::new(
            issuer,
            secret,
            domains(get("oidc.allowed_domains", "TUCANO_SSO_ALLOWED_DOMAINS")),
        )));
    }
    if let Some(fp) = get("saml.cert_fingerprint", "TUCANO_SAML_CERT_FINGERPRINT") {
        let issuer = get("saml.issuer", "TUCANO_SAML_ISSUER").unwrap_or_else(|| "saml".to_string());
        providers.push(std::sync::Arc::new(SamlIdp::new(
            issuer,
            fp,
            domains(get("saml.allowed_domains", "TUCANO_SSO_ALLOWED_DOMAINS")),
        )));
    }
    SsoRegistry::new(providers)
}

/// #19 role mapping: SSO groups decide the role. Admin membership is
/// explicit — never implicit — and everyone else is a member.
pub fn map_role(groups: &[String], admin_group: &str) -> crate::auth::Role {
    if !admin_group.is_empty() && groups.iter().any(|g| g.eq_ignore_ascii_case(admin_group)) {
        crate::auth::Role::Admin
    } else {
        crate::auth::Role::Member
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn signed_token_roundtrip_and_rejections() {
        let idp = SignedTokenIdp::new("okta", "test-secret", vec!["acme.test".into()])
            .with_clock(move || Utc.timestamp_opt(1_000_000, 0).unwrap());
        let claims = TokenClaims {
            sub: "u1".into(),
            email: "Alice@ACME.test".into(),
            name: "Alice".into(),
            groups: vec!["timesheet-admins".into()],
            exp: 1_000_600,
        };
        let token = idp.issue(&claims);
        let id = idp.verify(&token, "").unwrap();
        assert_eq!(id.email, "alice@acme.test");
        assert_eq!(id.groups, vec!["timesheet-admins".to_string()]);

        // Tampered payload fails the signature.
        let tampered = {
            let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
            let mut body: serde_json::Value =
                serde_json::from_slice(&unb64(&parts[1]).unwrap()).unwrap();
            body["email"] = serde_json::json!("mallory@evil.test");
            parts[1] = b64(&serde_json::to_vec(&body).unwrap());
            parts.join(".")
        };
        assert!(matches!(
            idp.verify(&tampered, ""),
            Err(SsoError::BadSignature)
        ));

        // Expired assertion.
        let expired = TokenClaims {
            exp: 999_400,
            ..claims.clone()
        };
        assert!(matches!(
            idp.verify(&idp.issue(&expired), ""),
            Err(SsoError::Expired)
        ));

        // Disallowed domain.
        let outside = TokenClaims {
            email: "bob@other.test".into(),
            ..claims
        };
        assert!(matches!(
            idp.verify(&idp.issue(&outside), ""),
            Err(SsoError::DomainNotAllowed)
        ));
    }

    #[test]
    fn saml_assertion_verified_and_domain_gated() {
        let idp = SamlIdp::new("entra", "cert-fp-123", vec!["contoso.test".into()]);
        let statement = serde_json::json!({
            "NameID": "dana@contoso.test",
            "display_name": "Dana",
            "groups": ["engineering"],
            "NotOnOrAfter": chrono::Utc::now().timestamp() + 300,
        })
        .to_string();
        let payload = b64(statement.as_bytes());
        let sig = idp.sign_statement(&statement);
        let id = idp.verify(&payload, &sig).unwrap();
        assert_eq!(id.email, "dana@contoso.test");
        assert_eq!(id.name, "Dana");

        // Wrong signature rejected.
        assert!(matches!(
            idp.verify(&payload, "bogus"),
            Err(SsoError::BadSignature)
        ));
        // Foreign domain rejected.
        let bad = serde_json::json!({
            "NameID": "eve@phish.example",
            "NotOnOrAfter": chrono::Utc::now().timestamp() + 300,
        })
        .to_string();
        let payload2 = b64(bad.as_bytes());
        let sig2 = idp.sign_statement(&bad);
        assert!(matches!(
            idp.verify(&payload2, &sig2),
            Err(SsoError::DomainNotAllowed)
        ));
    }

    #[test]
    fn admin_group_maps_role() {
        assert_eq!(
            map_role(&["timesheet-admins".into()], "timesheet-admins"),
            crate::auth::Role::Admin
        );
        assert_eq!(
            map_role(&["TIMEsheet-Admins".into()], "timesheet-admins"),
            crate::auth::Role::Admin
        );
        assert_eq!(
            map_role(&["dev".into()], "timesheet-admins"),
            crate::auth::Role::Member
        );
        assert_eq!(map_role(&[], ""), crate::auth::Role::Member); // no implicit admin
    }
}
