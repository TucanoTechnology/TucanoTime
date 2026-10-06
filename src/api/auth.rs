//! Authentication and SSO: session issue/verify endpoints, bootstrap of the
//! first admin, and the #32 assertion consumer.

use super::*;
use crate::auth;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginInput {
    email: String,
    password: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapInput {
    name: String,
    email: String,
    password: String,
}

fn set_cookie(state: &AppState, user: &User) -> (http::HeaderMap, PublicUser) {
    let token = state.session.issue(user, state.clock.now());
    let mut headers = http::HeaderMap::new();
    if let Ok(v) = state.session.cookie(&token, 60 * 60 * 24).parse() {
        headers.insert(http::header::SET_COOKIE, v);
    }
    (headers, PublicUser::from(user))
}

/// Create the first administrator. Open only while no users exist.
pub async fn bootstrap(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<BootstrapInput>,
) -> ApiResult {
    if app.rate.is_locked("bootstrap") {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many attempts; try again later",
        ));
    }
    if app.store.has_users()? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "initialised",
            "an administrator already exists",
        ));
    }
    let email = normalise_email(&input.email)
        .ok_or_else(|| ApiError::validation(vec![FieldError::new("email", "invalid email")]))?;
    if !valid_password(&input.password) {
        return Err(ApiError::validation(vec![FieldError::new(
            "password",
            "must be at least 8 characters",
        )]));
    }
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(ApiError::validation(vec![FieldError::new(
            "name",
            "required, at most 120 characters",
        )]));
    }
    // Hash outside the lock; the check-then-insert is atomic in the store
    // (review A12/B4: two racing bootstraps must not mint two admins).
    let password_hash = auth::hash_password(&input.password)
        .map_err(|_| ApiError::internal("password hashing failed".into()))?;
    let user = User {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        email: email.clone(),
        role: Role::Admin,
        active: true,
        default_rate_minor: 0,
        cost_rate_minor: 0,
        password_hash,
        created_at: app.clock.now(),
        session_version: 1,
    };
    match app.store.put_user_if_none(&user) {
        Ok(()) => {}
        Err(crate::store::StoreError::AlreadyExists(_)) => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "initialised",
                "an administrator already exists",
            ));
        }
        Err(other) => return Err(other.into()),
    }
    app.rate.reset("bootstrap");
    app.audit.record("bootstrap", &email, app.clock.now());
    let (headers, pubuser) = set_cookie(&app, &user);
    Ok((StatusCode::CREATED, headers, Json(pubuser)).into_response())
}

pub async fn login(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<LoginInput>,
) -> ApiResult {
    let email = normalise_email(&input.email).unwrap_or_default();
    let key = format!("login:{email}");
    if app.rate.is_locked(&key) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many failed attempts; try again later",
        ));
    }
    let user = app.store.get_user_by_email(&email)?;
    let ok = user
        .as_ref()
        .is_some_and(|u| u.active && verify_password(&input.password, &u.password_hash));
    if !ok {
        // Same response for unknown email and bad password (no user enumeration).
        app.rate.record_failure(&key);
        app.audit.record("login_failed", &email, app.clock.now());
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid email or password",
        ));
    }
    app.rate.reset(&key);
    app.audit.record("login_ok", &email, app.clock.now());
    let (headers, pubuser) = set_cookie(&app, &user.unwrap());
    Ok((headers, Json(pubuser)).into_response())
}

pub async fn logout(State(app): State<AppState>, req: axum::extract::Request) -> ApiResult {
    // Revoke the current session so the cookie stops working before expiry (#45).
    if let Some(header) = req
        .headers()
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        && let Some(token) = token_from_cookie_header(header)
        && let Some(claims) = app.session.verify(token, app.clock.now())
    {
        app.revocations
            .revoke(&claims.jti, claims.exp, app.clock.now());
        app.audit
            .record("logout", &claims.uid.to_string(), app.clock.now());
    }
    let mut headers = http::HeaderMap::new();
    let cookie = format!(
        "{}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0",
        auth::SESSION_COOKIE
    );
    if app.session.secure() {
        // Rebuild with Secure when configured.
        let cookie = format!("{cookie}; Secure");
        if let Ok(v) = cookie.parse() {
            headers.insert(http::header::SET_COOKIE, v);
        }
    } else if let Ok(v) = cookie.parse() {
        headers.insert(http::header::SET_COOKIE, v);
    }
    Ok((headers, StatusCode::NO_CONTENT).into_response())
}

