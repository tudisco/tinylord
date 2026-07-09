//! Control plane: the `_system.db` registry of databases, principals, and grants
//! (§3, §4.1). All auth decisions read from here.
//!
//! Tokens are 256-bit CSPRNG values shown to the operator exactly once; only
//! their SHA-256 hash is stored (§6).

use crate::config::Config;
use crate::db::pragmas::apply_pragmas;
use crate::encryption::Encryption;
use anyhow::{Context, Result};
use base64::Engine;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use zeroize::Zeroizing;

#[derive(Debug, Clone)]
pub struct BrowserUser {
    pub id: String,
}

/// Access role, ordered `Read < Write < Admin` (§6). The derived `Ord` follows
/// declaration order, so `>=` implements "has at least this role".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "read" => Some(Role::Read),
            "write" => Some(Role::Write),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

/// A registered database record (§4.1).
#[derive(Debug, Serialize)]
pub struct DatabaseRecord {
    pub name: String,
    pub created_at: i64,
}

/// A registered index, as listed by the API (§7.6).
#[derive(Debug, Serialize)]
pub struct IndexRecord {
    pub name: String,
    pub collection: String,
    pub path: String,
    pub is_unique: bool,
}

/// Handle to the control-plane database. Cheap to clone (pooled).
#[derive(Clone)]
pub struct System {
    pool: r2d2::Pool<SqliteConnectionManager>,
}

/// SHA-256 hex of a bearer token (§6).
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// Generate a fresh 256-bit token: `(plaintext, hash)`. The plaintext is
/// base64url (no padding) and is shown to the operator exactly once.
pub fn generate_token() -> (Zeroizing<String>, String) {
    use rand::RngCore;
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(&mut bytes[..]);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes[..]);
    let hash = hash_token(&token);
    (Zeroizing::new(token), hash)
}

impl System {
    /// Open (creating if needed) `_system.db` under `data_dir`, applying the
    /// encryption key first (§20.3) and initializing the schema.
    pub fn open(cfg: &Config, encryption: &Encryption) -> Result<System> {
        std::fs::create_dir_all(&cfg.server.data_dir)
            .with_context(|| format!("creating data dir {}", cfg.server.data_dir.display()))?;
        let path = cfg.server.data_dir.join("_system.db");
        let system = Self::open_at(&path, encryption, cfg.writer.busy_timeout_ms)?;
        system.init_schema()?;
        Ok(system)
    }

    fn open_at(path: &Path, encryption: &Encryption, busy_timeout_ms: u32) -> Result<System> {
        // Fail fast on a wrong key with ONE clean error, before building the pool
        // (which would otherwise retry the un-openable file until it times out).
        {
            let probe = rusqlite::Connection::open(path).context("opening _system.db")?;
            encryption.apply_to(&probe)?;
        }

        let enc = encryption.clone();
        let manager = SqliteConnectionManager::file(path).with_init(move |conn| {
            // KEY FIRST (§20.3), before any other SQL.
            if let Some(sql) = enc.pragma_key_sql() {
                conn.execute_batch(&sql)?;
            }
            apply_pragmas(conn, busy_timeout_ms)?;
            Ok(())
        });
        let pool = r2d2::Pool::builder()
            .max_size(4)
            .connection_timeout(std::time::Duration::from_secs(5))
            .build(manager)
            .context("building _system.db pool")?;
        Ok(System { pool })
    }

    fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.pool.get().context("system pool")
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS databases (
              name        TEXT PRIMARY KEY,
              created_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS principals (
              id          TEXT PRIMARY KEY,
              name        TEXT NOT NULL,
              is_admin    INTEGER NOT NULL DEFAULT 0,
              token_hash  TEXT NOT NULL,
              disabled    INTEGER NOT NULL DEFAULT 0,
              created_at  INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS ux_principals_token ON principals(token_hash);

            CREATE TABLE IF NOT EXISTS grants (
              principal_id  TEXT NOT NULL,
              database_name TEXT NOT NULL,
              role          TEXT NOT NULL,
              PRIMARY KEY (principal_id, database_name)
            );
            "#,
        )?;
        // Lightweight migration for instances created before browser login.
        let mut stmt = conn.prepare("PRAGMA table_info(principals)")?;
        let columns: Vec<String> = stmt.query_map([], |r| r.get(1))?.collect::<Result<_, _>>()?;
        if !columns.iter().any(|c| c == "username") {
            conn.execute_batch("ALTER TABLE principals ADD COLUMN username TEXT; ALTER TABLE principals ADD COLUMN password_hash TEXT;")?;
            conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS ux_principals_username ON principals(username) WHERE username IS NOT NULL;")?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS browser_access_tokens (token_hash TEXT PRIMARY KEY, principal_id TEXT NOT NULL, expires_at INTEGER NOT NULL);\n             CREATE INDEX IF NOT EXISTS ix_browser_access_expiry ON browser_access_tokens(expires_at);\n             CREATE TABLE IF NOT EXISTS browser_sessions (token_hash TEXT PRIMARY KEY, principal_id TEXT NOT NULL, csrf_hash TEXT NOT NULL, expires_at INTEGER NOT NULL, created_at INTEGER NOT NULL);\n             CREATE INDEX IF NOT EXISTS ix_browser_sessions_expiry ON browser_sessions(expires_at);",
        )?;
        Ok(())
    }

