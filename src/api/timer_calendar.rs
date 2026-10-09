//! Time capture and calendar: the running timer, ICS/OAuth calendar feeds
//! (#15, #36), notifications (#22) and recurring schedules (#61).

use super::*;

/// Current running timer for the caller, with live elapsed, or null.
pub async fn get_timer(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    match app.store.get_timer(actor.0.id)? {
        Some(t) => {
            let now = app.clock.now();
            let hundredths = elapsed_hundredths(t.started_at, now);
            Ok(Json(serde_json::json!({
                "timer": t,
                "elapsed_seconds": (now - t.started_at).num_seconds().max(0),
                "elapsed_hours": Hours(hundredths),
            }))
            .into_response())
        }
        None => Ok(Json(serde_json::Value::Null).into_response()),
    }
}

/// Start a timer (one active per user).
pub async fn start_timer(
    State(app): State<AppState>,
    actor: AuthUser,
    ValidJson(input): ValidJson<StartTimerInput>,
) -> ApiResult {
    if app.store.get_timer(actor.0.id)?.is_some() {
        return Err(ApiError::conflict("a timer is already running"));
    }
    let mut errors = Vec::new();
    if app.store.get_customer(input.customer_id)?.is_none() {
        errors.push(FieldError::new("customer_id", "customer does not exist"));
    }
    let project = app
        .store
        .get_project(input.customer_id, &input.project_code.0)?;
    if project.is_none() {
        errors.push(FieldError::new(
            "project_code",
            "project does not exist for this customer",
        ));
    }
    if let Some(tc) = &input.task_code {
        let task = app
            .store
            .get_task(input.customer_id, &input.project_code.0, &tc.0)?;
        if task.is_none() {
            errors.push(FieldError::new(
                "task_code",
                "task does not exist for this project",
            ));
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let timer = Timer {
        user_id: actor.0.id,
        customer_id: input.customer_id,
        project_code: input.project_code,
        task_code: input.task_code,
        note: input.note,
        started_at: app.clock.now(),
    };
    app.store.put_timer(&timer)?;
    Ok((StatusCode::CREATED, Json(timer)).into_response())
}

/// Stop the timer: create a `timer`-sourced entry for the elapsed time.
pub async fn stop_timer(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    let timer = app
        .store
        .get_timer(actor.0.id)?
        .ok_or_else(|| ApiError::not_found("timer"))?;
    let now = app.clock.now();
    let entry = Entry {
        id: Uuid::new_v4(),
        date: now.date_naive(),
        customer_id: timer.customer_id,
        user_id: Some(timer.user_id),
        project_code: timer.project_code,
        task_code: timer.task_code,
        hours: Hours(elapsed_hundredths(timer.started_at, now)),
        note: timer.note,
        billable: true,
        source: Source::Timer,
        created_at: now,
        updated_at: now,
    };
    // Entry + timer-clear in one lock (review B4): a failure between the
    // two left the timer running, so a retry double-logged.
    // #214: if a concurrent stop claimed the timer between our unlocked read
    // and this transaction, finish_timer refuses — report the same honest 404
    // as the pre-read above, never a phantom double-logged entry.
    app.store
        .finish_timer(timer.user_id, &entry)
        .map_err(|e| match e {
            crate::store::StoreError::NotFound => ApiError::not_found("timer"),
            other => other.into(),
        })?;
    Ok(Json(entry_json(&entry)).into_response())
}

/// Discard the running timer without creating an entry.
pub async fn discard_timer(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    app.store.delete_timer(actor.0.id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------- calendar --

/// List calendar events overlapping the range, from the configured ICS feed
/// (vault key `calendar.ics_url` or `TUCANO_CALENDAR_ICS`). #15.
pub async fn calendar_events(
    State(app): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> ApiResult {
    let (from, to) = q.dates()?;
    // #36: Google/Microsoft OAuth providers take precedence when configured;
    // otherwise the #15 ICS feed remains the default (locked decision).
    if let Some(source) = crate::calendar_oauth::configured_provider(app.vault.as_deref())
        .and_then(|p| crate::calendar_oauth::source_for(&p, app.vault.as_deref()))
    {
        let events = blocking("calendar provider fetch", move || {
            source.fetch(from, to).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "calendar_fetch",
                    "could not fetch the calendar provider",
                )
            })
        })
        .await?;
        return Ok(Json(serde_json::json!({ "events": events })).into_response());
    }
    let source = crate::providers::resolve_secret(
        app.vault.as_deref(),
        "calendar.ics_url",
        "TUCANO_CALENDAR_ICS",
    )
    .filter(|s| !s.is_empty())
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "calendar_not_configured",
            "no calendar feed is configured",
        )
    })?;
    let text = blocking("calendar fetch", move || {
        crate::calendar::fetch_ics(&source).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "calendar_fetch",
                "could not fetch the calendar feed",
            )
        })
    })
    .await?;
    let events = crate::calendar::parse_ics(&text, from, to);
    Ok(Json(serde_json::json!({ "events": events })).into_response())
}

