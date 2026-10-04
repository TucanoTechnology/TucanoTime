//! Encrypted credential vault (#77). Admin-entered integration secrets
//! (Stripe keys, OAuth client secrets, SMTP password, ICS feed URLs, …) are
//! stored **encrypted at rest** with AES-256-GCM under a master key supplied
//! out-of-band via `TUCANO_SECRET_KEY`. Values are never returned to any client
//! (only a masked hint) and never logged. If the master key is absent the vault
//! is disabled (fail-closed) rather than storing plaintext.
//!
//! The whole key→value map is encrypted as one blob (`secrets.bin` =
//! nonce ‖ ciphertext). Integrations consume it via [`SecretVault::get`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, Nonce, OsRng};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault is not configured (set TUCANO_SECRET_KEY)")]
    Disabled,
    #[error("invalid master key")]
    BadKey,
    #[error("could not decrypt the vault (wrong key or corrupt file)")]
    Decrypt,
    #[error("io failure")]
    Io,
}

pub struct SecretVault {
    path: PathBuf,
    key: [u8; KEY_LEN],
    data: Mutex<BTreeMap<String, String>>,
}

impl SecretVault {
    /// Open (or create) the vault. `master_key` should come from
    /// `TUCANO_SECRET_KEY` (exactly 32 bytes). Returns `Ok(None)` when no key
    /// is provided — the caller treats the feature as disabled.
    pub fn open(root: &Path, master_key: Option<&str>) -> Result<Option<Self>, VaultError> {
        let Some(raw) = master_key.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let key_bytes = raw.as_bytes();
        if key_bytes.len() != KEY_LEN {
            return Err(VaultError::BadKey);
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(key_bytes);

        let path = root.join("secrets.bin");
        let data = if path.exists() {
            let blob = fs::read(&path).map_err(|_| VaultError::Io)?;
            let json = Self::decrypt(&key, &blob)?;
            serde_json::from_slice(&json).map_err(|_| VaultError::Decrypt)?
        } else {
            BTreeMap::new()
        };
        Ok(Some(Self {
            path,
            key,
            data: Mutex::new(data),
        }))
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.key).expect("32-byte key")
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher()
            .encrypt(&nonce, plaintext)
            .map_err(|_| VaultError::Decrypt)?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ct);
        Ok(blob)
    }

    fn decrypt(key: &[u8; KEY_LEN], blob: &[u8]) -> Result<Vec<u8>, VaultError> {
        if blob.len() < NONCE_LEN {
            return Err(VaultError::Decrypt);
        }
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| VaultError::BadKey)?;
        let nonce = Nonce::<Aes256Gcm>::from_slice(nonce_bytes);
        cipher.decrypt(nonce, ct).map_err(|_| VaultError::Decrypt)
    }

    fn persist(&self, map: &BTreeMap<String, String>) -> Result<(), VaultError> {
        let json = serde_json::to_vec(map).map_err(|_| VaultError::Decrypt)?;
        let blob = self.encrypt(&json)?;
        fs::write(&self.path, blob).map_err(|_| VaultError::Io)
    }

    /// Store/overwrite a secret. `key` is a provider path e.g. `stripe.secret_key`.
    pub fn put(&self, key: &str, value: &str) -> Result<(), VaultError> {
        let mut map = lock(&self.data);
        map.insert(key.to_string(), value.to_string());
        self.persist(&map)
    }

    pub fn get(&self, key: &str) -> Option<String> {
        lock(&self.data).get(key).cloned()
    }

    pub fn remove(&self, key: &str) -> Result<bool, VaultError> {
        let mut map = lock(&self.data);
        let removed = map.remove(key).is_some();
        self.persist(&map)?;
        Ok(removed)
    }

    /// Configured secret keys (names only — never values).
    pub fn keys(&self) -> Vec<String> {
        lock(&self.data).keys().cloned().collect()
    }

    /// A masked hint for display: last 4 chars of the value, or empty.
    pub fn hint(&self, key: &str) -> String {
        match lock(&self.data).get(key) {
            Some(v) if v.len() >= 4 => format!("••••{}", &v[v.len() - 4..]),
            Some(_) => "••••".to_string(),
            None => String::new(),
        }
    }
}

fn lock(
    m: &Mutex<BTreeMap<String, String>>,
) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "unit-test-vault-key-000000000000"; // 32 bytes, non-secret fixture

    #[test]
    fn disabled_without_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SecretVault::open(dir.path(), None).unwrap().is_none());
        assert!(SecretVault::open(dir.path(), Some("")).unwrap().is_none());
    }

    #[test]
    fn rejects_bad_key_length() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            SecretVault::open(dir.path(), Some("tooshort")),
            Err(VaultError::BadKey)
        ));
    }

    #[test]
    fn roundtrip_masks_and_persists_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SecretVault::open(dir.path(), Some(KEY)).unwrap().unwrap();
        vault
            .put("stripe.secret_key", "unit-test-secret-value-abcdef")
            .unwrap();
        assert_eq!(
            vault.get("stripe.secret_key").as_deref(),
            Some("unit-test-secret-value-abcdef")
        );
        assert_eq!(vault.hint("stripe.secret_key"), "••••cdef");
        assert_eq!(vault.keys(), vec!["stripe.secret_key".to_string()]);

        // On-disk blob must not contain the plaintext.
        let blob = fs::read(dir.path().join("secrets.bin")).unwrap();
        assert!(
            !blob.windows(10).any(|w| w == b"secret-value"),
            "plaintext leaked to disk"
        );

        // Reopen: value survives (decrypted with the same key).
        let v2 = SecretVault::open(dir.path(), Some(KEY)).unwrap().unwrap();
        assert_eq!(
            v2.get("stripe.secret_key").as_deref(),
            Some("unit-test-secret-value-abcdef")
        );
    }

    #[test]
    fn wrong_key_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        SecretVault::open(dir.path(), Some(KEY))
            .unwrap()
            .unwrap()
            .put("k", "v")
            .unwrap();
        assert!(matches!(
            SecretVault::open(dir.path(), Some("wrong-vault-key-0000000000000000")),
            Err(VaultError::Decrypt)
        ));
    }
}
