//! Encryption at rest via SQLCipher (§20).
//!
//! The instance-wide raw key (32 bytes, 64 hex chars) is resolved once at
//! startup from one of three sources and held in a `Zeroizing` buffer so it is
//! wiped from memory on drop. It is applied to EVERY connection — writer,
//! reader pool, and `_system.db` — as the first statement, before any other
//! SQL (§20.3).
//!
//! The key is never placed in config, never logged, and never returned by the
//! API.

use crate::config::{Config, EncryptionConfig, KeySource};
use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::path::Path;
use zeroize::Zeroizing;

/// Resolved encryption state shared across the process. Cheap to clone (the key
/// buffer lives behind an `Arc`).
#[derive(Clone)]
pub struct Encryption {
    inner: std::sync::Arc<Inner>,
}

enum Inner {
    /// Encryption enabled; holds the 64-hex-char key.
    Enabled(Zeroizing<String>),
    /// Explicitly disabled (`--allow-unencrypted`); connections open in the
    /// clear, which SQLCipher treats exactly like standard SQLite.
    Disabled,
}

impl Encryption {
    /// Construct a disabled (plaintext) encryption state.
    pub fn disabled() -> Self {
        Self {
            inner: std::sync::Arc::new(Inner::Disabled),
        }
    }

    /// Construct an enabled encryption state from an already-validated hex key.
    pub fn from_hex(hex_key: Zeroizing<String>) -> Self {
        Self {
            inner: std::sync::Arc::new(Inner::Enabled(hex_key)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        matches!(*self.inner, Inner::Enabled(_))
    }

    /// The `PRAGMA key = "x'...'"` statement to run first on a connection, or
    /// `None` when encryption is disabled. The 64-hex key is validated at
    /// construction so interpolation here is safe (charset `[0-9a-f]`).
    pub fn pragma_key_sql(&self) -> Option<String> {
        match &*self.inner {
            Inner::Enabled(k) => Some(format!("PRAGMA key = \"x'{}'\";", k.as_str())),
            Inner::Disabled => None,
        }
    }

    /// Apply the key to a freshly opened connection and verify it opens. MUST be
    /// called before any other SQL runs on the connection (§20.3).
    pub fn apply_to(&self, conn: &Connection) -> Result<()> {
        if let Some(sql) = self.pragma_key_sql() {
            conn.execute_batch(&sql)
                .context("issuing PRAGMA key")?;
            verify_open(conn)?;
        }
        Ok(())
    }

}

/// Run a trivial read to confirm the key decrypts the database. On a wrong key
/// SQLCipher returns "file is not a database"; we surface a clean error that
/// never echoes the key (§20.3, §20.8).
fn verify_open(conn: &Connection) -> Result<()> {
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("database could not be opened with the configured key"))
}

/// Validate that a string is exactly 64 lowercase-or-uppercase hex characters
/// (a 32-byte raw key). Returns the normalized lowercase form.
pub fn validate_hex_key(s: &str) -> Result<String> {
    let trimmed = s.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("encryption key must be exactly 64 hexadecimal characters (a 32-byte key)");
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Generate a cryptographically random 32-byte key as 64 hex chars.
pub fn generate_hex_key() -> Zeroizing<String> {
    use rand::RngCore;
    let mut buf = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(&mut buf[..]);
    Zeroizing::new(hex::encode(&buf[..]))
}

/// Resolve the encryption state at process start (§20.2, §20.4).
///
/// - `allow_unencrypted` corresponds to the `--allow-unencrypted` CLI flag.
/// - When encryption is enabled but no key can be resolved and the source is a
///   key_file, a fresh key is generated and written with 0600 perms, with a
///   loud one-time warning.
/// - Refuses to start in ambiguous cases (enabled, source configured, key
///   missing/unreadable) rather than silently mismatching (§20.4).
pub fn resolve(cfg: &Config, allow_unencrypted: bool) -> Result<Encryption> {
    let enc = &cfg.encryption;

    // Disabling encryption requires BOTH the config flag and the CLI opt-out.
    if !enc.enabled {
        if !allow_unencrypted {
            bail!(
                "encryption is disabled in config but --allow-unencrypted was not passed; \
                 refusing to store data in plaintext without an explicit opt-out"
            );
        }
        tracing::warn!(
            "ENCRYPTION DISABLED: all databases will be stored UNENCRYPTED (plaintext on disk)"
        );
        return Ok(Encryption::disabled());
    }

    let key = resolve_key(enc)?;
    Ok(Encryption::from_hex(key))
}

/// Resolve just the key material from the configured source, generating a new
/// key file on first run when appropriate.
fn resolve_key(enc: &EncryptionConfig) -> Result<Zeroizing<String>> {
    match enc.key_source {
        KeySource::Env => {
            let raw = std::env::var("TINYLORD_ENCRYPTION_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "key_source = \"env\" but TINYLORD_ENCRYPTION_KEY is not set (64 hex chars)"
                )
            })?;
            let hex = validate_hex_key(&raw)?;
            Ok(Zeroizing::new(hex))
        }
        KeySource::Keyring => resolve_keyring(enc),
        KeySource::KeyFile => resolve_key_file(&enc.key_file),
    }
}

/// Read the key from a 0600 key file, or generate one on first run.
fn resolve_key_file(path: &Path) -> Result<Zeroizing<String>> {
    if path.exists() {
        check_key_file_perms(path)?;
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let hex = validate_hex_key(&raw)
            .with_context(|| format!("key file {} is malformed", path.display()))?;
        return Ok(Zeroizing::new(hex));
    }

    // First run: generate and persist a fresh key with restrictive perms (§20.4).
    let key = generate_hex_key();
    write_key_file(path, &key)?;
    tracing::warn!(
        path = %path.display(),
        "Generated encryption key at {}. BACK THIS UP OFFLINE — losing this key \
         permanently destroys all data. It is not recoverable.",
        path.display()
    );
    Ok(key)
}

/// Write `key` to `path` with 0600 permissions, creating parent dirs.
pub fn write_key_file(path: &Path, key: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating key directory {}", parent.display()))?;
        }
    }
    std::fs::write(path, key)
        .with_context(|| format!("writing key file {}", path.display()))?;
    set_owner_only_perms(path)?;
    Ok(())
}

/// Refuse to start if the key file is readable by group/other (§20.2, §20.8).
fn check_key_file_perms(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("stat key file {}", path.display()))?
            .permissions()
            .mode();
        // Any group/other permission bit set is too loose.
        if mode & 0o077 != 0 {
            bail!(
                "key file {} has permissions {:o}; must be 0600 (owner-only). \
                 Fix with: chmod 600 {}",
                path.display(),
                mode & 0o7777,
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path; // Permission model differs on non-unix; nothing to enforce.
    }
    Ok(())
}

fn set_owner_only_perms(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting 0600 on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(feature = "keyring")]
fn resolve_keyring(enc: &EncryptionConfig) -> Result<Zeroizing<String>> {
    let entry = keyring::Entry::new(&enc.keyring_service, &enc.keyring_account)
        .context("opening OS keyring entry")?;
    let raw = entry
        .get_password()
        .context("no key found in OS keyring; provision it first")?;
    let hex = validate_hex_key(&raw)?;
    Ok(Zeroizing::new(hex))
}

#[cfg(not(feature = "keyring"))]
fn resolve_keyring(_enc: &EncryptionConfig) -> Result<Zeroizing<String>> {
    bail!(
        "key_source = \"keyring\" requires building tinylord with `--features keyring`; \
         this binary was built without OS keyring support"
    )
}
