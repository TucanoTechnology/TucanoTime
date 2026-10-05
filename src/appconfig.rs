//! Externalised configuration (#94). Settings that decide *how the app
//! behaves* (not what it stores) used to be env-only, and env dies with the
//! container: a restart or image update silently reverted them. This module
//! gives every non-secret knob a durable home in `<data>/config.json` with a
//! fixed precedence:
//!
//! ```text
//! explicit env var  >  config.json entry  >  built-in default
//! ```
//!
//! Secret material (vault key, session secret, SMTP passwords, webhook/tokens)
//! deliberately does NOT live here — config.json is plaintext next to the
//! data it protects. Secrets keep the env/vault story (#77) plus the new
//! `*_FILE` mounts; the whitelist below is enforced on write, so
//! `PUT /admin/config` physically cannot persist a secret by accident.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    File,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `true` when the env var is *present at all* (existing demo-flag
    /// semantics) or when the file says true.
    BoolFlag,
    Int {
        min: i64,
        max: i64,
        default: i64,
    },
    Str {
        default: &'static str,
    },
    OptionalStr,
    /// Comma-separated list in both env and file.
    Csv,
}

pub struct KeyDef {
    /// Key inside config.json.
    pub name: &'static str,
    /// Overriding environment variable.
    pub env: &'static str,
    pub kind: Kind,
    pub description: &'static str,
}

/// The complete whitelist of externalisable knobs. Anything not here cannot be
/// read from or written to config.json.
pub const KEYS: &[KeyDef] = &[
    KeyDef {
        name: "reminder_days",
        env: "TUCANO_REMINDER_DAYS",
        kind: Kind::Int {
            min: 1,
            max: 365,
            default: 7,
        },
        description: "Days between overdue-invoice email reminders (#35)",
    },
    KeyDef {
        name: "sso_admin_group",
        env: "TUCANO_SSO_ADMIN_GROUP",
        kind: Kind::Str { default: "" },
        description: "IdP group that maps to the admin role on SSO login (#32)",
    },
    KeyDef {
        name: "sso_allowed_domains",
        env: "TUCANO_SSO_ALLOWED_DOMAINS",
        kind: Kind::Csv,
        description: "Comma-separated email domains allowed for SSO (#32)",
    },
    KeyDef {
        name: "calendar_oauth_redirect",
        env: "TUCANO_CALENDAR_OAUTH_REDIRECT",
        kind: Kind::Str {
            default: "/calendar/oauth/callback",
        },
        description: "Redirect URI registered with the OAuth calendar app (#36)",
    },
    KeyDef {
        name: "google_calendar_client_id",
        env: "TUCANO_GOOGLE_CLIENT_ID",
        kind: Kind::OptionalStr,
        description: "Google OAuth client id (public half of the pair; the secret stays env/vault) (#36)",
    },
    KeyDef {
        name: "ms_calendar_client_id",
        env: "TUCANO_MS_CLIENT_ID",
        kind: Kind::OptionalStr,
        description: "Microsoft OAuth client id (secret stays env/vault) (#36)",
    },
    KeyDef {
        name: "qbo_base_url",
        env: "TUCANO_QBO_BASE_URL",
        kind: Kind::OptionalStr,
        description: "Override the QuickBooks API base URL (#33)",
    },
    KeyDef {
        name: "xero_base_url",
        env: "TUCANO_XERO_BASE_URL",
        kind: Kind::OptionalStr,
        description: "Override the Xero API base URL (#33)",
    },
    KeyDef {
        name: "max_docs",
        env: "TUCANO_MAX_DOCS",
        kind: Kind::Int {
            min: 10,
            max: 1_000_000,
            default: 50_000,
        },
        description: "Per-collection document cap before writes are refused",
    },
    KeyDef {
        name: "stripe_demo",
        env: "TUCANO_STRIPE_FAKE",
        kind: Kind::BoolFlag,
        description: "Demo mode: accept Stripe webhooks without signature verification (NEVER for production)",
    },
    KeyDef {
        name: "paypal_demo",
        env: "TUCANO_PAYPAL_FAKE",
        kind: Kind::BoolFlag,
        description: "Demo mode: accept PayPal webhooks without signature verification (NEVER for production)",
    },
];

pub fn key_def(name: &str) -> Option<&'static KeyDef> {
    KEYS.iter().find(|k| k.name == name)
}

/// The loaded file contents (only whitelisted keys are kept).
#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    stored: Map<String, Value>,
    path: PathBuf,
}

