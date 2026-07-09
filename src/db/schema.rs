//! Per-database schema DDL (§4.2) and document-storage mode (§20.1).
//!
//! ## Storage mode
//! SQLite 3.45+ ships JSONB (a compact binary JSON encoding). The bundled
//! SQLCipher build may track an older base version, so we detect the effective
//! `sqlite_version()` once at startup and pick a storage mode:
//! - `Jsonb`    → column is `BLOB`, documents stored via `jsonb(?)`.
//! - `TextJson` → column is `TEXT`, documents stored via `json(?)`.
//! Reads use `json(doc)` in both modes, and `json_extract` is identical, so the
//! rest of the service is oblivious to the choice.

use rusqlite::Connection;

/// How documents are physically stored. Decided once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocStorage {
    Jsonb,
    TextJson,
}

impl DocStorage {
    /// Detect the storage mode from the effective SQLite version. Opens a
    /// throwaway in-memory connection (no key needed).
    pub fn detect() -> Self {
        match Connection::open_in_memory()
            .and_then(|c| c.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0)))
        {
            Ok(v) if version_at_least(&v, 3, 45, 0) => DocStorage::Jsonb,
            Ok(v) => {
                tracing::warn!(
                    "SQLite base version {v} < 3.45.0; falling back to TEXT JSON storage"
                );
                DocStorage::TextJson
            }
            Err(_) => DocStorage::TextJson,
        }
    }

    /// SQL column type for the `doc` column.
    pub fn col_type(self) -> &'static str {
        match self {
            DocStorage::Jsonb => "BLOB",
            DocStorage::TextJson => "TEXT",
        }
    }

    /// SQL function that ingests JSON *text* into the stored form.
    pub fn store_fn(self) -> &'static str {
        match self {
            DocStorage::Jsonb => "jsonb",
            DocStorage::TextJson => "json",
        }
    }

    /// CHECK clause validating the stored `doc`. `json_valid(doc, 8)` validates
    /// a strict JSONB blob (2-arg form exists only in 3.45+); the 1-arg form
    /// validates JSON text.
    pub fn check_clause(self) -> &'static str {
        match self {
            DocStorage::Jsonb => "json_valid(doc, 8)",
            DocStorage::TextJson => "json_valid(doc)",
        }
    }
}

fn version_at_least(v: &str, maj: u32, min: u32, patch: u32) -> bool {
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let a = parts.next().unwrap_or(0);
    let b = parts.next().unwrap_or(0);
    let c = parts.next().unwrap_or(0);
    (a, b, c) >= (maj, min, patch)
}

/// Initialize the per-database internal tables (`_changelog`, `_indexes`).
/// Idempotent (`IF NOT EXISTS`). §4.2.
pub fn init_database(conn: &Connection, storage: DocStorage) -> rusqlite::Result<()> {
    let doc_col = storage.col_type();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS _changelog (
          seq         INTEGER PRIMARY KEY,   -- monotonic per-db sequence = rowid
          collection  TEXT NOT NULL,
          op          TEXT NOT NULL,         -- 'insert' | 'update' | 'delete'
          doc_id      TEXT NOT NULL,
          doc         {doc_col},             -- new state; NULL for delete
          ts          INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS _indexes (
          name        TEXT PRIMARY KEY,
          collection  TEXT NOT NULL,
          path        TEXT NOT NULL,
          is_unique   INTEGER NOT NULL
        );
        "#
    ))?;
    Ok(())
}

/// Create a collection table on first write. `name` MUST already be validated by
/// `crate::ids::valid_name` before calling — it is interpolated as a SQL
/// identifier. Idempotent.
///
/// The table is a rowid table (no `WITHOUT ROWID`) because the `update_hook`
/// does not fire for WITHOUT ROWID tables (§5.3).
pub fn ensure_collection(
    conn: &Connection,
    name: &str,
    storage: DocStorage,
) -> rusqlite::Result<()> {
    debug_assert!(crate::ids::valid_name(name), "collection name must be validated");
    let doc_col = storage.col_type();
    let check = storage.check_clause();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS coll_{name} (
          rowid       INTEGER PRIMARY KEY,     -- internal; drives update_hook
          id          TEXT NOT NULL UNIQUE,    -- public ULID
          doc         {doc_col} NOT NULL,
          created_at  INTEGER NOT NULL,
          updated_at  INTEGER NOT NULL,
          CHECK ({check})
        );
        "#
    ))?;
    Ok(())
}

/// True if a collection table exists (without creating it).
pub fn collection_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    debug_assert!(crate::ids::valid_name(name));
    let table = format!("coll_{name}");
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [&table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}
