//! Per-database writer actor (§3, §5.2–5.4).
//!
//! Each open database has exactly ONE writer connection owned by ONE OS thread,
//! fed by a bounded channel. Every write for that database goes through here, so
//! `SQLITE_BUSY` lock contention cannot occur by construction, and the writer —
//! which already holds each document in hand — is the single place that records
//! `_changelog` rows and emits realtime events after commit.
//!
//! ## Group commit (§5.2)
//! When multiple writes are queued, up to `group_commit_max_batch` are wrapped
//! in one transaction to amortize commit overhead. Each op runs inside its own
//! SAVEPOINT so a per-op failure (e.g. a unique conflict) rolls back only that
//! op — the rest of the batch still commits. A lone write is committed
//! immediately, never delayed waiting for companions.
//!
//! ## update_hook (§5.3)
//! The hook is registered on this connection to honor the single-writer
//! invariant and act as a tripwire that writes are observable. Event *payloads*
//! are NOT taken from the hook (there is a read-after-commit race); the writer
//! records changes explicitly.

use crate::config::{RealtimeConfig, WriterConfig};
use crate::db::schema::{self, DocStorage};
use crate::encryption::Encryption;
use crate::errors::{map_sqlite_err, ApiError, ApiResult};
use crate::realtime::{ChangeEvent, Op};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};

/// A document as returned to clients (the §7.4 envelope, minus JSON framing).
#[derive(Debug, Clone)]
pub struct DocRecord {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub doc: serde_json::Value,
}

/// Commands accepted by the writer actor. Each carries a oneshot reply channel.
pub enum WriteCommand {
    Create {
        collection: String,
        doc: serde_json::Value,
        reply: oneshot::Sender<ApiResult<DocRecord>>,
    },
    /// Full replace / upsert. Reply carries (record, created?) where `created`
    /// is true when the document did not previously exist.
    Replace {
        collection: String,
        id: String,
        doc: serde_json::Value,
        reply: oneshot::Sender<ApiResult<(DocRecord, bool)>>,
    },
    Delete {
        collection: String,
        id: String,
        reply: oneshot::Sender<ApiResult<bool>>,
    },
    CreateIndex {
        collection: String,
        name: String,
        path: String,
        unique: bool,
        reply: oneshot::Sender<ApiResult<()>>,
    },
    DropIndex {
        name: String,
        reply: oneshot::Sender<ApiResult<bool>>,
    },
    /// Consistent snapshot via `VACUUM INTO` (§7.2, §10). Reply carries bytes.
    Snapshot {
        dest: PathBuf,
        reply: oneshot::Sender<ApiResult<u64>>,
    },
    /// Periodic WAL checkpoint (§5.2). Fired by the registry's ticker.
    Checkpoint,
}

/// Cloneable handle used by request handlers to submit writes.
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriteCommand>,
}

impl WriterHandle {
    async fn dispatch<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<ApiResult<T>>) -> WriteCommand,
    ) -> ApiResult<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| ApiError::internal("writer channel closed"))?;
        rx.await
            .map_err(|_| ApiError::internal("writer dropped reply"))?
    }

    pub async fn create_document(
        &self,
        collection: String,
        doc: serde_json::Value,
    ) -> ApiResult<DocRecord> {
        self.dispatch(|reply| WriteCommand::Create {
            collection,
            doc,
            reply,
        })
        .await
    }

    pub async fn replace_document(
        &self,
        collection: String,
        id: String,
        doc: serde_json::Value,
    ) -> ApiResult<(DocRecord, bool)> {
        self.dispatch(|reply| WriteCommand::Replace {
            collection,
            id,
            doc,
            reply,
        })
        .await
    }

    pub async fn delete_document(&self, collection: String, id: String) -> ApiResult<bool> {
        self.dispatch(|reply| WriteCommand::Delete {
            collection,
            id,
            reply,
        })
        .await
    }

    pub async fn create_index(
        &self,
        collection: String,
        name: String,
        path: String,
        unique: bool,
    ) -> ApiResult<()> {
        self.dispatch(|reply| WriteCommand::CreateIndex {
            collection,
            name,
            path,
            unique,
            reply,
        })
        .await
    }

    pub async fn drop_index(&self, name: String) -> ApiResult<bool> {
        self.dispatch(|reply| WriteCommand::DropIndex { name, reply })
            .await
    }

    pub async fn snapshot(&self, dest: PathBuf) -> ApiResult<u64> {
        self.dispatch(|reply| WriteCommand::Snapshot { dest, reply })
            .await
    }

    /// Best-effort checkpoint trigger (used by the registry ticker).
    pub fn try_checkpoint(&self) {
        let _ = self.tx.try_send(WriteCommand::Checkpoint);
    }
}

