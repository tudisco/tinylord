//! Ephemeral pub/sub channels and presence.
//!
//! Unlike the document realtime stream (§9), these events are never persisted:
//! there is no changelog, no sequence numbers, and no resume. Publishing does
//! not go through the writer actor — it is a direct in-memory fan-out over a
//! second per-database `tokio::sync::broadcast` channel. Delivery is
//! best-effort: a subscriber that lags past the channel capacity silently drops
//! the missed events and continues.
//!
//! Every event carries the originating `client_id`; each subscriber excludes
//! events bearing its own `client_id`, so a client never receives its own
//! messages nor its own presence join/leave.

use crate::api::AppState;
use crate::auth::Principal;
use crate::db::DbHandle;
use crate::errors::{ApiError, ApiResult};
use crate::ids::now_ms;
use crate::system::Role;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// A single ephemeral event broadcast to every subscriber of one database. The
/// receiver filters by `channel` and drops events matching its own `client_id`.
#[derive(Debug, Clone)]
pub struct PubSubEvent {
    pub channel: String,
    /// Originating client. Used for sender exclusion at delivery time.
    pub client_id: String,
    pub ts: i64,
    pub kind: EventKind,
}

/// Either an application message or a synthetic presence transition.
#[derive(Debug, Clone)]
pub enum EventKind {
    Message(serde_json::Value),
    Presence(PresenceKind),
}

#[derive(Debug, Clone, Copy)]
pub enum PresenceKind {
    Join,
    Leave,
}

impl PresenceKind {
    fn as_str(self) -> &'static str {
        match self {
            PresenceKind::Join => "join",
            PresenceKind::Leave => "leave",
        }
    }
}

/// One present client on a channel. Removed only when `count` reaches zero, so a
/// client that opens several subscriptions stays present until the last closes.
#[derive(Debug, Clone)]
pub struct PresenceEntry {
    pub connected_at: i64,
    pub count: u32,
}

/// Per-database presence: `channel -> (client_id -> entry)`.
pub type PresenceMap = HashMap<String, HashMap<String, PresenceEntry>>;

/// Create a new per-database ephemeral broadcast channel.
pub fn new_channel(capacity: usize) -> broadcast::Sender<PubSubEvent> {
    broadcast::channel(capacity.max(1)).0
}

// ---- Validation ------------------------------------------------------------

/// Validate a client identifier: non-empty, at most 128 chars, and restricted
/// to a conservative printable charset (alphanumerics, `-`, `_`). This is
/// deliberately stricter than necessary so identifiers never carry control
/// characters into logs or SSE frames.
fn valid_client_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    id.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn require_client_id(id: &str) -> ApiResult<()> {
    if valid_client_id(id) {
        Ok(())
    } else {
        Err(ApiError::validation(
            "invalid client_id; must be 1-128 chars of [A-Za-z0-9_-]",
        ))
    }
}

/// Reject every request when the feature is disabled, hiding it as `not_found`.
fn ensure_enabled(state: &AppState) -> ApiResult<()> {
    if state.config.pubsub.enabled {
        Ok(())
    } else {
        Err(ApiError::not_found("API route not found"))
    }
}

// ---- Publish ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct PublishBody {
    client_id: String,
    data: serde_json::Value,
}

#[derive(Serialize)]
struct PublishResult {
    delivered: usize,
}

/// `POST /v1/db/{db}/channels/{channel}/publish` (grant `write`). Broadcasts one
/// ephemeral message; nothing is persisted. Returns the number of subscribers
/// the event was handed to (best-effort — zero receivers is not an error).
pub async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path((db, channel)): Path<(String, String)>,
    axum::Json(body): axum::Json<PublishBody>,
) -> ApiResult<Response> {
    ensure_enabled(&state)?;
    state.authorize(&principal, &db, Role::Write).await?;
    crate::ids::require_valid_name("channel", &channel)?;
    require_client_id(&body.client_id)?;

    // Size-check the payload against the per-event limit (distinct from and
    // tighter than the global request body limit).
    let size = serde_json::to_vec(&body.data).map(|v| v.len()).unwrap_or(0);
    if size > state.config.pubsub.max_event_bytes {
        return Err(ApiError::payload_too_large("event payload too large"));
    }

    let handle = state.open_db(&db).await?;
    let event = PubSubEvent {
        channel,
        client_id: body.client_id,
        ts: now_ms(),
        kind: EventKind::Message(body.data),
    };
    // `send` errors only when there are no receivers; that is a valid outcome.
    let delivered = handle.pubsub_tx.send(event).unwrap_or(0);

    Ok(axum::Json(PublishResult { delivered }).into_response())
}

// ---- Presence roster -------------------------------------------------------

#[derive(Serialize)]
struct PresenceClient {
    client_id: String,
    connected_at: i64,
}

#[derive(Serialize)]
struct PresenceRoster {
    clients: Vec<PresenceClient>,
}

