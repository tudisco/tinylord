# BUILD SPEC: `tinylord` — a tiny schemaless realtime datastore

You are implementing a complete, production-quality service from this spec. Follow it literally; where it says "must," it's a hard requirement. Prefer boring, correct code over clever code. Do not add features not listed here. When a detail is unspecified, choose the simplest option consistent with the stated goals and note the choice in a code comment.

## 1. What this is

`tinylord` is a single, headless (no UI), self-hostable HTTP service written in Rust. It is a multi-tenant, **schemaless document datastore** backed by **one SQLite file per logical database**, exposing:

1. A small **document CRUD + query API** (Mongo-ish filters, no SQL exposed to clients).
2. **Best-effort realtime change subscriptions** over Server-Sent Events (SSE).
3. Minimal **admin** operations (create databases, principals, grants) driven by API + a CLI for first-run bootstrap.
4. **Consistent-snapshot backup** via an endpoint; continuous DR is delegated to an external Litestream sidecar (documented, not embedded).

The design goal is to be **smaller and lighter than PocketBase** — a single static binary that idles in single-digit MB — while giving browser-only apps the two things that otherwise force a custom backend: live updates and a place to keep data without provisioning a server per app.

### Non-goals (do NOT build these in v1)
- No web/admin UI of any kind. CLI + API only.
- No built-in TLS/ACME. Assume a reverse proxy terminates TLS. Bind to a plain HTTP socket.
- No file/blob storage, no S3-as-a-feature.
- No raw SQL exposed to clients, ever.
- No realtime delivery guarantees (best-effort only; see §9).
- No `$set`/`$inc`, no aggregation, no joins, no `$regex`, no nested boolean trees (see §8).
- No multi-node/replication/clustering. Single process per database file.
- The API **proxy** is a separate fast-follow module (§16). Build the auth core so it can be added without rework, but do NOT implement it in v1.

## 2. Tech stack (use these crates; pin to current stable releases)

- Async runtime: `tokio` (multi-thread).
- HTTP: `axum` + `tower` + `tower-http` (body-limit, cors, trace layers). Use axum's built-in SSE support.
- SQLite: `rusqlite` with features `["bundled", "hooks"]`. `bundled` compiles a recent SQLite (3.45+), which gives **JSONB** — rely on it. `hooks` gives `update_hook`.
- Reader pool: `r2d2` + `r2d2_sqlite`.
- IDs: `ulid`.
- Serialization: `serde`, `serde_json`.
- Config: `serde` + `toml`, with env-var overrides.
- CLI: `clap` (derive).
- Logging: `tracing` + `tracing-subscriber` (JSON output option).
- Rate limiting: `governor`.
- Random tokens: `rand` (CSPRNG). Hash tokens with `sha2` (SHA-256).
- OpenAPI: hand-write `openapi.json` served at `/openapi.json`, OR generate with `utoipa` if convenient. An accurate spec is required either way.

## 3. Architecture

Two planes, one shared auth core:

- **Control plane** — a single `_system.db` SQLite file holding the registry of databases, principals (accounts), hashed tokens, and grants. All auth decisions read from here.
- **Data plane** — one SQLite file per logical database at `<data_dir>/<db_name>.db`. Tenant documents live here.

The spine of the whole service is the **per-database writer actor**. This is the single most important design element:

- Each open database has exactly **one dedicated writer connection**, owned by one OS thread, fed by a bounded `mpsc` channel. All writes for that database go through it. This eliminates SQLite `SQLITE_BUSY` lock contention by construction (single writer) AND is what makes realtime possible (see §5).
- Reads use a small **r2d2 pool of read-only connections** per database; under WAL mode these run concurrently with the writer.
- The writer actor also owns the `update_hook`, assigns the per-database change sequence number, and emits realtime events after commit.

Concurrency management, realtime, and the single-writer constraint are **one component**, not three.

## 4. Data model

### 4.1 `_system.db` schema

