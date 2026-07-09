//! Browser login endpoints. Passwords and all session credentials are hashed
//! before persistence; only short-lived access tokens leave in JSON.

use crate::api::AppState;
use crate::errors::{ApiError, ApiResult};
use argon2::{password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString}, Argon2};
use axum::{extract::{ConnectInfo, State}, http::{header, HeaderMap, HeaderValue, StatusCode}, response::IntoResponse, Json};
use rand::rngs::OsRng;
use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Deserialize)]
pub struct Credentials { username: String, password: String }

pub fn valid_username(username: &str) -> bool {
    (3..=64).contains(&username.len()) && username.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn hash_password(password: &str) -> Result<String, ApiError> {
    if password.len() < 12 || password.len() > 1024 { return Err(ApiError::validation("password must be 12 to 1024 characters")); }
    Argon2::default().hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng)).map(|p| p.to_string()).map_err(ApiError::internal)
}

fn refresh_cookie(value: &str, max_age: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("tinylord_refresh={value}; Path=/v1/auth; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")).expect("cookie is valid")
}

fn csrf_cookie(value: &str, max_age: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    // The application is served from `/`, so its module must be able to read
    // this non-HttpOnly double-submit token after a page reload.
    HeaderValue::from_str(&format!("tinylord_csrf={value}; Path=/; SameSite=Strict; Max-Age={max_age}{secure}")).expect("cookie is valid")
}

fn clear_refresh_cookie(secure: bool) -> HeaderValue { refresh_cookie("", 0, secure) }
fn clear_csrf_cookie(secure: bool) -> HeaderValue { csrf_cookie("", 0, secure) }

fn response(access: &str, csrf: &str, refresh: &str, state: &AppState) -> impl IntoResponse {
    let mut r = Json(serde_json::json!({ "access_token": access, "token_type": "Bearer", "expires_in": state.config.auth.access_token_ttl_secs, "csrf_token": csrf })).into_response();
    r.headers_mut().append(header::SET_COOKIE, refresh_cookie(refresh, state.config.auth.refresh_token_ttl_secs, state.config.auth.secure_cookies));
    r.headers_mut().append(header::SET_COOKIE, csrf_cookie(csrf, state.config.auth.refresh_token_ttl_secs, state.config.auth.secure_cookies));
    r
}

pub async fn register(State(state): State<AppState>, Json(body): Json<Credentials>) -> ApiResult<impl IntoResponse> {
    if !state.system.registration_enabled(state.config.auth.public_registration).map_err(ApiError::internal)? {
        return Err(ApiError::forbidden("public registration is disabled"));
    }
    create_and_login(&state, body).await
}

pub async fn login(State(state): State<AppState>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Json(body): Json<Credentials>) -> ApiResult<impl IntoResponse> {
    let key = format!("{}:{}", addr.ip(), body.username);
    if !state.login_guard.check(&key) { return Err(ApiError::rate_limited("too many login attempts").with_retry_after(60)); }
    let user = state.system.browser_user(&body.username).map_err(ApiError::internal)?;
    let verified = user.as_ref().is_some_and(|(_, hash)| PasswordHash::new(hash).ok().is_some_and(|h| Argon2::default().verify_password(body.password.as_bytes(), &h).is_ok()));
    if !verified { state.login_guard.fail(&key); return Err(ApiError::unauthorized("invalid username or password")); }
    state.login_guard.success(&key);
    let (user, _) = user.expect("verified user exists");
    let (access, refresh, csrf) = state.system.issue_browser_tokens(&user.id, state.config.auth.access_token_ttl_secs, state.config.auth.refresh_token_ttl_secs).map_err(ApiError::internal)?;
    Ok(response(access.as_str(), csrf.as_str(), refresh.as_str(), &state))
}

async fn create_and_login(state: &AppState, body: Credentials) -> ApiResult<impl IntoResponse> {
    if !valid_username(&body.username) { return Err(ApiError::validation("username must be 3 to 64 letters, numbers, '_' or '-'")); }
    let password_hash = hash_password(&body.password)?;
    let user = state.system.create_browser_user(&body.username, &password_hash).map_err(|e| {
        if e.to_string().contains("UNIQUE constraint failed") { ApiError::conflict("username already exists") } else { ApiError::internal(e) }
    })?;
    let (access, refresh, csrf) = state.system.issue_browser_tokens(&user.id, state.config.auth.access_token_ttl_secs, state.config.auth.refresh_token_ttl_secs).map_err(ApiError::internal)?;
    Ok(response(access.as_str(), csrf.as_str(), refresh.as_str(), state))
}

fn refresh_cookie_value(headers: &HeaderMap) -> Option<String> {
    headers.get(header::COOKIE)?.to_str().ok()?.split(';').find_map(|part| part.trim().strip_prefix("tinylord_refresh=").map(str::to_string))
}

pub async fn refresh(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    let refresh = refresh_cookie_value(&headers).ok_or_else(|| ApiError::unauthorized("missing refresh session"))?;
    let csrf = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    let result = state.system.rotate_browser_session(&refresh, csrf, state.config.auth.access_token_ttl_secs, state.config.auth.refresh_token_ttl_secs).map_err(ApiError::internal)?;
    let Some((_id, access, refresh, csrf)) = result else { return Err(ApiError::unauthorized("invalid or expired refresh session")); };
    Ok(response(access.as_str(), csrf.as_str(), refresh.as_str(), &state))
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    let refresh = refresh_cookie_value(&headers).ok_or_else(|| ApiError::unauthorized("missing refresh session"))?;
    let csrf = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    if !state.system.revoke_browser_session(&refresh, csrf).map_err(ApiError::internal)? {
        return Err(ApiError::unauthorized("invalid or expired refresh session"));
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().append(header::SET_COOKIE, clear_refresh_cookie(state.config.auth.secure_cookies));
    r.headers_mut().append(header::SET_COOKIE, clear_csrf_cookie(state.config.auth.secure_cookies));
    Ok(r)
}

pub async fn me(principal: crate::auth::Principal) -> ApiResult<impl IntoResponse> {
    Ok(Json(serde_json::json!({ "id": principal.id, "name": principal.name })))
}
