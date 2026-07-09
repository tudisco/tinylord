//! Connection pragmas (§5.1).
//!
//! These are applied on EVERY connection at open, AFTER `PRAGMA key` has been
//! issued and verified by `crate::encryption` (§20.3 mandates key-first order).

use rusqlite::Connection;

/// Apply the standard pragma set to a connection.
///
/// `synchronous = NORMAL` under WAL can lose the last transaction(s) on power
/// loss but never corrupts the database. That trade-off is acceptable here and
/// is covered by the backup layer (§10).
pub fn apply_pragmas(conn: &Connection, busy_timeout_ms: u32) -> rusqlite::Result<()> {
    // journal_mode returns a row; use query form.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", busy_timeout_ms)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -8000)?; // ~8MB
    conn.pragma_update(None, "mmap_size", 134_217_728i64)?; // 128MB
    Ok(())
}
