//! Query & count endpoints (§7.5). Compiles the §8 filter to parameterized SQL
//! and executes it against the read-only pool.

use crate::api::{AppState, DocEnvelope};
use crate::db::schema;
use crate::errors::{map_sqlite_err, ApiError, ApiResult};
use crate::ids;
use crate::query::{self, CountRequest, QueryRequest};
use crate::system::Role;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params_from_iter;
use rusqlite::types::Value;

pub async fn query(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll)): Path<(String, String)>,
    Json(req): Json<QueryRequest>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Read).await?;
    ids::require_valid_name("collection", &coll)?;

    let where_clause = query::compile_filter(req.filter.as_ref())?;
    let order = query::compile_sort(&req.sort)?;

    let max = state.config.limits.max_query_limit;
    // Default page size when unspecified; always clamped to the configured max.
    let limit = req.limit.unwrap_or(50).min(max).max(1);
    let offset = req.offset.unwrap_or(0);

    let compatible = query::is_cursor_compatible(&req.sort);
    let use_cursor = compatible && req.cursor.is_some();

    let handle = state.open_db(&db).await?;
    let conn = handle
        .read_pool
        .get()
        .map_err(|e| ApiError::internal(format!("read pool: {e}")))?;

    // Querying a not-yet-created collection yields an empty result set.
    if !schema::collection_exists(&conn, &coll).map_err(map_sqlite_err)? {
        return Ok(Json(serde_json::json!({ "items": [], "next_cursor": null })));
    }

    let mut sql = format!(
        "SELECT id, json(doc), created_at, updated_at FROM coll_{coll} WHERE {}",
        where_clause.sql
    );
    let mut params: Vec<Value> = where_clause.params;
    if use_cursor {
        sql.push_str(" AND id > ?");
        params.push(Value::Text(req.cursor.clone().unwrap()));
    }
    sql.push(' ');
    sql.push_str(&order);
    // limit/offset are validated integers; inline them.
    sql.push_str(&format!(" LIMIT {limit}"));
    if !use_cursor {
        sql.push_str(&format!(" OFFSET {offset}"));
    }

    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map_err(map_sqlite_err)?;

    let mut items: Vec<DocEnvelope> = Vec::new();
    for row in rows {
        let (id, doc_text, created_at, updated_at) = row.map_err(map_sqlite_err)?;
        let mut doc: serde_json::Value =
            serde_json::from_str(&doc_text).map_err(ApiError::internal)?;
        if let Some(proj) = &req.projection {
            doc = apply_projection(doc, proj);
        }
        items.push(DocEnvelope {
            id,
            created_at,
            updated_at,
            doc,
        });
    }

    // A cursor-compatible full page implies there may be more rows.
    let next_cursor = if compatible && items.len() as u32 == limit {
        items.last().map(|e| e.id.clone())
    } else {
        None
    };

    Ok(Json(serde_json::json!({
        "items": items,
        "next_cursor": next_cursor
    })))
}

pub async fn count(
    State(state): State<AppState>,
    principal: crate::auth::Principal,
    Path((db, coll)): Path<(String, String)>,
    Json(req): Json<CountRequest>,
) -> ApiResult<impl IntoResponse> {
    state.authorize(&principal, &db, Role::Read).await?;
    ids::require_valid_name("collection", &coll)?;

    let where_clause = query::compile_filter(req.filter.as_ref())?;

    let handle = state.open_db(&db).await?;
    let conn = handle
        .read_pool
        .get()
        .map_err(|e| ApiError::internal(format!("read pool: {e}")))?;

    if !schema::collection_exists(&conn, &coll).map_err(map_sqlite_err)? {
        return Ok(Json(serde_json::json!({ "count": 0 })));
    }

    let sql = format!(
        "SELECT count(*) FROM coll_{coll} WHERE {}",
        where_clause.sql
    );
    let count: i64 = conn
        .query_row(&sql, params_from_iter(where_clause.params.iter()), |r| r.get(0))
        .map_err(map_sqlite_err)?;

    Ok(Json(serde_json::json!({ "count": count })))
}

/// Reduce a document to the projected field paths (dot notation), preserving
/// nesting. Missing paths are skipped.
fn apply_projection(doc: serde_json::Value, paths: &[String]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for path in paths {
        let segs: Vec<&str> = path.split('.').collect();
        if segs.iter().any(|s| s.is_empty()) {
            continue;
        }
        if let Some(v) = lookup(&doc, &segs) {
            insert_nested(&mut out, &segs, v.clone());
        }
    }
    serde_json::Value::Object(out)
}

fn lookup<'a>(doc: &'a serde_json::Value, segs: &[&str]) -> Option<&'a serde_json::Value> {
    let mut cur = doc;
    for s in segs {
        cur = cur.get(s)?;
    }
    Some(cur)
}

fn insert_nested(out: &mut serde_json::Map<String, serde_json::Value>, segs: &[&str], value: serde_json::Value) {
    if segs.len() == 1 {
        out.insert(segs[0].to_string(), value);
        return;
    }
    let entry = out
        .entry(segs[0].to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(map) = entry {
        insert_nested(map, &segs[1..], value);
    }
}
