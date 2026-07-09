# tinylord

TinyLord is a small, self-hosted application server for single-page HTML apps.
Run one binary, point a hostname at it, and your app can serve its own files,
sign users in, store JSON, and receive live updates from the same origin.

It is built for the moment when a static app has outgrown localStorage but does
not need a large backend platform. TinyLord gives that app:

- A fixed, loopback static-app listener for its HTML, JavaScript, and assets.
- Username/password login and a small browser client at `/tinylord.js`.
- Per-user database grants, a schemaless JSON document API, and realtime SSE.
- Encrypted SQLite storage, backups, and a single native binary to operate.

No SQL is exposed to the browser. The encryption key and operator credentials
stay on the server. Put it behind a Cloudflare Tunnel or another HTTPS proxy,
and a single-page app gets a modest backend without a separate application
server to build and maintain.

The whole client-side shape of a tiny todo app is just this:

```js
import { TinyLord } from "/tinylord.js";

const app = TinyLord.connect();
await app.login("ada", "correct horse battery staple");

const todos = app.collection("ada", "todos");
await todos.create({ title: "Buy coffee", done: false });

const { items } = await todos.query({ filter: { done: false } });
```

The server serves the module, keeps the refresh session in an HttpOnly cookie,
and applies the authenticated user’s database grants to every call.

## Why TinyLord?

TinyLord exists for small, self-hosted applications that need a dependable
place for documents, authentication, and realtime updates without giving up
ownership of the server or data.

