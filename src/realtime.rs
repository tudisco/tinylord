//! Realtime change events and the SSE subscribe endpoint (§9).
//!
//! Fan-out is per database via a `tokio::sync::broadcast` channel. Delivery is
//! best-effort: a subscriber that lags past the channel capacity receives a
//! single `resync` event and continues, rather than any attempt at perfect
//! recovery. Resume via `Last-Event-ID` replays retained `_changelog` rows.

use crate::api::AppState;
use crate::auth::Principal;
use crate::db::DbHandle;
use crate::errors::{map_sqlite_err, ApiError, ApiResult};
use crate::system::Role;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use tokio::sync::broadcast;

/// Change operation kind. Serializes to the lowercase strings used in events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Insert,
    Update,
    Delete,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Insert => "insert",
            Op::Update => "update",
            Op::Delete => "delete",
        }
    }
}

/// A single change, broadcast after commit and serialized as SSE `data` (§9).
#[derive(Debug, Clone, Serialize)]
pub struct ChangeEvent {
    pub seq: i64,
    pub collection: String,
    pub op: Op,
    pub id: String,
    /// New document state for insert/update; `null` for delete.
    pub doc: Option<serde_json::Value>,
}

/// Create a new per-database broadcast channel with the configured capacity.
pub fn new_channel(capacity: usize) -> broadcast::Sender<ChangeEvent> {
    broadcast::channel(capacity.max(1)).0
}

// ---- Equality-only filter for subscriptions (§9) ---------------------------

/// The equality-only subset used to filter a subscription: each field maps to a
/// scalar (equality), `{"$eq": v}`, or `{"$in": [..]}`. Anything else is a 400.
pub struct EqFilter {
    clauses: Vec<(Vec<String>, EqMatch)>,
}

enum EqMatch {
    Eq(serde_json::Value),
    In(Vec<serde_json::Value>),
}

impl EqFilter {
    /// Parse from a JSON object. Returns `None` for an absent/empty filter.
    pub fn parse(value: &serde_json::Value) -> ApiResult<Self> {
        let obj = value
            .as_object()
            .ok_or_else(|| ApiError::bad_request("subscribe filter must be a JSON object"))?;
        let mut clauses = Vec::new();
        for (key, val) in obj {
            if key.starts_with('$') {
                return Err(ApiError::bad_request(
                    "subscribe filter supports only field equality and $in",
                ));
            }
            let path = split_field(key)?;
            let m = match val {
                serde_json::Value::Object(map) => {
                    if map.len() != 1 {
                        return Err(ApiError::bad_request(
                            "subscribe filter operator map must have exactly one operator",
                        ));
                    }
                    let (op, arg) = map.iter().next().unwrap();
                    match op.as_str() {
                        "$eq" => EqMatch::Eq(arg.clone()),
                        "$in" => {
                            let arr = arg.as_array().ok_or_else(|| {
                                ApiError::bad_request("$in requires an array")
                            })?;
                            EqMatch::In(arr.clone())
                        }
                        _ => {
                            return Err(ApiError::bad_request(
                                "subscribe filter supports only $eq and $in",
                            ))
                        }
                    }
                }
                scalar => EqMatch::Eq(scalar.clone()),
            };
            clauses.push((path, m));
        }
        Ok(EqFilter { clauses })
    }

    /// Evaluate against a document. Deletes are handled by the caller (always
    /// forwarded), so this is only called for insert/update states.
    pub fn matches(&self, doc: &serde_json::Value) -> bool {
        self.clauses.iter().all(|(path, m)| {
            let actual = lookup(doc, path);
            match m {
                EqMatch::Eq(v) => actual.map(|a| a == v).unwrap_or(false),
                EqMatch::In(vs) => actual.map(|a| vs.iter().any(|v| v == a)).unwrap_or(false),
            }
        })
    }
}

fn split_field(key: &str) -> ApiResult<Vec<String>> {
    if key.is_empty() {
        return Err(ApiError::bad_request("empty field in subscribe filter"));
    }
    let mut segs = Vec::new();
    for seg in key.split('.') {
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(ApiError::bad_request("invalid field in subscribe filter"));
        }
        segs.push(seg.to_string());
    }
    Ok(segs)
}

