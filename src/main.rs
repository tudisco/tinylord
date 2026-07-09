//! tinylord — a tiny schemaless realtime document datastore.
//!
//! Entry point: CLI parsing (§14), config loading, and the `serve` command.

mod api;
mod auth;
mod config;
mod db;
mod encryption;
mod errors;
mod ids;
mod limits;
mod proxy;
mod query;
mod realtime;
mod system;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use db::schema::DocStorage;
use encryption::Encryption;
use std::path::PathBuf;
use std::sync::Arc;
use system::{Role, System};

#[derive(Parser)]
#[command(name = "tinylord", version, about = "A tiny schemaless realtime datastore")]
struct Cli {
    /// Path to the config file (TOML). Missing file → defaults.
    #[arg(long, default_value = "tinylord.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server; bootstraps the admin token on first run.
    Serve {
        /// Permit running with encryption disabled (requires `[encryption] enabled = false`).
        #[arg(long)]
        allow_unencrypted: bool,
    },
    /// Generate a random 32-byte encryption key (64 hex chars).
    Keygen {
        /// Write the key to this file (0600). If omitted, print to stdout once.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Administrative operations against `_system.db` (server need not run).
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Database operations against `_system.db` (server need not run).
    Db {
        #[command(subcommand)]
        cmd: DbCmd,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Rotate the global admin token (prints the new token once).
    ResetToken,
    /// Create a principal offline (prints token once).
    CreateUser {
        #[arg(long)]
        name: String,
        #[arg(long)]
        admin: bool,
    },
    /// Grant a role on a database to a principal.
    Grant {
        #[arg(long)]
        user: String,
        #[arg(long)]
        db: String,
        #[arg(long)]
        role: String,
    },
    /// Offline re-encryption: re-key `_system.db` and every data DB (§20.6).
    Rekey,
}

#[derive(Subcommand)]
enum DbCmd {
    /// Create a database.
    Create { name: String },
    /// List databases.
    List,
    /// Snapshot a database via VACUUM INTO.
    Snapshot { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    match cli.command {
        Command::Serve { allow_unencrypted } => serve(&cli.config, allow_unencrypted).await,
        Command::Keygen { out } => cmd_keygen(out),
        Command::Admin { cmd } => run_admin(&cli.config, cmd),
        Command::Db { cmd } => run_db(&cli.config, cmd),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("TINYLORD_LOG_JSON").map(|v| v == "1").unwrap_or(false);
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Resolve encryption for offline CLI commands. `allow_unencrypted` is implied
/// by the config's `enabled = false` here, so CLI tools open the same files the
/// server would.
fn cli_encryption(cfg: &Config) -> Result<Encryption> {
    encryption::resolve(cfg, true)
}

async fn serve(config_path: &PathBuf, allow_unencrypted: bool) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let encryption = encryption::resolve(&cfg, allow_unencrypted)?;
    if encryption.is_enabled() {
        tracing::info!("encryption at rest: ENABLED (SQLCipher)");
    }
    let storage = DocStorage::detect();
    tracing::info!(?storage, "document storage mode");

    let system = System::open(&cfg, &encryption)?;

    // Bootstrap: first run mints a global-admin token, printed once (§6).
    if system.count_principals()? == 0 {
        let (_id, token) = system.create_principal("admin", true)?;
        println!("========================================================");
        println!(" tinylord bootstrap: global admin token (shown ONCE)");
        println!(" {}", token.as_str());
        println!(" Store it securely. Rotate with `tinylord admin reset-token`.");
        println!("========================================================");
    }

    let registry = Arc::new(db::DbRegistry::new(&cfg, encryption.clone(), storage));
    let bind = cfg.server.bind.clone();
    let state = api::AppState::new(system, registry, cfg, encryption);
    let app = api::build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!("tinylord listening on http://{bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

fn cmd_keygen(out: Option<PathBuf>) -> Result<()> {
    let key = encryption::generate_hex_key();
    match out {
        Some(path) => {
            encryption::write_key_file(&path, &key)?;
            println!("wrote 32-byte key to {} (0600)", path.display());
            println!("BACK THIS UP OFFLINE — losing it permanently destroys all data.");
        }
        None => {
            println!("{}", key.as_str());
        }
    }
    Ok(())
}

fn run_admin(config_path: &PathBuf, cmd: AdminCmd) -> Result<()> {
    let cfg = Config::load(config_path)?;
    match cmd {
        AdminCmd::ResetToken => {
            let encryption = cli_encryption(&cfg)?;
            let system = System::open(&cfg, &encryption)?;
            let token = system.reset_admin_token()?;
            println!("new admin token (shown once):");
            println!("{}", token.as_str());
            Ok(())
        }
        AdminCmd::CreateUser { name, admin } => {
            let encryption = cli_encryption(&cfg)?;
            let system = System::open(&cfg, &encryption)?;
            let (id, token) = system.create_principal(&name, admin)?;
            println!("created principal {id}");
            println!("token (shown once): {}", token.as_str());
            Ok(())
        }
        AdminCmd::Grant { user, db, role } => {
            let role = Role::parse(&role)
                .ok_or_else(|| anyhow::anyhow!("role must be read|write|admin"))?;
            let encryption = cli_encryption(&cfg)?;
            let system = System::open(&cfg, &encryption)?;
            if !system.database_exists(&db)? {
                bail!("database '{db}' does not exist");
            }
            system.upsert_grant(&user, &db, role)?;
            println!("granted {} on {db} to {user}", role.as_str());
            Ok(())
        }
        AdminCmd::Rekey => cmd_rekey(&cfg),
    }
}

fn run_db(config_path: &PathBuf, cmd: DbCmd) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let encryption = cli_encryption(&cfg)?;
    let system = System::open(&cfg, &encryption)?;
    match cmd {
        DbCmd::Create { name } => {
            ids::require_valid_name("database", &name)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            match system.insert_database(&name)? {
                Some(_) => {
                    // Create and initialize the file's per-db schema.
                    init_data_db(&cfg, &encryption, &name)?;
                    println!("created database {name}");
                }
                None => bail!("database '{name}' already exists"),
            }
            Ok(())
        }
        DbCmd::List => {
            for d in system.list_databases()? {
                println!("{}\t{}", d.name, d.created_at);
            }
            Ok(())
        }
        DbCmd::Snapshot { name } => {
            if !system.database_exists(&name)? {
                bail!("database '{name}' does not exist");
            }
            std::fs::create_dir_all(&cfg.server.snapshot_dir)?;
            let dest = cfg
                .server
                .snapshot_dir
                .join(format!("{name}-{}.db", ids::new_ulid()));
            let bytes = snapshot_data_db(&cfg, &encryption, &name, &dest)?;
            println!("snapshot written to {} ({bytes} bytes)", dest.display());
            Ok(())
        }
    }
}

/// Open a data DB directly (key first + pragmas) for offline CLI operations.
fn open_data_conn(
    cfg: &Config,
    encryption: &Encryption,
    path: &std::path::Path,
) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    encryption.apply_to(&conn)?;
    db::pragmas::apply_pragmas(&conn, cfg.writer.busy_timeout_ms)?;
    Ok(conn)
}

fn init_data_db(cfg: &Config, encryption: &Encryption, name: &str) -> Result<()> {
    let path = cfg.server.data_dir.join(format!("{name}.db"));
    let conn = open_data_conn(cfg, encryption, &path)?;
    let storage = DocStorage::detect();
    db::schema::init_database(&conn, storage)?;
    Ok(())
}

fn snapshot_data_db(
    cfg: &Config,
    encryption: &Encryption,
    name: &str,
    dest: &std::path::Path,
) -> Result<u64> {
    let path = cfg.server.data_dir.join(format!("{name}.db"));
    let conn = open_data_conn(cfg, encryption, &path)?;
    let escaped = dest.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))?;
    Ok(std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0))
}

/// Offline re-encryption of every SQLite file with a fresh key (§20.6).
///
/// Steps: snapshot each DB (backup), `PRAGMA rekey` each, verify it opens with
/// the new key, and only then persist the new key. On any failure, restore from
/// the pre-rekey snapshots and abort without changing the key source.
fn cmd_rekey(cfg: &Config) -> Result<()> {
    let old = cli_encryption(cfg)?;
    if !old.is_enabled() {
        bail!("encryption is disabled; nothing to rekey");
    }

    let files = enumerate_db_files(&cfg.server.data_dir)?;
    if files.is_empty() {
        bail!("no database files found under {}", cfg.server.data_dir.display());
    }

    let new_key = encryption::generate_hex_key();
    let backup_dir = cfg.server.snapshot_dir.join(format!("prerekey-{}", ids::new_ulid()));
    std::fs::create_dir_all(&backup_dir)?;

    // 1) Back up every DB (encrypted under the OLD key) before touching anything.
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    for f in &files {
        let conn = open_data_conn(cfg, &old, f)?;
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let backup = backup_dir.join(&name);
        let escaped = backup.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))
            .with_context(|| format!("backing up {}", f.display()))?;
        backups.push((f.clone(), backup));
    }
    tracing::info!("backed up {} databases to {}", files.len(), backup_dir.display());

    // 2) Rekey each DB, verifying as we go. On failure, restore from backups.
    let result = (|| -> Result<()> {
        for f in &files {
            let conn = open_data_conn(cfg, &old, f)?;
            conn.execute_batch(&format!("PRAGMA rekey = \"x'{}'\";", new_key.as_str()))
                .with_context(|| format!("rekeying {}", f.display()))?;
            // Verify the DB opens and reads under the new key.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
                .map_err(|_| anyhow::anyhow!("verification failed after rekey of {}", f.display()))?;
        }
        Ok(())
    })();

    if let Err(e) = result {
        tracing::error!("rekey failed ({e}); restoring from backups");
        for (orig, backup) in &backups {
            // Remove any stale WAL/SHM then restore the pre-rekey copy.
            for suffix in ["-wal", "-shm"] {
                let p = PathBuf::from(format!("{}{}", orig.display(), suffix));
                let _ = std::fs::remove_file(p);
            }
            std::fs::copy(backup, orig)
                .with_context(|| format!("restoring {}", orig.display()))?;
        }
        bail!("rekey aborted and databases restored: {e}");
    }

    // 3) Persist the new key only after every DB succeeded.
    match cfg.encryption.key_source {
        config::KeySource::KeyFile => {
            encryption::write_key_file(&cfg.encryption.key_file, &new_key)?;
            println!("rekey complete; new key written to {}", cfg.encryption.key_file.display());
        }
        _ => {
            println!("rekey complete. Update your key source to the new key:");
            println!("{}", new_key.as_str());
        }
    }
    println!("Pre-rekey backups (OLD key) kept at {}", backup_dir.display());
    Ok(())
}

fn enumerate_db_files(data_dir: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !data_dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // Match `*.db` but not `*.db-wal` / `*.db-shm`.
            if name.ends_with(".db") {
                out.push(path);
            }
        }
    }
    Ok(out)
}
