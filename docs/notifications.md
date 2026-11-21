# Notification service

TinyLord can schedule notifications independently of an app's browser session. Supported providers are Pushover and Telegram. Provider-specific validation and HTTP delivery are isolated in `src/api/notifications/providers.rs`; other providers can be added without changing the queue API.

## Enable the service

```toml
[notifications]
enabled = true
```

Encryption must be enabled. The service stores credentials, jobs, and delivery receipts in `__notifications.db` using the same encryption key as the datastore. This internal database is not registered, cannot be addressed through public document APIs, and is not included in per-database admin snapshots. Include it with the other encrypted database files in filesystem backups.

The service runs once per minute. Each principal has its own provider settings and jobs. Authentication uses normal TinyLord bearer tokens; one principal cannot inspect, configure, or cancel another principal's jobs. Disabled principals do not send notifications. Jobs can be queued while their provider is disabled and wait for it to be enabled.

## Configure Pushover

`PUT /v1/notifications/settings`:

```json
{
  "provider": "pushover",
  "enabled": true,
  "token": "YOUR_APPLICATION_TOKEN",
  "user_key": "YOUR_PUSHOVER_USER_KEY"
}
```

The application token and user key must each contain 30 letters or digits. Enabling calls Pushover's validation endpoint before saving. Omitted or blank keys preserve saved values. `GET /v1/notifications/settings` returns the active `provider`, `enabled`, `configured`, per-provider configured flags in `providers`, and the most recent delivery result, but never credentials or Telegram chat IDs. Send `enabled: false` to pause delivery. Send `provider`, `enabled: false`, and `disconnect: true` to erase that provider’s credentials. Disconnecting an inactive provider preserves the active provider and its enabled state. A disconnected account can retain queued jobs; they expire if more than 24 hours overdue.

`POST /v1/notifications/test` sends a test to the configured account, limited to one per minute. Normal notification priority respects Pushover quiet hours.

## Configure Telegram

Create a bot with [BotFather](https://t.me/BotFather), open the bot and press Start for personal notifications, then supply its token and the destination chat ID:

```json
{
  "provider": "telegram",
  "enabled": true,
  "telegram_bot_token": "YOUR_BOTFATHER_TOKEN",
  "telegram_chat_id": "YOUR_CHAT_ID"
}
```

Send this body to `PUT /v1/notifications/settings`, or pass it to `client.notifications().configure(...)`. A chat ID may be a signed numeric string (including negative group IDs) or a public channel username such as `@my_channel`. Add the bot to a group before using it; channels need permission for the bot to post. Topic-specific forum delivery is not supported in this release.

Enabling checks the bot using `getMe` and the destination using `getChat`; these checks do not send a message. Actual notifications use `sendMessage`, plain text, and an optional Open app link button. See the [Telegram Bot API](https://core.telegram.org/bots/api#sendmessage). A successful validation does not guarantee delivery if access changes later; delivery errors appear in the notification status.

Each provider retains its own encrypted credentials. Switching back to Pushover can reuse its saved token and user key; blank fields preserve the selected provider’s saved credentials. Only one provider is active at a time, and changing the active provider affects pending jobs and future task reminders. Already delivered notifications are not replayed. Provider selection alone in the app does not change server settings until saved.

Bot tokens appear in Telegram's API URL, so HTTP errors and Telegram response descriptions are deliberately replaced by safe error messages. No raw token or chat ID is returned from the settings API. Telegram delivery, validation, and failure paths are tested against a loopback mock server; a live Telegram account has not been tested.

## Schedule, inspect, cancel

```javascript
const notifications = client.notifications();
const job = await notifications.schedule({
  title: "Appointment",
  message: "Your appointment starts in 15 minutes.",
  deliver_at: "2030-06-12T09:45:00-06:00",
  url: "https://example.org/appointments/123",
  idempotency_key: "appointment-123-reminder"
});
await notifications.status(job.id);
await notifications.cancel(job.id);
```

Equivalent endpoints:

- `POST /v1/notifications` — schedule; returns the durable job.
- `GET /v1/notifications/{id}` — read status.
- `DELETE /v1/notifications/{id}` — cancel before delivery starts.

`deliver_at` must be RFC 3339 with an explicit offset. Times are normalized to UTC. `message` is required (1–1000 characters); titles are at most 200 characters. An optional `url` must be HTTPS and at most 500 bytes; it is a notification link, never fetched by the server. The optional idempotency key is scoped to the principal. Repeating the same key and normalized payload returns the original job; a different payload returns 409. Jobs are not rescheduled in place: cancel the old job and use a new key.

States: `pending`, `sending`, `sent`, `failed`, `cancelled`, and `expired`. Delivery retries after five minutes, up to three attempts. More than 24 hours overdue expires without sending. At most five generic jobs per principal are attempted in one tick. A confirmed successful delivery is not sent again after a restart. Delivery is best effort: if the provider accepts a request but the response is lost, a retry can produce a duplicate. Exactly-once phone delivery is not guaranteed. `sent` means provider acceptance, not that the recipient read it.

## Optional Delegate adapter

```toml
[notifications.delegate]
enabled = true
public_url = "https://delegate.tud.ink/"
```

The adapter reads each enabled principal's `delegate/workspaces/workspace-{principal_id}` document after checking its current write grant. Active tasks with `reminder_at` (RFC 3339) are eligible; `reminder_id` identifies each scheduling/snooze operation. Done, archived, deleted, and cancelled tasks are skipped. The task is rechecked just before sending. Receipt writes never replace the workspace, so delivering a reminder cannot overwrite app edits.

The adapter also supports optional settings `morning_enabled`, `morning_time` (HH:MM, default 09:00), and `timezone` (IANA name, default UTC). The daily summary covers today's and overdue tasks, sends once per local date, and has a one-hour catch-up window. Task reminders catch up for up to 24 hours. The adapter attempts at most five messages per principal per tick, separately from generic jobs.

Notification links include `?task=<id>`. Delegate opens that task after sign-in and offers Done and Snooze. Completing a recurring task clears that occurrence's reminder; the next occurrence needs its own reminder.

## Validation

`cargo test` exercises notification selection, daylight-saving time, retry receipts, private credential responses, user isolation, queue idempotency, cancellation, and the encrypted internal database. Provider delivery requires a real configured account; no live push is sent by the automated suite.