pub async fn me(State(app): State<AppState>, req: axum::extract::Request) -> ApiResult {
    let user = authenticate(&app, req.headers())?;
    Ok(Json(PublicUser::from(&user)).into_response())
}

/// Public: whether an administrator exists yet (drives first-run setup UI).
pub async fn auth_status(State(app): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::json!({ "initialised": app.store.has_users()? })).into_response())
}

// --------------------------------------------------------------------- sso --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoAssertionInput {
    pub provider: String,
    /// Raw signed assertion (compact token or base64 SAML statement).
    pub payload: String,
    #[serde(default)]
    pub signature: String,
}

/// Public: which SSO providers are configured (drives the login screen).
pub async fn sso_providers(State(app): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::json!({ "providers": app.sso.names() })).into_response())
}

/// SSO assertion consumer (#32). Verifies the provider signature, maps groups
/// to the #19 roles, and just-in-time provisions the user, then issues the
/// same session cookie as local login. Local login keeps working alongside.
pub async fn sso_assertion(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<SsoAssertionInput>,
) -> ApiResult {
    let idp = provider_or_400(app.sso.get(&input.provider), "SSO")?;
    // Review A12: assertion verification gets the same lockout treatment as
    // login, so repeated forged/failed assertions cannot be brute-forced.
    let rate_key = format!("sso:{provider}", provider = input.provider);
    if app.rate.is_locked(&rate_key) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many failed attempts; try again later",
        ));
    }
    let identity = idp.verify(&input.payload, &input.signature).map_err(|e| {
        app.rate.record_failure(&rate_key);
        match e {
            crate::sso::SsoError::BadSignature | crate::sso::SsoError::Expired => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "sso_invalid",
                "assertion failed verification",
            ),
            other => {
                tracing::warn!(error = %other, "SSO assertion rejected");
                app.rate.reset(&rate_key); // well-formed-but-invalid is not an attack signal
                ApiError::bad_request("assertion rejected")
            }
        }
    })?;
    app.rate.reset(&rate_key);

    let existing = app.store.get_user_by_email(&identity.email)?;
    let user = match existing {
        Some(u) => {
            if !u.active {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "inactive",
                    "this account is deactivated",
                ));
            }
            // JIT group refresh: admins follow the mapped role on every login.
            let admin_group = std::env::var("TUCANO_SSO_ADMIN_GROUP").unwrap_or_default();
            let role = crate::sso::map_role(&identity.groups, &admin_group);
            if role != u.role && !admin_group.is_empty() {
                let mut updated = u.clone();
                updated.role = role;
                app.store.put_user(&updated)?;
                updated
            } else {
                u
            }
        }
        None => {
            // Just-in-time provisioning (#19): the IdP is the password store,
            // so local login gets an unsusable random password.
            let admin_group = app
                .cfg()
                .get_str("sso_admin_group", &crate::appconfig::process_env);
            let role = crate::sso::map_role(&identity.groups, &admin_group);
            let temp_password = Uuid::new_v4().to_string();
            new_user(
                if identity.name.is_empty() {
                    identity.email.as_str()
                } else {
                    identity.name.as_str()
                },
                &identity.email,
                &temp_password,
                NewUser {
                    role,
                    active: true,
                    default_rate_minor: 0,
                    cost_rate_minor: 0,
                },
                &app,
            )?
        }
    };
    app.audit.record(
        &format!("sso_login:{}", idp.name()),
        &identity.email,
        app.clock.now(),
    );
    let (headers, pubuser) = set_cookie(&app, &user);
    Ok((headers, Json(pubuser)).into_response())
}

/// Recent security audit events (admin only, #52).
pub async fn audit_log(State(app): State<AppState>) -> ApiResult {
    let events = app.audit.recent(200);
    Ok(Json(serde_json::json!({ "events": events })).into_response())
}
