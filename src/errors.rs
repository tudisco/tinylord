//! Error envelope and HTTP mapping (§13).
//!
//! Every error returned to a client uses the shape:
//! ```json
//! { "error": { "code": "not_found", "message": "human readable", "detail": null } }
//! ```
//! Messages must never leak SQL text or internal filesystem paths.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Stable machine-readable error codes and their HTTP status (§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BadRequest,
    Validation,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    PayloadTooLarge,
    RateLimited,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::Validation => "validation",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::PayloadTooLarge => "payload_too_large",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Internal => "internal",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            ErrorCode::BadRequest | ErrorCode::Validation => StatusCode::BAD_REQUEST,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A client-facing error. Carries an optional `detail` value and an optional
/// `Retry-After` header value (used for rate limiting, §11).
#[derive(Debug)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub detail: Option<serde_json::Value>,
    pub retry_after_secs: Option<u64>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            retry_after_secs: None,
        }
    }

    #[allow(dead_code)] // part of the error API; not every error carries detail yet
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    // Convenience constructors for the common cases.
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, msg)
    }
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Validation, msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, msg)
    }
    pub fn payload_too_large(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::PayloadTooLarge, msg)
    }
    pub fn rate_limited(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::RateLimited, msg)
    }

    /// Internal error. The public `message` is generic; the real cause is logged
    /// server-side and never returned to the client (§13: never leak SQL/paths).
    pub fn internal(context: impl std::fmt::Display) -> Self {
        tracing::error!("internal error: {context}");
        Self::new(ErrorCode::Internal, "internal error")
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorInner,
}

#[derive(Serialize)]
struct ErrorInner {
    code: &'static str,
    message: String,
    detail: Option<serde_json::Value>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.code.status();
        let body = Json(ErrorBody {
            error: ErrorInner {
                code: self.code.as_str(),
                message: self.message,
                detail: self.detail,
            },
        });
        let mut resp = (status, body).into_response();
        if let Some(secs) = self.retry_after_secs {
            if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

/// Map a `rusqlite::Error` to an `ApiError`, translating unique-constraint
/// violations to `409 conflict` and everything else to a logged `500 internal`
/// (never leaking the SQL string).
pub fn map_sqlite_err(e: rusqlite::Error) -> ApiError {
    use rusqlite::ffi::ErrorCode as SqliteCode;
    if let rusqlite::Error::SqliteFailure(err, _) = &e {
        if err.code == SqliteCode::ConstraintViolation {
            return ApiError::conflict("unique constraint violation");
        }
    }
    ApiError::internal(e)
}

pub type ApiResult<T> = Result<T, ApiError>;
