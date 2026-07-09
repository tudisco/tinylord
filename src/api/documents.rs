//! Document CRUD by id (§7.3, §7.4). Writes go through the writer actor; reads
//! use the read-only pool.

use crate::api::{AppState, DocEnvelope};
use crate::db::schema;
use crate::db::writer::DocRecord;
use crate::errors::{map_sqlite_err, ApiError, ApiResult};
use crate::ids;
use crate::limits::check_document_size;
use crate::system::Role;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

fn require_object(body: &serde_json::Value) -> ApiResult<()> {
    if body.is_object() {
        Ok(())
    } else {
        Err(ApiError::bad_request("document body must be a JSON object"))
    }
}

fn to_envelope(rec: DocRecord) -> DocEnvelope {
    DocEnvelope {
        id: rec.id,
        created_at: rec.created_at,
        updated_at: rec.updated_at,
        doc: rec.doc,
    }
}

pub async fn create(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Write).await?;
    ids::require_valid_name("collection", &coll)?;
    if ids::is_reserved_collection(&coll) {
        return Err(ApiError::validation("collection name is reserved"));
    }
    require_object(&body)?;
    check_document_size(&body, state.config.limits.max_document_bytes)?;

    let handle = state.open_db(&db).await?;
    let rec = handle.writer.create_document(coll, body).await?;
    Ok((StatusCode::CREATED, Json(to_envelope(rec))))
}

pub async fn get_doc(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll, id)): Path<(String, String, String)>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Read).await?;
    ids::require_valid_name("collection", &coll)?;

    let handle = state.open_db(&db).await?;
    match fetch_document(&handle, &coll, &id)? {
        Some(env) => Ok(Json(env)),
        None => Err(ApiError::not_found("document not found")),
    }
}

pub async fn put_doc(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll, id)): Path<(String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Write).await?;
    ids::require_valid_name("collection", &coll)?;
    if ids::is_reserved_collection(&coll) {
        return Err(ApiError::validation("collection name is reserved"));
    }
    require_object(&body)?;
    check_document_size(&body, state.config.limits.max_document_bytes)?;

    let handle = state.open_db(&db).await?;
    let (rec, created) = handle.writer.replace_document(coll, id, body).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(to_envelope(rec))))
}

pub async fn delete_doc(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll, id)): Path<(String, String, String)>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Write).await?;
    ids::require_valid_name("collection", &coll)?;

    let handle = state.open_db(&db).await?;
    let deleted = handle.writer.delete_document(coll, id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("document not found"))
    }
}

/// Read a single document via the read-only pool. Returns `None` if the
/// collection or document doesn't exist.
pub fn fetch_document(
    handle: &crate::db::DbHandle,
    coll: &str,
    id: &str,
) -> ApiResult<Option<DocEnvelope>> {
    let conn = handle
        .read_pool
        .get()
        .map_err(|e| ApiError::internal(format!("read pool: {e}")))?;

    if !schema::collection_exists(&conn, coll).map_err(map_sqlite_err)? {
        return Ok(None);
    }

    let row = conn.query_row(
        &format!(
            "SELECT id, json(doc), created_at, updated_at FROM coll_{coll} WHERE id = ?1"
        ),
        rusqlite::params![id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    );

    match row {
        Ok((id, doc_text, created_at, updated_at)) => {
            let doc = serde_json::from_str(&doc_text).map_err(ApiError::internal)?;
            Ok(Some(DocEnvelope {
                id,
                created_at,
                updated_at,
                doc,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(map_sqlite_err(e)),
    }
}
