//! Authentication & authorization (§6).
//!
//! A Bearer token is hashed and resolved to a `Principal` exactly once per
//! request by the extractor below, which also applies the per-principal rate
//! limit. Handlers then assert the required role via `AppState::authorize`.

use crate::api::AppState;
use crate::errors::ApiError;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

/// An authenticated caller. Attached to the request by the extractor.
#[derive(Debug, Clone)]
pub struct Principal {
    pub id: String,
    /// Human-readable principal name (for diagnostics/audit).
    #[allow(dead_code)]
    pub name: String,
    /// Global admin (bootstrap). May call `/v1/admin/*`, but is NOT implicitly
    /// granted data access (§6).
    pub is_admin: bool,
}

impl Principal {
    /// Assert this principal is a global admin (for `/v1/admin/*`).
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.is_admin {
            Ok(())
        } else {
            Err(ApiError::forbidden("global admin required"))
        }
    }
}

/// Extract the bearer token, resolve the principal, and apply the rate limit.
impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = bearer_token(parts)?;
        let principal = state
            .system
            .lookup_by_token(&token)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::unauthorized("invalid or disabled token"))?;

        // Per-principal rate limit (§11). Applied here so every authenticated
        // route is covered uniformly.
        if let Err(retry_after) = state.rate_guard.check(&principal.id) {
            return Err(ApiError::rate_limited("rate limit exceeded").with_retry_after(retry_after));
        }

        Ok(principal)
    }
}

/// A principal that must be a global admin. Convenience extractor for admin
/// routes. The inner `Principal` is available to handlers that need the id.
pub struct AdminPrincipal(#[allow(dead_code)] pub Principal);

impl FromRequestParts<AppState> for AdminPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let principal = Principal::from_request_parts(parts, state).await?;
        principal.require_admin()?;
        Ok(AdminPrincipal(principal))
    }
}

fn bearer_token(parts: &Parts) -> Result<String, ApiError> {
    let header = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?
        .to_str()
        .map_err(|_| ApiError::unauthorized("malformed Authorization header"))?;
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or_else(|| ApiError::unauthorized("expected 'Bearer <token>'"))?;
    if token.is_empty() {
        return Err(ApiError::unauthorized("empty bearer token"));
    }
    Ok(token.to_string())
}
