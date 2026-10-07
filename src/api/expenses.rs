//! Expenses: categories, expense records (#24) and the submission/claim
//! approval workflows built on them.

use super::*;

pub async fn list_categories(State(app): State<AppState>) -> ApiResult {
    app.store.ensure_default_categories()?;
    let categories = app.store.list_categories()?;
    Ok(Json(serde_json::json!({ "categories": categories })).into_response())
}

pub async fn create_category(
    State(app): State<AppState>,
    ValidJson(input): ValidJson<CategoryInput>,
) -> ApiResult {
    let draft = validate_category_input(&input).map_err(ApiError::validation)?;
    let category = Category {
        id: Uuid::new_v4(),
        name: draft.name,
        default_billable: draft.default_billable,
        active: draft.active,
    };
    app.store.put_category(&category)?;
    Ok((StatusCode::CREATED, Json(category)).into_response())
}

pub async fn update_category(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<CategoryInput>,
) -> ApiResult {
    let draft = validate_category_input(&input).map_err(ApiError::validation)?;
    let mut category = app
        .store
        .get_category(id)?
        .ok_or_else(|| ApiError::not_found("category"))?;
    category.name = draft.name;
    category.default_billable = draft.default_billable;
    category.active = draft.active;
    app.store.update_category(&category)?;
    Ok(Json(category).into_response())
}