```sql
CREATE TABLE databases (
  name        TEXT PRIMARY KEY,
  created_at  INTEGER NOT NULL   -- unix ms
);

CREATE TABLE principals (
  id          TEXT PRIMARY KEY,  -- ULID
  name        TEXT NOT NULL,
  is_admin    INTEGER NOT NULL DEFAULT 0,  -- global admin (bootstrap)
  token_hash  TEXT NOT NULL,     -- hex SHA-256 of bearer token
  disabled    INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX ux_principals_token ON principals(token_hash);

CREATE TABLE grants (
  principal_id  TEXT NOT NULL,
  database_name TEXT NOT NULL,
  role          TEXT NOT NULL,   -- 'read' | 'write' | 'admin'
  PRIMARY KEY (principal_id, database_name)
);
```

### 4.2 Per-database schema
Each collection is a table `coll_<name>` created on first write to that collection. **Must be a rowid table** (do not declare `WITHOUT ROWID`) — the `update_hook` does not fire for WITHOUT ROWID tables.

```sql
CREATE TABLE coll_<name> (
  rowid       INTEGER PRIMARY KEY,       -- internal; drives update_hook
  id          TEXT NOT NULL UNIQUE,      -- public ULID
  doc         BLOB NOT NULL,             -- JSONB (via jsonb(?))
  created_at  INTEGER NOT NULL,          -- unix ms
  updated_at  INTEGER NOT NULL,
  CHECK (json_valid(doc))
);
```

Collection-name validation: `^[a-zA-Z][a-zA-Z0-9_]{0,63}$`. Reject anything else. Never string-interpolate a collection name into SQL without passing this validation first.

Also per database:

```sql
-- change log; its PRIMARY KEY IS the per-db sequence number.
CREATE TABLE _changelog (
  seq         INTEGER PRIMARY KEY,       -- monotonic, = rowid
  collection  TEXT NOT NULL,
  op          TEXT NOT NULL,             -- 'insert' | 'update' | 'delete'
  doc_id      TEXT NOT NULL,
  doc         BLOB,                      -- new JSONB state; NULL for delete
  ts          INTEGER NOT NULL
);

-- registry of user-declared indexes, for listing/management.
CREATE TABLE _indexes (
  name        TEXT PRIMARY KEY,
  collection  TEXT NOT NULL,
  path        TEXT NOT NULL,             -- JSON path, e.g. '$.email'
  is_unique   INTEGER NOT NULL
);
```

### 4.3 Documents, IDs, timestamps
- IDs are **server-generated ULIDs** (sortable, chronological). Clients never supply IDs on create.
- Store documents as **JSONB**: `INSERT ... VALUES (jsonb(?1), ...)` where `?1` is the JSON text. Return documents to clients as JSON text via `json(doc)`.
- `created_at`/`updated_at` are set server-side (unix ms). On create both equal now; on replace, `created_at` is preserved and `updated_at` is set to now. The API returns them as top-level metadata fields, not inside `doc` (see §7.4).

## 5. Storage engine details (critical correctness section)

### 5.1 Pragmas — set on EVERY connection at open

```
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA busy_timeout = <config busy_timeout_ms>;
PRAGMA foreign_keys = ON;
PRAGMA temp_store   = MEMORY;
PRAGMA cache_size   = -8000;   -- ~8MB
PRAGMA mmap_size    = 134217728;
```

`synchronous=NORMAL` under WAL can lose the last transaction(s) on power loss but never corrupts. This is acceptable for this service and is covered by the backup layer. Document this in a comment.

### 5.2 Writer actor
- One thread + one write connection per open database. Requests arrive over a bounded `mpsc`; each carries a oneshot reply channel.
- **Group commit (config-gated, default on):** when more than one write is queued, drain up to `group_commit_max_batch` and wrap them in a single transaction to amortize the fsync. When only one write is queued, commit it immediately — never delay a lone write waiting for companions.
- Periodic checkpoint: run `PRAGMA wal_checkpoint(TRUNCATE)` on an interval (or tune `wal_autocheckpoint`) so the WAL doesn't grow unbounded.

### 5.3 update_hook wiring — obey these caveats exactly
The `update_hook` is **per-connection and in-process only**. It fires only for writes on the connection it's registered on. Registering it on the single writer connection is therefore sufficient to observe every write — provided nothing else ever writes the file. **Requirement: nothing outside the writer actor may open the DB file for writing.**

