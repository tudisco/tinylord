//! Admin endpoints (§7.2). All require a global-admin token except `snapshot`,
//! which also accepts a per-database `admin` grant.

use crate::api::AppState;
use crate::auth::{AdminPrincipal, Principal};
use crate::errors::{ApiError, ApiResult};
use crate::ids;
use crate::system::Role;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct CreateDatabaseBody {
    name: String,
}

pub async fn create_database(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
    Json(body): Json<CreateDatabaseBody>,
) -> ApiResult<impl IntoResponse> {
    ids::require_valid_name("database", &body.name)?;
    match state.system.insert_database(&body.name).map_err(ApiError::internal)? {
        Some(created_at) => {
            // Create the file and initialize its per-db schema (§7.2).
            state.registry.init_new_database(&body.name).await?;
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({ "name": body.name, "created_at": created_at })),
            ))
        }
        None => Err(ApiError::conflict("database already exists")),
    }
}

pub async fn list_databases(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
) -> ApiResult<impl IntoResponse> {
    let dbs = state.system.list_databases().map_err(ApiError::internal)?;
    Ok(Json(serde_json::json!({ "databases": dbs })))
}

pub async fn delete_database(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
    Path(db): Path<String>,
) -> ApiResult<impl IntoResponse> {
    ids::require_valid_name("database", &db)?;
    if !state.system.database_exists(&db).map_err(ApiError::internal)? {
        return Err(ApiError::not_found("database not found"));
    }
    state.registry.drop_database(&db).await?;
    state
        .system
        .delete_database_record(&db)
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CreatePrincipalBody {
    name: String,
    #[serde(default)]
    is_admin: bool,
    password: Option<String>,
}

pub async fn create_principal(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
    Json(body): Json<CreatePrincipalBody>,
) -> ApiResult<impl IntoResponse> {
    if let Some(password) = body.password {
        if body.is_admin {
            return Err(ApiError::validation("browser users cannot be global admins"));
        }
        if !super::browser_auth::valid_username(&body.name) {
            return Err(ApiError::validation("username must be 3 to 64 letters, numbers, '_' or '-'"));
        }
        let hash = super::browser_auth::hash_password(&password)?;
        let user = state.system.create_browser_user(&body.name, &hash).map_err(|e| {
            if e.to_string().contains("UNIQUE constraint failed") { ApiError::conflict("username already exists") } else { ApiError::internal(e) }
        })?;
        return Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": user.id, "name": body.name }))));
    }
    let (id, token) = state
        .system
        .create_principal(&body.name, body.is_admin)
        .map_err(ApiError::internal)?;
    // Token plaintext is returned exactly once (§6).
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "token": token.as_str() })),
    ))
}

pub async fn delete_principal(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    // Soft-disable (§7.2).
    if state.system.disable_principal(&id).map_err(ApiError::internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("principal not found"))
    }
}

#[derive(Deserialize)]
pub struct CreateGrantBody {
    principal_id: String,
    database: String,
    role: String,
}

pub async fn create_grant(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
    Json(body): Json<CreateGrantBody>,
) -> ApiResult<impl IntoResponse> {
    ids::require_valid_name("database", &body.database)?;
    let role = Role::parse(&body.role)
        .ok_or_else(|| ApiError::validation("role must be 'read', 'write', or 'admin'"))?;
    if !state
        .system
        .database_exists(&body.database)
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::not_found("database not found"));
    }
    state
        .system
        .upsert_grant(&body.principal_id, &body.database, role)
        .map_err(ApiError::internal)?;
    Ok(Json(serde_json::json!({
        "principal_id": body.principal_id,
        "database": body.database,
        "role": role.as_str()
    })))
}

#[derive(Deserialize)]
pub struct DeleteGrantBody {
    principal_id: String,
    database: String,
}

pub async fn delete_grant(
    State(state): State<AppState>,
    _admin: AdminPrincipal,
    Json(body): Json<DeleteGrantBody>,
) -> ApiResult<impl IntoResponse> {
    state
        .system
        .delete_grant(&body.principal_id, &body.database)
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/admin/databases/{db}/snapshot` — global admin OR per-db admin.
pub async fn snapshot(
    State(state): State<AppState>,
    principal: Principal,
    Path(db): Path<String>,
) -> ApiResult<impl IntoResponse> {
    state.authorize_snapshot(&principal, &db).await?;
    let handle = state.open_db(&db).await?;

    std::fs::create_dir_all(&state.config.server.snapshot_dir).map_err(ApiError::internal)?;
    let filename = format!("{db}-{}.db", ids::new_ulid());
    let dest = state.config.server.snapshot_dir.join(filename);

    let bytes = handle.writer.snapshot(dest.clone()).await?;
    Ok(Json(serde_json::json!({
        "path": dest.to_string_lossy(),
        "bytes": bytes
    })))
}
