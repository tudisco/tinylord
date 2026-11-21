//! Optional task notifications. Credentials and delivery receipts live in an
//! encrypted internal database with no public database registration.
use super::{documents::fetch_document, AppState};
use crate::{
    auth::Principal,
    db::{writer::Precondition, DbHandle},
    errors::{ApiError, ApiResult},
    system::Role,
};
use axum::{
    extract::{Path, State},
    Json,
};
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

mod delegate;
mod providers;
use delegate::notices;
use providers::deliver;

const PRIVATE_DB: &str = "__notifications";
const DATABASE: &str = "delegate";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    provider: providers::Provider,
    enabled: bool,
    token: String,
    user_key: String,
    telegram_bot_token: String,
    telegram_chat_id: String,
    timezone: String,
    morning_enabled: bool,
    morning_time: String,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            provider: providers::Provider::default(),
            enabled: false,
            token: String::new(),
            user_key: String::new(),
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            timezone: "UTC".into(),
            morning_enabled: false,
            morning_time: "09:00".into(),
        }
    }
}
#[derive(Deserialize)]
pub struct SettingsInput {
    #[serde(default)]
    provider: Option<providers::Provider>,
    enabled: bool,
    token: Option<String>,
    user_key: Option<String>,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default)]
    morning_enabled: bool,
    #[serde(default = "default_morning_time")]
    morning_time: String,
    #[serde(default)]
    disconnect: bool,
}
fn default_timezone() -> String {
    "UTC".into()
}
fn default_morning_time() -> String {
    "09:00".into()
}
fn http_client() -> ApiResult<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(ApiError::internal)
}
async fn authorize(state: &AppState, principal: &Principal) -> ApiResult<Arc<DbHandle>> {
    if !state.config.notifications.enabled {
        return Err(ApiError::not_found(
            "Reminders are not enabled on this server",
        ));
    }
    let _ = principal; // Authentication scopes all settings and jobs to this principal.
    state.registry.get_or_open(PRIVATE_DB).await
}
fn preferences(db: &DbHandle, id: &str) -> ApiResult<Preferences> {
    fetch_document(db, "configs", id)?
        .map(|row| serde_json::from_value(row.doc).map_err(ApiError::internal))
        .unwrap_or_else(|| Ok(Preferences::default()))
}
fn provider_configured(prefs: &Preferences, provider: providers::Provider) -> bool {
    match provider {
        providers::Provider::Pushover => valid_key(&prefs.token) && valid_key(&prefs.user_key),
        providers::Provider::Telegram => {
            providers::valid_telegram_token(&prefs.telegram_bot_token)
                && providers::valid_telegram_chat(&prefs.telegram_chat_id)
        }
    }
}
fn public_settings(prefs: &Preferences, status: Option<Value>) -> Value {
    json!({"provider":prefs.provider,"enabled":prefs.enabled,"configured":provider_configured(prefs,prefs.provider),
        "providers":{"pushover":{"configured":provider_configured(prefs,providers::Provider::Pushover)},"telegram":{"configured":provider_configured(prefs,providers::Provider::Telegram)}},
        "timezone":prefs.timezone,"morning_enabled":prefs.morning_enabled,"morning_time":prefs.morning_time,
        "last_delivery":status})
}
pub async fn get_settings(
    State(state): State<AppState>,
    principal: Principal,
) -> ApiResult<Json<Value>> {
    let db = authorize(&state, &principal).await?;
    let prefs = preferences(&db, &principal.id)?;
    let status = fetch_document(&db, "status", &principal.id)?.map(|row| row.doc);
    Ok(Json(public_settings(&prefs, status)))
}
fn valid_key(key: &str) -> bool {
    key.len() == 30 && key.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn minutes(value: &str) -> Option<u32> {
    if value.len() != 5 || value.as_bytes()[2] != b':' {
        return None;
    }
    let (h, m) = value.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}
fn updated_preferences(mut previous: Preferences, input: SettingsInput) -> ApiResult<Preferences> {
    let selected_provider = input.provider.unwrap_or(previous.provider);
    if input.disconnect {
        match selected_provider {
            providers::Provider::Pushover => {
                previous.token.clear();
                previous.user_key.clear();
            }
            providers::Provider::Telegram => {
                previous.telegram_bot_token.clear();
                previous.telegram_chat_id.clear();
            }
        }
        if selected_provider == previous.provider {
            previous.enabled = false;
        }
        return Ok(previous);
    }
    previous.provider = selected_provider;
    input
        .timezone
        .parse::<Tz>()
        .map_err(|_| ApiError::validation("Choose a valid timezone"))?;
    if minutes(&input.morning_time).is_none() {
        return Err(ApiError::validation("Morning summary time must be HH:MM"));
    }
    for (new, old) in [
        (input.token, &mut previous.token),
        (input.user_key, &mut previous.user_key),
    ] {
        if let Some(value) = new.filter(|value| !value.trim().is_empty()) {
            if !valid_key(value.trim()) {
                return Err(ApiError::validation(
                    "Pushover keys must contain 30 letters or digits",
                ));
            }
            *old = value.trim().to_owned();
        }
    }
    if let Some(value) = input
        .telegram_bot_token
        .filter(|value| !value.trim().is_empty())
    {
        if !providers::valid_telegram_token(value.trim()) {
            return Err(ApiError::validation(
                "Enter a valid Telegram bot token from BotFather",
            ));
        }
        previous.telegram_bot_token = value.trim().to_owned();
    }
    if let Some(value) = input
        .telegram_chat_id
        .filter(|value| !value.trim().is_empty())
    {
        if !providers::valid_telegram_chat(value.trim()) {
            return Err(ApiError::validation(
                "Enter a numeric Telegram chat ID or @channel username",
            ));
        }
        previous.telegram_chat_id = value.trim().to_owned();
    }
    previous.enabled = input.enabled;
    previous.timezone = input.timezone;
    previous.morning_enabled = input.morning_enabled;
    previous.morning_time = input.morning_time;
    if previous.enabled && !provider_configured(&previous, previous.provider) {
        return Err(ApiError::validation(match previous.provider {
            providers::Provider::Pushover => {
                "Enter both your Pushover application token and user key"
            }
            providers::Provider::Telegram => "Enter both your Telegram bot token and chat ID",
        }));
    }
    Ok(previous)
}
pub async fn save_settings(
    State(state): State<AppState>,
    principal: Principal,
    Json(input): Json<SettingsInput>,
) -> ApiResult<Json<Value>> {
    let db = authorize(&state, &principal).await?;
    let prefs = updated_preferences(preferences(&db, &principal.id)?, input)?;
    if prefs.enabled {
        providers::validate(&http_client()?, &prefs).await?;
    }
    db.writer
        .replace_document(
            "configs".into(),
            principal.id,
            serde_json::to_value(&prefs).map_err(ApiError::internal)?,
            Precondition::None,
        )
        .await?;
    Ok(Json(public_settings(&prefs, None)))
}
pub async fn test_notification(
    State(state): State<AppState>,
    principal: Principal,
) -> ApiResult<Json<Value>> {
    let db = authorize(&state, &principal).await?;
    let prefs = preferences(&db, &principal.id)?;
    if !prefs.enabled {
        return Err(ApiError::validation(
            "Save and enable your notification provider first",
        ));
    }
    // Persist the throttle so repeated clicks or restarts cannot flood a device.
    let guard = state.notification_guard.lock().await;
    let now = Utc::now().timestamp();
    if fetch_document(&db, "tests", &principal.id)?
        .is_some_and(|row| now - row.doc["at"].as_i64().unwrap_or(0) < 60)
    {
        return Err(ApiError::rate_limited(
            "Please wait a minute before sending another test",
        ));
    }
    db.writer
        .replace_document(
            "tests".into(),
            principal.id.clone(),
            json!({"at":now}),
            Precondition::None,
        )
        .await?;
    drop(guard);
    let result = deliver(&http_client()?, &prefs, "TinyLord · Test notification", "Your notification provider is connected. Scheduled notifications will arrive here even when your app is closed.", "").await;
    record_status(&db, &principal.id, result, now).await?;
    result.map_err(ApiError::bad_request)?;
    Ok(Json(json!({"ok":true})))
}
async fn record_status(
    db: &DbHandle,
    id: &str,
    result: Result<(), &'static str>,
    now: i64,
) -> ApiResult<()> {
    db.writer.replace_document("status".into(), id.into(), json!({"at":now,"ok":result.is_ok(),"message":result.err().unwrap_or("Delivered to notification provider")}), Precondition::None).await?;
    Ok(())
}
fn configurations(db: &DbHandle) -> ApiResult<Vec<(String, Preferences)>> {
    let conn = db.read_pool.get().map_err(ApiError::internal)?;
    if !crate::db::schema::collection_exists(&conn, "configs").map_err(ApiError::internal)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare("SELECT id, json(doc) FROM coll_configs")
        .map_err(ApiError::internal)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(ApiError::internal)?;
    let mut result = Vec::new();
    for row in rows {
        let (id, text) = row.map_err(ApiError::internal)?;
        if let Ok(prefs) = serde_json::from_str::<Preferences>(&text) {
            if prefs.enabled {
                result.push((id, prefs));
            }
        }
    }
    Ok(result)
}
fn eligible(receipt: Option<&Value>, now: i64) -> bool {
    receipt.map_or(true, |r| {
        r["state"] != "sent"
            && r["attempts"].as_u64().unwrap_or(0) < 3
            && r["retry_at"].as_i64().unwrap_or(0) <= now
    })
}
async fn tick(state: &AppState, client: &reqwest::Client) -> ApiResult<()> {
    if !state.config.notifications.delegate.enabled {
        return Ok(());
    }
    if !state
        .system
        .database_exists(DATABASE)
        .map_err(ApiError::internal)?
    {
        return Ok(());
    }
    let private = state.registry.get_or_open(PRIVATE_DB).await?;
    let db = state.open_db(DATABASE).await?;
    let active: std::collections::HashSet<_> = state
        .system
        .find_principals(None)
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|p| !p.disabled)
        .map(|p| p.id)
        .collect();
    for (id, prefs) in configurations(&private)? {
        if !active.contains(&id) {
            continue;
        }
        if !state
            .system
            .grant_role(&id, DATABASE)
            .map_err(ApiError::internal)?
            .is_some_and(|r| r >= Role::Write)
        {
            continue;
        }
        let Some(workspace) = fetch_document(&db, "workspaces", &format!("workspace-{id}"))? else {
            continue;
        };
        let now = Utc::now();
        let mut sent = 0;
        for notice in notices(&workspace.doc, &prefs, now) {
            let key = hex::encode(Sha256::digest(format!("{id}:{}", notice.key).as_bytes()));
            let receipt = fetch_document(&private, "deliveries", &key)?.map(|r| r.doc);
            if !eligible(receipt.as_ref(), now.timestamp()) {
                continue;
            }
            // Recheck the current task immediately before sending, including completion,
            // deletion, cancellation, and snooze changes since this tick's first read.
            if let Some(task_id) = &notice.task_id {
                let latest = fetch_document(&db, "workspaces", &format!("workspace-{id}"))?;
                let still_due = latest
                    .as_ref()
                    .and_then(|w| w.doc["tasks"].as_array())
                    .is_some_and(|tasks| {
                        tasks.iter().any(|t| {
                            t["id"] == *task_id
                                && t["status"] != "done"
                                && t["archived"] != true
                                && t["reminder_at"] == notice.reminder_at
                                && t["reminder_id"].as_str().unwrap_or("") == notice.reminder_id
                        })
                    });
                if !still_due {
                    continue;
                }
            }
            let attempts = receipt
                .as_ref()
                .and_then(|r| r["attempts"].as_u64())
                .unwrap_or(0)
                + 1;
            private
                .writer
                .replace_document(
                    "deliveries".into(),
                    key.clone(),
                    json!({"state":"sending","attempts":attempts,"retry_at":now.timestamp()+300}),
                    Precondition::None,
                )
                .await?;
            let mut link = reqwest::Url::parse(&state.config.notifications.delegate.public_url)
                .map_err(ApiError::internal)?;
            if let Some(task_id) = &notice.task_id {
                link.query_pairs_mut().append_pair("task", task_id);
            }
            let result = deliver(
                client,
                &prefs,
                &notice.title,
                &notice.message,
                link.as_str(),
            )
            .await;
            private.writer.replace_document("deliveries".into(), key, json!({"state":if result.is_ok(){"sent"}else{"failed"},"attempts":attempts,"retry_at":now.timestamp()+300}), Precondition::None).await?;
            record_status(&private, &id, result, now.timestamp()).await?;
            sent += 1;
            if sent >= 5 {
                break;
            }
        }
    }
    Ok(())
}
pub fn start(state: AppState) -> anyhow::Result<()> {
    if !state.config.notifications.enabled {
        return Ok(());
    }
    anyhow::ensure!(
        state.config.encryption.enabled,
        "Notifications require encrypted storage"
    );
    if state.config.notifications.delegate.enabled {
        let url = reqwest::Url::parse(&state.config.notifications.delegate.public_url)?;
        anyhow::ensure!(
            url.scheme() == "https" && url.host_str().is_some(),
            "notifications.delegate.public_url must be an HTTPS URL"
        );
    }
    let client =
        http_client().map_err(|_| anyhow::anyhow!("Could not initialize notification client"))?;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if tick_jobs(&state, &client).await.is_err() {
                tracing::warn!("Notification queue check failed; will retry on the next tick");
            }
            if tick(&state, &client).await.is_err() {
                tracing::warn!("Reminder check failed; will retry on the next tick");
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        "2026-09-04T15:00:00Z".parse().unwrap()
    }
    fn prefs() -> Preferences {
        Preferences {
            enabled: true,
            timezone: "America/Mexico_City".into(),
            morning_enabled: true,
            ..Default::default()
        }
    }
    #[test]
    fn only_active_due_tasks_and_local_morning() {
        let workspace = json!({"tasks":[
            {"id":"a","title":"Due","reminder_at":"2026-09-04T14:59:00Z"},
            {"id":"b","status":"done","reminder_at":"2026-09-04T14:59:00Z"},
            {"id":"c","archived":true,"reminder_at":"2026-09-04T14:59:00Z"},
            {"id":"d","reminder_at":"2026-09-04T16:00:00Z"},
            {"id":"e","reminder_at":"2026-09-01T14:59:00Z"}]});
        let result = notices(&workspace, &prefs(), now());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].task_id.as_deref(), Some("a"));
        assert_eq!(result[1].key, "morning:2026-09-04");
        assert!(notices(
            &json!({"tasks":[]}),
            &prefs(),
            now() + chrono::Duration::hours(2)
        )
        .is_empty());
    }
    #[test]
    fn dedup_retries_and_snooze_identity() {
        assert!(!eligible(Some(&json!({"state":"sent"})), 1000));
        assert!(!eligible(
            Some(&json!({"state":"sending","retry_at":1100})),
            1000
        ));
        assert!(!eligible(
            Some(&json!({"state":"failed","attempts":3})),
            1000
        ));
        assert!(eligible(
            Some(&json!({"state":"failed","attempts":1,"retry_at":900})),
            1000
        ));
        let a = notices(
            &json!({"tasks":[{"id":"a","reminder_at":"2026-09-04T14:59:00Z","reminder_id":"first"}]}),
            &prefs(),
            now(),
        );
        let b = notices(
            &json!({"tasks":[{"id":"a","reminder_at":"2026-09-04T14:59:00Z","reminder_id":"snoozed"}]}),
            &prefs(),
            now(),
        );
        assert_ne!(a[0].key, b[0].key);
    }
    #[test]
    fn credentials_are_write_only_and_blank_preserves() {
        let previous = Preferences {
            token: "a".repeat(30),
            user_key: "b".repeat(30),
            ..Default::default()
        };
        let input = SettingsInput {
            provider: None,
            enabled: true,
            token: Some("".into()),
            user_key: None,
            telegram_bot_token: None,
            telegram_chat_id: None,
            timezone: "UTC".into(),
            morning_enabled: false,
            morning_time: "09:00".into(),
            disconnect: false,
        };
        let updated = updated_preferences(previous, input).unwrap();
        assert_eq!(updated.token, "a".repeat(30));
        let public = public_settings(&updated, None).to_string();
        assert!(!public.contains(&"a".repeat(30)) && !public.contains(&"b".repeat(30)));
        assert!(minutes("25:00").is_none());
        assert!(!crate::ids::valid_name(PRIVATE_DB));
    }
    #[test]
    fn legacy_settings_and_provider_disconnect_preserve_pushover() {
        let previous: Preferences = serde_json::from_value(
            json!({"token":"a".repeat(30),"user_key":"b".repeat(30),"enabled":true}),
        )
        .unwrap();
        assert!(provider_configured(
            &previous,
            providers::Provider::Pushover
        ));
        assert!(!provider_configured(
            &previous,
            providers::Provider::Telegram
        ));
        let update: SettingsInput = serde_json::from_value(json!({"provider":"telegram","enabled":false,"telegram_bot_token":format!("123456:{}","c".repeat(35)),"telegram_chat_id":"-1001234567890"})).unwrap();
        let mut both = updated_preferences(previous, update).unwrap();
        both.provider = providers::Provider::Pushover;
        both.enabled = true;
        let visible = public_settings(&both, None).to_string();
        assert!(
            !visible.contains(&both.token)
                && !visible.contains(&both.telegram_bot_token)
                && !visible.contains(&both.telegram_chat_id)
        );
        let disconnect: SettingsInput = serde_json::from_value(
            json!({"provider":"telegram","enabled":false,"disconnect":true}),
        )
        .unwrap();
        let result = updated_preferences(both, disconnect).unwrap();
        assert!(result.enabled && matches!(result.provider, providers::Provider::Pushover));
        assert!(provider_configured(&result, providers::Provider::Pushover));
        assert!(!provider_configured(&result, providers::Provider::Telegram));
    }
    #[test]
    fn morning_handles_dst_and_empty_lists() {
        let prefs = Preferences {
            timezone: "America/New_York".into(),
            morning_enabled: true,
            ..Default::default()
        };
        assert_eq!(
            notices(&json!({}), &prefs, "2026-01-04T14:00:00Z".parse().unwrap()).len(),
            1
        );
        assert_eq!(
            notices(&json!({}), &prefs, "2026-07-04T13:00:00Z".parse().unwrap()).len(),
            1
        );
    }
}