/// `GET /v1/db/{db}/channels/{channel}/presence` (grant `read`). Returns the
/// current roster for the channel.
pub async fn presence(
    State(state): State<AppState>,
    principal: Principal,
    Path((db, channel)): Path<(String, String)>,
) -> ApiResult<Response> {
    ensure_enabled(&state)?;
    state.authorize(&principal, &db, Role::Read).await?;
    crate::ids::require_valid_name("channel", &channel)?;

    let handle = state.open_db(&db).await?;
    let clients = {
        let map = handle.presence.read().expect("presence lock");
        map.get(&channel)
            .map(|clients| {
                clients
                    .iter()
                    .map(|(client_id, entry)| PresenceClient {
                        client_id: client_id.clone(),
                        connected_at: entry.connected_at,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    Ok(axum::Json(PresenceRoster { clients }).into_response())
}

// ---- Subscribe (SSE) -------------------------------------------------------

#[derive(Deserialize)]
pub struct SubscribeQuery {
    client_id: String,
}

/// `GET /v1/db/{db}/channels/{channel}/subscribe?client_id=...` (grant `read`).
/// Streams `message` and `presence` events for the channel, excluding those
/// originating from the caller's own `client_id`. Ephemeral: no resume.
pub async fn subscribe(
    State(state): State<AppState>,
    principal: Principal,
    Path((db, channel)): Path<(String, String)>,
    Query(q): Query<SubscribeQuery>,
) -> ApiResult<Response> {
    ensure_enabled(&state)?;
    state.authorize(&principal, &db, Role::Read).await?;
    crate::ids::require_valid_name("channel", &channel)?;
    require_client_id(&q.client_id)?;

    let handle = state.open_db(&db).await?;
    // Subscribe to the live stream BEFORE registering presence so this
    // connection cannot miss a concurrent event during setup.
    let rx = handle.pubsub_tx.subscribe();

    // Register presence and, on first appearance of this client, announce a join
    // to everyone else (sender exclusion drops it for this client's own streams).
    let guard = PresenceGuard::register(handle.clone(), channel.clone(), q.client_id.clone());

    let stream = event_stream(rx, channel, q.client_id, guard);
    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(25))
                .text("ka"),
        )
        .into_response())
}

fn message_event(ev: &PubSubEvent, data: &serde_json::Value) -> Event {
    let payload = serde_json::json!({
        "channel": ev.channel,
        "client_id": ev.client_id,
        "ts": ev.ts,
        "data": data,
    });
    Event::default()
        .event("message")
        .json_data(&payload)
        .unwrap_or_else(|_| Event::default().event("message").data("{}"))
}

fn presence_event(ev: &PubSubEvent, kind: PresenceKind) -> Event {
    let payload = serde_json::json!({
        "type": kind.as_str(),
        "client_id": ev.client_id,
        "ts": ev.ts,
    });
    Event::default()
        .event("presence")
        .json_data(&payload)
        .unwrap_or_else(|_| Event::default().event("presence").data("{}"))
}

/// Build the SSE stream: forward channel events (excluding the caller's own
/// `client_id`), drop on lag, end on close. The presence guard is owned by the
/// stream so it deregisters when the client disconnects.
fn event_stream(
    mut rx: broadcast::Receiver<PubSubEvent>,
    channel: String,
    client_id: String,
    guard: PresenceGuard,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        // Kept alive for the lifetime of the stream; dropped (deregistering
        // presence and emitting a leave) when the client disconnects.
        let _guard = guard;
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if ev.channel != channel || ev.client_id == client_id {
                        continue;
                    }
                    match &ev.kind {
                        EventKind::Message(data) => yield Ok(message_event(&ev, data)),
                        EventKind::Presence(kind) => yield Ok(presence_event(&ev, *kind)),
                    }
                }
                // Best-effort: a lagging subscriber simply drops missed events.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

// ---- Presence guard --------------------------------------------------------

/// RAII presence registration. Constructing it increments the per-channel
/// client count (emitting a `join` on first appearance); dropping it decrements
/// the count (emitting a `leave` when the last connection closes).
struct PresenceGuard {
    handle: Arc<DbHandle>,
    channel: String,
    client_id: String,
}

impl PresenceGuard {
    fn register(handle: Arc<DbHandle>, channel: String, client_id: String) -> Self {
        let first = {
            let mut map = handle.presence.write().expect("presence lock");
            let clients = map.entry(channel.clone()).or_default();
            match clients.get_mut(&client_id) {
                Some(entry) => {
                    entry.count += 1;
                    false
                }
                None => {
                    clients.insert(
                        client_id.clone(),
                        PresenceEntry {
                            connected_at: now_ms(),
                            count: 1,
                        },
                    );
                    true
                }
            }
        };
        if first {
            let _ = handle.pubsub_tx.send(PubSubEvent {
                channel: channel.clone(),
                client_id: client_id.clone(),
                ts: now_ms(),
                kind: EventKind::Presence(PresenceKind::Join),
            });
        }
        Self {
            handle,
            channel,
            client_id,
        }
    }
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let last = {
            let mut map = self.handle.presence.write().expect("presence lock");
            let mut last = false;
            if let Some(clients) = map.get_mut(&self.channel) {
                if let Some(entry) = clients.get_mut(&self.client_id) {
                    entry.count -= 1;
                    if entry.count == 0 {
                        clients.remove(&self.client_id);
                        last = true;
                    }
                }
                if clients.is_empty() {
                    map.remove(&self.channel);
                }
            }
            last
        };
        if last {
            let _ = self.handle.pubsub_tx.send(PubSubEvent {
                channel: self.channel.clone(),
                client_id: self.client_id.clone(),
                ts: now_ms(),
                kind: EventKind::Presence(PresenceKind::Leave),
            });
        }
    }
}
