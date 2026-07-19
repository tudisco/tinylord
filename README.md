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

For small, self-hosted apps that need documents, auth, and realtime updates
without giving up ownership of the server or data. Firebase is hosted;
[Appwrite](https://appwrite.io/) solves a much broader problem than a small
private app needs. [PocketBase](https://pocketbase.io/) was the clearest
inspiration — TinyLord takes an even narrower path: a small Rust binary, one
encrypted SQLite file per logical database, a schemaless document API, and a
deliberately modest browser client. Not a replacement for any of them; the
smaller, faster-to-understand choice when its capabilities are enough.

---

## Table of contents

- [Architecture in one paragraph](#architecture-in-one-paragraph)
- [Build](#build)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Static applications & deployment](#static-applications--deployment)
- [Encryption at rest](#encryption-at-rest)
- [Backups & disaster recovery (Litestream)](#backups--disaster-recovery-litestream)
- [What this is not (v1 non-goals)](#what-this-is-not-v1-non-goals)

**Developer reference → [`docs/api.md`](docs/api.md)**: authentication & roles,
the browser client, the CLI, every HTTP endpoint (with curl), the query
language, realtime SSE, pub/sub & presence, and error codes.

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
| `pubsub`      | `enabled`                | `true`                      | Enable ephemeral channel messaging and presence |
| `pubsub`      | `max_event_bytes`        | `65536`                     | Maximum serialized publish payload |
| `pubsub`      | `channel_capacity`        | `256`                       | In-memory channel buffer; lagging subscribers lose events |
| `admin_ui`    | `enabled`                | `false`                     | Serve the embedded global-admin interface |
| `admin_ui`    | `path`                   | `/0/`                       | File-only URL path for the embedded admin interface |
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
name = "myapp"
bind = "127.0.0.1:9300"
directory = "/srv/myapp/public"
spa_fallback = true
```

The static handler serves only the configured directory and rejects traversal.
`/v1/*`, `/health`, and `/openapi.json` always take precedence over static
files. With `spa_fallback = true`, unknown non-API paths serve `index.html`;
unknown API paths remain `404`.

Keep every listener on loopback. A Cloudflare Tunnel may map a hostname to its
corresponding local port, such as `http://127.0.0.1:9300`; TinyLord does not
terminate public TLS itself.

Example `systemd` unit (`/etc/systemd/system/tinylord.service`):

```ini
[Unit]
Description=tinylord
After=network-online.target

[Service]
User=tinylord
Group=tinylord
WorkingDirectory=/srv/tinylord
ExecStart=/srv/tinylord/tinylord serve --config /srv/tinylord/tinylord.toml
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

### Built-in admin UI

TinyLord includes a small, embedded [Riot.js](https://riot.js.org/) operator
page. It is disabled by default. To turn it on, add this to the server config
and restart the service:

```toml
[admin_ui]
enabled = true
```

When `path` is omitted, open `https://your-host/0/` (or the equivalent
loopback/Tunnel hostname) and
enter the global admin token. The UI keeps that token only in page memory; it
does not write it to browser storage. It can list databases and principals,
inspect grants, create databases and browser users, grant database access, and
toggle public registration. Use the JSON admin API for automation or features
outside this small operator surface.

The page and Riot runtime are compiled into the TinyLord binary. No CDN request
or external admin asset is needed at runtime. Keep `/0/` behind the same HTTPS,
network, and operator-access controls as the rest of the admin API.

To move it, set a different path in the same file and restart the service:

```toml
[admin_ui]
enabled = true
path = "/operator/"
```

The path must be a safe slash-separated path and cannot overlap `/v1/` or other
TinyLord routes. The setting is deliberately file-only.

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

## What this is not (v1 non-goals)

No web UI beyond the small embedded operator page. No built-in TLS/ACME
(terminate TLS at a reverse proxy). No blob storage. No raw SQL for clients.
No realtime delivery guarantees. No `$set`/`$inc`, aggregation, joins,
`$regex`, or nested boolean trees. No multi-node/replication/clustering.

---

## License

[MIT](LICENSE) © 2026 tudisco.biz.
