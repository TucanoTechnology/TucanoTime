// Structured error responses. Every failure leaves as
//
//   { "error": { "code": "...", "message": "...", "fields": [...] } }
//
// with no paths, stack traces or raw filesystem errors in the body. The
// machine-readable contract in openapi.json fixes this shape.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::domain::FieldError;
use crate::store::StoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub fields: Vec<FieldError>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            fields: Vec::new(),
        }
    }

    pub fn validation(fields: Vec<FieldError>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "validation_failed",
            message: "payload failed validation".into(),
            fields,
        }
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} does not exist"),
        )
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// A corrupt or unreadable stored document: server-side 500, logged with
    /// detail, generic message out.
    pub fn internal(context: String) -> Self {
        tracing::error!(%context, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the request could not be completed",
        )
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => ApiError::not_found("resource"),
            StoreError::AlreadyExists(m) => ApiError::conflict(m),
            StoreError::Conflict(m) => ApiError::conflict(m),
            StoreError::RangeTooLarge => ApiError::bad_request(e.to_string()),
            StoreError::LockTimeout => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "write_lock_busy",
                "another write is in progress; retry shortly",
            ),
            StoreError::TooManyItems => {
                // A collection exceeded MAX_DOCS — a server-side limit, not a
                // client error; report generically without leaking the cap.
                ApiError::internal("collection exceeds the configured size limit".into())
            }
            StoreError::Io(detail) => ApiError::internal(detail),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "error": { "code": self.code, "message": self.message }
        });
        if !self.fields.is_empty() {
            body["error"]["fields"] = serde_json::to_value(&self.fields).unwrap_or_default();
        }
        let mut resp = (self.status, axum::Json(body)).into_response();
        // A busy write lock is transient; tell the client when to retry (#62).
        if self.code == "write_lock_busy"
            && let Ok(v) = "1".parse()
        {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, v);
        }
        resp
    }
}

// Rejected JSON payloads arrive through `ValidJson`; the shared 422 shape
// above is produced there, so no axum-rejection glue lives in this module.