/// Context the writer loop needs, bundled so the thread body stays readable.
struct Ctx {
    storage: DocStorage,
    writer_cfg: WriterConfig,
    realtime_cfg: RealtimeConfig,
    max_database_bytes: u64,
    broadcast_tx: broadcast::Sender<ChangeEvent>,
}

/// Spawn the writer actor for a database. Opens the connection on the dedicated
/// thread (Connection is not Sync) and reports open failures — notably a wrong
/// encryption key — back to the caller before returning the handle.
pub async fn spawn(
    path: &Path,
    encryption: &Encryption,
    storage: DocStorage,
    writer_cfg: &WriterConfig,
    realtime_cfg: &RealtimeConfig,
    max_database_bytes: u64,
    broadcast_tx: broadcast::Sender<ChangeEvent>,
) -> anyhow::Result<WriterHandle> {
    let (tx, rx) = mpsc::channel::<WriteCommand>(1024);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

    let path = path.to_path_buf();
    let encryption = encryption.clone();
    let ctx = Ctx {
        storage,
        writer_cfg: writer_cfg.clone(),
        realtime_cfg: realtime_cfg.clone(),
        max_database_bytes,
        broadcast_tx,
    };

    std::thread::Builder::new()
        .name(format!("tinylord-writer:{}", path.display()))
        .spawn(move || {
            let conn = match open_writer_conn(&path, &encryption, &ctx.writer_cfg) {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            run_loop(conn, rx, ctx);
        })?;

    match ready_rx.await {
        Ok(Ok(())) => Ok(WriterHandle { tx }),
        Ok(Err(msg)) => Err(anyhow::anyhow!(msg)),
        Err(_) => Err(anyhow::anyhow!("writer thread died during startup")),
    }
}

/// Open and configure the single writer connection: key first, pragmas, WAL
/// autocheckpoint, internal schema, and the update_hook.
fn open_writer_conn(
    path: &Path,
    encryption: &Encryption,
    writer_cfg: &WriterConfig,
) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    // KEY FIRST (§20.3) — verifies the key opens the database.
    encryption.apply_to(&conn)?;
    crate::db::pragmas::apply_pragmas(&conn, writer_cfg.busy_timeout_ms)?;
    // Bound WAL growth automatically; the registry ticker also runs TRUNCATE
    // checkpoints periodically (§5.2).
    conn.pragma_update(None, "wal_autocheckpoint", 1000)?;

    let storage = DocStorage::detect();
    schema::init_database(&conn, storage)?;

    // Register the update_hook on the sole writer connection. Payloads are not
    // read from it (§5.3); it is a tripwire proving writes are observable.
    let counter = Arc::new(AtomicU64::new(0));
    let c = counter.clone();
    conn.update_hook(Some(move |_action, _db: &str, table: &str, rowid: i64| {
        c.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(table, rowid, "update_hook fired");
    }));

    Ok(conn)
}

