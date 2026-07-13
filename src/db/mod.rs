//! Per-database handle registry (§3): opens, caches, and drops database handles.
//!
//! A `DbHandle` bundles the single writer actor, the read-only pool, and the
//! realtime broadcast channel for one logical database — the "one component"
//! that unifies concurrency, realtime, and the single-writer constraint.

pub mod pragmas;
pub mod reader;
pub mod schema;
pub mod writer;

use crate::config::Config;
use crate::encryption::Encryption;
use crate::errors::{ApiError, ApiResult};
use crate::api::pubsub::{self, PresenceMap, PubSubEvent};
use crate::realtime::{self, ChangeEvent};
use reader::ReadPool;
use schema::DocStorage;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use writer::WriterHandle;

/// An open database: writer actor + reader pool + realtime fan-out.
pub struct DbHandle {
    /// Retained for diagnostics/logging.
    #[allow(dead_code)]
    pub name: String,
    pub writer: WriterHandle,
    pub read_pool: ReadPool,
    pub broadcast_tx: broadcast::Sender<ChangeEvent>,
    /// Ephemeral pub/sub fan-out. Independent of `broadcast_tx`: events here are
    /// never persisted and never carry a sequence number.
    pub pubsub_tx: broadcast::Sender<PubSubEvent>,
    /// Live presence roster per channel for this database.
    pub presence: std::sync::RwLock<PresenceMap>,
    checkpoint_task: tokio::task::JoinHandle<()>,
}

impl Drop for DbHandle {
    fn drop(&mut self) {
        // Stop the periodic checkpoint ticker; the writer thread exits on its
        // own once all WriterHandle clones are dropped.
        self.checkpoint_task.abort();
    }
}

/// Lazily opens and caches database handles.
pub struct DbRegistry {
    handles: Mutex<HashMap<String, Arc<DbHandle>>>,
    data_dir: PathBuf,
    encryption: Encryption,
    storage: DocStorage,
    config: Config,
}

impl DbRegistry {
    pub fn new(config: &Config, encryption: Encryption, storage: DocStorage) -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            data_dir: config.server.data_dir.clone(),
            encryption,
            storage,
            config: config.clone(),
        }
    }

    fn db_path(&self, name: &str) -> PathBuf {
        self.data_dir.join(format!("{name}.db"))
    }

    /// Initialize a brand-new database file's internal schema (called by admin
    /// create, §7.2). Opens through the writer so the single-writer invariant
    /// holds from the very first write.
    pub async fn init_new_database(&self, name: &str) -> ApiResult<()> {
        // Opening the handle runs `init_database` in the writer's open path.
        let _ = self.get_or_open(name).await?;
        Ok(())
    }

    /// Get a cached handle or open a new one.
    pub async fn get_or_open(&self, name: &str) -> ApiResult<Arc<DbHandle>> {
        let mut map = self.handles.lock().await;
        if let Some(h) = map.get(name) {
            return Ok(h.clone());
        }
        let handle = self.open_handle(name).await?;
        let arc = Arc::new(handle);
        map.insert(name.to_string(), arc.clone());
        Ok(arc)
    }

    async fn open_handle(&self, name: &str) -> ApiResult<DbHandle> {
        let path = self.db_path(name);
        let broadcast_tx = realtime::new_channel(self.config.realtime.sse_channel_capacity);
        let pubsub_tx = pubsub::new_channel(self.config.pubsub.channel_capacity);

        let writer = writer::spawn(
            &path,
            &self.encryption,
            self.storage,
            &self.config.writer,
            &self.config.realtime,
            self.config.limits.max_database_bytes,
            broadcast_tx.clone(),
        )
        .await
        .map_err(|e| ApiError::internal(format!("opening writer for {name}: {e}")))?;

        let read_pool = reader::build_pool(&path, &self.encryption, self.config.writer.busy_timeout_ms)
            .map_err(|e| ApiError::internal(format!("opening read pool for {name}: {e}")))?;

        // Periodic WAL checkpoint ticker (§5.2).
        let checkpoint_task = {
            let writer = writer.clone();
            let secs = self.config.writer.wal_checkpoint_secs.max(1);
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(secs));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    writer.try_checkpoint();
                }
            })
        };

        Ok(DbHandle {
            name: name.to_string(),
            writer,
            read_pool,
            broadcast_tx,
            pubsub_tx,
            presence: std::sync::RwLock::new(PresenceMap::new()),
            checkpoint_task,
        })
    }

    /// Close any cached handle and delete the database file plus `-wal`/`-shm`
    /// (§7.2). Irreversible.
    pub async fn drop_database(&self, name: &str) -> ApiResult<()> {
        {
            let mut map = self.handles.lock().await;
            map.remove(name); // drops the handle → writer/reader/ticker torn down
        }
        // Best-effort file removal. On unix, unlinking a still-open file is safe;
        // in-flight readers finish against the unlinked inode.
        let base = self.db_path(name);
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                base.clone()
            } else {
                PathBuf::from(format!("{}{}", base.display(), suffix))
            };
            if p.exists() {
                if let Err(e) = std::fs::remove_file(&p) {
                    tracing::warn!("failed to remove {}: {e}", p.display());
                }
            }
        }
        Ok(())
    }
}
