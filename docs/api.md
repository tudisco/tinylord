# TinyLord API reference

Developer-facing reference: the auth model, the browser client, the CLI, every
HTTP endpoint (with curl), the query language, realtime SSE, and pub/sub.
For install, configuration, deployment, encryption, and backups, see the
[README](../README.md).

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
`[auth].public_registration = true`. That configuration value is the default;
a global admin can inspect or persistently change the live policy without a
restart:

```bash
# Inspect the effective policy.
curl -s "$BASE/v1/admin/auth/registration" -H "Authorization: Bearer $ADMIN"

# Deliberately open or close public signup.
curl -s -X PUT "$BASE/v1/admin/auth/registration" \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"enabled":true}'
```

An operator can reset a browser user's password with
`POST /v1/admin/principals/password` and `{ "name", "password" }`. The reset
revokes that user's browser access and refresh sessions and returns the
principal ID for grant management.

`POST /v1/auth/login` accepts `{ "username", "password" }` and returns a
15-minute access token plus a CSRF token. It also sets a rotating, HttpOnly,
SameSite=Strict refresh cookie and a separate JS-readable, SameSite=Strict CSRF
cookie. Passwords are Argon2id hashes; access tokens, refresh sessions, and
CSRF values are stored only as SHA-256 hashes. Login
failures use a generic response and are limited by source IP and username.

Use the returned access token only in memory as `Authorization: Bearer ...`.
The browser module reads the `tinylord_csrf` cookie and sends it in
`X-CSRF-Token` for `POST /v1/auth/refresh` and `POST /v1/auth/logout`; refresh
rotates both cookies. This lets a new client instance restore the session after
a page reload without storing credentials in web storage. In
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
| `login(username, password)` | Session object | Stores the returned access token in the client instance; the server sets refresh and CSRF cookies. |
| `refresh()` | New session object | Uses the HttpOnly refresh cookie and rotates it. Call after a `401` due to access-token expiry. |
| `logout()` | `undefined` | Revokes the refresh session and clears the in-memory tokens. |
| `me()` | `{ id, name }` | Confirms the current access token. |
| `db(name).collection(name)` | Collection | Equivalent to `collection(database, collection)`. |

A session object has `{ access_token, token_type, expires_in, csrf_token }`.
Do not save it to localStorage, sessionStorage, URLs, or application records.
The client retains it only in memory, so call `refresh()` after a page reload;
the JS-readable CSRF cookie and HttpOnly refresh cookie make that safe restore
possible without web storage.

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
| `tinylord admin list-users [--name SUBSTR]` | List principals (id, name, type, status, grants); look up an id by name |
| `tinylord admin grant --user ID\|NAME --db NAME --role read\|write\|admin` | Grant a role (accepts a principal id or an exact user name) |
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

# List principals for a custom admin interface. No password or token material
# is returned; each record includes its type, status, and database grants.
curl -s $BASE/v1/admin/principals -H "Authorization: Bearer $ADMIN"
# Add ?name=ada for a case-insensitive name/username search.
# {"principals":[{"id":"01J...","name":"ada","username":"ada", \
#   "kind":"browser","is_admin":false,"disabled":false,"created_at":..., \
#   "grants":[{"database":"app","role":"write"}]}]}

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

# Reset a browser user's password and revoke its browser access/refresh tokens.
curl -s -X POST $BASE/v1/admin/principals/password \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"name":"ada","password":"new-long-password"}'

# Read or set the public-registration policy.
curl -s $BASE/v1/admin/auth/registration -H "Authorization: Bearer $ADMIN"
curl -s -X PUT $BASE/v1/admin/auth/registration \
  -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"enabled":false}'

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

## Pub/sub & presence

Pub/sub channels are for short-lived application events such as typing
indicators, cursor positions, notifications, and “who is online” state. They
are separate from document realtime: messages live only in memory, are never
written to SQLite, and disappear when the process restarts.

The feature is enabled by default. Disable it with:

```toml
[pubsub]
enabled = false
```

Channel names use the same safe-name rules as database collections. A principal
needs `write` access on the database to publish and `read` access to subscribe
or read presence. The three endpoints are:

```text
POST /v1/db/{db}/channels/{channel}/publish
GET  /v1/db/{db}/channels/{channel}/subscribe?client_id=...
GET  /v1/db/{db}/channels/{channel}/presence
```

The simplest browser usage is through the bundled client:

```js
const room = app.db("app").channel("lobby");

// Requires a database write grant. The result is { delivered }.
await room.publish({ type: "typing", user: "ada" });

// Requires a database read grant.
console.log(await room.presence());

const abort = new AbortController();
for await (const event of room.subscribe({ signal: abort.signal })) {
  if (event.type === "message") {
    console.log("message", event.data.data, "from", event.data.client_id);
  }
  if (event.type === "presence") {
    console.log(event.data.type, event.data.client_id);
  }
}
// Call abort.abort() when the page/component is disposed.
```

`TinyLord` assigns each client instance a stable random `client_id`. A channel
subscriber does not receive its own published messages or its own join/leave
presence events. The server sends `message` events with this shape:

```json
{
  "channel": "lobby",
  "client_id": "ada-browser",
  "ts": 1780000000000,
  "data": { "type": "typing", "user": "ada" }
}
```

Presence events have `event: presence` and data shaped as
`{ "type": "join" | "leave", "client_id": "...", "ts": 1780000000000 }`.
The presence endpoint returns `{ "clients": [{ "client_id", "connected_at" }] }`.

For non-JavaScript clients, publish with a bearer token:

```bash
curl -s -X POST "$BASE/v1/db/app/channels/lobby/publish" \
  -H "Authorization: Bearer $USER" -H 'content-type: application/json' \
  -d '{"client_id":"operator-console","data":{"type":"notice","text":"Hello"}}'

curl -N "$BASE/v1/db/app/channels/lobby/subscribe?client_id=operator-console" \
  -H "Authorization: Bearer $USER"
```

Delivery is best-effort. There is no persistence, changelog, sequence number,
resume, or `resync` event for pub/sub. If a subscriber falls behind the
in-memory buffer, missed events are dropped. Re-read durable application state
from the document API whenever an event is only a notification that state may
have changed.

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