The hook hands you only `(op, table, rowid)` — NOT the row contents. Do **not** re-query by rowid to fetch the payload (there is a real read-after-commit visibility race). Instead the writer, which already has the document in hand, records the change.

The hook does NOT fire for: `WITHOUT ROWID` tables; `DELETE FROM t` without a `WHERE` (truncate optimization); rows deleted by `ON CONFLICT REPLACE`. Therefore:
- Use rowid tables (already required).
- Always delete with an explicit `WHERE id = ?`. Never `DELETE FROM coll_x`.
- Never use `INSERT OR REPLACE`. Implement upsert as explicit `INSERT ... ON CONFLICT(id) DO UPDATE SET ...` **without** REPLACE semantics, or a select-then-insert/update inside the txn.

### 5.4 Event emission (transaction-safe)
1. During a write transaction, the writer builds the intended change event(s) in memory (op, collection, doc_id, new doc or none).
2. In the **same transaction**, the writer inserts the corresponding row(s) into `_changelog`. The `_changelog.seq` (its rowid PK) becomes the authoritative sequence number.
3. On **commit success**, the writer broadcasts those events (with their seq) to the in-memory fan-out for that database. On rollback, discard them — never broadcast uncommitted changes.
4. After broadcasting, trim `_changelog` to the configured retention (keep last N rows).

Fan-out per database is a `tokio::sync::broadcast` channel. Subscribers that lag past the channel capacity are handled per §9.

## 6. Auth

- Bearer tokens: `Authorization: Bearer <token>`. Tokens are 256-bit CSPRNG values, base64url-encoded. Only their **SHA-256 hash** is stored; the plaintext is shown to the operator exactly once at creation and never again.
- Request auth: hash the presented token, look up the principal, reject if missing/disabled, then authorize:
  - **Global admin** (`is_admin=1`): may call all `/v1/admin/*` endpoints. Not implicitly granted data access.
  - **Data access**: requires a `grants` row for `(principal, database)` with sufficient role. Role ordering: `read` < `write` < `admin`. `read` allows GET/query/count/subscribe; `write` adds create/replace/delete; `admin` adds index management and snapshot for that database.
- Implement as an axum extractor/middleware that resolves the principal once and attaches it to request extensions; endpoint handlers assert the required role.
- The admin token must never be usable from the browser by design; document that operators should only ever ship narrowly-scoped user tokens to frontends.

### Bootstrap
On `serve`, if `_system.db` has zero principals, generate one global-admin token, **print it to stdout once**, store its hash, and continue. Provide `tinylord admin reset-token` to rotate it.

## 7. HTTP API

All responses are JSON. All errors use the envelope in §13. Version prefix `/v1`. Enforce `request_body_bytes` limit globally via `tower-http`.

### 7.1 Health
- `GET /health` → `200 {"status":"ok"}` (no auth).
- `GET /openapi.json` → the OpenAPI document (no auth).

### 7.2 Admin (global-admin token required)
- `POST /v1/admin/databases` `{ "name": "..." }` → creates `<name>.db`, initializes per-db schema. `409` if exists. Name validation same rules as collections.
- `GET  /v1/admin/databases` → `{ "databases": [{"name","created_at"}] }`
- `DELETE /v1/admin/databases/{db}` → closes handles, deletes the file (and `-wal`/`-shm`). Irreversible; require exact name match.
- `POST /v1/admin/principals` `{ "name": "...", "is_admin": false }` → `{ "id", "token" }` (token shown once).
- `DELETE /v1/admin/principals/{id}` → disables (soft) or deletes.
- `POST /v1/admin/grants` `{ "principal_id","database","role" }` → upserts grant.
- `DELETE /v1/admin/grants` `{ "principal_id","database" }` → removes grant.
- `POST /v1/admin/databases/{db}/snapshot` → runs `VACUUM INTO '<snapshot_path>'`; returns `{ "path", "bytes" }`. (Global admin OR per-db `admin`.)

