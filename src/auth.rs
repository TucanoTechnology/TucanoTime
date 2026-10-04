//! Authentication: users, password hashing (argon2) and stateless signed
//! session tokens (HMAC-SHA256). Sessions are carried in an HttpOnly cookie.
//!
//! The stateless design is deliberate for a filesystem app (no session store),
//! but it means logout cannot revoke before expiry — tracked as a hardening
//! ticket (session revocation). Secrets never appear in responses or logs.

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "tt_session";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub role: Role,
    pub active: bool,
    /// The person's default hourly rate in minor units (#21). 0 = unset, in
    /// which case billing falls through to the customer default.
    #[serde(default)]
    pub default_rate_minor: u64,
    pub password_hash: String,
    pub created_at: DateTime<Utc>,
}

/// A user without the password hash — what the API ever returns.
#[derive(Debug, Clone, Serialize)]
pub struct PublicUser {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub role: Role,
    pub active: bool,
    pub default_rate_minor: u64,
}

impl From<&User> for PublicUser {
    fn from(u: &User) -> Self {
        Self {
            id: u.id,
            name: u.name.clone(),
            email: u.email.clone(),
            role: u.role,
            active: u.active,
            default_rate_minor: u.default_rate_minor,
        }
    }
}

/// Hash a password with argon2id and a random salt.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    // argon2id, random salt generated internally (getrandom feature).
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
}

/// Constant-time-ish verify (argon2 handles the comparison).
pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(ph) => Argon2::default()
            .verify_password(password.as_bytes(), &ph)
            .is_ok(),
        Err(_) => false,
    }
}

#[derive(Serialize, Deserialize)]
struct Claims {
    uid: String,
    jti: String,
    exp: i64,
}

/// Verified session token contents.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionClaims {
    pub uid: Uuid,
    pub jti: String,
    pub exp: i64,
}

/// Issues and verifies signed session tokens: `base64(json claims).base64(hmac)`.
pub struct Session {
    secret: Vec<u8>,
    ttl_secs: i64,
    secure: bool,
}

impl Session {
    pub fn new(secret: Vec<u8>, ttl_secs: i64, secure: bool) -> Self {
        Self {
            secret,
            ttl_secs,
            secure,
        }
    }

    pub fn secure(&self) -> bool {
        self.secure
    }

    fn mac(&self, msg: &[u8]) -> Vec<u8> {
        let mut m =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        m.update(msg);
        m.finalize().into_bytes().to_vec()
    }

    pub fn issue(&self, user: &User, now: DateTime<Utc>) -> String {
        let claims = Claims {
            uid: user.id.to_string(),
            jti: Uuid::new_v4().to_string(),
            exp: now.timestamp() + self.ttl_secs,
        };
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims serialise"));
        let sig = URL_SAFE_NO_PAD.encode(self.mac(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Returns the verified claims if the token is authentic and unexpired.
    pub fn verify(&self, token: &str, now: DateTime<Utc>) -> Option<SessionClaims> {
        let (payload, sig) = token.split_once('.')?;
        let provided = URL_SAFE_NO_PAD.decode(sig).ok()?;
        // verify_slice is constant-time.
        let mut m = Hmac::<Sha256>::new_from_slice(&self.secret).ok()?;
        m.update(payload.as_bytes());
        m.verify_slice(&provided).ok()?;
        let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let claims: Claims = serde_json::from_slice(&json).ok()?;
        if claims.exp < now.timestamp() {
            return None;
        }
        Some(SessionClaims {
            uid: Uuid::parse_str(&claims.uid).ok()?,
            jti: claims.jti,
            exp: claims.exp,
        })
    }

    /// Build a `Set-Cookie` header value for a freshly issued token.
    pub fn cookie(&self, token: &str, max_age: i64) -> String {
        let mut c =
            format!("{SESSION_COOKIE}={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age={max_age}");
        if self.secure {
            c.push_str("; Secure");
        }
        c
    }
}

/// Extract the session token from a raw `Cookie` header value.
pub fn token_from_cookie_header(header: &str) -> Option<&str> {
    header.split(';').map(str::trim).find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == SESSION_COOKIE).then_some(v)
    })
}

