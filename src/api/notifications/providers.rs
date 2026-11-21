//! Provider adapters own their credential formats and fixed outbound endpoints.
use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Provider {
    #[default]
    Pushover,
    Telegram,
}

pub(super) async fn validate(client: &reqwest::Client, prefs: &Preferences) -> ApiResult<()> {
    match prefs.provider {
        Provider::Pushover => pushover_validate(client, prefs).await,
        Provider::Telegram => telegram_validate_at(client, prefs, TELEGRAM_API).await,
    }
}
pub(super) async fn deliver(
    client: &reqwest::Client,
    prefs: &Preferences,
    title: &str,
    message: &str,
    url: &str,
) -> Result<(), &'static str> {
    match prefs.provider {
        Provider::Pushover => pushover_deliver(client, prefs, title, message, url).await,
        Provider::Telegram => {
            telegram_deliver_at(client, prefs, title, message, url, TELEGRAM_API).await
        }
    }
}

async fn pushover_validate(client: &reqwest::Client, prefs: &Preferences) -> ApiResult<()> {
    let response = client
        .post("https://api.pushover.net/1/users/validate.json")
        .form(&[("token", &prefs.token), ("user", &prefs.user_key)])
        .send()
        .await
        .map_err(|_| ApiError::bad_request("Could not reach Pushover. Please try again"))?;
    let ok = response.status().is_success();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !ok || body["status"] != 1 {
        return Err(ApiError::validation(
            "Pushover could not validate these keys. Check your application token and user key",
        ));
    }
    Ok(())
}

async fn pushover_deliver(
    client: &reqwest::Client,
    prefs: &Preferences,
    title: &str,
    message: &str,
    url: &str,
) -> Result<(), &'static str> {
    let message: String = message.chars().take(1000).collect();
    let title: String = title.chars().take(200).collect();
    let response = client
        .post("https://api.pushover.net/1/messages.json")
        .form(&[
            ("token", prefs.token.as_str()),
            ("user", prefs.user_key.as_str()),
            ("title", title.as_str()),
            ("message", message.as_str()),
            ("url", url),
            ("url_title", "Open app"),
            ("priority", "0"),
        ])
        .send()
        .await
        .map_err(|_| "Pushover could not be reached; delivery will retry")?;
    let ok = response.status().is_success();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if ok && body["status"] == 1 {
        Ok(())
    } else {
        Err("Pushover rejected delivery; check your keys and message quota")
    }
}

const TELEGRAM_API: &str = "https://api.telegram.org";