### 7.3 Documents (grant on `{db}` required)
- `POST   /v1/db/{db}/collections/{coll}/documents` — body is an arbitrary JSON object. Server mints ULID. Returns the created document (§7.4). Requires `write`.
- `GET    /v1/db/{db}/collections/{coll}/documents/{id}` — returns the document or `404`. Requires `read`.
- `PUT    /v1/db/{db}/collections/{coll}/documents/{id}` — **full replace / upsert**: create if absent, else replace `doc` wholesale, preserving `created_at`, updating `updated_at`. Requires `write`.
- `DELETE /v1/db/{db}/collections/{coll}/documents/{id}` — deletes (explicit WHERE). `404` if absent. Requires `write`.

### 7.4 Document envelope (responses)

```json
{ "id": "01J...", "created_at": 1719000000000, "updated_at": 1719000000000,
  "doc": { ...user fields... } }
```

### 7.5 Query & count (grant `read`)
- `POST /v1/db/{db}/collections/{coll}/query` — body per §8. Returns `{ "items": [<envelope>...], "next_cursor": "..."|null }`.
- `POST /v1/db/{db}/collections/{coll}/count` — body `{ "filter": {...} }` → `{ "count": N }`.

### 7.6 Indexes (grant `admin`)
- `POST   /v1/db/{db}/collections/{coll}/indexes` `{ "path":"$.email", "unique":true }` → creates an expression index and registers it. On unique-violation during creation, return `409`.
- `GET    /v1/db/{db}/collections/{coll}/indexes` → list from `_indexes`.
- `DELETE /v1/db/{db}/collections/{coll}/indexes/{name}` → drop + unregister.

### 7.7 Realtime (grant `read`)
- `GET /v1/db/{db}/collections/{coll}/subscribe` — SSE stream. Optional query param `filter` (URL-encoded JSON, equality-only subset) to receive only matching changes. Honors `Last-Event-ID` for resume (§9). Content-Type `text/event-stream`.

## 8. Query language (v1 — deliberately minimal)

Request body:

```json
{
  "filter":     { ... },
  "sort":       [ ["created_at","desc"], ["name","asc"] ],
  "limit":      50,
  "offset":     0,
  "cursor":     null,
  "projection": ["name","email"]
}
```

### Filter semantics
- Top-level keys are **field paths** in dot notation, mapped to JSON paths: `"user.age"` → `$.user.age`. Multiple keys are **AND**ed.
- A field value that is a scalar means equality: `{"status":"active"}` → `json_extract(doc,'$.status') = ?`.
- A field value that is an object is an operator map. Supported operators ONLY: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`. Example: `{"age":{"$gte":18}}`, `{"role":{"$in":["a","b"]}}`.
- One optional top-level `"$or"`: an array of sub-filters (each a flat AND-map as above). The `$or` group is ANDed with the remaining top-level fields. **No nesting beyond this** — reject nested `$and`/`$or`, `$or` inside a field, etc., with `400`.
- Compile to parameterized SQL (`json_extract(doc, ?) <op> ?`); never interpolate values. Validate every path against `^\$(\.[a-zA-Z0-9_]+|\[[0-9]+\])+$` or the plain `$`-prefixed dotted form you build from client keys.

### Sort / pagination / projection
- `sort`: array of `[path, "asc"|"desc"]`. Default sort: `id ASC` (ULID → chronological).
- Pagination: **`limit` + `offset` is required and always works** (clamp `limit` to `max_query_limit`). Additionally support an opaque **`cursor`** for the default/`id`-sorted case: cursor = last returned `id`; the next page is `WHERE id > ?`. Return `next_cursor` when a cursor-compatible query has more rows; else `null`. For custom-sort deep pagination, offset is acceptable (document that it's O(n) for deep pages).
- `projection`: optional array of field paths; when present, return only those fields inside `doc` (plus the envelope metadata). Implement by extracting listed paths; omit if you prefer, but it's cheap.

## 9. Realtime spec (best-effort)

- Transport: **SSE**. Each event:

```
  id: <seq>
  event: change
  data: {"seq":N,"collection":"...","op":"insert|update|delete","id":"...","doc":{...}|null}