impl AppConfig {
    /// Load `<root>/config.json`. Missing file is fine; a corrupt one is
    /// surfaced to the caller (refusing to start is right for a broken store).
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("config.json");
        let stored = match std::fs::read(&path) {
            Ok(bytes) => {
                let v: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("corrupt {}: {e}", path.display()))?;
                v.as_object().cloned().unwrap_or_default()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        // Drop unknown keys defensively (written by an older/newer version).
        let stored = stored
            .into_iter()
            .filter(|(k, _)| key_def(k).is_some())
            .collect();
        Ok(Self { stored, path })
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    fn env_lookup(env: &dyn Fn(&str) -> Option<String>, def: &KeyDef) -> Option<String> {
        env(def.env).filter(|s| !s.is_empty() || def.kind == Kind::BoolFlag)
    }

    /// Resolve one key with precedence env > file > default. `env_fn` returns
    /// `Some(_)` when the variable is set (even to "") so `BoolFlag` keeps the
    /// "present means true" semantics.
    pub fn resolve(
        &self,
        def: &'static KeyDef,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> (Value, Source) {
        if let Some(raw) = Self::env_lookup(env, def) {
            let v = match def.kind {
                Kind::BoolFlag => Value::Bool(true), // presence = on (matches old env semantics)
                Kind::Int { min, max, .. } => raw
                    .parse::<i64>()
                    .ok()
                    .filter(|v| (min..=max).contains(v))
                    .map(Value::from)
                    .unwrap_or_else(|| {
                        tracing::warn!(key = def.name, value = %raw, "invalid int from env, ignoring");
                        Value::Null
                    }),
                Kind::Csv | Kind::Str { .. } | Kind::OptionalStr => Value::String(raw),
            };
            if v.is_null() {
                // fall through to file/default on a parse failure
            } else {
                return (v, Source::Env);
            }
        }
        if let Some(v) = self.stored.get(def.name) {
            return (v.clone(), Source::File);
        }
        let d = match def.kind {
            Kind::BoolFlag => Value::Bool(false),
            Kind::Int { default, .. } => Value::from(default),
            Kind::Str { default } => Value::String(default.to_string()),
            Kind::OptionalStr | Kind::Csv => Value::Null,
        };
        (d, Source::Default)
    }

    /// Convenience typed accessors used by the boot/integration sites.
    pub fn get_int(&self, name: &str, env: &dyn Fn(&str) -> Option<String>) -> i64 {
        let def = key_def(name).expect("whitelisted key");
        match self.resolve(def, env).0 {
            Value::Number(n) => n.as_i64().unwrap_or(0),
            _ => 0,
        }
    }
    pub fn get_str(&self, name: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
        let def = key_def(name).expect("whitelisted key");
        match self.resolve(def, env).0 {
            Value::String(s) => s,
            _ => String::new(),
        }
    }
    pub fn get_optional_str(
        &self,
        name: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        let def = key_def(name).expect("whitelisted key");
        match self.resolve(def, env).0 {
            Value::String(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
    pub fn get_bool_flag(&self, name: &str, env: &dyn Fn(&str) -> Option<String>) -> bool {
        let def = key_def(name).expect("whitelisted key");
        matches!(self.resolve(def, env).0, Value::Bool(true))
    }
    pub fn get_csv(&self, name: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        let def = key_def(name).expect("whitelisted key");
        match self.resolve(def, env).0 {
            Value::String(s) => s
                .split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Validate one whitelisted patch value against its kind. Returns the
    /// normalised value to store, or an error message for the API.
    pub fn validate(def: &'static KeyDef, value: &Value) -> Result<Value, String> {
        match (def.kind, value) {
            (Kind::BoolFlag, Value::Bool(b)) => Ok(Value::Bool(*b)),
            (Kind::Int { min, max, .. }, Value::Number(n)) => match n.as_i64() {
                Some(v) if (min..=max).contains(&v) => Ok(Value::from(v)),
                Some(v) => Err(format!("{v} outside {min}..={max}")),
                None => Err("not an integer".into()),
            },
            (Kind::Csv, Value::String(s)) | (Kind::Str { .. }, Value::String(s)) => {
                if s.len() > 500 {
                    return Err("too long".into());
                }
                Ok(Value::String(s.clone()))
            }
            (Kind::OptionalStr, Value::String(s)) => {
                if s.len() > 300 {
                    return Err("too long".into());
                }
                Ok(Value::String(s.clone()))
            }
            (Kind::OptionalStr, Value::Null) => Ok(Value::Null),
            _ => Err(format!(
                "{} expects {}",
                def.name,
                match def.kind {
                    Kind::BoolFlag => "a boolean",
                    Kind::Int { .. } => "an integer",
                    Kind::Csv | Kind::Str { .. } | Kind::OptionalStr => "a string",
                }
            )),
        }
    }

    /// Merge a validated patch and write the whole file atomically.
    pub fn update(&mut self, patch: Map<String, Value>) -> Result<(), String> {
        for (k, v) in patch {
            match key_def(&k) {
                Some(def) => {
                    let v = Self::validate(def, &v)?;
                    if v.is_null() {
                        self.stored.remove(&k);
                    } else {
                        self.stored.insert(k, v);
                    }
                }
                // Unknown or secret-shaped keys are refused by the whitelist.
                None => return Err(format!("unknown configuration key: {k}")),
            }
        }
        self.persist()
    }

    fn persist(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&Value::Object(self.stored.clone()))
            .map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The effective table for `GET /admin/config`: every whitelisted key with
    /// its resolved value, source, and a default. Secret material is not part
    /// of the whitelist, so nothing needs masking here.
    pub fn effective(&self, env: &dyn Fn(&str) -> Option<String>) -> Vec<Value> {
        KEYS.iter()
            .map(|def| {
                let (value, source) = self.resolve(def, env);
                let default = match def.kind {
                    Kind::BoolFlag => Value::Bool(false),
                    Kind::Int { default, .. } => Value::from(default),
                    Kind::Str { default } => Value::String(default.to_string()),
                    Kind::OptionalStr | Kind::Csv => Value::Null,
                };
                serde_json::json!({
                    "key": def.name,
                    "value": value,
                    "source": match source {
                        Source::Env => "env", Source::File => "file", Source::Default => "default"
                    },
                    "default": default,
                    "description": def.description,
                })
            })
            .collect()
    }
}

/// Process env adapter used at the real call sites; tests inject a map.
#[must_use]
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn file(json: &str) -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        AppConfig::load(dir.path()).unwrap()
    }

    #[test]
    fn precedence_env_then_file_then_default() {
        let cfg = file(r#"{"reminder_days": 14}"#);
        // default when neither set
        let empty = AppConfig::empty();
        let env_none = env_from(&[]);
        assert_eq!(
            empty
                .resolve(key_def("reminder_days").unwrap(), &env_none)
                .0,
            Value::from(7)
        );
        // file wins over default
        assert_eq!(
            cfg.resolve(key_def("reminder_days").unwrap(), &env_none).0,
            Value::from(14)
        );
        // env wins over file
        let env = env_from(&[("TUCANO_REMINDER_DAYS", "3")]);
        let (v, src) = cfg.resolve(key_def("reminder_days").unwrap(), &env);
        assert_eq!(v, Value::from(3));
        assert_eq!(src, Source::Env);
    }

    #[test]
    fn bool_flag_present_means_true() {
        let cfg = AppConfig::empty();
        let env = env_from(&[("TUCANO_STRIPE_FAKE", "")]);
        assert!(cfg.get_bool_flag("stripe_demo", &env));
        let file_cfg = file(r#"{"paypal_demo": true}"#);
        assert!(file_cfg.get_bool_flag("paypal_demo", &env_from(&[])));
        assert!(!cfg.get_bool_flag("paypal_demo", &env_from(&[])));
    }

    #[test]
    fn unknown_and_secret_shaped_keys_refused_update_drops_unknown_load() {
        let mut cfg = AppConfig::empty();
        let mut patch = Map::new();
        patch.insert("smtp_password".into(), Value::String("hunter2".into()));
        assert!(cfg.update(patch).is_err());
        // load silently drops keys not on the whitelist
        let loaded = file(r#"{"bogus": 1, "reminder_days": 5}"#);
        let v = loaded
            .resolve(key_def("reminder_days").unwrap(), &env_from(&[]))
            .0;
        assert_eq!(v, Value::from(5));
        assert!(!loaded.stored.contains_key("bogus"));
    }

    #[test]
    fn range_validation_and_persist_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        let mut cfg = AppConfig::load(dir.path()).unwrap();
        let mut bad = Map::new();
        bad.insert("reminder_days".into(), Value::from(0)); // below min 1
        assert!(cfg.update(bad).is_err());
        let mut good = Map::new();
        good.insert("reminder_days".into(), Value::from(21));
        good.insert("sso_admin_group".into(), Value::String("tt-admins".into()));
        cfg.update(good).unwrap();
        let reloaded = AppConfig::load(dir.path()).unwrap();
        let env = env_from(&[]);
        assert_eq!(reloaded.get_int("reminder_days", &env), 21);
        assert_eq!(reloaded.get_str("sso_admin_group", &env), "tt-admins");
    }

    #[test]
    fn csv_parsing() {
        let env = env_from(&[("TUCANO_SSO_ALLOWED_DOMAINS", "acme.test, other.test ,")]);
        assert_eq!(
            AppConfig::empty().get_csv("sso_allowed_domains", &env),
            vec!["acme.test".to_string(), "other.test".to_string()]
        );
    }
}