    // ---- Principals --------------------------------------------------------

    pub fn count_principals(&self) -> Result<i64> {
        let conn = self.conn()?;
        let n = conn.query_row("SELECT count(*) FROM principals", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Create a principal and return `(id, plaintext_token)`. Only the hash is
    /// persisted; the plaintext is the caller's single chance to capture it.
    pub fn create_principal(&self, name: &str, is_admin: bool) -> Result<(String, Zeroizing<String>)> {
        let id = crate::ids::new_ulid();
        let (token, hash) = generate_token();
        let now = crate::ids::now_ms();
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO principals (id, name, is_admin, token_hash, disabled, created_at) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![id, name, is_admin as i64, hash, now],
        )?;
        Ok((id, token))
    }

    /// Look up a principal by presented token. Returns `None` if unknown or
    /// disabled.
    pub fn lookup_by_token(&self, token: &str) -> Result<Option<crate::auth::Principal>> {
        let hash = hash_token(token);
        let conn = self.conn()?;
        let result = conn.query_row(
            "SELECT id, name, is_admin, disabled FROM principals WHERE token_hash = ?1",
            params![hash],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, i64>(3)? != 0,
                ))
            },
        );
        match result {
            Ok((id, name, is_admin, disabled)) => {
                if disabled {
                    Ok(None)
                } else {
                    Ok(Some(crate::auth::Principal { id, name, is_admin }))
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn create_browser_user(&self, username: &str, password_hash: &str) -> Result<BrowserUser> {
        let id = crate::ids::new_ulid();
        let (_unused, token_hash) = generate_token();
        let now = crate::ids::now_ms();
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO principals (id, name, is_admin, token_hash, disabled, created_at, username, password_hash) VALUES (?1, ?2, 0, ?3, 0, ?4, ?5, ?6)",
            params![id, username, token_hash, now, username, password_hash],
        )?;
        Ok(BrowserUser { id })
    }

    pub fn browser_user(&self, username: &str) -> Result<Option<(BrowserUser, String)>> {
        let conn = self.conn()?;
        let r = conn.query_row(
            "SELECT id, password_hash FROM principals WHERE username = ?1 AND disabled = 0",
            params![username],
            |r| Ok((BrowserUser { id: r.get(0)? }, r.get(1)?)),
        );
        match r { Ok(v) => Ok(Some(v)), Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None), Err(e) => Err(e.into()) }
    }

    pub fn issue_browser_tokens(&self, principal_id: &str, access_ttl_secs: i64, refresh_ttl_secs: i64) -> Result<(Zeroizing<String>, Zeroizing<String>, Zeroizing<String>)> {
        let (access, access_hash) = generate_token();
        let (refresh, refresh_hash) = generate_token();
        let (csrf, csrf_hash) = generate_token();
        let now = crate::ids::now_ms();
        let conn = self.conn()?;
        conn.execute("DELETE FROM browser_access_tokens WHERE expires_at <= ?1", params![now])?;
        conn.execute("DELETE FROM browser_sessions WHERE expires_at <= ?1", params![now])?;
        conn.execute("INSERT INTO browser_access_tokens (token_hash, principal_id, expires_at) VALUES (?1, ?2, ?3)", params![access_hash, principal_id, now + access_ttl_secs * 1000])?;
        conn.execute("INSERT INTO browser_sessions (token_hash, principal_id, csrf_hash, expires_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![refresh_hash, principal_id, csrf_hash, now + refresh_ttl_secs * 1000, now])?;
        Ok((access, refresh, csrf))
    }

    pub fn rotate_browser_session(&self, refresh: &str, csrf: &str, access_ttl_secs: i64, refresh_ttl_secs: i64) -> Result<Option<(String, Zeroizing<String>, Zeroizing<String>, Zeroizing<String>)>> {
        let hash = hash_token(refresh); let csrf_hash = hash_token(csrf); let now = crate::ids::now_ms();
        let conn = self.conn()?;
        let principal_id: Option<String> = conn.query_row("SELECT principal_id FROM browser_sessions WHERE token_hash = ?1 AND csrf_hash = ?2 AND expires_at > ?3", params![hash, csrf_hash, now], |r| r.get(0)).ok();
        let Some(principal_id) = principal_id else { return Ok(None); };
        conn.execute("DELETE FROM browser_sessions WHERE token_hash = ?1", params![hash])?;
        drop(conn);
        let (access, new_refresh, new_csrf) = self.issue_browser_tokens(&principal_id, access_ttl_secs, refresh_ttl_secs)?;
        Ok(Some((principal_id, access, new_refresh, new_csrf)))
    }

    pub fn revoke_browser_session(&self, refresh: &str, csrf: &str) -> Result<bool> {
        let n = self.conn()?.execute(
            "DELETE FROM browser_sessions WHERE token_hash = ?1 AND csrf_hash = ?2",
            params![hash_token(refresh), hash_token(csrf)],
        )?;
        Ok(n > 0)
    }

    pub fn lookup_browser_access_token(&self, token: &str) -> Result<Option<crate::auth::Principal>> {
        let conn = self.conn()?; let now = crate::ids::now_ms(); let hash = hash_token(token);
        let r = conn.query_row("SELECT p.id, p.name, p.is_admin, p.disabled FROM browser_access_tokens t JOIN principals p ON p.id=t.principal_id WHERE t.token_hash=?1 AND t.expires_at>?2", params![hash, now], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,i64>(2)? != 0, r.get::<_,i64>(3)? != 0)));
        match r { Ok((id,name,is_admin,disabled)) if !disabled => Ok(Some(crate::auth::Principal {id,name,is_admin})), Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None), Err(e) => Err(e.into()) }
    }

    /// Soft-disable a principal. Returns false if no such principal.
    pub fn disable_principal(&self, id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let n = conn.execute(
            "UPDATE principals SET disabled = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(n > 0)
    }

    /// Rotate the bootstrap global-admin token. If no admin exists, one is
    /// created. Returns the new plaintext token.
    pub fn reset_admin_token(&self) -> Result<Zeroizing<String>> {
        let conn = self.conn()?;
        let admin_id: Option<String> = conn
            .query_row(
                "SELECT id FROM principals WHERE is_admin = 1 ORDER BY created_at LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok();
        drop(conn);

        match admin_id {
            Some(id) => {
                let (token, hash) = generate_token();
                let conn = self.conn()?;
                conn.execute(
                    "UPDATE principals SET token_hash = ?1, disabled = 0 WHERE id = ?2",
                    params![hash, id],
                )?;
                Ok(token)
            }
            None => {
                let (_id, token) = self.create_principal("admin", true)?;
                Ok(token)
            }
        }
    }

    // ---- Databases ---------------------------------------------------------

    /// Insert a database record. Returns `Some(created_at)` when inserted, or
    /// `None` if it already existed.
    pub fn insert_database(&self, name: &str) -> Result<Option<i64>> {
        let conn = self.conn()?;
        let now = crate::ids::now_ms();
        let n = conn.execute(
            "INSERT OR IGNORE INTO databases (name, created_at) VALUES (?1, ?2)",
            params![name, now],
        )?;
        Ok(if n > 0 { Some(now) } else { None })
    }

    pub fn database_exists(&self, name: &str) -> Result<bool> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM databases WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn list_databases(&self) -> Result<Vec<DatabaseRecord>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT name, created_at FROM databases ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(DatabaseRecord {
                name: r.get(0)?,
                created_at: r.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_database_record(&self, name: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM databases WHERE name = ?1", params![name])?;
        // Grants for a dropped database are no longer meaningful.
        conn.execute("DELETE FROM grants WHERE database_name = ?1", params![name])?;
        Ok(())
    }

    // ---- Grants ------------------------------------------------------------

    pub fn upsert_grant(&self, principal_id: &str, database: &str, role: Role) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO grants (principal_id, database_name, role) VALUES (?1, ?2, ?3) \
             ON CONFLICT(principal_id, database_name) DO UPDATE SET role = excluded.role",
            params![principal_id, database, role.as_str()],
        )?;
        Ok(())
    }

    pub fn delete_grant(&self, principal_id: &str, database: &str) -> Result<bool> {
        let conn = self.conn()?;
        let n = conn.execute(
            "DELETE FROM grants WHERE principal_id = ?1 AND database_name = ?2",
            params![principal_id, database],
        )?;
        Ok(n > 0)
    }

    /// The role granted to `principal_id` on `database`, if any (§6).
    pub fn grant_role(&self, principal_id: &str, database: &str) -> Result<Option<Role>> {
        let conn = self.conn()?;
        let role: Option<String> = conn
            .query_row(
                "SELECT role FROM grants WHERE principal_id = ?1 AND database_name = ?2",
                params![principal_id, database],
                |r| r.get(0),
            )
            .ok();
        Ok(role.and_then(|r| Role::parse(&r)))
    }
}
