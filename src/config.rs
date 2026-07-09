//! Configuration loading (§12, §20.5).
//!
//! Config is loaded from a TOML file, then per-key environment overrides are
//! applied. Override variables are named `TINYLORD_<SECTION>_<KEY>` in upper
//! snake case, e.g. `TINYLORD_SERVER_BIND`, `TINYLORD_LIMITS_MAX_QUERY_LIMIT`.
//!
//! The encryption *key material* is never part of this file; only the pointer
//! to where the key lives (`key_source`, `key_file`) is configured here. The
//! key itself is resolved separately in `crate::encryption`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level configuration. Every section has defaults so a missing or partial
/// `tinylord.toml` still yields a usable configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub limits: LimitsConfig,
    pub writer: WriterConfig,
    pub realtime: RealtimeConfig,
    pub cors: CorsConfig,
    pub encryption: EncryptionConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub data_dir: PathBuf,
    pub snapshot_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_document_bytes: u64,
    pub max_database_bytes: u64,
    pub max_query_limit: u32,
    pub request_body_bytes: usize,
    pub rate_per_minute: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WriterConfig {
    pub busy_timeout_ms: u32,
    pub group_commit: bool,
    pub group_commit_max_batch: usize,
    pub wal_checkpoint_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RealtimeConfig {
    pub changelog_retention: i64,
    pub sse_channel_capacity: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Explicit allow-list of origins. Never `*` when Authorization is used (§15).
    pub allowed_origins: Vec<String>,
}

/// Where the encryption key comes from. The key value is NEVER stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    KeyFile,
    Env,
    Keyring,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EncryptionConfig {
    /// Encryption at rest is on by default (§20). Setting this to `false` still
    /// requires the `--allow-unencrypted` CLI flag to actually take effect.
    pub enabled: bool,
    pub key_source: KeySource,
    /// Path to a 0600 file holding the 64-hex-char key. Used when
    /// `key_source = "key_file"`, and also as the target for auto-generation on
    /// first run (§20.4).
    pub key_file: PathBuf,
    /// Service name used to look up the key in the OS keyring when
    /// `key_source = "keyring"`.
    pub keyring_service: String,
    pub keyring_account: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8090".to_string(),
            data_dir: PathBuf::from("./data"),
            snapshot_dir: PathBuf::from("./snapshots"),
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_document_bytes: 1_048_576,
            max_database_bytes: 1_073_741_824,
            max_query_limit: 500,
            request_body_bytes: 2_097_152,
            rate_per_minute: 600,
        }
    }
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5000,
            group_commit: true,
            group_commit_max_batch: 64,
            wal_checkpoint_secs: 60,
        }
    }
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            changelog_retention: 10_000,
            sse_channel_capacity: 256,
        }
    }
}

impl Default for CorsConfig {
    fn default() -> Self {
        // Matches the documented example. Operators must override for production.
        Self {
            allowed_origins: vec!["http://localhost:5173".to_string()],
        }
    }
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            key_source: KeySource::KeyFile,
            key_file: PathBuf::from("./secrets/tinylord.key"),
            keyring_service: "tinylord".to_string(),
            keyring_account: "instance-key".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            limits: LimitsConfig::default(),
            writer: WriterConfig::default(),
            realtime: RealtimeConfig::default(),
            cors: CorsConfig::default(),
            encryption: EncryptionConfig::default(),
        }
    }
}

impl Config {
    /// Load from `path` if it exists (otherwise start from defaults), then apply
    /// environment overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
            toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing config {}: {e}", path.display()))?
        } else {
            Config::default()
        };
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// Apply `TINYLORD_<SECTION>_<KEY>` overrides. Only a curated set of scalar
    /// keys is overridable; complex/list values stay file-only.
    fn apply_env_overrides(&mut self) {
        use std::env::var;

        if let Ok(v) = var("TINYLORD_SERVER_BIND") {
            self.server.bind = v;
        }
        if let Ok(v) = var("TINYLORD_SERVER_DATA_DIR") {
            self.server.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = var("TINYLORD_SERVER_SNAPSHOT_DIR") {
            self.server.snapshot_dir = PathBuf::from(v);
        }

        parse_env("TINYLORD_LIMITS_MAX_DOCUMENT_BYTES", &mut self.limits.max_document_bytes);
        parse_env("TINYLORD_LIMITS_MAX_DATABASE_BYTES", &mut self.limits.max_database_bytes);
        parse_env("TINYLORD_LIMITS_MAX_QUERY_LIMIT", &mut self.limits.max_query_limit);
        parse_env("TINYLORD_LIMITS_REQUEST_BODY_BYTES", &mut self.limits.request_body_bytes);
        parse_env("TINYLORD_LIMITS_RATE_PER_MINUTE", &mut self.limits.rate_per_minute);

        parse_env("TINYLORD_WRITER_BUSY_TIMEOUT_MS", &mut self.writer.busy_timeout_ms);
        parse_env("TINYLORD_WRITER_GROUP_COMMIT", &mut self.writer.group_commit);
        parse_env("TINYLORD_WRITER_GROUP_COMMIT_MAX_BATCH", &mut self.writer.group_commit_max_batch);
        parse_env("TINYLORD_WRITER_WAL_CHECKPOINT_SECS", &mut self.writer.wal_checkpoint_secs);

        parse_env("TINYLORD_REALTIME_CHANGELOG_RETENTION", &mut self.realtime.changelog_retention);
        parse_env("TINYLORD_REALTIME_SSE_CHANNEL_CAPACITY", &mut self.realtime.sse_channel_capacity);

        parse_env("TINYLORD_ENCRYPTION_ENABLED", &mut self.encryption.enabled);
        if let Ok(v) = var("TINYLORD_ENCRYPTION_KEY_FILE") {
            self.encryption.key_file = PathBuf::from(v);
        }
    }
}

/// Parse a value from an env var into `slot`, ignoring unparseable values with a
/// warning so a typo never silently changes behavior.
fn parse_env<T: std::str::FromStr>(name: &str, slot: &mut T) {
    if let Ok(v) = std::env::var(name) {
        match v.parse::<T>() {
            Ok(parsed) => *slot = parsed,
            Err(_) => tracing::warn!("ignoring unparseable env override {name}={v}"),
        }
    }
}
