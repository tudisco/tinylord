//! Index management (§7.6). All operations require the per-db `admin` grant.

use crate::api::AppState;
use crate::errors::{map_sqlite_err, ApiError, ApiResult};
use crate::ids;
use crate::system::{IndexRecord, Role};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct CreateIndexBody {
    /// JSON path, e.g. `$.email`.
    path: String,
    #[serde(default)]
    unique: bool,
}

pub async fn create(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll)): Path<(String, String)>,
    Json(body): Json<CreateIndexBody>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Admin).await?;
    ids::require_valid_name("collection", &coll)?;

    if !ids::valid_json_path(&body.path) {
        return Err(ApiError::validation(
            "path must be a JSON path like '$.email'",
        ));
    }
    let name = generate_index_name(&coll, &body.path);

    let handle = state.open_db(&db).await?;
    handle
        .writer
        .create_index(coll.clone(), name.clone(), body.path.clone(), body.unique)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "name": name,
            "collection": coll,
            "path": body.path,
            "unique": body.unique
        })),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Admin).await?;
    ids::require_valid_name("collection", &coll)?;

    let handle = state.open_db(&db).await?;
    let conn = handle
        .read_pool
        .get()
        .map_err(|e| ApiError::internal(format!("read pool: {e}")))?;

    let mut stmt = conn
        .prepare("SELECT name, collection, path, is_unique FROM _indexes WHERE collection = ?1 ORDER BY name")
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(rusqlite::params![coll], |r| {
            Ok(IndexRecord {
                name: r.get(0)?,
                collection: r.get(1)?,
                path: r.get(2)?,
                is_unique: r.get::<_, i64>(3)? != 0,
            })
        })
        .map_err(map_sqlite_err)?;
    let mut indexes = Vec::new();
    for r in rows {
        indexes.push(r.map_err(map_sqlite_err)?);
    }
    Ok(Json(serde_json::json!({ "indexes": indexes })))
}

pub async fn drop(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll, name)): Path<(String, String, String)>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Admin).await?;
    ids::require_valid_name("collection", &coll)?;
    // The index name we generated is a plain identifier; validate before use.
    ids::require_valid_name("index", &name)?;

    let handle = state.open_db(&db).await?;
    let dropped = handle.writer.drop_index(name).await?;
    if dropped {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("index not found"))
    }
}

/// Build a valid, reasonably-unique index identifier from the collection and
/// path. Shape: `ix_<coll>_<sanitized-path>_<suffix>`, capped at 64 chars.
fn generate_index_name(coll: &str, path: &str) -> String {
    let sani: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let sani = sani.trim_matches('_');
    // A short suffix from a fresh ULID keeps names unique across re-creates.
    let ulid = ids::new_ulid();
    let suffix = &ulid[ulid.len().saturating_sub(6)..];
    let mut name = format!("ix_{coll}_{sani}_{}", suffix.to_ascii_lowercase());
    name.truncate(64);
    // Guarantee validity even after truncation edge cases.
    if !ids::valid_name(&name) {
        name = format!("ix_{}", ulid.to_ascii_lowercase());
        name.truncate(64);
    }
    name
}