/// Build the session signer from config. In production a strong secret is
/// required (#44): missing or shorter than 32 bytes fails fast rather than
/// silently using an ephemeral key that logs everyone out on restart.
pub fn make_session(secret: Option<String>, production: bool) -> Result<Session, String> {
    match secret.filter(|s| !s.is_empty()) {
        Some(s) if s.len() >= 32 => Ok(Session::new(s.into_bytes(), 60 * 60 * 24, production)),
        Some(_) if production => Err(
            "TUCANO_SESSION_SECRET is set but shorter than 32 bytes; refuse to start in production"
                .to_string(),
        ),
        Some(s) => {
            tracing::warn!("TUCANO_SESSION_SECRET is <32 bytes; use a longer key in production");
            Ok(Session::new(s.into_bytes(), 60 * 60 * 24, production))
        }
        None if production => Err(
            "TUCANO_SESSION_SECRET is required in production (generate: openssl rand -hex 32)"
                .to_string(),
        ),
        None => {
            tracing::warn!(
                "TUCANO_SESSION_SECRET unset: sessions are ephemeral (restart logs everyone out)"
            );
            let mut key = [0u8; 32];
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(&mut key);
            Ok(Session::new(key.to_vec(), 60 * 60 * 24, production))
        }
    }
}

// ------------------------------------------------------------- validation --

pub fn normalise_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_lowercase();
    let ok = e.len() <= 200 && e.contains('@') && !e.contains(' ') && e.matches('@').count() == 1;
    if ok { Some(e) } else { None }
}

pub fn valid_password(raw: &str) -> bool {
    (8..=200).contains(&raw.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_verifies_and_rejects_wrong() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("correct horse battery", "not-a-hash"));
    }

    #[test]
    fn session_roundtrips_and_expires() {
        let s = Session::new(b"0123456789abcdef0123456789abcdef".to_vec(), 3600, false);
        let user = User {
            id: Uuid::new_v4(),
            name: "A".into(),
            email: "a@b.co".into(),
            role: Role::Admin,
            active: true,
            default_rate_minor: 0,
            password_hash: String::new(),
            created_at: Utc::now(),
        };
        let now = Utc::now();
        let token = s.issue(&user, now);
        assert_eq!(s.verify(&token, now).map(|c| c.uid), Some(user.id));
        // Tampered signature fails.
        assert_eq!(s.verify(&format!("{token}x"), now).map(|c| c.uid), None);
        // Expired fails.
        let later = now + chrono::Duration::hours(2);
        assert_eq!(s.verify(&token, later).map(|c| c.uid), None);
    }

    #[test]
    fn cookie_header_parsing() {
        assert_eq!(
            token_from_cookie_header("other=1; tt_session=abc.def"),
            Some("abc.def")
        );
        assert_eq!(token_from_cookie_header("other=1"), None);
    }

    #[test]
    fn email_and_password_validation() {
        assert_eq!(
            normalise_email("  Bob@Example.COM "),
            Some("bob@example.com".into())
        );
        assert!(normalise_email("nope").is_none());
        assert!(normalise_email("a b@c.com").is_none());
        assert!(!valid_password("short"));
        assert!(valid_password("longenough"));
    }

    #[test]
    fn make_session_requires_strong_secret_in_production() {
        // Production with no secret -> refuse.
        assert!(make_session(None, true).is_err());
        // Production with a short secret -> refuse.
        assert!(make_session(Some("tooshort".into()), true).is_err());
        // Production with a strong secret -> ok, Secure cookie.
        let s = make_session(Some("0123456789abcdef0123456789abcdef".into()), true).unwrap();
        assert!(s.secure());
        // Dev with no secret -> ephemeral, non-secure.
        let d = make_session(None, false).unwrap();
        assert!(!d.secure());
    }
}
