export class TinyLordError extends Error {
  constructor(message, { status, code, detail } = {}) {
    super(message);
    this.name = "TinyLordError";
    this.status = status;
    this.code = code;
    this.detail = detail;
  }
}

export class TinyLord {
  constructor({ baseUrl = "", fetch: fetchImpl = globalThis.fetch, readCookie = browserCookie, clientId } = {}) {
    if (typeof fetchImpl !== "function") throw new TypeError("fetch is required");
    this.baseUrl = baseUrl.replace(/\/$/, "");
    // Browser fetch is a Web API method and must keep the Window receiver.
    this.fetch = (input, init) => fetchImpl.call(globalThis, input, init);
    this.readCookie = readCookie;
    this.accessToken = null;
    this.csrfToken = null;
    // Stable per-instance identifier reused for channel publish/subscribe so the
    // server can exclude a client's own events. Overridable for deterministic ids.
    this.clientId = clientId || newClientId();
  }

  static connect(options) { return new TinyLord(options); }

  async register(username, password) { return this.#authenticate("register", { username, password }); }
  async login(username, password) { return this.#authenticate("login", { username, password }); }

  async refresh() {
    return this.#saveSession(await this._request("/v1/auth/refresh", {
      method: "POST", headers: this._headers({ csrf: true }), credentials: "same-origin",
    }));
  }

  async logout() {
    await this._request("/v1/auth/logout", { method: "POST", headers: this._headers({ csrf: true }), credentials: "same-origin" });
    this.accessToken = null;
    this.csrfToken = null;
  }

  async me() { return this._request("/v1/auth/me", { headers: this._headers() }); }
  db(name) { return new Database(this, name); }
  collection(database, collection) { return this.db(database).collection(collection); }

  async #authenticate(action, body) {
    return this.#saveSession(await this._request(`/v1/auth/${action}`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), credentials: "same-origin",
    }));
  }

  #saveSession(session) {
    this.accessToken = session.access_token;
    this.csrfToken = session.csrf_token;
    return session;
  }

  _headers({ csrf = false, extra = {} } = {}) {
    const headers = new Headers(extra);
    if (this.accessToken) headers.set("authorization", `Bearer ${this.accessToken}`);
    const csrfToken = this.readCookie("tinylord_csrf") || this.csrfToken;
    if (csrf && csrfToken) headers.set("x-csrf-token", csrfToken);
    return headers;
  }

  async _request(path, options = {}) {
    const response = await this.fetch(`${this.baseUrl}${path}`, options);
    if (response.status === 204) return undefined;
    const payload = await response.json().catch(() => null);
    if (!response.ok) {
      const error = payload?.error;
      throw new TinyLordError(error?.message || `request failed (${response.status})`, { status: response.status, code: error?.code, detail: error?.detail });
    }
    return payload;
  }
}

function browserCookie(name) {
  if (typeof document === "undefined") return null;
  const prefix = `${name}=`;
  const item = document.cookie.split(";").map((part) => part.trim()).find((part) => part.startsWith(prefix));
  return item ? decodeURIComponent(item.slice(prefix.length)) : null;
}

class Database {
  constructor(client, name) { this.client = client; this.name = encodeURIComponent(name); }
  collection(name) { return new Collection(this.client, this.name, encodeURIComponent(name)); }
  channel(name) { return new Channel(this.client, this.name, encodeURIComponent(name)); }
}

class Collection {
  constructor(client, database, name) { this.client = client; this.path = `/v1/db/${database}/collections/${name}`; }
  async create(document) { return this.client._request(`${this.path}/documents`, this.#json("POST", document)); }
  async get(id) { return this.client._request(`${this.path}/documents/${encodeURIComponent(id)}`, { headers: this.client._headers() }); }
  async put(id, document) { return this.client._request(`${this.path}/documents/${encodeURIComponent(id)}`, this.#json("PUT", document)); }
  async delete(id) { return this.client._request(`${this.path}/documents/${encodeURIComponent(id)}`, { method: "DELETE", headers: this.client._headers() }); }
  async query(options = {}) { return this.client._request(`${this.path}/query`, this.#json("POST", options)); }
  async count(filter = {}) { return this.client._request(`${this.path}/count`, this.#json("POST", { filter })); }

  async *subscribe({ signal, ...options } = {}) {
    const query = new URLSearchParams();
    if (options.filter != null) query.set("filter", JSON.stringify(options.filter));
    const headers = this.client._headers();
    if (options.lastEventId != null) headers.set("last-event-id", String(options.lastEventId));
    const response = await this.client.fetch(`${this.client.baseUrl}${this.path}/subscribe${query.size ? `?${query}` : ""}`, { headers, signal });
    yield* readEventStream(response);
  }

  #json(method, body) {
    return { method, headers: this.client._headers({ extra: { "content-type": "application/json" } }), body: JSON.stringify(body) };
  }
}

class Channel {
  constructor(client, database, name) { this.client = client; this.path = `/v1/db/${database}/channels/${name}`; }

  async publish(data) {
    return this.client._request(`${this.path}/publish`, {
      method: "POST",
      headers: this.client._headers({ extra: { "content-type": "application/json" } }),
      body: JSON.stringify({ client_id: this.client.clientId, data }),
    });
  }

  async presence() { return this.client._request(`${this.path}/presence`, { headers: this.client._headers() }); }

  async *subscribe({ signal } = {}) {
    const query = new URLSearchParams({ client_id: this.client.clientId });
    const response = await this.client.fetch(`${this.client.baseUrl}${this.path}/subscribe?${query}`, { headers: this.client._headers(), signal });
    for await (const event of readEventStream(response)) yield { type: event.type, data: event.data };
  }
}

/// Generate a stable client identifier. Prefers a UUID when available.
function newClientId() {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  return `c-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/// Parse an SSE response body, yielding `{ type, id, data }` per dispatched
/// event (`data` parsed as JSON). Shared by document and channel subscriptions.
/// Comment lines (keep-alives such as `:ka`) are ignored.
async function* readEventStream(response) {
  if (!response.ok) {
    const payload = await response.json().catch(() => null);
    const error = payload?.error;
    throw new TinyLordError(error?.message || `subscription failed (${response.status})`, { status: response.status, code: error?.code, detail: error?.detail });
  }
  if (!response.body) throw new TinyLordError("streaming response body is unavailable");
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let buffered = "";
  let event = {};
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffered += value;
      let lineEnd;
      while ((lineEnd = buffered.indexOf("\n")) >= 0) {
        const line = buffered.slice(0, lineEnd).replace(/\r$/, "");
        buffered = buffered.slice(lineEnd + 1);
        if (!line) {
          if (event.data !== undefined) yield { type: event.event || "message", id: event.id, data: JSON.parse(event.data) };
          event = {};
        } else if (line.startsWith("event:")) event.event = line.slice(6).trim();
        else if (line.startsWith("id:")) event.id = line.slice(3).trim();
        else if (line.startsWith("data:")) event.data = `${event.data || ""}${line.slice(5).trimStart()}`;
      }
    }
  } finally { reader.releaseLock(); }
}