pub async fn delete_category(State(app): State<AppState>, Path(id): Path<Uuid>) -> ApiResult {
    app.store.delete_category(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /expenses` filters: all optional, unknown params tolerated (#100).
#[derive(Debug, serde::Deserialize)]
pub struct ExpenseQuery {
    #[serde(default)]
    customer_id: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

pub async fn list_expenses(
    State(app): State<AppState>,
    actor: AuthUser,
    Query(q): Query<ExpenseQuery>,
) -> ApiResult {
    let mut expenses = app.store.list_expenses()?;
    if let Some(cid) = &q.customer_id {
        let cid = Uuid::parse_str(cid)
            .map_err(|_| ApiError::bad_request("'customer_id' must be a UUID"))?;
        expenses.retain(|e| e.customer_id == cid);
    }
    if let (Some(from), Some(to)) = (&q.from, &q.to) {
        let (from, to) = (parse_date(from)?, parse_date(to)?);
        expenses.retain(|e| e.date >= from && e.date <= to);
    }
    expenses.retain(|e| visible_to(&actor.0, e.user_id));
    Ok(Json(serde_json::json!({ "expenses": expenses })).into_response())
}

pub async fn create_expense(
    State(app): State<AppState>,
    actor: AuthUser,
    ValidJson(input): ValidJson<ExpenseInput>,
) -> ApiResult {
    let draft = validate_expense_input(&input).map_err(ApiError::validation)?;
    let mut errors = Vec::new();
    if app.store.get_customer(draft.customer_id)?.is_none() {
        errors.push(FieldError::new("customer_id", "customer does not exist"));
    }
    if let Some(code) = &draft.project_code
        && app.store.get_project(draft.customer_id, code)?.is_none()
    {
        errors.push(FieldError::new(
            "project_code",
            "project does not exist for this customer",
        ));
    }
    if let Some(cat) = draft.category_id
        && app.store.get_category(cat)?.is_none()
    {
        errors.push(FieldError::new("category_id", "category does not exist"));
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let now = app.clock.now();
    let expense = Expense {
        id: Uuid::new_v4(),
        date: draft.date,
        customer_id: draft.customer_id,
        user_id: Some(actor.0.id),
        project_code: draft.project_code.map(ProjectCode),
        category_id: draft.category_id,
        amount_minor: draft.amount_minor,
        currency: Currency(draft.currency),
        billable: draft.billable,
        note: draft.note,
        receipt_name: draft.receipt_name,
        receipt_b64: draft.receipt_b64,
        created_at: now,
        updated_at: now,
    };
    app.store.put_expense(&expense)?;
    Ok((StatusCode::CREATED, Json(expense)).into_response())
}

pub async fn get_expense_handler(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let expense = app
        .store
        .get_expense(id)?
        .filter(|e| visible_to(&actor.0, e.user_id))
        .ok_or_else(|| ApiError::not_found("expense"))?;
    Ok(Json(expense).into_response())
}

pub async fn delete_expense(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult {
    // Ownership check before delete (404 if not the member's own record).
    let owned = app
        .store
        .get_expense(id)?
        .is_some_and(|e| visible_to(&actor.0, e.user_id));
    if !owned {
        return Err(ApiError::not_found("expense"));
    }
    // An expense on an issued invoice is locked (#25).
    let invoiced = app
        .store
        .list_invoices()?
        .iter()
        .filter(|i| i.status == InvoiceStatus::Issued)
        .any(|i| i.lines.iter().any(|l| l.expense_id == Some(id)));
    if invoiced {
        return Err(ApiError::conflict(
            "expense is on an issued invoice; delete the invoice draft or void it first",
        ));
    }
    // An expense in a submitted/approved claim is locked (#24).
    let claimed = app.store.list_claims()?.iter().any(|c| {
        (c.state == ClaimState::Submitted || c.state == ClaimState::Approved)
            && c.expense_ids.contains(&id)
    });
    if claimed {
        return Err(ApiError::conflict(
            "expense is on a submitted/approved claim; reject it first",
        ));
    }
    app.store.delete_expense(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ----------------------------------------------------------- submissions --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitInput {
    pub week_start: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInput {
    pub decision: String,
    #[serde(default)]
    pub comment: String,
}

pub async fn list_submissions(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    let submissions: Vec<Submission> = app
        .store
        .list_submissions()?
        .into_iter()
        .filter(|s| visible_to(&actor.0, Some(s.user_id)))
        .collect();
    Ok(Json(serde_json::json!({ "submissions": submissions })).into_response())
}

/// Submit the acting user's timesheet for a week; locks its entries.
pub async fn create_submission(
    State(app): State<AppState>,
    actor: AuthUser,
    ValidJson(input): ValidJson<SubmitInput>,
) -> ApiResult {
    let week_start = parse_date(&input.week_start)?;
    let week_end = week_start
        .checked_add_days(chrono::Days::new(6))
        .ok_or_else(|| ApiError::bad_request("invalid week"))?;
    let all = app.store.list_range(week_start, week_end)?;
    // Entries already locked (submitted/approved) can't be resubmitted.
    // #185: an entry whose lock state cannot be verified is not submitted.
    let mut entry_ids: Vec<Uuid> = Vec::new();
    for e in all.iter().filter(|e| e.user_id == Some(actor.0.id)) {
        match app.locks.entry_lock(e.id) {
            Ok(None) => entry_ids.push(e.id),
            Ok(Some(_)) => {}
            Err(err) => {
                tracing::warn!(error = ?err.0, entry = %e.id, "lock state unavailable; refusing submit");
                return Err(ApiError::lock_unavailable());
            }
        }
    }
    if entry_ids.is_empty() {
        return Err(ApiError::conflict(
            "no unlocked entries to submit this week",
        ));
    }
    let now = app.clock.now();
    let submission = Submission {
        id: Uuid::new_v4(),
        user_id: actor.0.id,
        week_start,
        week_end,
        state: SubmissionState::Submitted,
        entry_ids,
        comment: String::new(),
        created_at: now,
        submitted_at: Some(now),
        decided_at: None,
    };
    app.store.put_submission(&submission)?;
    Ok((StatusCode::CREATED, Json(submission)).into_response())
}

/// Approve or reject a submitted timesheet (admin only, #51). Rejecting releases its locks.
pub async fn decide_submission(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<DecisionInput>,
) -> ApiResult {
    ensure_admin(&actor.0)?;
    let mut submission = app
        .store
        .get_submission(id)?
        .ok_or_else(|| ApiError::not_found("submission"))?;
    if submission.state != SubmissionState::Submitted {
        return Err(ApiError::conflict(
            "only a submitted timesheet can be decided",
        ));
    }
    submission.state = match input.decision.as_str() {
        "approve" => SubmissionState::Approved,
        "reject" => SubmissionState::Rejected,
        other => {
            return Err(ApiError::validation(vec![FieldError::new(
                "decision",
                format!("'{other}' is not approve or reject"),
            )]));
        }
    };
    submission.comment = input.comment;
    submission.decided_at = Some(app.clock.now());
    app.store.put_submission(&submission)?;
    Ok(Json(submission).into_response())
}

// ------------------------------------------------------------------ claims --

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimDecisionInput {
    pub decision: String,
    #[serde(default)]
    pub comment: String,
}

pub async fn list_claims(State(app): State<AppState>, actor: AuthUser) -> ApiResult {
    let claims: Vec<ExpenseClaim> = app
        .store
        .list_claims()?
        .into_iter()
        .filter(|c| visible_to(&actor.0, Some(c.user_id)))
        .collect();
    Ok(Json(serde_json::json!({ "claims": claims })).into_response())
}

/// Create a draft reimbursement claim over a set of the caller's expenses.
pub async fn create_claim(
    State(app): State<AppState>,
    actor: AuthUser,
    ValidJson(input): ValidJson<ClaimInput>,
) -> ApiResult {
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        return Err(ApiError::validation(vec![FieldError::new(
            "title",
            "required, at most 120 characters",
        )]));
    }
    if input.expense_ids.is_empty() {
        return Err(ApiError::validation(vec![FieldError::new(
            "expense_ids",
            "at least one expense is required",
        )]));
    }
    let mut errors = Vec::new();
    let mut total: u64 = 0;
    let mut currency: Option<Currency> = None;
    // #187: a repeated expense id used to sum its amount twice into the
    // persisted claim total shown to approvers (while referencing one
    // expense). Reject duplicates before any work.
    let mut seen = std::collections::HashSet::new();
    for id in &input.expense_ids {
        if !seen.insert(id) {
            return Err(ApiError::validation(vec![FieldError::new(
                "expense_ids",
                format!("expense {id} is listed more than once"),
            )]));
        }
    }
    for id in &input.expense_ids {
        let Some(x) = app.store.get_expense(*id)? else {
            errors.push(FieldError::new(
                "expense_ids",
                format!("unknown expense {id}"),
            ));
            continue;
        };
        if !visible_to(&actor.0, x.user_id) {
            errors.push(FieldError::new(
                "expense_ids",
                "expense belongs to another user",
            ));
            continue;
        }
        total += x.amount_minor;
        match &currency {
            None => currency = Some(x.currency.clone()),
            Some(c) if *c == x.currency => {}
            Some(c) => {
                errors.push(FieldError::new(
                    "expense_ids",
                    format!("mixed currencies ({} and {})", c.0, x.currency.0),
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    let claim = ExpenseClaim {
        id: Uuid::new_v4(),
        user_id: actor.0.id,
        title: title.to_owned(),
        expense_ids: input.expense_ids.clone(),
        total_minor: total,
        currency: currency.expect("non-empty"),
        state: ClaimState::Draft,
        comment: String::new(),
        created_at: app.clock.now(),
        submitted_at: None,
        decided_at: None,
    };
    app.store.put_claim(&claim)?;
    Ok((StatusCode::CREATED, Json(claim)).into_response())
}

pub async fn submit_claim(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut claim = app
        .store
        .get_claim(id)?
        .filter(|c| visible_to(&actor.0, Some(c.user_id)))
        .ok_or_else(|| ApiError::not_found("claim"))?;
    if claim.state != ClaimState::Draft {
        return Err(ApiError::conflict("only a draft claim can be submitted"));
    }
    claim.state = ClaimState::Submitted;
    claim.submitted_at = Some(app.clock.now());
    app.store.put_claim(&claim)?;
    Ok(Json(claim).into_response())
}

/// Approve or reject a submitted claim (admin only, #24).
pub async fn decide_claim(
    State(app): State<AppState>,
    actor: AuthUser,
    Path(id): Path<Uuid>,
    ValidJson(input): ValidJson<ClaimDecisionInput>,
) -> ApiResult {
    ensure_admin(&actor.0)?;
    let mut claim = app
        .store
        .get_claim(id)?
        .ok_or_else(|| ApiError::not_found("claim"))?;
    if claim.state != ClaimState::Submitted {
        return Err(ApiError::conflict("only a submitted claim can be decided"));
    }
    claim.state = match input.decision.as_str() {
        "approve" => ClaimState::Approved,
        "reject" => ClaimState::Rejected,
        other => {
            return Err(ApiError::validation(vec![FieldError::new(
                "decision",
                format!("'{other}' is not approve or reject"),
            )]));
        }
    };
    claim.comment = input.comment;
    claim.decided_at = Some(app.clock.now());
    app.store.put_claim(&claim)?;
    Ok(Json(claim).into_response())
}