// ---------------------------------------------------------- notifications --

/// `GET /api/calendar/oauth/start` — optional `provider` param (default google).
#[derive(Debug, serde::Deserialize)]
pub struct OAuthStartQuery {
    #[serde(default)]
    provider: Option<String>,
}

/// Starts the OAuth consent flow for a calendar provider (#36, admin). Returns
/// the provider's authorize URL; the browser is redirected there.
pub async fn calendar_oauth_start(
    State(app): State<AppState>,
    Query(q): Query<OAuthStartQuery>,
) -> ApiResult {
    let provider = q
        .provider
        .unwrap_or_else(|| "google".to_owned())
        .to_ascii_lowercase();
    if !matches!(provider.as_str(), "google" | "microsoft") {
        return Err(ApiError::bad_request(
            "provider must be google or microsoft",
        ));
    }
    let cfg_key = match provider.as_str() {
        "google" => "google_calendar_client_id",
        _ => "ms_calendar_client_id",
    };
    let cfg = app.cfg();
    let client_id = crate::providers::resolve_setting(
        &cfg,
        app.vault.as_deref(),
        cfg_key,
        &format!("calendar.{provider}.client_id"),
        &provider_env(&provider, "CLIENT_ID"),
    )
    .filter(|s| !s.is_empty())
    .ok_or_else(|| {
        ApiError::validation(vec![crate::domain::FieldError::new(
            "client_id",
            "the provider's OAuth client id is not configured in the vault",
        )])
    })?;
    let redirect = app
        .cfg()
        .get_str("calendar_oauth_redirect", &crate::appconfig::process_env);
    let state = app
        .oauth_flows
        .start(&provider, app.clock.now().timestamp());
    let url = crate::calendar_oauth::authorize_url(&provider, &client_id, &redirect, &state);
    Ok(Json(serde_json::json!({ "url": url })).into_response())
}

/// `GET /api/calendar/oauth/callback` — the provider redirects back with these.
#[derive(Debug, serde::Deserialize)]
pub struct OAuthCallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// OAuth callback for the calendar consent flow (#36). Validates the CSRF
/// state, exchanges the code, and stores tokens in the vault — they are never
/// echoed back or logged.
#[allow(clippy::unused_async)] // axum handler shape
pub async fn calendar_oauth_callback(
    State(app): State<AppState>,
    Query(q): Query<OAuthCallbackQuery>,
) -> ApiResult {
    let code = q.code.unwrap_or_default();
    let state = q.state.unwrap_or_default();
    let Some(provider) = app.oauth_flows.take(&state, app.clock.now().timestamp()) else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "bad_state",
            "unknown or expired OAuth state",
        ));
    };
    if code.is_empty() {
        return Err(ApiError::bad_request("missing authorization code"));
    }
    let get = |key: &str| -> Option<String> { app.vault.as_ref().and_then(|v| v.get(key)) };
    let client_id = get(&format!("calendar.{provider}.client_id")).unwrap_or_default();
    let client_secret = get(&format!("calendar.{provider}.client_secret")).unwrap_or_default();
    let redirect = app
        .cfg()
        .get_str("calendar_oauth_redirect", &crate::appconfig::process_env);
    let vault = app.vault.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_vault",
            "secret vault is not enabled (set TUCANO_SECRET_KEY)",
        )
    })?;
    // The code exchange is a live provider call; run it off the async core.
    let (provider2, code2, cid, cs, red) = (
        provider.clone(),
        code.clone(),
        client_id.clone(),
        client_secret.clone(),
        redirect.clone(),
    );
    let tokens = blocking("oauth exchange", move || {
        crate::calendar_oauth::exchange_code(&provider2, &code2, &cid, &cs, &red).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "oauth_exchange",
                "authorization code exchange failed",
            )
        })
    })
    .await?;
    let (access, refresh) = tokens;
    vault
        .put(&format!("calendar.{provider}.access_token"), &access)
        .map_err(|_| ApiError::internal("vault write failed".into()))?;
    if !refresh.is_empty() {
        vault
            .put(&format!("calendar.{provider}.refresh_token"), &refresh)
            .map_err(|_| ApiError::internal("vault write failed".into()))?;
    }
    // Turn the provider on only after tokens are safely stored.
    vault
        .put("calendar.provider", &provider)
        .map_err(|_| ApiError::internal("vault write failed".into()))?;
    app.audit
        .record("calendar_oauth", &provider, app.clock.now());
    Ok(Json(serde_json::json!({ "connected": provider })).into_response())
}

