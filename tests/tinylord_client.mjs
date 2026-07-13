import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const source = await readFile(new URL("../tinylord.js", import.meta.url));
const { TinyLord, TinyLordError } = await import(
  `data:text/javascript;base64,${source.toString("base64")}`
);

const calls = [];
const client = TinyLord.connect({
  fetch: async (url, options = {}) => {
    calls.push({ url, options });
    if (url.endsWith("/v1/auth/login")) {
      return Response.json({
        access_token: "access-token",
        csrf_token: "csrf-token",
        token_type: "Bearer",
        expires_in: 900,
      });
    }
    if (url.includes("/subscribe")) {
      return new Response('event: change\nid: 7\ndata: {"title":"Ship it"}\n\n');
    }
    if (url.includes("/documents") && options.method === "POST") {
      return Response.json({ id: "task-1", doc: { title: "Ship it" } });
    }
    if (url.endsWith("/missing")) {
      return Response.json(
        { error: { code: "not_found", message: "document not found", detail: null } },
        { status: 404 },
      );
    }
    throw new Error(`unexpected request: ${url}`);
  },
});

const session = await client.login("delegate", "long-enough-password");
assert.equal(session.access_token, "access-token");

const tasks = client.collection("delegate", "tasks");
const created = await tasks.create({ title: "Ship it" });
assert.equal(created.id, "task-1");
assert.equal(calls.at(-1).options.headers.get("authorization"), "Bearer access-token");

const stream = tasks.subscribe();
const event = (await stream.next()).value;
assert.deepEqual(event, { type: "change", id: "7", data: { title: "Ship it" } });
await stream.return();

const restored = TinyLord.connect({
  readCookie: (name) => name === "tinylord_csrf" ? "csrf-from-cookie" : null,
  fetch: async (url, options = {}) => {
    assert.equal(url, "/v1/auth/refresh");
    assert.equal(options.headers.get("x-csrf-token"), "csrf-from-cookie");
    return Response.json({
      access_token: "restored-access-token",
      csrf_token: "rotated-csrf-token",
      token_type: "Bearer",
      expires_in: 900,
    });
  },
});
assert.equal((await restored.refresh()).access_token, "restored-access-token");

let fetchReceiver;
const receiverClient = TinyLord.connect({
  fetch: function () {
    fetchReceiver = this;
    return Response.json({ ok: true });
  },
});
await receiverClient._request("/receiver");
assert.equal(fetchReceiver, globalThis);

await assert.rejects(
  () => client._request("/missing"),
  (error) => error instanceof TinyLordError && error.status === 404 && error.code === "not_found",
);

// Channel publish / subscribe / presence wire format, with two instances sharing
// a mock transport (the subscriber sees the publisher's message, not its own).
const publisher = TinyLord.connect({
  clientId: "publisher-1",
  fetch: async (url, options = {}) => {
    assert.ok(url.endsWith("/v1/db/room/channels/lobby/publish"));
    assert.equal(options.method, "POST");
    const body = JSON.parse(options.body);
    assert.equal(body.client_id, "publisher-1");
    assert.deepEqual(body.data, { cursor: 42 });
    return Response.json({ delivered: 1 });
  },
});
const published = await publisher.db("room").channel("lobby").publish({ cursor: 42 });
assert.deepEqual(published, { delivered: 1 });

const subscriber = TinyLord.connect({
  clientId: "subscriber-2",
  fetch: async (url) => {
    if (url.includes("/channels/lobby/subscribe")) {
      assert.ok(url.includes("client_id=subscriber-2"));
      return new Response(
        ':ka\n\n' +
        'event: presence\ndata: {"type":"join","client_id":"publisher-1","ts":1}\n\n' +
        'event: message\ndata: {"channel":"lobby","client_id":"publisher-1","ts":2,"data":{"cursor":42}}\n\n',
      );
    }
    if (url.includes("/channels/lobby/presence")) {
      return Response.json({ clients: [{ client_id: "publisher-1", connected_at: 1 }] });
    }
    throw new Error(`unexpected request: ${url}`);
  },
});

const channel = subscriber.db("room").channel("lobby");
const channelStream = channel.subscribe();
const first = (await channelStream.next()).value;
assert.deepEqual(first, { type: "presence", data: { type: "join", client_id: "publisher-1", ts: 1 } });
const second = (await channelStream.next()).value;
assert.deepEqual(second, { type: "message", data: { channel: "lobby", client_id: "publisher-1", ts: 2, data: { cursor: 42 } } });
await channelStream.return();

const roster = await channel.presence();
assert.deepEqual(roster, { clients: [{ client_id: "publisher-1", connected_at: 1 }] });

console.log("tinylord browser client tests passed");