/// The writer's main loop. Runs on the dedicated OS thread.
fn run_loop(mut conn: Connection, mut rx: mpsc::Receiver<WriteCommand>, ctx: Ctx) {
    let mut pending: Option<WriteCommand> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => match rx.blocking_recv() {
                Some(c) => c,
                None => break, // all handles dropped → shut down
            },
        };

        match cmd {
            WriteCommand::Checkpoint => {
                if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
                    tracing::warn!("wal_checkpoint failed: {e}");
                }
            }
            WriteCommand::Snapshot { dest, reply } => {
                let _ = reply.send(do_snapshot(&conn, &dest));
            }
            WriteCommand::CreateIndex {
                collection,
                name,
                path,
                unique,
                reply,
            } => {
                let _ = reply.send(do_create_index(&conn, ctx.storage, &collection, &name, &path, unique));
            }
            WriteCommand::DropIndex { name, reply } => {
                let _ = reply.send(do_drop_index(&conn, &name));
            }
            // Remaining variants are the batchable data ops.
            batchable => {
                let mut batch = vec![batchable];
                if ctx.writer_cfg.group_commit {
                    while batch.len() < ctx.writer_cfg.group_commit_max_batch {
                        match rx.try_recv() {
                            Ok(cmd) if is_batchable(&cmd) => batch.push(cmd),
                            Ok(other) => {
                                // Non-batchable command: process it next iteration.
                                pending = Some(other);
                                break;
                            }
                            Err(_) => break, // empty or disconnected
                        }
                    }
                }
                process_batch(&mut conn, batch, &ctx);
            }
        }
    }
}

fn is_batchable(cmd: &WriteCommand) -> bool {
    matches!(
        cmd,
        WriteCommand::Create { .. } | WriteCommand::Replace { .. } | WriteCommand::Delete { .. }
    )
}

/// A resolved per-op action, deferred until the batch's outer transaction
/// resolves. `finish(committed)` sends the op's reply and returns the event to
/// broadcast (only present on a committed success).
type Finish = Box<dyn FnOnce(bool) -> Option<ChangeEvent>>;

/// Execute a batch of data ops in one transaction with per-op savepoints, then
/// commit, broadcast committed events, and trim the changelog (§5.4).
fn process_batch(conn: &mut Connection, batch: Vec<WriteCommand>, ctx: &Ctx) {
    // Begin the outer transaction. If this fails, fail every op with internal.
    if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
        let err = || ApiError::internal(format!("begin failed: {e}"));
        for cmd in batch {
            reply_with_error(cmd, err());
        }
        return;
    }

    let mut finishes: Vec<Finish> = Vec::with_capacity(batch.len());
    for cmd in batch {
        // Each op is isolated in a savepoint so its failure doesn't poison the
        // batch. Savepoint name is reused because it is released each iteration.
        if conn.execute_batch("SAVEPOINT sp").is_err() {
            reply_with_error(cmd, ApiError::internal("savepoint failed"));
            continue;
        }
        let outcome = apply_op(conn, ctx, cmd);
        match outcome {
            OpResult::Success(finish) => {
                let _ = conn.execute_batch("RELEASE sp");
                finishes.push(finish);
            }
            OpResult::Failed(finish) => {
                let _ = conn.execute_batch("ROLLBACK TO sp; RELEASE sp");
                finishes.push(finish);
            }
        }
    }

    let committed = conn.execute_batch("COMMIT").is_ok();
    if !committed {
        // COMMIT failed: everything rolled back. Best-effort explicit rollback.
        let _ = conn.execute_batch("ROLLBACK");
    }

    // Resolve replies and collect committed events.
    let mut events = Vec::new();
    for finish in finishes {
        if let Some(ev) = finish(committed) {
            events.push(ev);
        }
    }

    if committed && !events.is_empty() {
        for ev in &events {
            // Best-effort broadcast; no receivers is not an error (§9).
            let _ = ctx.broadcast_tx.send(ev.clone());
        }
        trim_changelog(conn, ctx.realtime_cfg.changelog_retention);
    }
}

enum OpResult {
    Success(Finish),
    Failed(Finish),
}