fn lookup<'a>(doc: &'a serde_json::Value, path: &[String]) -> Option<&'a serde_json::Value> {
    let mut cur = doc;
    for seg in path {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

// ---- SSE endpoint ----------------------------------------------------------

#[derive(Deserialize)]
pub struct SubscribeQuery {
    /// URL-encoded JSON, equality-only subset.
    filter: Option<String>,
}

/// `GET /v1/db/{db}/collections/{coll}/subscribe` (§7.7). Requires `read`.
pub async fn subscribe(
    State(state): State<AppState>,
    principal: Principal,
    Path((db, coll)): Path<(String, String)>,
    Query(q): Query<SubscribeQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    state.authorize(&principal, &db, Role::Read).await?;
    crate::ids::require_valid_name("collection", &coll)?;

    // Parse the optional equality filter up front so bad filters fail at connect.
    let filter = match q.filter {
        Some(ref raw) => {
            let value: serde_json::Value = serde_json::from_str(raw)
                .map_err(|_| ApiError::bad_request("filter is not valid JSON"))?;
            Some(EqFilter::parse(&value)?)
        }
        None => None,
    };

    let last_event_id = parse_last_event_id(&headers);

    let handle = state.open_db(&db).await?;
    // Subscribe BEFORE reading the changelog so no event is missed in the gap
    // between replay and attaching to the live stream.
    let rx = handle.broadcast_tx.subscribe();

    let plan = build_resume_plan(&handle, &coll, last_event_id, filter.as_ref())?;

    let stream = event_stream(rx, coll, filter, plan);
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn parse_last_event_id(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// What to send before attaching to the live stream.
struct ResumePlan {
    resync: bool,
    replay: Vec<ChangeEvent>,
    /// Live events with `seq <= max_sent` are dropped (already delivered).
    max_sent: i64,
}

fn build_resume_plan(
    handle: &DbHandle,
    coll: &str,
    last_event_id: Option<i64>,
    filter: Option<&EqFilter>,
) -> ApiResult<ResumePlan> {
    let conn = handle
        .read_pool
        .get()
        .map_err(|e| ApiError::internal(format!("read pool: {e}")))?;

    let current_max: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM _changelog", [], |r| r.get(0))
        .map_err(map_sqlite_err)?;

    let Some(last) = last_event_id else {
        // Fresh subscriber: only future events.
        return Ok(ResumePlan {
            resync: false,
            replay: Vec::new(),
            max_sent: current_max,
        });
    };

    let min_seq: Option<i64> = conn
        .query_row("SELECT MIN(seq) FROM _changelog", [], |r| r.get(0))
        .map_err(map_sqlite_err)?;

    // If the requested resume point predates the oldest retained row, we cannot
    // replay the gap → resync and attach live (§9).
    if let Some(min) = min_seq {
        if last + 1 < min {
            return Ok(ResumePlan {
                resync: true,
                replay: Vec::new(),
                max_sent: current_max,
            });
        }
    }

    // Replay retained rows after `last` for this collection, applying the filter.
    let mut stmt = conn
        .prepare(
            "SELECT seq, op, doc_id, json(doc) FROM _changelog \
             WHERE seq > ?1 AND collection = ?2 ORDER BY seq",
        )
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(rusqlite::params![last, coll], |row| {
            let seq: i64 = row.get(0)?;
            let op: String = row.get(1)?;
            let doc_id: String = row.get(2)?;
            let doc_text: Option<String> = row.get(3)?;
            Ok((seq, op, doc_id, doc_text))
        })
        .map_err(map_sqlite_err)?;

    let mut replay = Vec::new();
    let mut max_sent = last;
    for row in rows {
        let (seq, op, doc_id, doc_text) = row.map_err(map_sqlite_err)?;
        max_sent = max_sent.max(seq);
        let op = parse_op(&op);
        let doc = doc_text.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        if !event_passes(op, doc.as_ref(), filter) {
            continue;
        }
        replay.push(ChangeEvent {
            seq,
            collection: coll.to_string(),
            op,
            id: doc_id,
            doc,
        });
    }

    Ok(ResumePlan {
        resync: false,
        replay,
        max_sent,
    })
}

fn parse_op(s: &str) -> Op {
    match s {
        "insert" => Op::Insert,
        "update" => Op::Update,
        _ => Op::Delete,
    }
}

/// Whether an event passes the subscription filter. Deletes are always
/// forwarded (§9); inserts/updates are evaluated against the doc.
fn event_passes(op: Op, doc: Option<&serde_json::Value>, filter: Option<&EqFilter>) -> bool {
    match filter {
        None => true,
        Some(f) => match op {
            Op::Delete => true,
            _ => doc.map(|d| f.matches(d)).unwrap_or(false),
        },
    }
}

fn change_to_sse(ev: &ChangeEvent) -> Event {
    // `id: <seq>` lets clients resume via Last-Event-ID (§9).
    Event::default()
        .id(ev.seq.to_string())
        .event("change")
        .json_data(ev)
        .unwrap_or_else(|_| Event::default().event("change").data("{}"))
}

fn resync_event() -> Event {
    Event::default().event("resync").data("{}")
}

/// Build the SSE stream: optional resync, replayed rows, then the live channel
/// with dedup and lag handling.
fn event_stream(
    mut rx: broadcast::Receiver<ChangeEvent>,
    coll: String,
    filter: Option<EqFilter>,
    plan: ResumePlan,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        if plan.resync {
            yield Ok(resync_event());
        }
        for ev in plan.replay {
            yield Ok(change_to_sse(&ev));
        }

        let mut max_sent = plan.max_sent;
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if ev.collection != coll || ev.seq <= max_sent {
                        continue;
                    }
                    if !event_passes(ev.op, ev.doc.as_ref(), filter.as_ref()) {
                        // Still advance the watermark so we don't reconsider it.
                        max_sent = ev.seq;
                        continue;
                    }
                    max_sent = ev.seq;
                    yield Ok(change_to_sse(&ev));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Best-effort: tell the client to re-read current state (§9).
                    yield Ok(resync_event());
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
