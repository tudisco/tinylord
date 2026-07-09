# tinylord

A tiny, self-hostable, **schemaless realtime document datastore** — one static
binary backed by **one SQLite file per logical database**. It gives browser-only
apps the two things that otherwise force a custom backend:

- **Live updates** over Server-Sent Events (best-effort realtime subscriptions).
- **A place to keep JSON documents** with a Mongo-ish query API — no SQL ever
  exposed to clients, no server to provision per app.

It is deliberately smaller than PocketBase: headless (no UI), a document CRUD +
query API, SSE change streams, minimal admin, consistent-snapshot backup, and
**encryption at rest via SQLCipher, on by default**.

Storage is encrypted with AES-256 (SQLCipher). Data files, snapshots, and the
control database are all encrypted; the key is server-side only and never
transits config, logs, or the API.

---

## Table of contents

- [Architecture in one paragraph](#architecture-in-one-paragraph)
- [Build](#build)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Encryption at rest](#encryption-at-rest)
- [Authentication & roles](#authentication--roles)
- [CLI reference](#cli-reference)
- [HTTP API reference](#http-api-reference) (every endpoint, with curl)
- [Query language](#query-language)
- [Realtime (SSE)](#realtime-sse)
- [Backups & disaster recovery (Litestream)](#backups--disaster-recovery-litestream)
- [Limits & errors](#limits--errors)
- [What this is not (v1 non-goals)](#what-this-is-not-v1-non-goals)

---

## Architecture in one paragraph

Two planes share one auth core. The **control plane** is a single `_system.db`
holding databases, principals (accounts), hashed tokens, and grants. The **data
plane** is one SQLite file per logical database under `data_dir`. The spine is
the **per-database writer actor**: each open database has exactly one writer
connection on one thread, fed by a channel — so `SQLITE_BUSY` lock contention
cannot happen, and the writer (which already holds each document) is the single
place that records the change log and emits realtime events after commit. Reads
use a small pool of read-only connections that run concurrently under WAL.

---

## Build

Requires a recent stable Rust toolchain and a C compiler (SQLCipher and a
vendored OpenSSL are compiled and statically linked, so the binary is
self-contained — no system SQLite or OpenSSL needed).

```bash
cargo build --release
# binary at target/release/tinylord
```

Optional OS-keyring key source:

```bash
cargo build --release --features keyring
```

Run the tests:

```bash
cargo test
```

---

## Quick start

```bash
# 1) First run bootstraps a global-admin token and generates an encryption key.
tinylord serve --config tinylord.toml
# stdout prints the admin token ONCE — copy it:
#   tinylord bootstrap: global admin token (shown ONCE)
#   <ADMIN_TOKEN>
# and warns that it generated ./secrets/tinylord.key — back that up offline.
```

In another shell (replace `$ADMIN` with the printed token):

```bash
ADMIN=<ADMIN_TOKEN>
BASE=http://127.0.0.1:8090

# 2) Create a database.
curl -s -X POST $BASE/v1/admin/databases \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"name":"app"}'

# 3) Create a narrowly-scoped user token (this is what you ship to the browser).
curl -s -X POST $BASE/v1/admin/principals \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"name":"web"}'
# -> {"id":"01J...","token":"<USER_TOKEN>"}   (token shown once)

# 4) Grant that user write access on the database.
curl -s -X POST $BASE/v1/admin/grants \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"principal_id":"01J...","database":"app","role":"write"}'

# 5) Use the USER token for data operations.
USER=<USER_TOKEN>
curl -s -X POST $BASE/v1/db/app/collections/users/documents \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"name":"Ada","email":"ada@example.com","age":36}'
```

> **Never ship the admin token to a browser.** Only ever hand out narrowly-scoped
> user tokens (per-database `read`/`write`). The admin token is operator-only.

---

## Configuration

Config is TOML (`tinylord.toml`), with per-key environment overrides named
`TINYLORD_<SECTION>_<KEY>` (upper snake case), e.g. `TINYLORD_SERVER_BIND`.
See the fully-commented [`tinylord.toml`](tinylord.toml) in this repo. Summary:

| Section       | Key                      | Default                     | Meaning |
|---------------|--------------------------|-----------------------------|---------|
| `server`      | `bind`                   | `127.0.0.1:8090`            | Listen address (plain HTTP; put TLS on a reverse proxy) |
| `server`      | `data_dir`               | `./data`                    | Where `_system.db` and `<db>.db` live |
| `server`      | `snapshot_dir`           | `./snapshots`               | Where `VACUUM INTO` snapshots go |
| `limits`      | `max_document_bytes`     | `1048576`                   | Reject larger documents (413) |
| `limits`      | `max_database_bytes`     | `1073741824`                | Reject writes past this size (413); `0` = unlimited |
| `limits`      | `max_query_limit`        | `500`                       | Query `limit` is clamped to this |
| `limits`      | `request_body_bytes`     | `2097152`                   | Global HTTP body limit |
| `limits`      | `rate_per_minute`        | `600`                       | Per-principal rate limit; `0` = disabled |
| `writer`      | `busy_timeout_ms`        | `5000`                      | SQLite busy timeout |
| `writer`      | `group_commit`           | `true`                      | Batch queued writes into one transaction |
| `writer`      | `group_commit_max_batch` | `64`                        | Max writes per group commit |
| `writer`      | `wal_checkpoint_secs`    | `60`                        | Periodic `wal_checkpoint(TRUNCATE)` interval |
| `realtime`    | `changelog_retention`    | `10000`                     | Rows kept in `_changelog` for SSE resume |
| `realtime`    | `sse_channel_capacity`   | `256`                       | Broadcast buffer; lagging subscribers get a `resync` |
| `cors`        | `allowed_origins`        | `["http://localhost:5173"]` | Explicit allow-list; never `*` with tokens |
| `encryption`  | `enabled`                | `true`                      | Encryption at rest (SQLCipher) |
| `encryption`  | `key_source`             | `key_file`                  | `key_file` \| `env` \| `keyring` |
| `encryption`  | `key_file`               | `./secrets/tinylord.key`     | 0600 file holding the 64-hex key |

---

## Encryption at rest

All SQLite files — `_system.db` and every `<db>.db` — are encrypted with
SQLCipher (AES-256). **On by default.** The instance-wide key is a raw 32-byte
value represented as 64 hex characters.

### Provisioning the key

Priority order at startup: **keyring → key_file → env**.

- **key_file** (default): a 0600 file containing the 64-hex key. On first run,
  if the file is absent, tinylord **generates** one and writes it with 0600
  permissions, printing a one-time warning. tinylord refuses to start if the file
  is group/other-readable.
- **env**: read from `TINYLORD_ENCRYPTION_KEY` (64 hex chars).
- **keyring**: OS credential store (build with `--features keyring`).

Generate a key up front instead of relying on auto-generation:

```bash
tinylord keygen --out ./secrets/tinylord.key   # writes 0600
# or print to stdout:
tinylord keygen
```

> **Back up the key offline.** Losing it permanently and irrecoverably destroys
> all data. The key is never written to config, logs, error messages, or any API
> response.

### Rotating the key (rekey)

Offline re-encryption of `_system.db` and every data DB with a fresh key. **Stop
the server first.** tinylord snapshots every database before rekeying and, on any
failure, restores from those snapshots and aborts without changing the key.

```bash
tinylord admin rekey --config tinylord.toml
# writes the new key to key_file (or prints it for env/keyring sources)
# pre-rekey backups (under the OLD key) are kept under snapshot_dir/prerekey-*
```

### Running unencrypted (not recommended)

Set `[encryption] enabled = false` **and** pass `--allow-unencrypted`:

```bash
tinylord serve --allow-unencrypted
```

### JSONB vs text-JSON

SQLite 3.45+ ships JSONB (compact binary JSON). tinylord detects the effective
`sqlite_version()` at startup: ≥ 3.45 uses JSONB storage; otherwise it falls
back to text-JSON storage automatically. Behavior is identical either way; the
active mode is logged at startup.

---

## Authentication & roles

- **Bearer tokens**: `Authorization: Bearer <token>`. Tokens are 256-bit CSPRNG
  values (base64url). Only the SHA-256 hash is stored; the plaintext is shown
  **once** at creation.
- **Global admin** (`is_admin=1`) may call all `/v1/admin/*` endpoints, but is
  **not** implicitly granted data access.
- **Data access** requires a grant on `(principal, database)`. Roles are ordered
  `read < write < admin`:
  - `read` — GET / query / count / subscribe
  - `write` — adds create / replace / delete
  - `admin` — adds index management and snapshot for that database

---

## CLI reference

All admin/db subcommands operate directly on `_system.db`; the server need not be
running. They all accept `--config <path>` (default `tinylord.toml`).

| Command | Purpose |
|---------|---------|
| `tinylord serve [--allow-unencrypted]` | Run the server; bootstrap admin token on first run |
| `tinylord keygen [--out <path>]` | Generate a 32-byte key (0600 file, or stdout) |
| `tinylord admin reset-token` | Rotate the global admin token (prints once) |
| `tinylord admin create-user --name NAME [--admin]` | Create a principal offline (prints token once) |
| `tinylord admin grant --user ID --db NAME --role read\|write\|admin` | Grant a role |
| `tinylord admin rekey` | Offline re-encryption with a fresh key |
| `tinylord db create NAME` | Create a database |
| `tinylord db list` | List databases |
| `tinylord db snapshot NAME` | Write a `VACUUM INTO` snapshot |

---

## HTTP API reference

All responses are JSON (except SSE). Errors use the envelope:

```json
{ "error": { "code": "not_found", "message": "human readable", "detail": null } }
```

Version prefix is `/v1`. Below, `$ADMIN` is a global-admin token and `$USER` is a
scoped user token.

### Health & spec (no auth)

```bash
curl -s $BASE/health
# {"status":"ok"}

curl -s $BASE/openapi.json      # OpenAPI 3.1 document
```

### Admin (global-admin token)

```bash
# Create a database (409 if it exists).
curl -s -X POST $BASE/v1/admin/databases \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"name":"app"}'
# {"name":"app","created_at":1719000000000}

# List databases.
curl -s $BASE/v1/admin/databases -H "Authorization: Bearer $ADMIN"
# {"databases":[{"name":"app","created_at":...}]}

# Delete a database (irreversible: removes the file and -wal/-shm).
curl -s -X DELETE $BASE/v1/admin/databases/app -H "Authorization: Bearer $ADMIN"
# 204

# Create a principal (token shown once).
curl -s -X POST $BASE/v1/admin/principals \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"name":"web","is_admin":false}'
# {"id":"01J...","token":"..."}

# Disable a principal.
curl -s -X DELETE $BASE/v1/admin/principals/01J... -H "Authorization: Bearer $ADMIN"
# 204

# Upsert a grant.
curl -s -X POST $BASE/v1/admin/grants \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"principal_id":"01J...","database":"app","role":"write"}'

# Remove a grant.
curl -s -X DELETE $BASE/v1/admin/grants \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"principal_id":"01J...","database":"app"}'
# 204

# Consistent snapshot (global admin OR per-db admin). Encrypted under the same key.
curl -s -X POST $BASE/v1/admin/databases/app/snapshot -H "Authorization: Bearer $ADMIN"
# {"path":"./snapshots/app-01J....db","bytes":28672}
```

### Documents (grant on the database)

A document is any JSON object. IDs are server-minted ULIDs; clients never supply
them on create. Responses use the envelope:

```json
{ "id": "01J...", "created_at": 1719000000000, "updated_at": 1719000000000,
  "doc": { "...user fields..." } }
```

```bash
# Create (requires write). Returns 201 + envelope.
curl -s -X POST $BASE/v1/db/app/collections/users/documents \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"name":"Ada","email":"ada@example.com","age":36}'

# Get by id (requires read). 404 if absent.
curl -s $BASE/v1/db/app/collections/users/documents/01J... \
  -H "Authorization: Bearer $USER"

# Replace / upsert (requires write). Creates if absent (201), else replaces
# wholesale (200), preserving created_at and bumping updated_at.
curl -s -X PUT $BASE/v1/db/app/collections/users/documents/01J... \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"name":"Ada Lovelace","email":"ada@example.com","age":37}'

# Delete (requires write). 204, or 404 if absent.
curl -s -X DELETE $BASE/v1/db/app/collections/users/documents/01J... \
  -H "Authorization: Bearer $USER"
```

### Query & count (grant read)

```bash
# Query (see "Query language" below).
curl -s -X POST $BASE/v1/db/app/collections/users/query \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"filter":{"age":{"$gte":18}},"sort":[["created_at","desc"]],"limit":50}'
# {"items":[<envelope>,...],"next_cursor":"01J..."|null}

# Count.
curl -s -X POST $BASE/v1/db/app/collections/users/count \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"filter":{"age":{"$gte":18}}}'
# {"count":42}
```

### Indexes (grant admin)

```bash
# Create an expression index over a JSON path (409 if a unique index would
# violate existing data). The server mints the index name.
curl -s -X POST $BASE/v1/db/app/collections/users/indexes \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"path":"$.email","unique":true}'
# {"name":"ix_users_email_ab12cd","collection":"users","path":"$.email","unique":true}

# List indexes.
curl -s $BASE/v1/db/app/collections/users/indexes -H "Authorization: Bearer $USER"

# Drop an index by name.
curl -s -X DELETE $BASE/v1/db/app/collections/users/indexes/ix_users_email_ab12cd \
  -H "Authorization: Bearer $USER"
# 204
```

### Realtime (grant read)

```bash
# Subscribe to changes for a collection (SSE). Optional URL-encoded `filter`.
curl -N $BASE/v1/db/app/collections/users/subscribe \
  -H "Authorization: Bearer $USER"
```

See [Realtime (SSE)](#realtime-sse).

---

## Query language

Deliberately minimal (no `$regex`, no aggregation, no joins, no nested boolean
trees). Request body:

```json
{
  "filter":     { "age": { "$gte": 18 }, "status": "active" },
  "sort":       [ ["created_at","desc"], ["name","asc"] ],
  "limit":      50,
  "offset":     0,
  "cursor":     null,
  "projection": ["name","email"]
}
```

**Filter semantics**

- Top-level keys are field paths in dot notation (`"user.age"` → `$.user.age`).
  Multiple keys are **AND**ed.
- A scalar value means equality: `{"status":"active"}`.
- An object value is an operator map. Supported operators only:
  `$eq $ne $gt $gte $lt $lte $in`. Example: `{"age":{"$gte":18}}`,
  `{"role":{"$in":["a","b"]}}`.
- One optional top-level `$or`: an array of flat AND-maps, ANDed with the
  remaining top-level fields. **No nesting beyond this** — nested `$and`/`$or`
  or `$or` inside a field is a `400`.
- Everything compiles to parameterized SQL over `json_extract`; values are never
  interpolated.

**Sort / pagination / projection**

- `sort`: `[path, "asc"|"desc"]` pairs. Default sort is `id ASC` (ULID →
  chronological). `id`, `created_at`, `updated_at` sort by the metadata columns;
  any other path sorts by `json_extract(doc, path)`.
- Pagination: `limit` + `offset` always works (limit is clamped to
  `max_query_limit`). For the default/`id`-sorted case, an opaque **cursor** is
  also supported: pass `cursor` = the last `id` you saw; `next_cursor` is
  returned when more rows may exist, else `null`. Deep offset pagination with a
  custom sort is O(n) for far pages.
- `projection`: optional list of field paths; only those fields are returned
  inside `doc` (plus the envelope metadata).

---

## Realtime (SSE)

Transport is Server-Sent Events. Each change is:

```
id: 42
event: change
data: {"seq":42,"collection":"users","op":"insert","id":"01J...","doc":{...}|null}
```

- `op` is `insert` | `update` | `delete`. `doc` is the new state for
  insert/update, `null` for delete.
- `seq` is the per-database change sequence (monotonic, gap-free at write time).
- **Resume**: send `Last-Event-ID: <seq>` on connect to replay retained
  `_changelog` rows after `<seq>`, then attach live. If `<seq>` predates the
  oldest retained row, the server sends a single `event: resync` (re-read current
  state) and attaches live.
- **Best-effort**: a subscriber that lags past `sse_channel_capacity` receives
  `event: resync` and continues. There are no delivery guarantees and no
  persisted per-subscriber cursors — this is intentional.
- **Filtering**: an optional equality-only `filter` query param (URL-encoded
  JSON; scalars + `$in`) limits which changes you receive. Deletes are always
  forwarded.
- Fan-out is in-memory per database (single process only).

Browser example:

```js
const url = `https://your-host/v1/db/app/collections/users/subscribe`;
// EventSource can't set headers; use a query token via a reverse proxy, or a
// fetch-based SSE client that sets Authorization: Bearer <USER_TOKEN>.
const es = new EventSource(url, { withCredentials: false });
es.addEventListener("change", (e) => console.log(JSON.parse(e.data)));
es.addEventListener("resync", () => { /* re-fetch current state */ });
```

---

## Backups & disaster recovery (Litestream)

### In-process snapshots

`VACUUM INTO` produces a clean single-file copy. When encryption is on, the
snapshot is itself **encrypted under the same key**, so it is safe at rest and
re-opens with the instance key. Restore = copy the file back.

```bash
# Via the API (global admin or per-db admin):
curl -s -X POST $BASE/v1/admin/databases/app/snapshot -H "Authorization: Bearer $ADMIN"
# Or via the CLI (server may be stopped):
tinylord db snapshot app
```

### Continuous DR with Litestream (external sidecar)

Continuous replication is delegated to [Litestream](https://litestream.io),
which runs as a **separate process** replicating your SQLite files to
S3-compatible storage with point-in-time restore. It requires **zero code in
tinylord**.

> **S3 here is a backup destination for the operator, not a user-facing feature.**

**Important caveat — Litestream + SQLCipher is unverified.** Stock Litestream is
not SQLCipher-aware and may be unable to read/replicate an **encrypted** database
file. Do not assume this DR path works when encryption is enabled. Treat
"Litestream + SQLCipher" as unverified, not supported-by-default.

**Recommended DR when encryption is ON:** ship the encrypted `VACUUM INTO`
snapshots to object storage on a schedule (a simple file copy). For example, a
cron job:

```bash
# every 15 minutes: snapshot each database and sync the encrypted files to S3
*/15 * * * * tinylord db snapshot app && aws s3 sync ./snapshots s3://my-bucket/tinylord/
```

**If you run unencrypted** (not recommended), a stock Litestream sidecar works
directly against the data files. Example `litestream.yml`:

```yaml
dbs:
  - path: ./data/app.db
    replicas:
      - type: s3
        bucket: my-bucket
        path: tinylord/app
        region: us-east-1
```

```bash
litestream replicate -config litestream.yml
# restore:
litestream restore -config litestream.yml ./data/app.db
```

---

## Limits & errors

| HTTP | `code` | When |
|------|--------|------|
| 400 | `bad_request` / `validation` | malformed body, invalid name/path/filter |
| 401 | `unauthorized` | missing/invalid/disabled token |
| 403 | `forbidden` | insufficient role / not admin |
| 404 | `not_found` | database, document, or index absent |
| 409 | `conflict` | name collision, unique-index violation |
| 413 | `payload_too_large` | document or database size limit |
| 429 | `rate_limited` | per-principal rate limit (includes `Retry-After`) |
| 500 | `internal` | unexpected server error (never leaks SQL/paths) |

Collection and database names must match `^[a-zA-Z][a-zA-Z0-9_]{0,63}$`.

---

## What this is not (v1 non-goals)

No web/admin UI. No built-in TLS/ACME (terminate TLS at a reverse proxy). No
blob storage. No raw SQL for clients. No realtime delivery guarantees. No
`$set`/`$inc`, aggregation, joins, `$regex`, or nested boolean trees. No
multi-node/replication/clustering. The **API proxy** (a pinned-route gateway for
secret-gated third-party APIs) is a documented fast-follow module — the auth,
token, rate-limit, and logging layers are built so it can be added without
rework, but it ships no code in v1.

---

## License

[MIT](LICENSE) © 2026 tudisco.biz.

You're free to use, modify, and redistribute this software, including
commercially — the only condition is that you keep the copyright and license
notice (see [`LICENSE`](LICENSE)) in copies or substantial portions.