/// Apply a single data op inside the current savepoint, returning a deferred
/// finisher. Never sends the reply itself.
fn apply_op(conn: &Connection, ctx: &Ctx, cmd: WriteCommand) -> OpResult {
    match cmd {
        WriteCommand::Create {
            collection,
            doc,
            reply,
        } => match do_create(conn, ctx, &collection, doc) {
            Ok((rec, event)) => OpResult::Success(Box::new(move |committed| {
                if committed {
                    let _ = reply.send(Ok(rec));
                    Some(event)
                } else {
                    let _ = reply.send(Err(ApiError::internal("write rolled back")));
                    None
                }
            })),
            Err(e) => OpResult::Failed(Box::new(move |_| {
                let _ = reply.send(Err(e));
                None
            })),
        },
        WriteCommand::Replace {
            collection,
            id,
            doc,
            reply,
        } => match do_replace(conn, ctx, &collection, &id, doc) {
            Ok((rec, created, event)) => OpResult::Success(Box::new(move |committed| {
                if committed {
                    let _ = reply.send(Ok((rec, created)));
                    Some(event)
                } else {
                    let _ = reply.send(Err(ApiError::internal("write rolled back")));
                    None
                }
            })),
            Err(e) => OpResult::Failed(Box::new(move |_| {
                let _ = reply.send(Err(e));
                None
            })),
        },
        WriteCommand::Delete {
            collection,
            id,
            reply,
        } => match do_delete(conn, ctx, &collection, &id) {
            Ok(Some(event)) => OpResult::Success(Box::new(move |committed| {
                if committed {
                    let _ = reply.send(Ok(true));
                    Some(event)
                } else {
                    let _ = reply.send(Err(ApiError::internal("write rolled back")));
                    None
                }
            })),
            // Not found: nothing changed. Treated as a "success" path (no event),
            // reply is Ok(false) which handlers translate to 404.
            Ok(None) => OpResult::Success(Box::new(move |_| {
                let _ = reply.send(Ok(false));
                None
            })),
            Err(e) => OpResult::Failed(Box::new(move |_| {
                let _ = reply.send(Err(e));
                None
            })),
        },
        // Non-data ops never reach here.
        _ => unreachable!("apply_op received a non-data command"),
    }
}