/// Generic, durable notifications usable by any authenticated application.
#[derive(Serialize, Deserialize)]
pub struct JobInput {
    title: String,
    message: String,
    deliver_at: String,
    #[serde(default)]
    url: String,
    /// Reusing this key with the same payload returns the existing job.
    #[serde(default)]
    idempotency_key: Option<String>,
}
fn job_key(owner: &str, id: &str) -> String {
    hex::encode(Sha256::digest(format!("{owner}:{id}").as_bytes()))
}
fn validate_job(input: &JobInput) -> ApiResult<()> {
    if input.message.trim().is_empty()
        || input.message.chars().count() > 1000
        || input.title.chars().count() > 200
    {
        return Err(ApiError::validation(
            "Provide a message of 1–1000 characters and a title of at most 200 characters",
        ));
    }
    DateTime::parse_from_rfc3339(&input.deliver_at)
        .map_err(|_| ApiError::validation("deliver_at must include an explicit timezone offset"))?;
    if !input.url.is_empty() {
        let url = reqwest::Url::parse(&input.url)
            .map_err(|_| ApiError::validation("url must be an HTTPS link"))?;
        if url.scheme() != "https" || input.url.len() > 500 {
            return Err(ApiError::validation(
                "url must be an HTTPS link of at most 500 bytes",
            ));
        }
    }
    if input
        .idempotency_key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.len() > 128)
    {
        return Err(ApiError::validation(
            "idempotency_key must contain 1–128 bytes",
        ));
    }
    Ok(())
}
pub async fn schedule(
    State(state): State<AppState>,
    principal: Principal,
    Json(mut input): Json<JobInput>,
) -> ApiResult<Json<Value>> {
    let db = authorize(&state, &principal).await?;
    let _guard = state.notification_guard.lock().await;
    validate_job(&input)?;
    input.deliver_at = DateTime::parse_from_rfc3339(&input.deliver_at)
        .map_err(ApiError::internal)?
        .with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let id = input
        .idempotency_key
        .clone()
        .unwrap_or_else(crate::ids::new_ulid);
    let key = job_key(&principal.id, &id);
    if let Some(row) = fetch_document(&db, "jobs", &key)? {
        if row.doc["request"] != serde_json::to_value(&input).map_err(ApiError::internal)? {
            return Err(ApiError::conflict(
                "This idempotency key already belongs to another notification",
            ));
        }
        return Ok(Json(row.doc));
    }
    let job = json!({"id":id,"owner":principal.id,"request":input,"state":"pending","attempts":0,"retry_at":0});
    db.writer
        .replace_document("jobs".into(), key, job.clone(), Precondition::None)
        .await?;
    Ok(Json(job))
}
pub async fn job_status(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = authorize(&state, &principal).await?;
    let row = fetch_document(&db, "jobs", &job_key(&principal.id, &id))?
        .ok_or_else(|| ApiError::not_found("Notification not found"))?;
    Ok(Json(row.doc))
}
pub async fn cancel(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = authorize(&state, &principal).await?;
    let _guard = state.notification_guard.lock().await;
    let key = job_key(&principal.id, &id);
    let mut job = fetch_document(&db, "jobs", &key)?
        .ok_or_else(|| ApiError::not_found("Notification not found"))?
        .doc;
    if job["state"] == "sent" || job["state"] == "sending" {
        return Err(ApiError::conflict(
            "This notification has already been sent or delivery has started",
        ));
    }
    job["state"] = json!("cancelled");
    db.writer
        .replace_document("jobs".into(), key, job.clone(), Precondition::None)
        .await?;
    Ok(Json(job))
}
fn pending_jobs(db: &DbHandle) -> ApiResult<Vec<(String, Value)>> {
    let conn = db.read_pool.get().map_err(ApiError::internal)?;
    if !crate::db::schema::collection_exists(&conn, "jobs").map_err(ApiError::internal)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT id, json(doc) FROM coll_jobs WHERE json_extract(doc, '$.state') IN ('pending','sending','failed') AND json_extract(doc, '$.attempts') < 3 ORDER BY json_extract(doc, '$.request.deliver_at') LIMIT 1000").map_err(ApiError::internal)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(ApiError::internal)?;
    let mut result = Vec::new();
    for row in rows {
        let (id, text) = row.map_err(ApiError::internal)?;
        if let Ok(value) = serde_json::from_str(&text) {
            result.push((id, value));
        }
    }
    Ok(result)
}
async fn tick_jobs(state: &AppState, client: &reqwest::Client) -> ApiResult<()> {
    let db = state.registry.get_or_open(PRIVATE_DB).await?;
    let now = Utc::now();
    let active: std::collections::HashSet<_> = state
        .system
        .find_principals(None)
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|p| !p.disabled)
        .map(|p| p.id)
        .collect();
    let mut counts = std::collections::HashMap::<String, u32>::new();
    for (key, _) in pending_jobs(&db)? {
        let guard = state.notification_guard.lock().await;
        // Refresh after the queue scan so a cancellation is observed before delivery.
        let Some(row) = fetch_document(&db, "jobs", &key)? else {
            continue;
        };
        let mut job = row.doc;
        if job["state"] == "cancelled" || !eligible(Some(&job), now.timestamp()) {
            continue;
        }
        let Ok(at) =
            DateTime::parse_from_rfc3339(job["request"]["deliver_at"].as_str().unwrap_or(""))
        else {
            continue;
        };
        if at.timestamp() > now.timestamp() {
            continue;
        }
        if now.timestamp() - at.timestamp() > 86400 {
            job["state"] = json!("expired");
            db.writer
                .replace_document("jobs".into(), key, job, Precondition::None)
                .await?;
            continue;
        }
        let owner = job["owner"].as_str().unwrap_or("").to_owned();
        if !active.contains(&owner) {
            continue;
        }
        let count = counts.entry(owner.clone()).or_default();
        if *count >= 5 {
            continue;
        }
        let prefs = preferences(&db, &owner)?;
        if !prefs.enabled {
            continue;
        }
        job["state"] = json!("sending");
        job["attempts"] = json!(job["attempts"].as_u64().unwrap_or(0) + 1);
        job["retry_at"] = json!(now.timestamp() + 300);
        db.writer
            .replace_document(
                "jobs".into(),
                key.clone(),
                job.clone(),
                Precondition::None,
            )
            .await?;
        drop(guard);
        let result = deliver(
            client,
            &prefs,
            job["request"]["title"].as_str().unwrap_or("Notification"),
            job["request"]["message"].as_str().unwrap_or(""),
            job["request"]["url"].as_str().unwrap_or(""),
        )
        .await;
        job["state"] = json!(if result.is_ok() { "sent" } else { "failed" });
        job["last_error"] = json!(result.err());
        db.writer
            .replace_document("jobs".into(), key, job, Precondition::None)
            .await?;
        record_status(&db, &owner, result, now.timestamp()).await?;
        *count += 1;
    }
    Ok(())
}