pub(super) fn valid_telegram_token(value: &str) -> bool {
    let Some((id, secret)) = value.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|b| b.is_ascii_digit())
        && (20..=200).contains(&secret.len())
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub(super) fn valid_telegram_chat(value: &str) -> bool {
    if let Some(name) = value.strip_prefix('@') {
        return !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    }
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<i64>().is_ok_and(|id| id != 0)
}
// The production caller always uses TELEGRAM_API. A private base argument lets
// tests exercise the actual HTTP transport against a loopback mock server.
async fn telegram_call(
    client: &reqwest::Client,
    prefs: &Preferences,
    method: &str,
    body: Value,
    base: &str,
) -> Result<Value, &'static str> {
    if !valid_telegram_token(&prefs.telegram_bot_token) {
        return Err("Telegram bot token is invalid");
    }
    let response = client
        .post(format!("{base}/bot{}/{method}", prefs.telegram_bot_token))
        .json(&body)
        .send()
        .await
        .map_err(|_| "Telegram could not be reached; delivery will retry")?;
    let ok = response.status().is_success();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    // Never surface Telegram response text or reqwest errors: the bot token is
    // part of the request URL and must not appear in logs or user-facing errors.
    if ok && value["ok"] == true {
        Ok(value["result"].clone())
    } else {
        Err("Telegram rejected the request; check the bot token, chat ID, and bot access")
    }
}
async fn telegram_validate_at(
    client: &reqwest::Client,
    prefs: &Preferences,
    base: &str,
) -> ApiResult<()> {
    telegram_call(client, prefs, "getMe", json!({}), base)
        .await
        .map_err(ApiError::validation)?;
    telegram_call(client,prefs,"getChat",json!({"chat_id":prefs.telegram_chat_id}),base).await
        .map_err(|_| ApiError::validation("Telegram cannot access this chat. Open your bot and press Start, or add it to the target chat, then check the chat ID"))?;
    Ok(())
}
fn telegram_payload(prefs: &Preferences, title: &str, message: &str, url: &str) -> Value {
    let title: String = title.chars().take(200).collect();
    let message: String = message.chars().take(1000).collect();
    let text = if title.is_empty() {
        message
    } else {
        format!("{title}\n\n{message}")
    };
    let mut payload = json!({"chat_id":prefs.telegram_chat_id,"text":text,"link_preview_options":{"is_disabled":true}});
    if !url.is_empty() {
        payload["reply_markup"] = json!({"inline_keyboard":[[{"text":"Open app","url":url}]]});
    }
    payload
}
async fn telegram_deliver_at(
    client: &reqwest::Client,
    prefs: &Preferences,
    title: &str,
    message: &str,
    url: &str,
    base: &str,
) -> Result<(), &'static str> {
    telegram_call(
        client,
        prefs,
        "sendMessage",
        telegram_payload(prefs, title, message, url),
        base,
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod telegram_tests {
    use super::*;
    use axum::{
        extract::{OriginalUri, State},
        http::StatusCode,
        routing::post,
        Json, Router,
    };
    use std::collections::VecDeque;
    type Requests = Arc<tokio::sync::Mutex<Vec<(String, Value)>>>;
    type Replies = Arc<tokio::sync::Mutex<VecDeque<(StatusCode, Value)>>>;
    async fn mock(
        replies: Vec<(StatusCode, Value)>,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        let requests: Requests = Arc::default();
        let replies: Replies = Arc::new(tokio::sync::Mutex::new(replies.into()));
        let app = Router::new()
            .route(
                "/{*path}",
                post(
                    |State((requests, replies)): State<(Requests, Replies)>,
                     OriginalUri(uri): OriginalUri,
                     Json(body): Json<Value>| async move {
                        requests.lock().await.push((uri.path().to_owned(), body));
                        let (status, body) = replies
                            .lock()
                            .await
                            .pop_front()
                            .expect("unexpected outbound request");
                        (status, Json(body))
                    },
                ),
            )
            .with_state((requests.clone(), replies));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, requests, task)
    }
    fn prefs() -> Preferences {
        Preferences {
            provider: Provider::Telegram,
            telegram_bot_token: format!("123456:{}", "a".repeat(35)),
            telegram_chat_id: "-1001234567890".into(),
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn validation_does_not_send_and_delivery_includes_link() {
        let (base, requests, task) = mock(vec![
            (StatusCode::OK, json!({"ok":true,"result":{"id":123456}})),
            (
                StatusCode::OK,
                json!({"ok":true,"result":{"id":-1001234567890_i64}}),
            ),
            (StatusCode::OK, json!({"ok":true,"result":{"message_id":1}})),
        ])
        .await;
        let client = http_client().unwrap();
        let prefs = prefs();
        telegram_validate_at(&client, &prefs, &base).await.unwrap();
        assert_eq!(requests.lock().await.len(), 2);
        telegram_deliver_at(
            &client,
            &prefs,
            "Task <title>",
            "Plain *text*",
            "https://example.org/?task=123",
            &base,
        )
        .await
        .unwrap();
        let calls = requests.lock().await;
        assert!(
            calls[0].0.ends_with("/getMe")
                && calls[1].0.ends_with("/getChat")
                && calls[2].0.ends_with("/sendMessage")
        );
        assert_eq!(calls[2].1["chat_id"], "-1001234567890");
        assert_eq!(calls[2].1["text"], "Task <title>\n\nPlain *text*");
        assert!(calls[2].1.get("parse_mode").is_none());
        assert_eq!(
            calls[2].1["reply_markup"]["inline_keyboard"][0][0]["url"],
            "https://example.org/?task=123"
        );
        task.abort();
    }
    #[tokio::test]
    async fn provider_failures_do_not_expose_bot_token() {
        let prefs = prefs();
        for status in [
            StatusCode::OK,
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let (base, _, task) = mock(vec![(
                status,
                json!({"ok":false,"description":prefs.telegram_bot_token}),
            )])
            .await;
            let error =
                telegram_deliver_at(&http_client().unwrap(), &prefs, "Title", "Body", "", &base)
                    .await
                    .unwrap_err();
            assert!(!error.contains(&prefs.telegram_bot_token));
            task.abort();
        }
        let error = telegram_deliver_at(
            &http_client().unwrap(),
            &prefs,
            "Title",
            "Body",
            "",
            "http://127.0.0.1:1",
        )
        .await
        .unwrap_err();
        assert!(!error.contains(&prefs.telegram_bot_token));
    }
    #[test]
    fn validates_destinations_and_rejects_url_injection() {
        assert!(valid_telegram_token(&prefs().telegram_bot_token));
        assert!(!valid_telegram_token("123:aaaaaaaaaaaaaaaaaaaa/other"));
        assert!(
            valid_telegram_chat("123456789")
                && valid_telegram_chat("-1001234567890")
                && valid_telegram_chat("@my_channel")
        );
        assert!(
            !valid_telegram_chat("0")
                && !valid_telegram_chat("https://example.org")
                && !valid_telegram_chat("999999999999999999999999")
        );
        let payload = telegram_payload(&prefs(), "", "Body", "");
        assert_eq!(payload["text"], "Body");
        assert!(payload.get("reply_markup").is_none());
    }
}
