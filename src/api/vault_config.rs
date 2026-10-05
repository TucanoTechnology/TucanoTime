//! Secret vault and externalised configuration (#77, #94): the admin surface
//! for masked secret CRUD and the whitelisted `config.json` knobs, plus the
//! #52 audit-log view.

use super::*;

/// `GET /admin/config` (#94): the effective externalised configuration —
/// every whitelisted non-secret knob with its resolved value, the source it
/// came from (env / config.json / default) and its default. The whitelist has
/// no secret material by construction, so nothing needs masking; anything
/// shaped like a secret cannot even be persisted (whitelist enforced on write).
pub async fn admin_config(State(app): State<AppState>) -> ApiResult {
    let env_fn = crate::appconfig::process_env;
    let effective = app.cfg().effective(&env_fn);
    Ok(Json(serde_json::json!({
        "config": effective,
        "path": app.store.root().join("config.json").display().to_string(),
        "vault_enabled": app.vault.is_some(),
    }))
    .into_response())
}

/// `PUT /admin/config` (#94): patch whitelisted keys into `<data>/config.json`.
/// Unknown or secret-shaped keys are refused. Takes effect on the next boot
/// for boot-time knobs (session/vault resolution) and immediately for
/// request-time ones (SSO domains/admin group, OAuth redirect).
pub async fn update_config(
    State(app): State<AppState>,
    ValidJson(patch): ValidJson<serde_json::Map<String, serde_json::Value>>,
) -> ApiResult {
    let mut cfg =
        crate::appconfig::AppConfig::load(app.store.root()).map_err(ApiError::internal)?;
    cfg.update(patch)
        .map_err(|e| ApiError::validation(vec![FieldError::new("config", e)]))?;
    // Reflect the change for request-time readers.
    app.reload_config(Arc::new(cfg));
    Ok(Json(serde_json::json!({ "ok": true })).into_response())
}

fn valid_secret_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 100
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn require_vault(app: &AppState) -> Result<&crate::vault::SecretVault, ApiError> {
    app.vault.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault_disabled",
            "credential vault is not configured (set TUCANO_SECRET_KEY)",
        )
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretInput {
    pub value: String,
}

/// List configured secret keys with masked hints — never the values (#77).
pub async fn list_secrets(State(app): State<AppState>) -> ApiResult {
    let vault = require_vault(&app)?;
    let secrets: Vec<serde_json::Value> = vault
        .keys()
        .into_iter()
        .map(|k| serde_json::json!({ "key": k, "hint": vault.hint(&k) }))
        .collect();
    Ok(Json(serde_json::json!({ "secrets": secrets })).into_response())
}

/// Store/overwrite a secret (admin only). The value is never echoed back.
pub async fn set_secret(
    State(app): State<AppState>,
    Path(key): Path<String>,
    ValidJson(input): ValidJson<SecretInput>,
) -> ApiResult {
    let vault = require_vault(&app)?;
    if !valid_secret_key(&key) {
        return Err(ApiError::validation(vec![FieldError::new(
            "key",
            "use letters, digits, '.', '_' or '-' (max 100)",
        )]));
    }
    vault
        .put(&key, &input.value)
        .map_err(|e| ApiError::internal(format!("vault write failed: {e}")))?;
    app.audit.record("secret_set", &key, app.clock.now());
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Delete a secret (admin only).
pub async fn delete_secret(State(app): State<AppState>, Path(key): Path<String>) -> ApiResult {
    let vault = require_vault(&app)?;
    let removed = vault
        .remove(&key)
        .map_err(|e| ApiError::internal(format!("vault write failed: {e}")))?;
    if !removed {
        return Err(ApiError::not_found("secret"));
    }
    app.audit.record("secret_deleted", &key, app.clock.now());
    Ok(StatusCode::NO_CONTENT.into_response())
}
