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
    /// The person's internal cost rate in minor units, for profitability (#29).
    #[serde(default)]
    pub cost_rate_minor: u64,
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
    pub cost_rate_minor: u64,
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
            cost_rate_minor: u.cost_rate_minor,
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
/// Resolve the session signing secret (#94). Precedence:
/// `TUCANO_SESSION_SECRET` > `TUCANO_SESSION_SECRET_FILE` (mount-friendly) >
/// `<root>/session.key` — which is **auto-created** with fresh randomness on
/// first boot so a plain container restart/update no longer logs everyone
/// out. Returns `(secret, ephemeral)`; `secret` is `None` only when the file
/// could not be created (read-only volume), in which case the caller keeps
/// the old ephemeral behaviour.
pub fn resolve_session_secret(
    root: &std::path::Path,
    env_secret: Option<String>,
    env_secret_file: Option<String>,
    production: bool,
) -> (Option<String>, bool) {
    if let Some(s) = env_secret.filter(|x| !x.is_empty()) {
        return (Some(s), false);
    }
    if let Some(path) = env_secret_file.filter(|x| !x.is_empty()) {
        match std::fs::read_to_string(&path) {
            Ok(v) => {
                let v = v.trim().to_owned();
                if v.len() >= 32 || !production {
                    return (Some(v), false);
                }
                tracing::warn!(
                    "{path} contains a session secret shorter than 32 bytes; ignoring in production"
                );
            }
            Err(e) => tracing::warn!("cannot read TUCANO_SESSION_SECRET_FILE {path}: {e}"),
        }
    }
    let key_path = root.join("session.key");
    if let Ok(existing) = std::fs::read_to_string(&key_path) {
        let existing = existing.trim().to_owned();
        if existing.len() >= 32 {
            return (Some(existing), false);
        }
        tracing::warn!("{} is too short; regenerating", key_path.display());
    }
    // Generate a fresh key and persist it on the data volume.
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    match write_private_file(&key_path, secret.as_bytes()) {
        Ok(()) => (Some(secret), false),
        Err(e) => {
            tracing::warn!(
                "could not persist {} ({e}); sessions stay ephemeral — restart logs users out",
                key_path.display()
            );
            (None, true)
        }
    }
}

/// Write a file with `0600` permissions (owner read/write only), atomically.
pub fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

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
            cost_rate_minor: 0,
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

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use chrono::TimeZone;

    fn user(email: &str) -> User {
        User {
            id: uuid::Uuid::new_v4(),
            name: "A".into(),
            email: email.into(),
            role: Role::Member,
            active: true,
            default_rate_minor: 0,
            cost_rate_minor: 0,
            password_hash: String::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn session_secret_precedence_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // env wins and nothing is written
        let (s, _) = resolve_session_secret(root, Some("x".repeat(40)), None, false);
        assert_eq!(s.as_deref(), Some("x".repeat(40).as_str()));
        assert!(!root.join("session.key").exists());
        // no env anywhere -> auto-create on the volume
        let (s1, _) = resolve_session_secret(root, None, None, false);
        let s1 = s1.expect("generated");
        assert!(root.join("session.key").exists());
        // second "boot" reads the same key: sessions survive restart (#94)
        let (s2, _) = resolve_session_secret(root, None, None, false);
        let s2 = s2.expect("persisted");
        assert_eq!(s1, s2);
        // a token issued before the restart verifies after it
        let u1 = user("a@b.co");
        let session1 = make_session(Some(s1.clone()), false).unwrap();
        let token = session1.issue(&u1, Utc.timestamp_opt(1_730_000_000, 0).unwrap());
        let session2 = make_session(Some(s2), false).unwrap();
        let claims = session2
            .verify(&token, Utc.timestamp_opt(1_730_000_100, 0).unwrap())
            .expect("cross-boot token still valid");
        assert_eq!(claims.uid, u1.id);
        // *_FILE variant is honoured
        let keyfile = root.join("mounted.key");
        std::fs::write(&keyfile, format!("{}\n", "k".repeat(32))).unwrap();
        let (s3, _) =
            resolve_session_secret(root, None, Some(keyfile.display().to_string()), false);
        assert_eq!(s3.as_deref(), Some("k".repeat(32).as_str()));
    }

    #[test]
    fn persisted_session_key_is_private() {
        let dir = tempfile::tempdir().unwrap();
        resolve_session_secret(dir.path(), None, None, false)
            .0
            .unwrap();
        let md = std::fs::metadata(dir.path().join("session.key")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(md.permissions().mode() & 0o777, 0o600);
    }
}
