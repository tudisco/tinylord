//! Read-only connection pool (§3).
//!
//! Under WAL, reads run concurrently with the single writer. Connections are
//! opened read-write-capable but pinned to `query_only = ON` so they can service
//! WAL reads (which touch the `-shm` file) while being unable to mutate data.
//! Every connection applies `PRAGMA key` FIRST (§20.3), then the standard
//! pragmas (§5.1).

use crate::db::pragmas::apply_pragmas;
use crate::encryption::Encryption;
use r2d2_sqlite::SqliteConnectionManager;
use std::path::Path;

pub type ReadPool = r2d2::Pool<SqliteConnectionManager>;

/// Build a read-only pool for the database file at `path`.
pub fn build_pool(
    path: &Path,
    encryption: &Encryption,
    busy_timeout_ms: u32,
) -> anyhow::Result<ReadPool> {
    let enc = encryption.clone();
    let manager = SqliteConnectionManager::file(path).with_init(move |conn| {
        // KEY FIRST — before any other SQL (§20.3).
        if let Some(sql) = enc.pragma_key_sql() {
            conn.execute_batch(&sql)?;
        }
        apply_pragmas(conn, busy_timeout_ms)?;
        // Defense in depth: readers must never write. All writes go through the
        // single writer actor (§3, §5.2).
        conn.pragma_update(None, "query_only", "ON")?;
        Ok(())
    });

    let pool = r2d2::Pool::builder()
        // A small pool is plenty: WAL allows concurrent readers on one file, and
        // each logical database gets its own pool.
        .max_size(4)
        .connection_timeout(std::time::Duration::from_secs(5))
        .build(manager)?;
    Ok(pool)
}