```

  `doc` is the new state for insert/update, `null` for delete.
- Sequence: the per-database `_changelog.seq`. Monotonic, gap-free at write time.
- **Resume:** on connect, if `Last-Event-ID: <seq>` is present, replay `_changelog` rows with `seq > last` (respecting the optional filter), then attach to the live broadcast. If `<last>` is older than the oldest retained changelog row, do NOT try to replay — send a single `event: resync` telling the client to re-read current state, then attach live.
- **Best-effort delivery:** if a subscriber lags past the broadcast channel capacity (`RecvError::Lagged`), do not attempt perfect recovery — send `event: resync` and continue. No persisted per-subscriber cursors. No delivery guarantee. This is intentional.
- Filtering: the optional `filter` is the equality-only subset (scalars + `$in`); evaluate each event against it before sending. Enter/leave semantics are NOT implemented in v1 (a doc that stops matching simply stops producing events for that subscriber; deletes are always forwarded).
- Fan-out is in-memory per database; single process only.

### 9.1 Pub/sub & presence (ephemeral)

A second, independent fan-out sits alongside the document change stream for
transient signalling — cursors, typing indicators, "user is online" — that must
never touch disk.

- **Endpoints** (channel name validated like a collection name; grant as noted):
  - `POST /v1/db/{db}/channels/{channel}/publish` (grant `write`) — body
    `{"client_id": "...", "data": <arbitrary JSON>}`. Broadcasts
    `{channel, client_id, ts, data}` to current subscribers; returns
    `{"delivered": <count>}`. The `data` payload is size-checked against
    `[pubsub] max_event_bytes` (`413` if exceeded).
  - `GET /v1/db/{db}/channels/{channel}/subscribe?client_id=...` (grant `read`)
    — SSE. Emits `event: message` for published events and `event: presence`
    (`{"type":"join"|"leave", client_id, ts}`) for roster transitions.
  - `GET /v1/db/{db}/channels/{channel}/presence` (grant `read`) — returns
    `{"clients": [{"client_id", "connected_at"}]}`.
- **Ephemeral, best-effort.** Publishing bypasses the writer actor entirely:
  nothing is persisted, there is no changelog, no sequence number, no
  `Last-Event-ID` resume, and no `resync`. A subscriber that lags past the
  broadcast buffer silently drops the missed events and continues.
- **Sender exclusion.** Every event carries its originating `client_id`; a
  subscriber never receives events bearing its own `client_id`. This covers both
  published messages and presence, so a client sees others' joins/leaves but not
  its own.
- **Presence per channel.** Tracked in memory per database as
  `channel -> (client_id -> {connected_at, count})`. A subscribe connection
  registers on connect and deregisters on disconnect; a `join` is broadcast on a
  client's first connection to a channel and a `leave` when its last connection
  closes (the same `client_id` may hold several connections).
- **Disable switch.** With `[pubsub] enabled = false` the three endpoints report
  `not_found`, hiding the feature. A config without a `[pubsub]` section keeps
  the defaults (enabled, 64 KiB events, 256-event buffer).

## 10. Backup

- **Snapshot endpoint** (§7.2): `VACUUM INTO` produces a clean single-file copy. Expose it; that's the whole in-process backup story.
- **Continuous DR** is external and optional: document a Litestream sidecar in the README. Litestream runs as a separate process, replicates the `<data_dir>/*.db` files to S3-compatible storage with point-in-time restore, and requires zero code in `tinylord`. Note explicitly: "S3 here is a backup destination for the operator, not a user-facing feature." Do not embed it.

## 11. Guardrails & limits

- Reject documents larger than `max_document_bytes` (`413`).
- Enforce `max_database_bytes`: before a write that would grow the DB past the cap, reject with `413`. (Check page_count × page_size.)
- Global request body limit `request_body_bytes` via `tower-http`.
- Clamp query `limit` to `max_query_limit`.
- **Rate limiting** (`governor`): per-principal request limits, configurable. Structure it so per-route limits can be added later (needed by the proxy module). `429` on exceed with a `Retry-After` header.

## 12. Config (`tinylord.toml`, env overrides as `TINYLORD_<SECTION>_<KEY>`)

```toml
[server]
bind      = "127.0.0.1:8090"
data_dir  = "./data"
snapshot_dir = "./snapshots"

[limits]
max_document_bytes = 1048576
max_database_bytes = 1073741824
max_query_limit    = 500
request_body_bytes = 2097152
rate_per_minute    = 600

[writer]
busy_timeout_ms        = 5000
group_commit           = true
group_commit_max_batch = 64
wal_checkpoint_secs    = 60

[realtime]
changelog_retention  = 10000
sse_channel_capacity = 256

[cors]
allowed_origins = ["http://localhost:5173"]   # never "*" when tokens are used
```

## 13. Error format

```json
{ "error": { "code": "not_found", "message": "human readable", "detail": null } }
```

Code → HTTP mapping: `bad_request`/`validation`→400, `unauthorized`→401, `forbidden`→403, `not_found`→404, `conflict`→409 (unique violation, name collision), `payload_too_large`→413, `rate_limited`→429, `internal`→500. Never leak SQL or internal paths in `message`.

## 14. CLI (`clap`)
- `tinylord serve [--config tinylord.toml]` — run the server; bootstrap admin token on first run.
- `tinylord admin reset-token` — rotate the global admin token.
- `tinylord admin create-user --name NAME [--admin]` — offline principal creation (prints token once).
- `tinylord admin grant --user ID --db NAME --role read|write|admin`
- `tinylord db create NAME` / `tinylord db list` / `tinylord db snapshot NAME`
(CLI admin commands operate directly on `_system.db`; the server need not be running.)

## 15. Security requirements (must)
- Never expose raw SQL to clients; only the §8 compiler runs, always parameterized.
- Never string-interpolate client-supplied collection names, paths, or values into SQL without the specified validation.
- Store only token hashes; show plaintext once.
- CORS from config allow-list; never `*` when Authorization is in play.
- The admin token is server/operator-only.
- (Forward-looking, for §16) design the grant model so a future `proxy` grant type slots in without schema churn.

## 16. Fast-follow modules (design for, do NOT build in v1)

### 16.1 API proxy (pinned-route gateway)
Purpose: let browser-only apps reach secret-gated third-party APIs (payments, LLMs, MLS/real-estate feeds, senders) without exposing keys and without standing up a backend. **Not** a generic `url=`-param proxy — that is SSRF and secret-leakage by design and is explicitly forbidden.
Shape when built:
- Operator declares named routes in config: `name`, fixed `upstream` base URL, allowed methods, allowed path prefix, and where to inject the secret (usually a header). Secrets come from env/secret store, never from the client.
- Client calls `POST /v1/proxy/{name}/{suffix}` with a normal scoped user token; a `proxy`-scoped grant authorizes the route. The service forwards to the fixed upstream with the caller's suffix/query/body, **strips the client Authorization header**, injects the upstream secret, applies the per-route rate limit, logs `(principal, route, ts)`, streams the response back.
- Backstop: validate each configured upstream resolves to a public IP (block loopback, link-local, RFC1918, cloud metadata) to guard against misconfig/DNS rebinding.
Build the v1 auth, token, rate-limit, and logging layers so this module only needs a config section, a grant type, and a handler.

### 16.2 Other deferred items
- `$set` / `$inc` partial updates + optimistic concurrency via a `_rev` field / `If-Match` (→ `409` on conflict).
- Live-queries with enter/leave semantics over the existing SSE stream.
- Optional per-collection JSON Schema validation (default off).
- An MCP server wrapper exposing the admin + data API as tools for agents.

## 17. Testing
- Unit: the §8 filter→SQL compiler (all operators, `$or`, rejection of nested/invalid forms, path validation).
- Integration: full CRUD + auth/role enforcement; unique-index conflict → 409; document/db size limits → 413.
- Realtime: subscribe, write, receive event with correct seq; `Last-Event-ID` replay; forced lag → `resync`.
- Correctness guards: a test asserting deletes use explicit WHERE and that `INSERT OR REPLACE` is never used (so update_hook events are never dropped); a concurrency test hammering one DB from many clients to prove writes serialize without `SQLITE_BUSY` surfacing to callers.

## 18. Build order (milestones)
- **M1** Config + `_system.db` + auth core + CLI bootstrap + create/list/drop database + `/health`.
- **M2** Writer actor + reader pool + pragmas + document CRUD by id (ULID, timestamps, JSONB).
- **M3** Query compiler (filter/sort/limit/offset/cursor/count) + on-demand indexes.
- **M4** Realtime: `_changelog`, update_hook wiring (with all §5.3 caveats), broadcast, SSE, resume/resync.
- **M5** Snapshot endpoint + limits + rate limiting + error polish + `openapi.json` + README (incl. Litestream sidecar).

## 19. Suggested layout

```
src/
  main.rs            # clap CLI, config, serve
  config.rs
  system.rs          # _system.db: databases, principals, grants
  auth.rs            # token hashing, extractor, role checks
  ids.rs             # ULID
  errors.rs          # error envelope + IntoResponse
  limits.rs          # size/rate guards
  db/
    mod.rs           # per-db handle registry (open/close/drop)
    pragmas.rs
    schema.rs        # collection/_changelog/_indexes DDL
    writer.rs        # writer actor, group commit, update_hook, changelog, emit
    reader.rs        # r2d2 read pool
  query.rs           # filter -> parameterized SQL
  realtime.rs        # broadcast registry + SSE handler + resume
  api/
    mod.rs           # router assembly
    admin.rs
    documents.rs
    query_ep.rs
    indexes.rs
  proxy.rs           # fast-follow; leave as documented stub
```

Deliver a compiling, tested Rust project matching this spec. Include a README covering config, bootstrap, every endpoint with a curl example, and the Litestream sidecar setup.

## 20. Encryption at rest (SQLCipher) — DEFAULT ON

The datastore uses **SQLCipher** (Zetetic's fork of SQLite adding transparent AES-256 encryption) for all SQLite files — both `_system.db` (which holds token hashes and grants) and every per-database data file. Encryption is **on by default**; running unencrypted requires an explicit opt-out flag. The key is server-side only and is never exposed to API clients, never logged, and never stored in plaintext config.

### 20.1 Build changes (amends §2)
- Replace the rusqlite feature set. SQLCipher **overrides** the plain `bundled` feature; you cannot use both. Use:

```toml
  rusqlite = { version = "<current>", features = ["bundled-sqlcipher-vendored-openssl", "hooks"] }
```

  `bundled-sqlcipher-vendored-openssl` auto-enables `bundled-sqlcipher` and statically links a vendored OpenSSL, so the binary stays self-contained with no system crypto dependency. `hooks` is still required for `update_hook`.
- Add crates: `zeroize` (zero key material in memory), and optionally `keyring` (OS credential store) if implementing the keyring key source.
- **JSONB caveat:** the bundled SQLCipher may track an older SQLite base version than plain `bundled`. At build/startup, verify the effective SQLite base is ≥ 3.45.0 (query `sqlite_version()`); if it is, use `jsonb(?)` / JSONB storage as specified. If it is **not** ≥ 3.45, fall back to **text JSON** storage (`json(?)` into a `TEXT` column, `json_extract` unchanged) — everything else in the spec works identically. Decide this once at startup and log which mode is active.

### 20.2 Key material
- The encryption key is a **raw 32-byte key**, represented as 64 hex chars. Prefer raw keys over passphrases (no per-open PBKDF2 cost, unambiguous).
- **One instance-wide key** encrypts all files in v1 (`_system.db` and all data DBs). Per-database keys are a future option; do not implement now.
- Key sources, resolved in this priority order at process start:
  1. OS keyring via `keyring` crate (if `key_source = "keyring"`), else
  2. a keyfile path (`key_file`) containing the 64-hex-char key, which must have `0600` permissions (refuse to start if perms are looser), else
  3. env var `TINYLORD_ENCRYPTION_KEY` (64 hex chars).
- The key **must never** appear in `tinylord.toml`, logs, error messages, tracing spans, or any API response. Hold it in a `Zeroizing<[u8;32]>` / `Zeroizing<String>` so it is wiped on drop.

### 20.3 Applying the key (correctness-critical)
- SQLCipher derives/prepares the key **just-in-time before the first database operation**. Therefore, on EVERY connection open — the per-database writer connection AND every reader-pool connection AND `_system.db` connections — the FIRST statement executed must be:

```sql
  PRAGMA key = "x'<64-hex-key>'";
```

  before ANY other SQL (before the §5.1 pragmas, before schema, before reads). Then immediately verify by running `SELECT count(*) FROM sqlite_master;` — if it errors with "file is not a database", the key is wrong; fail fast with a clear (non-key-leaking) error.
- Order within connection setup: `PRAGMA key` → verify → then `journal_mode=WAL` and the rest of §5.1.
- Never interpolate the key via string formatting into logs. Bind/format it only into the PRAGMA statement string, which is executed and then dropped.

### 20.4 Default-on behavior & bootstrap (amends §6 bootstrap)
- On first `serve`: if no key is resolvable from any source AND `key_file` is configured, **generate** a random 32-byte key, write it to `key_file` with `0600` perms, and log a loud one-time warning: *"Generated encryption key at <path>. BACK THIS UP OFFLINE — losing this key permanently destroys all data. It is not recoverable."* Then proceed encrypted.
- If encryption is explicitly disabled (`--allow-unencrypted` flag or `[encryption] enabled = false`), skip all `PRAGMA key` calls; SQLCipher with no key behaves exactly like standard SQLite. Log a clear warning that data is stored unencrypted.
- Refuse to start in the ambiguous case (encryption enabled, key source configured, but key missing/unreadable) rather than silently creating a new unopenable-vs-existing mismatch.

### 20.5 Config additions (amends §12)

```toml
[encryption]
enabled    = true                # default; false stores plaintext (requires --allow-unencrypted too)
key_source = "key_file"          # "key_file" | "env" | "keyring"
key_file   = "./secrets/tinylord.key"   # 0600; contains 64 hex chars
# key itself is NEVER placed in this file
```

### 20.6 CLI additions (amends §14)
- `tinylord keygen [--out <path>]` — generate a cryptographically random 32-byte key as 64 hex chars; write to `--out` (0600) or print to stdout once. Used to provision the key before first run.
- `tinylord admin rekey` — offline re-encryption via `PRAGMA rekey = "x'<new>'"` (SQLite `sqlite3_rekey`) applied to `_system.db` and every data DB. Requirements: server must be stopped; take a snapshot of every DB first (§7.2 / §10) before rekeying; verify each DB opens with the new key afterward; on any failure, restore from the pre-rekey snapshot and abort. Update the key source only after all DBs succeed.

### 20.7 Backup interaction (amends §10 — important)
- `VACUUM INTO` executed on a keyed SQLCipher connection produces an **encrypted** snapshot under the same key. Snapshots therefore remain protected at rest — good. Restore = copy the file back; it opens with the same instance key.
- **Litestream caveat:** the stock Litestream binary is not SQLCipher-aware and may be unable to read/replicate an encrypted database file. Do not assume the external-sidecar DR path works when encryption is enabled. In the README, document this explicitly and recommend, when encryption is on, shipping the encrypted `VACUUM INTO` snapshots to object storage via a simple scheduled file copy as the DR mechanism, unless/until a SQLCipher-capable replication path is verified. Treat "Litestream + SQLCipher" as unverified, not supported-by-default.

### 20.8 Testing additions (amends §17)
- Open an encrypted DB with the correct key → succeeds; with a wrong key → "file is not a database" surfaced as a clean internal error (never leaking the key).
- Confirm `PRAGMA key` is issued as the first statement on writer, reader-pool, and `_system.db` connections (assert ordering).
- Confirm the key never appears in log output (scan captured logs in a test).
- `VACUUM INTO` snapshot of an encrypted DB is itself non-plaintext (header is not the `"SQLite format 3\0"` magic) and re-opens with the instance key.
- `rekey` round-trip: write data → rekey → old key fails, new key opens and returns the data.
- Startup: keyfile with loose permissions (e.g. 0644) → refuse to start.

### 20.9 Security notes
- Encryption at rest protects the file if disk/backup media is stolen or a snapshot leaks. It does NOT protect data in use, nor replace the auth model (§6) — a valid token still reads plaintext via the API. Both layers are needed.
- The whole point: the operator provisions the key once (`keygen`), stores it in the OS keyring or a 0600 keyfile / secret store, and the service opens every database internally without the key ever transiting config, logs, or the API. That is the "open without exposing the password" property.
