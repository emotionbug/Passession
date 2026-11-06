//! Which daemon attention events become OS notifications (original
//! ui/client_activity.dart `_notify` and `_updateBadge`).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct NotificationRequest {
    pub id: i32,
    pub title: String,
    pub body: Option<String>,
    /// Click payload; routed through `PlatformRoute::from_notification_data`.
    pub data: Map<String, Value>,
    /// Deduplicates deliveries of the same event (bounded to 512 keys).
    pub event_key: Option<String>,
    /// `None` uses the desktop "play sound" setting (always sound on mobile).
    pub sound: Option<bool>,
}

/// What the focused window currently shows.
#[derive(Debug, Clone, Default)]
pub struct AttentionView {
    pub visible: bool,
    pub server_id: Option<String>,
    /// `"agent"` or `"execution_log"`.
    pub kind: Option<String>,
    pub id: Option<String>,
}

impl AttentionView {
    fn shows(&self, server_id: &str, kind: &str, id: &str) -> bool {
        self.visible
            && self.server_id.as_deref() == Some(server_id)
            && self.kind.as_deref() == Some(kind)
            && self.id.as_deref() == Some(id)
    }
}

fn text<'a>(map: &'a Value, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Maps an `agent_attention_required` / `terminal_attention_required` host
/// event to a notification, or `None` when it must not notify.
pub fn attention_notification(server_id: &str, event: &Value, view: &AttentionView) -> Option<NotificationRequest> {
    let kind = text(event, "type");
    let payload = event.get("payload").filter(|p| p.is_object())?;
    if payload.get("shouldNotify") != Some(&Value::Bool(true)) {
        return None;
    }
    let empty = Value::Object(Map::new());
    let (title, body, data, event_key) = match kind {
        "agent_attention_required" => {
            let agent_id = text(payload, "agentId");
            let reason = payload.get("reason").cloned().unwrap_or(Value::Null);
            if reason == "error" || view.shows(server_id, "agent", agent_id) {
                return None;
            }
            let notification = payload.get("notification").filter(|n| n.is_object()).unwrap_or(&empty);
            let fallback = if reason == "permission" { "Approval requested" } else { "Session finished" };
            let title = notification
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(fallback)
                .to_owned();
            let body = text(notification, "body").to_owned();
            let mut data = Map::new();
            data.insert("serverId".into(), server_id.into());
            data.insert("agentId".into(), agent_id.into());
            data.insert("reason".into(), reason.clone());
            if let Some(Value::Object(extra)) = notification.get("data") {
                for (key, value) in extra {
                    data.insert(key.clone(), value.clone());
                }
            }
            let key = json!([server_id, agent_id, reason, payload.get("timestamp").cloned().unwrap_or(Value::Null)]);
            (title, body, data, key.to_string())
        }
        "terminal_attention_required" => {
            let terminal_id = text(payload, "terminalId");
            if view.shows(server_id, "execution_log", terminal_id) {
                return None;
            }
            let title = text(payload, "title").to_owned();
            let body = text(payload, "body").to_owned();
            let mut data = Map::new();
            let payload_server = payload.get("serverId").and_then(Value::as_str).unwrap_or(server_id);
            data.insert("serverId".into(), payload_server.into());
            data.insert("terminalId".into(), terminal_id.into());
            data.insert("cwd".into(), payload.get("cwd").cloned().unwrap_or(Value::Null));
            if let Some(workspace) = payload.get("workspaceId").filter(|v| !v.is_null()) {
                data.insert("workspaceId".into(), workspace.clone());
            }
            let key = json!([server_id, terminal_id, payload.get("reason").cloned().unwrap_or(Value::Null), title, body]);
            (title, body, data, key.to_string())
        }
        _ => return None,
    };
    Some(NotificationRequest {
        id: notification_id(&event_key),
        title,
        body: Some(body),
        data,
        event_key: Some(event_key),
        sound: None,
    })
}

/// First four bytes of SHA-256(eventKey), big endian, masked to 31 bits.
pub fn notification_id(event_key: &str) -> i32 {
    let hash = Sha256::digest(event_key.as_bytes());
    let value = (u32::from(hash[0]) << 24) | (u32::from(hash[1]) << 16) | (u32::from(hash[2]) << 8) | u32::from(hash[3]);
    (value & 0x7fff_ffff) as i32
}

/// Original `_updateBadge`: agents requiring attention that are not archived.
/// Each item is an agent row (`{agent: {...}}`) or a bare agent object.
pub fn attention_badge_count<'a>(agents: impl Iterator<Item = &'a Value>) -> u32 {
    agents
        .filter(|row| {
            let agent = match row.get("agent") {
                Some(Value::Object(map)) if !map.is_empty() => row.get("agent").unwrap(),
                _ => row,
            };
            agent.get("requiresAttention") == Some(&Value::Bool(true))
                && agent.get("archivedAt").is_none_or(Value::is_null)
        })
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_attention_matches_original_payload() {
        let event = json!({"type":"agent_attention_required","payload":{"shouldNotify":true,"agentId":"a","reason":"permission","timestamp":"t","notification":{"body":"b","data":{"x":1}}}});
        let request = attention_notification("s", &event, &AttentionView::default()).unwrap();
        assert_eq!(request.title, "Approval requested");
        assert_eq!(request.body.as_deref(), Some("b"));
        assert_eq!(request.data["x"], 1);
        assert_eq!(request.event_key.as_deref(), Some(r#"["s","a","permission","t"]"#));
        let focused = AttentionView { visible: true, server_id: Some("s".into()), kind: Some("agent".into()), id: Some("a".into()) };
        assert!(attention_notification("s", &event, &focused).is_none());
    }

    #[test]
    fn errors_and_unflagged_events_do_not_notify() {
        let error = json!({"type":"agent_attention_required","payload":{"shouldNotify":true,"agentId":"a","reason":"error"}});
        assert!(attention_notification("s", &error, &AttentionView::default()).is_none());
        let quiet = json!({"type":"terminal_attention_required","payload":{"shouldNotify":false}});
        assert!(attention_notification("s", &quiet, &AttentionView::default()).is_none());
    }

    #[test]
    fn badge_counts_unarchived_attention() {
        let rows = [json!({"agent":{"requiresAttention":true}}), json!({"requiresAttention":true,"archivedAt":"x"}), json!({"requiresAttention":false})];
        assert_eq!(attention_badge_count(rows.iter()), 1);
    }
}
