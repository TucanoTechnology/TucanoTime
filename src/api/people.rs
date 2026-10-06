//! People and organisation reference data: users (#19) plus the customer →
//! project → task hierarchy that every entry and expense points at.

use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserInput {
    name: String,
    email: String,
    password: String,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default = "default_active_true")]
    active: bool,
    #[serde(default)]
    default_rate_minor: u64,
    #[serde(default)]
    cost_rate_minor: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserUpdateInput {
    name: Option<String>,
    email: Option<String>,
    role: Option<Role>,
    active: Option<bool>,
    default_rate_minor: Option<u64>,
    cost_rate_minor: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPasswordInput {
    password: String,
}

pub async fn list_users(State(app): State<AppState>) -> ApiResult {
    let users: Vec<PublicUser> = app
        .store
        .list_users()?
        .iter()
        .map(PublicUser::from)
        .collect();
    Ok(Json(serde_json::json!({ "users": users })).into_response())
}

pub async fn create_user(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<UserInput>,
) -> ApiResult {
    let mut errors = Vec::new();
    crate::domain::validate_rate_minor(input.default_rate_minor, "default_rate_minor", &mut errors);
    crate::domain::validate_rate_minor(input.cost_rate_minor, "cost_rate_minor", &mut errors);
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let email = normalise_email(&input.email)
        .ok_or_else(|| ApiError::validation(vec![FieldError::new("email", "invalid email")]))?;
    if app.store.get_user_by_email(&email)?.is_some() {
        return Err(ApiError::conflict("a user with that email exists"));
    }
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
    let user = new_user(
        name,
        &email,
        &input.password,
        NewUser {
            role: input.role.unwrap_or(Role::Member),
            active: input.active,
            default_rate_minor: input.default_rate_minor,
            cost_rate_minor: input.cost_rate_minor,
        },
        &app,
    )?;
    app.audit.record("user_created", &email, app.clock.now());
    Ok((StatusCode::CREATED, Json(PublicUser::from(&user))).into_response())
}

pub async fn update_user(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<UserUpdateInput>,
) -> ApiResult {
    let mut errors = Vec::new();
    if let Some(rate) = input.default_rate_minor {
        crate::domain::validate_rate_minor(rate, "default_rate_minor", &mut errors);
    }
    if let Some(rate) = input.cost_rate_minor {
        crate::domain::validate_rate_minor(rate, "cost_rate_minor", &mut errors);
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let mut user = app
        .store
        .get_user(id)?
        .ok_or_else(|| ApiError::not_found("user"))?;
    let security_changed = input.role.is_some_and(|role| role != user.role)
        || input.active.is_some_and(|active| active != user.active);
    let expected = user.clone();
    if actor.0.id == id
        && ((input.role.is_some_and(|role| role != Role::Admin)) || input.active == Some(false))
    {
        return Err(ApiError::conflict(
            "you cannot deactivate or demote your own account",
        ));
    }
    if let Some(name) = input.name.as_deref() {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(ApiError::validation(vec![FieldError::new(
                "name",
                "required, at most 120 characters",
            )]));
        }
        user.name = name.to_owned();
    }
    if let Some(email) = input.email.as_deref() {
        let email = normalise_email(email)
            .ok_or_else(|| ApiError::validation(vec![FieldError::new("email", "invalid email")]))?;
        if app
            .store
            .get_user_by_email(&email)?
            .is_some_and(|other| other.id != id)
        {
            return Err(ApiError::conflict("a user with that email exists"));
        }
        user.email = email;
    }
    if let Some(role) = input.role {
        user.role = role;
    }
    if let Some(active) = input.active {
        user.active = active;
    }
    if let Some(rate) = input.default_rate_minor {
        user.default_rate_minor = rate;
    }
    if let Some(rate) = input.cost_rate_minor {
        user.cost_rate_minor = rate;
    }
    let active_admins = app
        .store
        .list_users()?
        .into_iter()
        .filter(|u| {
            (u.id != id && u.active && u.role == Role::Admin)
                || (u.id == id && user.active && user.role == Role::Admin)
        })
        .count();
    if active_admins == 0 {
        return Err(ApiError::conflict(
            "at least one active administrator is required",
        ));
    }
    if security_changed {
        user.session_version += 1;
    }
    app.store.put_user_if_unchanged(&user, &expected)?;
    app.audit
        .record("user_updated", &user.email, app.clock.now());
    Ok(Json(PublicUser::from(&user)).into_response())
}

pub async fn change_user_password(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<UserPasswordInput>,
) -> ApiResult {
    let mut user = app
        .store
        .get_user(id)?
        .ok_or_else(|| ApiError::not_found("user"))?;
    let expected = user.clone();
    if !valid_password(&input.password) {
        return Err(ApiError::validation(vec![FieldError::new(
            "password",
            "must be at least 8 characters",
        )]));
    }
    user.password_hash = crate::auth::hash_password(&input.password)
        .map_err(|_| ApiError::internal("password hashing failed".into()))?;
    user.session_version += 1;
    app.store.put_user_if_unchanged(&user, &expected)?;
    app.audit
        .record("user_password_changed", &user.email, app.clock.now());
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn delete_user(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult {
    if actor.0.id == id {
        return Err(ApiError::conflict("you cannot delete your own account"));
    }
    let target = app
        .store
        .get_user(id)?
        .ok_or_else(|| ApiError::not_found("user"))?;
    if target.role == Role::Admin {
        let active_admins = app
            .store
            .list_users()?
            .into_iter()
            .filter(|u| u.active && u.role == Role::Admin && u.id != id)
            .count();
        if active_admins == 0 {
            return Err(ApiError::conflict(
                "at least one active administrator is required",
            ));
        }
    }
    if app
        .store
        .list_all_entries()?
        .iter()
        .any(|entry| entry.user_id == Some(id))
        || app
            .store
            .list_expenses()?
            .iter()
            .any(|expense| expense.user_id == Some(id))
        || app
            .store
            .list_submissions()?
            .iter()
            .any(|submission| submission.user_id == id)
        || app
            .store
            .list_claims()?
            .iter()
            .any(|claim| claim.user_id == id)
        || app.store.get_timer(id)?.is_some()
    {
        return Err(ApiError::conflict(
            "user has recorded history and cannot be deleted; deactivate instead",
        ));
    }
    app.store.delete_user(id)?;
    app.audit
        .record("user_deleted", &target.email, app.clock.now());
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ------------------------------------------------------------- customers --

pub async fn list_customers(State(app): State<AppState>) -> ApiResult {
    let customers = app.store.list_customers()?;
    Ok(Json(serde_json::json!({ "customers": customers })).into_response())
}

pub async fn create_customer(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<CustomerInput>,
) -> ApiResult {
    let draft = validate_customer_input(&input).map_err(ApiError::validation)?;
    let customer = Customer {
        id: Uuid::new_v4(),
        name: draft.name,
        currency: Currency(draft.currency),
        default_rate_minor: draft.default_rate_minor,
        active: draft.active,
        email: draft.email,
        payment_terms: draft.payment_terms,
        invoice_notes: draft.invoice_notes,
        invoice_subject: draft.invoice_subject,
        address: draft.address,
        contacts: draft.contacts,
        tax_hundredths: draft.tax_hundredths,
        discount_hundredths: draft.discount_hundredths,
    };
    app.store.put_customer(&customer)?;
    Ok((StatusCode::CREATED, Json(&customer)).into_response())
}

pub async fn get_customer_handler(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    Ok(Json(get_customer(&app.store, id)?).into_response())
}

pub async fn update_customer(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<CustomerInput>,
) -> ApiResult {
    get_customer(&app.store, id)?;
    let draft = validate_customer_input(&input).map_err(ApiError::validation)?;
    let updated = Customer {
        id,
        name: draft.name,
        currency: Currency(draft.currency),
        default_rate_minor: draft.default_rate_minor,
        active: draft.active,
        email: draft.email,
        payment_terms: draft.payment_terms,
        invoice_notes: draft.invoice_notes,
        invoice_subject: draft.invoice_subject,
        address: draft.address,
        contacts: draft.contacts,
        tax_hundredths: draft.tax_hundredths,
        discount_hundredths: draft.discount_hundredths,
    };
    app.store.put_customer(&updated)?;
    Ok(Json(&updated).into_response())
}

pub async fn delete_customer(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_customer(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// -------------------------------------------------------------- projects --

pub async fn list_projects(State(app): State<AppState>, Path(cid): Path<Uuid>) -> ApiResult {
    get_customer(&app.store, cid)?;
    let projects = app.store.list_projects(cid)?;
    Ok(Json(serde_json::json!({ "projects": projects })).into_response())
}

pub async fn create_project(
    State(app): State<AppState>,
    Path(cid): Path<Uuid>,
    ValidJson(input): ValidJson<ProjectInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    let draft = validate_project_input(&input).map_err(ApiError::validation)?;
    if app.store.get_project(cid, &draft.code)?.is_some() {
        return Err(ApiError::conflict(format!(
            "project {} already exists",
            draft.code
        )));
    }
    let project = build_project(cid, &draft);
    app.store.put_project(&project)?;
    Ok((StatusCode::CREATED, Json(&project)).into_response())
}

fn build_project(cid: Uuid, draft: &crate::domain::ProjectDraft) -> Project {
    Project {
        customer_id: cid,
        code: ProjectCode(draft.code.clone()),
        name: draft.name.clone(),
        currency: Currency(draft.currency.clone()),
        rate_minor: draft.rate_minor,
        active: draft.active,
        budget_hours: draft.budget_hours,
        budget_amount_minor: draft.budget_amount_minor,
    }
}

pub async fn get_project_handler(
    State(app): State<AppState>,
    Path((cid, code)): Path<(Uuid, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    let project = app
        .store
        .get_project(cid, &code.0)?
        .ok_or_else(|| ApiError::not_found("project"))?;
    Ok(Json(project).into_response())
}

pub async fn update_project(
    State(app): State<AppState>,
    Path((cid, code)): Path<(Uuid, ProjectCode)>,
    ValidJson(input): ValidJson<ProjectInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_project(cid, &code.0)?.is_none() {
        return Err(ApiError::not_found("project"));
    }
    if input.code != code {
        return Err(ApiError::validation(vec![FieldError::new(
            "code",
            "path is authoritative; body code must match it",
        )]));
    }
    let draft = validate_project_input(&input).map_err(ApiError::validation)?;
    let project = build_project(cid, &draft);
    app.store.put_project(&project)?;
    Ok(Json(&project).into_response())
}

pub async fn delete_project(
    State(app): State<AppState>,
    Path((cid, code)): Path<(Uuid, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    app.store.delete_project(cid, &code.0)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ----------------------------------------------------------------- tasks --

pub async fn list_tasks(
    State(app): State<AppState>,
    Path((cid, pcode)): Path<(Uuid, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_project(cid, &pcode.0)?.is_none() {
        return Err(ApiError::not_found("project"));
    }
    let tasks = app.store.list_tasks(cid, &pcode.0)?;
    Ok(Json(serde_json::json!({ "tasks": tasks })).into_response())
}

pub async fn create_task(
    State(app): State<AppState>,
    Path((cid, pcode)): Path<(Uuid, ProjectCode)>,
    ValidJson(input): ValidJson<TaskInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_project(cid, &pcode.0)?.is_none() {
        return Err(ApiError::not_found("project"));
    }
    let draft = validate_task_input(&input).map_err(ApiError::validation)?;
    if app.store.get_task(cid, &pcode.0, &draft.code)?.is_some() {
        return Err(ApiError::conflict(format!(
            "task {} already exists",
            draft.code
        )));
    }
    let task = build_task(cid, &pcode, &draft);
    app.store.put_task(&task)?;
    Ok((StatusCode::CREATED, Json(&task)).into_response())
}

fn build_task(cid: Uuid, pcode: &ProjectCode, draft: &crate::domain::TaskDraft) -> Task {
    Task {
        customer_id: cid,
        project_code: pcode.clone(),
        code: ProjectCode(draft.code.clone()),
        name: draft.name.clone(),
        active: draft.active,
    }
}

pub async fn get_task_handler(
    State(app): State<AppState>,
    Path((cid, pcode, code)): Path<(Uuid, ProjectCode, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    let task = app
        .store
        .get_task(cid, &pcode.0, &code.0)?
        .ok_or_else(|| ApiError::not_found("task"))?;
    Ok(Json(task).into_response())
}

pub async fn update_task(
    State(app): State<AppState>,
    Path((cid, pcode, code)): Path<(Uuid, ProjectCode, ProjectCode)>,
    ValidJson(input): ValidJson<TaskInput>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    if app.store.get_task(cid, &pcode.0, &code.0)?.is_none() {
        return Err(ApiError::not_found("task"));
    }
    if input.code != code {
        return Err(ApiError::validation(vec![FieldError::new(
            "code",
            "path is authoritative; body code must match it",
        )]));
    }
    let draft = validate_task_input(&input).map_err(ApiError::validation)?;
    let task = build_task(cid, &pcode, &draft);
    app.store.put_task(&task)?;
    Ok(Json(&task).into_response())
}

pub async fn delete_task(
    State(app): State<AppState>,
    Path((cid, pcode, code)): Path<(Uuid, ProjectCode, ProjectCode)>,
) -> ApiResult {
    get_customer(&app.store, cid)?;
    app.store.delete_task(cid, &pcode.0, &code.0)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