fn reply_with_error(cmd: WriteCommand, err: ApiError) {
    match cmd {
        WriteCommand::Create { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WriteCommand::Replace { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WriteCommand::Delete { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        _ => {}
    }
}

// ---- Individual operations -------------------------------------------------

fn do_create(
    conn: &Connection,
    ctx: &Ctx,
    collection: &str,
    doc: serde_json::Value,
) -> ApiResult<(DocRecord, ChangeEvent)> {
    check_db_size(conn, ctx.max_database_bytes)?;
    schema::ensure_collection(conn, collection, ctx.storage).map_err(map_sqlite_err)?;

    let id = crate::ids::new_ulid();
    let now = crate::ids::now_ms();
    let doc_text = serde_json::to_string(&doc).map_err(ApiError::internal)?;
    let store = ctx.storage.store_fn();

    conn.execute(
        &format!(
            "INSERT INTO coll_{collection} (id, doc, created_at, updated_at) \
             VALUES (?1, {store}(?2), ?3, ?3)"
        ),
        params![id, doc_text, now],
    )
    .map_err(map_sqlite_err)?;

    let seq = insert_changelog(conn, ctx.storage, collection, Op::Insert, &id, Some(&doc_text), now)?;
    let rec = DocRecord {
        id: id.clone(),
        created_at: now,
        updated_at: now,
        doc: doc.clone(),
    };
    let event = ChangeEvent {
        seq,
        collection: collection.to_string(),
        op: Op::Insert,
        id,
        doc: Some(doc),
    };
    Ok((rec, event))
}

fn do_replace(
    conn: &Connection,
    ctx: &Ctx,
    collection: &str,
    id: &str,
    doc: serde_json::Value,
) -> ApiResult<(DocRecord, bool, ChangeEvent)> {
    check_db_size(conn, ctx.max_database_bytes)?;
    schema::ensure_collection(conn, collection, ctx.storage).map_err(map_sqlite_err)?;

    let now = crate::ids::now_ms();
    let doc_text = serde_json::to_string(&doc).map_err(ApiError::internal)?;
    let store = ctx.storage.store_fn();

    // Look up existing created_at to decide insert-vs-update and preserve it.
    let existing: Option<i64> = conn
        .query_row(
            &format!("SELECT created_at FROM coll_{collection} WHERE id = ?1"),
            params![id],
            |r| r.get(0),
        )
        .ok();

    let (created, created_at, op) = match existing {
        Some(created_at) => {
            // Full replace of the doc; created_at preserved, updated_at bumped.
            conn.execute(
                &format!(
                    "UPDATE coll_{collection} SET doc = {store}(?1), updated_at = ?2 WHERE id = ?3"
                ),
                params![doc_text, now, id],
            )
            .map_err(map_sqlite_err)?;
            (false, created_at, Op::Update)
        }
        None => {
            conn.execute(
                &format!(
                    "INSERT INTO coll_{collection} (id, doc, created_at, updated_at) \
                     VALUES (?1, {store}(?2), ?3, ?3)"
                ),
                params![id, doc_text, now],
            )
            .map_err(map_sqlite_err)?;
            (true, now, Op::Insert)
        }
    };

    let seq = insert_changelog(conn, ctx.storage, collection, op, id, Some(&doc_text), now)?;
    let rec = DocRecord {
        id: id.to_string(),
        created_at,
        updated_at: now,
        doc: doc.clone(),
    };
    let event = ChangeEvent {
        seq,
        collection: collection.to_string(),
        op,
        id: id.to_string(),
        doc: Some(doc),
    };
    Ok((rec, created, event))
}

fn do_delete(
    conn: &Connection,
    ctx: &Ctx,
    collection: &str,
    id: &str,
) -> ApiResult<Option<ChangeEvent>> {
    // The collection may not exist yet; that is simply "not found".
    if !schema::collection_exists(conn, collection).map_err(map_sqlite_err)? {
        return Ok(None);
    }
    let now = crate::ids::now_ms();
    // ALWAYS delete with an explicit WHERE so the update_hook fires (§5.3).
    let changed = conn
        .execute(
            &format!("DELETE FROM coll_{collection} WHERE id = ?1"),
            params![id],
        )
        .map_err(map_sqlite_err)?;
    if changed == 0 {
        return Ok(None);
    }
    let seq = insert_changelog(conn, ctx.storage, collection, Op::Delete, id, None, now)?;
    Ok(Some(ChangeEvent {
        seq,
        collection: collection.to_string(),
        op: Op::Delete,
        id: id.to_string(),
        doc: None,
    }))
}

/// Insert a changelog row and return its seq (the row's PRIMARY KEY / rowid).
fn insert_changelog(
    conn: &Connection,
    storage: DocStorage,
    collection: &str,
    op: Op,
    doc_id: &str,
    doc_text: Option<&str>,
    ts: i64,
) -> ApiResult<i64> {
    let store = storage.store_fn();
    match doc_text {
        Some(t) => conn.execute(
            &format!(
                "INSERT INTO _changelog (collection, op, doc_id, doc, ts) \
                 VALUES (?1, ?2, ?3, {store}(?4), ?5)"
            ),
            params![collection, op.as_str(), doc_id, t, ts],
        ),
        None => conn.execute(
            "INSERT INTO _changelog (collection, op, doc_id, doc, ts) \
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![collection, op.as_str(), doc_id, ts],
        ),
    }
    .map_err(map_sqlite_err)?;
    Ok(conn.last_insert_rowid())
}

/// Trim `_changelog` to the last `retention` rows (§5.4). Uses an explicit
/// WHERE (never a bare `DELETE FROM`) so the truncate optimization can't skip
/// the hook.
fn trim_changelog(conn: &Connection, retention: i64) {
    if retention <= 0 {
        return;
    }
    let res = conn.execute(
        "DELETE FROM _changelog WHERE seq <= \
         (SELECT COALESCE(MAX(seq), 0) FROM _changelog) - ?1",
        params![retention],
    );
    if let Err(e) = res {
        tracing::warn!("changelog trim failed: {e}");
    }
}

/// Approximate DB-size guard (§11). We reject once the current on-disk size has
/// reached the cap; exact pre-write prediction of growth is not attempted.
fn check_db_size(conn: &Connection, max_bytes: u64) -> ApiResult<()> {
    if max_bytes == 0 {
        return Ok(()); // 0 = unlimited
    }
    let page_count = pragma_i64(conn, "page_count")?;
    let page_size = pragma_i64(conn, "page_size")?;
    let size = (page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64);
    if size >= max_bytes {
        return Err(ApiError::payload_too_large("database size limit reached"));
    }
    Ok(())
}

/// Read an integer-valued pragma robustly. SQLCipher returns some pragmas (e.g.
/// `page_size`) as a TEXT column named `cipher_page_size`, so accept either an
/// integer or a numeric string rather than assuming the column type.
fn pragma_i64(conn: &Connection, name: &str) -> ApiResult<i64> {
    conn.query_row(&format!("PRAGMA {name}"), [], |r| {
        use rusqlite::types::ValueRef;
        Ok(match r.get_ref(0)? {
            ValueRef::Integer(i) => i,
            ValueRef::Real(f) => f as i64,
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(0),
            _ => 0,
        })
    })
    .map_err(map_sqlite_err)
}

fn do_create_index(
    conn: &Connection,
    storage: DocStorage,
    collection: &str,
    name: &str,
    path: &str,
    unique: bool,
) -> ApiResult<()> {
    // Names and path are validated by the handler before dispatch; assert here.
    debug_assert!(crate::ids::valid_name(collection));
    debug_assert!(crate::ids::valid_name(name));
    debug_assert!(crate::ids::valid_json_path(path));

    schema::ensure_collection(conn, collection, storage).map_err(map_sqlite_err)?;

    let uniq = if unique { "UNIQUE " } else { "" };
    // `path` is validated to the JSON-path grammar, so embedding it as a string
    // literal is safe. Build a partial expression index over json_extract.
    let sql = format!(
        "CREATE {uniq}INDEX {name} ON coll_{collection} (json_extract(doc, '{path}'))"
    );
    // A UNIQUE index over existing duplicate data fails with a constraint error
    // → surfaced as 409 (§7.6).
    conn.execute_batch(&sql).map_err(|e| {
        if let rusqlite::Error::SqliteFailure(err, _) = &e {
            if err.code == rusqlite::ffi::ErrorCode::ConstraintViolation {
                return ApiError::conflict("index would violate uniqueness on existing data");
            }
        }
        map_sqlite_err(e)
    })?;

    conn.execute(
        "INSERT INTO _indexes (name, collection, path, is_unique) VALUES (?1, ?2, ?3, ?4)",
        params![name, collection, path, unique as i64],
    )
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn do_drop_index(conn: &Connection, name: &str) -> ApiResult<bool> {
    debug_assert!(crate::ids::valid_name(name));
    let existed: bool = conn
        .query_row(
            "SELECT count(*) FROM _indexes WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .map_err(map_sqlite_err)?
        > 0;
    if !existed {
        return Ok(false);
    }
    conn.execute_batch(&format!("DROP INDEX IF EXISTS {name}"))
        .map_err(map_sqlite_err)?;
    conn.execute("DELETE FROM _indexes WHERE name = ?1", params![name])
        .map_err(map_sqlite_err)?;
    Ok(true)
}

/// Snapshot via `VACUUM INTO`. On a keyed connection the output is itself
/// encrypted under the same key (§20.7). Returns the snapshot size in bytes.
fn do_snapshot(conn: &Connection, dest: &Path) -> ApiResult<u64> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(ApiError::internal)?;
    }
    // VACUUM INTO takes a string literal; escape single quotes. The path is
    // server-constructed (snapshot_dir + validated db name), not raw client input.
    let escaped = dest.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))
        .map_err(map_sqlite_err)?;
    let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    Ok(bytes)
}
