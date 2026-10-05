//! Shared plumbing for the integration providers (#101).
//!
//! Before this module, `payments`, `sso`, `accounting` and `calendar_oauth`
//! each re-implemented the `vault.get(k).or_else(env)` ladder — with drifted
//! precedence — and each kept a near-identical name-keyed registry. The
//! ladders live here once, and `Registry<T>` replaces the three copies.
//!
//! Two resolution lanes exist *by design* and must not be collapsed:
//! - **secrets** (#77): admin-entered vault values win; env is only the
//!   boot fallback for deployments where the vault key is absent. A secret
//!   can never enter `config.json` (the whitelist refuses secret-shaped keys,
//!   #94), so config.json never participates.
//! - **non-secret knobs** (#94): env > config.json > vault > none — the
//!   documented precedence for public settings like base URLs and client ids.

use std::sync::Arc;

use crate::appconfig::{self, AppConfig};
use crate::vault::SecretVault;

/// A provider is addressable by its stable name.
pub trait NamedProvider {
    fn name(&self) -> &str;
}

/// Name-keyed registry of enabled providers — the single generic behind
/// `PaymentRegistry`, `AccountingRegistry` and `SsoRegistry` (#101).
pub struct Registry<T: ?Sized> {
    providers: Vec<Arc<T>>,
}

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
        }
    }
}

impl<T: NamedProvider + ?Sized> Registry<T> {
    #[must_use]
    pub fn new(providers: Vec<Arc<T>>) -> Self {
        Self { providers }
    }

    /// Look a provider up by name. Case-insensitive: SSO historically
    /// tolerated casing drift in IdP names, and payments/accounting names are
    /// lowercase constants, so widening the other two cannot change any
    /// existing resolution.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<T>> {
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

/// The secret lane (#77): vault first, env only as the fallback when the
/// vault holds nothing for `vault_key`.
#[must_use]
pub fn resolve_secret(vault: Option<&SecretVault>, vault_key: &str, env: &str) -> Option<String> {
    vault
        .and_then(|v| v.get(vault_key))
        .or_else(|| std::env::var(env).ok())
}

/// The public-knob lane (#94): env > config.json > vault > env-on-vault-key.
/// `cfg_key` is the `config.json` whitelist name, `vault_key`/`env` the
/// legacy vault/env spellings of the same knob.
#[must_use]
pub fn resolve_setting(
    cfg: &AppConfig,
    vault: Option<&SecretVault>,
    cfg_key: &str,
    vault_key: &str,
    env: &str,
) -> Option<String> {
    cfg.get_optional_str(cfg_key, &appconfig::process_env)
        .or_else(|| resolve_secret(vault, vault_key, env))
}
