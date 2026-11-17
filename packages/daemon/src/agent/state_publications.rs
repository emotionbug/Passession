use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

/// GUI content comparisons treat exactly these stamps as activity (see
/// gui/common/state_delta.rs); user-message time, attention, usage, permissions,
/// provider configuration and every other exposed field are immediate content.
pub(super) fn same_content(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return left == right;
    };
    let activity = |key: &str| matches!(key, "updatedAt" | "lastActivityAt");
    left.iter()
        .filter(|(key, _)| !activity(key))
        .all(|(key, value)| right.get(key) == Some(value))
        && right
            .keys()
            .filter(|key| !activity(key))
            .all(|key| left.contains_key(key))
}

#[derive(Clone, Default)]
pub(super) struct StatePublications(Arc<StdMutex<HashMap<String, StatePublication>>>);
struct StatePublication {
    published: Value,
    pending: Option<Value>,
    sent_at: tokio::time::Instant,
    timer: bool,
}
impl StatePublications {
    pub(super) fn remove(&self, id: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }
    pub(super) fn note_snapshot(&self, snapshot: &Value) {
        let mut states = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = snapshot["id"].as_str().and_then(|id| states.get_mut(id))
            && state.pending.is_some()
        {
            // History applications do not publish live state; their pending
            // timer must nevertheless retain the latest successful snapshot.
            state.pending = Some(snapshot.clone());
        }
    }
    pub(super) fn publish(
        &self,
        snapshot: &Value,
        sender: &tokio::sync::broadcast::Sender<Arc<Value>>,
    ) {
        let Some(id) = snapshot["id"].as_str() else {
            return;
        };
        let exposed = super::wire::snapshot_payload(snapshot);
        let mut states = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = states
            .entry(id.to_owned())
            .or_insert_with(|| StatePublication {
                published: Value::Null,
                pending: None,
                sent_at: tokio::time::Instant::now(),
                timer: false,
            });
        if state.published == exposed {
            state.pending = None;
            return;
        }
        if !same_content(&state.published, &exposed) {
            state.published = exposed;
            state.pending = None;
            state.sent_at = tokio::time::Instant::now();
            let _ = sender.send(Arc::new(json!({"type":"agent_state","agent":snapshot})));
            return;
        }
        state.pending = Some(snapshot.clone());
        if state.timer {
            return;
        }
        state.timer = true;
        let states = self.clone();
        let id = id.to_owned();
        let sender = sender.clone();
        tokio::spawn(async move {
            loop {
                let deadline = {
                    let states = states.0.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(state) = states.get(&id) else { return };
                    state.sent_at + Duration::from_secs(1)
                };
                tokio::time::sleep_until(deadline).await;
                let mut states = states.0.lock().unwrap_or_else(|e| e.into_inner());
                let Some(state) = states.get_mut(&id) else {
                    return;
                };
                if state.sent_at + Duration::from_secs(1) > tokio::time::Instant::now() {
                    continue;
                }
                if let Some(snapshot) = state.pending.take() {
                    let exposed = super::wire::snapshot_payload(&snapshot);
                    if state.published != exposed {
                        state.published = exposed;
                        state.sent_at = tokio::time::Instant::now();
                        let _ =
                            sender.send(Arc::new(json!({"type":"agent_state","agent":snapshot})));
                    }
                }
                state.timer = false;
                return;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn timestamp_bursts_have_one_latest_trailing_publish_and_visible_changes_are_immediate() {
        let publications = StatePublications::default();
        let (sender, mut events) = tokio::sync::broadcast::channel(32);
        let mut snapshot = json!({"id":"agent","title":"old","updatedAt":"original"});
        snapshot["lifecycle"] = json!("running");
        publications.publish(&snapshot, &sender);
        assert_eq!(events.try_recv().unwrap()["agent"]["updatedAt"], "original");
        for timestamp in ["one", "two", "latest"] {
            snapshot["updatedAt"] = json!(timestamp);
            publications.publish(&snapshot, &sender);
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(events.try_recv().unwrap()["agent"]["updatedAt"], "latest");
        assert!(events.try_recv().is_err());
        snapshot["updatedAt"] = json!("pending");
        publications.publish(&snapshot, &sender);
        snapshot["title"] = json!("visible");
        publications.publish(&snapshot, &sender);
        assert_eq!(events.try_recv().unwrap()["agent"]["title"], "visible");
        snapshot["updatedAt"] = json!("after visible");
        publications.publish(&snapshot, &sender);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            events.try_recv().unwrap()["agent"]["updatedAt"],
            "after visible"
        );
        assert!(events.try_recv().is_err());
        snapshot["_private"] = json!("not displayed");
        publications.publish(&snapshot, &sender);
        assert!(events.try_recv().is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn manager_publication_needs_no_storage_and_removal_cancels_pending_state() {
        let manager = super::super::AgentManager::new(std::sync::Weak::new());
        assert!(manager.storage.get().is_none());
        let mut events = manager.subscribe_events();
        let mut snapshot = json!({"id":"agent","lifecycle":"running","updatedAt":"original"});
        manager.publish_state(&snapshot);
        assert_eq!(events.try_recv().unwrap()["agent"]["updatedAt"], "original");
        snapshot["updatedAt"] = json!("pending");
        manager.publish_state(&snapshot);
        manager.publications.remove("agent");
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(events.try_recv().is_err());
    }
}
