//! Optional Delegate workspace adapter. Generic notification jobs do not depend on it.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct Notice {
    pub key: String,
    pub title: String,
    pub message: String,
    pub task_id: Option<String>,
    pub reminder_id: String,
    pub reminder_at: String,
}
pub(super) fn notices(workspace: &Value, prefs: &Preferences, now: DateTime<Utc>) -> Vec<Notice> {
    let mut notices = Vec::new();
    let active: Vec<&Value> = workspace["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["archived"] != true && t["status"] != "done")
        .collect();
    for task in &active {
        let Some(at) = task["reminder_at"].as_str() else {
            continue;
        };
        let Ok(date) = DateTime::parse_from_rfc3339(at) else {
            continue;
        };
        // Catch up after a short outage, without sending an old backlog weeks later.
        if date.timestamp() > now.timestamp() || now.timestamp() - date.timestamp() > 86400 {
            continue;
        }
        let Some(id) = task["id"].as_str() else {
            continue;
        };
        let revision = task["reminder_id"].as_str().unwrap_or("");
        notices.push(Notice {
            key: format!("task:{id}:{at}:{revision}"),
            title: "Delegate · Task reminder".into(),
            message: task["title"].as_str().unwrap_or("Task reminder").into(),
            task_id: Some(id.into()),
            reminder_id: revision.into(),
            reminder_at: at.into(),
        });
    }
    if prefs.morning_enabled {
        if let (Ok(tz), Some(scheduled)) =
            (prefs.timezone.parse::<Tz>(), minutes(&prefs.morning_time))
        {
            let local = now.with_timezone(&tz);
            let clock = local.hour() * 60 + local.minute();
            // A one-hour catch-up window accommodates a restart without an afternoon digest.
            if clock >= scheduled && clock < scheduled + 60 {
                let today = local.format("%Y-%m-%d").to_string();
                let mut due: Vec<_> = active
                    .iter()
                    .filter(|t| {
                        t["due_date"]
                            .as_str()
                            .is_some_and(|date| !date.is_empty() && date <= today.as_str())
                    })
                    .collect();
                due.sort_by_key(|t| t["due_date"].as_str().unwrap_or(""));
                let mut message = if due.is_empty() {
                    "No tasks due today. Enjoy the breathing room.".into()
                } else {
                    format!("{} task(s) due today or overdue:", due.len())
                };
                for t in due.iter().take(5) {
                    message.push_str(&format!("\n• {}", t["title"].as_str().unwrap_or("Task")));
                }
                if due.len() > 5 {
                    message.push_str(&format!("\n…and {} more.", due.len() - 5));
                }
                notices.push(Notice {
                    key: format!("morning:{today}"),
                    title: "Delegate · Your morning summary".into(),
                    message,
                    task_id: None,
                    reminder_id: String::new(),
                    reminder_at: String::new(),
                });
            }
        }
    }
    notices
}