Firebase is excellent, but it is a hosted service: the backend, deployment
shape, and operational boundaries are not yours to own in the same direct way.
[Appwrite](https://appwrite.io/) is a capable open-source platform, but it
solves a much broader problem than a small private application needs. Its
feature set and operational footprint were more than this project wanted.

[PocketBase](https://pocketbase.io/) was the clearest inspiration. Its compact
single-binary approach, straightforward API, and focus on getting an app
working quickly are exactly the qualities TinyLord aims to preserve. TinyLord
takes a narrower path: a small Rust binary, one encrypted SQLite file per
logical database, a schemaless document API, and a deliberately modest browser
client. The goal is not to replace PocketBase, Appwrite, or Firebase; it is to
be the smaller, faster-to-understand choice when those capabilities are enough.

---

## Table of contents

- [Architecture in one paragraph](#architecture-in-one-paragraph)
- [Why TinyLord?](#why-tinylord)
- [Build](#build)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Static applications & deployment](#static-applications--deployment)
- [Encryption at rest](#encryption-at-rest)
- [Authentication & roles](#authentication--roles)
- [Browser authentication](#browser-authentication)
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
node tests/tinylord_client.mjs
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

`[[static_apps]]` is intentionally file-only. Each entry needs a unique safe
`name`, a unique loopback `bind` address, and an existing `directory`; all are
validated and canonicalized before the server starts.

## Static applications & deployment

TinyLord can serve a static application and the API from the same origin by
adding one listener per application:

```toml
[[static_apps]]
name = "delegate"
bind = "127.0.0.1:9300"
directory = "/home/tudisco/DelegateServer/public"
spa_fallback = true
```

The static handler uses the configured directory only and rejects traversal.
`/v1/*`, `/health`, and `/openapi.json` always take precedence over static
files. With `spa_fallback = true`, unknown non-API paths serve `index.html`;
unknown API paths remain `404`. Files receive normal MIME types from the static
file service. Deploy fingerprinted assets so ordinary HTTP revalidation stays
safe while browsers retain immutable asset names efficiently.

Keep every listener on loopback. A Cloudflare Tunnel may map a hostname to its
corresponding local port, such as `http://127.0.0.1:9300`; TinyLord does not
terminate public TLS itself.

Example `systemd` unit (`/etc/systemd/system/tinylord.service`):

```ini
[Unit]
Description=tinylord
After=network-online.target

[Service]
User=tudisco
Group=tudisco
WorkingDirectory=/home/tudisco/DelegateServer
ExecStart=/home/tudisco/DelegateServer/tinylord serve --config /home/tudisco/DelegateServer/tinylord.toml
Restart=on-failure
RestartSec=3
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

The service user must own `data/`, `snapshots/`, and `secrets/`; do not make
the encryption key readable by other users. Back up the key separately from
the encrypted database files—both are required for recovery.

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

## Browser authentication

Browser users are principals too, so the existing per-database grants remain
the authorization source of truth. An operator can create one through
`POST /v1/admin/principals` with `{ "name": "delegate", "password": "..." }`,
then grant that returned `id` a database role. This form does not return an
operator bearer token. Public `POST /v1/auth/register` is disabled unless
`[auth].public_registration = true`.

`POST /v1/auth/login` accepts `{ "username", "password" }` and returns a
15-minute access token plus a CSRF token. It also sets a rotating, HttpOnly,
SameSite=Strict refresh cookie. Passwords are Argon2id hashes; access tokens,
refresh sessions, and CSRF values are stored only as SHA-256 hashes. Login
failures use a generic response and are limited by source IP and username.

Use the returned access token only in memory as `Authorization: Bearer ...`.
Send the CSRF value in `X-CSRF-Token` for `POST /v1/auth/refresh` and
`POST /v1/auth/logout`; refresh rotates the session and CSRF value. In
production `secure_cookies = true` requires HTTPS (including a Cloudflare
Tunnel origin). Set it to `false` only for local HTTP development.

SSE still uses bearer authorization. Browser clients that need realtime should
use a `fetch()` streaming client with the short-lived access token; do not put
tokens in query strings.

### Browser client module

Every TinyLord listener serves a browser-native ES module at `/tinylord.js`.
It requires no bundler and keeps the access token only in the module instance:

```html
<script type="module">
  import { TinyLord } from "/tinylord.js";

  const tinylord = TinyLord.connect();
  await tinylord.login("delegate", "correct horse battery staple");

  const tasks = tinylord.collection("delegate", "tasks");
  await tasks.create({ title: "Ship it", done: false });
  const { items } = await tasks.query({ filter: { done: false } });

  for await (const event of tasks.subscribe()) {
    if (event.type === "change") console.log(event.data);
  }
</script>
```

### Client API reference

Create one client per signed-in browser session. `baseUrl` defaults to the
current origin; set it only when deliberately calling a different origin.

```js
const tinylord = TinyLord.connect({ baseUrl: "" });
```

| Call | Result | Notes |
|------|--------|-------|
| `register(username, password)` | Session object | Works only when public registration is enabled. |
| `login(username, password)` | Session object | Stores the returned access and CSRF tokens in the client instance. |
| `refresh()` | New session object | Uses the HttpOnly refresh cookie and rotates it. Call after a `401` due to access-token expiry. |
| `logout()` | `undefined` | Revokes the refresh session and clears the in-memory tokens. |
| `me()` | `{ id, name }` | Confirms the current access token. |
| `db(name).collection(name)` | Collection | Equivalent to `collection(database, collection)`. |

A session object has `{ access_token, token_type, expires_in, csrf_token }`.
Do not save it to localStorage, sessionStorage, URLs, or application records.
The client retains it only in memory, so refresh after a page reload.

Every collection method returns the server JSON envelope unchanged:

```js
const tasks = tinylord.collection("delegate", "tasks");

const created = await tasks.create({ title: "Write docs" });
const one = await tasks.get(created.id);
const changed = await tasks.put(created.id, { title: "Write clear docs" });
await tasks.delete(created.id);

const page = await tasks.query({
  filter: { done: false },
  sort: [["updated_at", "desc"]],
  limit: 25,
});
const total = await tasks.count({ done: false });
```

`subscribe()` is an async generator. Its values are
`{ type, id, data }`, where `type` is normally `change` or `resync`, `id` is
the SSE sequence number when present, and `data` is parsed JSON. Pass `filter`
for the SSE equality-filter subset, `lastEventId` to request replay from a
previous event ID, and an `AbortSignal` to stop listening:

```js
const controller = new AbortController();
let lastEventId;

try {
  for await (const event of tasks.subscribe({ lastEventId, signal: controller.signal })) {
    lastEventId = event.id ?? lastEventId;
    if (event.type === "resync") {
      // Re-query the collection; missed events cannot be replayed.
      await tasks.query({});
    } else if (event.type === "change") {
      console.log(event.data);
    }
  }
} finally {
  controller.abort();
}
```

All failed requests throw `TinyLordError`, with `status`, `code`, and `detail`
in addition to the message. A normal recovery path is: on `401`, call
`refresh()` once and retry the original request; if that refresh also fails,
clear local UI state and show the sign-in screen. Do not retry validation,
permission, or conflict errors blindly.

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