fn provider_env(provider: &str, suffix: &str) -> String {
    format!("TUCANO_{}_{}", provider.to_ascii_uppercase(), suffix)
}

/// The caller's notifications, newest first (#22).
pub async fn list_notifications(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    let mut list = app.store.list_notifications(actor.0.id)?;
    if list
        .iter()
        .any(|notification| notification.kind == "no_time_today")
    {
        let today = app.clock.now().date_naive();
        let has_time_today = app
            .store
            .list_range(today, today)?
            .iter()
            .any(|entry| entry.user_id == Some(actor.0.id));
        list.retain(|notification| {
            notification.kind != "no_time_today"
                || (notification.created_at.date_naive() == today && !has_time_today)
        });
    }
    list.sort_by_key(|n| std::cmp::Reverse(n.created_at));
    let unread = list.iter().filter(|n| !n.read).count();
    Ok(Json(serde_json::json!({ "notifications": list, "unread": unread })).into_response())
}

/// Mark the caller's notifications read.
pub async fn mark_notifications_read(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    app.store.mark_notifications_read(actor.0.id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --------------------------------------------------------------- schedules --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInput {
    pub customer_id: Uuid,
    pub cadence: crate::domain::Cadence,
    pub mode: crate::domain::RecurMode,
    #[serde(default)]
    pub retainer_amount_minor: u64,
    pub currency: Currency,
    #[serde(default = "default_active_true")]
    pub active: bool,
}

pub async fn list_schedules(State(app): State<AppState>) -> ApiResult {
    let schedules = app.store.list_schedules()?;
    Ok(Json(serde_json::json!({ "schedules": schedules })).into_response())
}

pub async fn create_schedule(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<ScheduleInput>,
) -> ApiResult {
    get_customer(&app.store, input.customer_id)?;
    if input.mode == crate::domain::RecurMode::Retainer && input.retainer_amount_minor == 0 {
        return Err(ApiError::validation(vec![FieldError::new(
            "retainer_amount_minor",
            "must be > 0 for a retainer schedule",
        )]));
    }
    let schedule = crate::domain::RecurringSchedule {
        id: Uuid::new_v4(),
        customer_id: input.customer_id,
        cadence: input.cadence,
        mode: input.mode,
        retainer_amount_minor: input.retainer_amount_minor,
        currency: input.currency,
        active: input.active,
        last_period_end: None,
        created_at: app.clock.now(),
    };
    app.store.put_schedule(&schedule)?;
    Ok((StatusCode::CREATED, Json(schedule)).into_response())
}

/// Body for `PATCH /schedules/{id}` (#136): the management UI's pause/resume.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulePatch {
    pub active: bool,
}

/// Pause or resume a recurring schedule. Identity, cadence and the
/// `last_period_end` cursor are preserved: resuming continues where billing
/// stopped, it does not re-bill a period.
pub async fn update_schedule(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(patch): ValidJson<SchedulePatch>,
) -> ApiResult {
    let Some(schedule) = app.store.get_schedule(id)? else {
        return Err(ApiError::not_found("schedule"));
    };
    let updated = crate::domain::RecurringSchedule {
        active: patch.active,
        ..schedule
    };
    app.store.put_schedule(&updated)?;
    app.audit.record(
        if patch.active {
            "schedule_resumed"
        } else {
            "schedule_paused"
        },
        &format!("{}:{}", id, updated.customer_id),
        app.clock.now(),
    );
    Ok(Json(updated).into_response())
}

pub async fn delete_schedule(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_schedule(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
