//! Port of `lib/features/chat/chat_delivery.dart`: host-scoped delivery that runs regardless
//! of panel visibility. Receipts are followed through an owned `session.events`
//! subscription plus exact-ID queries; client queues drain FIFO into an idle agent.

use super::drafts::*;
use super::json::*;
use super::service::{ComposerService, Pending, PanelNotice, SendJob, SendOrigin};
use paseo_gui_core::CoreError;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct HostDelivery {
    /// Draft keys whose drain is in flight.
    pub busy: HashSet<String>,
    /// `key:messageId` exact receipt queries issued on this connection.
    pub queried: HashSet<String>,
    pub receipt_observation: Option<String>,
    pub subscribing: bool,
    /// Draft key → timeline observation id (wakes the drain on turn events).
    pub observations: HashMap<String, String>,
    /// Observation ids that already delivered their snapshot.
    pub observed: HashSet<String>,
    pub generation: u64,
}

/// Events that can make a held queue drainable (Flutter `_event`).
fn wakes_queue(event: &Value) -> bool {
    let kind = s(get(event, "type"));
    let payload = get(event, "payload");
    kind == "agent_permission_resolved"
        || (kind == "agent_update" && get(payload, "agent").is_object() && get(get(payload, "agent"), "preparation").is_null())
        || (kind == "agent_stream"
            && matches!(
                s(get(get(payload, "event"), "type")),
                "turn_completed" | "turn_canceled" | "turn_failed" | "permission_resolved"
            ))
}

impl ComposerService {
    pub(super) fn on_host_event(&mut self, host: &str, event: &Value) {
        if self.supports(host, "agentMessageReceipts") {
            let receipt = get(get(event, "payload"), "receipt").clone();
            self.apply_receipt_everywhere(host, &receipt);
        }
        let observation_ids: Vec<String> = list_ref(get(event, "observations")).iter().map(display).collect();
        if s(get(event, "type")) == "subscription.restored" {
            let id = owned(get(get(event, "payload"), "observationId"));
            if let Some(delivery) = self.delivery.get_mut(host)
                && delivery.receipt_observation.as_deref() == Some(id.as_str())
            {
                delivery.queried.clear();
            }
            self.schedule(host);
        }
        self.creation_event(host, event, &observation_ids);
        if wakes_queue(event) {
            self.schedule(host);
        }
    }

    pub(super) fn connection_changed(&mut self, host: &str, closed: bool) {
        let generation = self.hosts.get(host).map(|h| h.generation).unwrap_or(0);
        let delivery = self.delivery.entry(host.to_owned()).or_default();
        if delivery.generation != generation {
            delivery.generation = generation;
            delivery.queried.clear();
        }
        self.creation_connection_changed(host, closed);
        self.schedule(host);
    }

    /// Flutter `_schedule` (the microtask body).
    pub(super) fn schedule(&mut self, host: &str) {
        if !self.connected(host) || !self.drafts.is_loaded() {
            return;
        }
        let Some(core) = self.core.clone() else { return };
        let receipts = self.supports(host, "agentMessageReceipts");
        let keys = self.drafts.agent_keys_for_server(host);
        for key in keys {
            if receipts {
                let delivery = self.delivery.entry(host.to_owned()).or_default();
                if delivery.receipt_observation.is_none() && !delivery.subscribing {
                    delivery.subscribing = true;
                    let request = core.observe(
                        host,
                        json!({"type": "session.events.set_subscription.request", "events": ["agent.message_receipt.update"]}),
                        "session.events.set_subscription.response",
                    );
                    self.pending.insert(request, Pending::ReceiptObservation { host: host.to_owned() });
                }
                let current = self.drafts.draft(&key).clone();
                let mut uncertain = vec![get(&current, "pending").clone()];
                uncertain.extend(
                    maps(get(&current, "queue"))
                        .into_iter()
                        .filter(|item| matches!(s(get(item, "state")), "sending" | "unacknowledged")),
                );
                for message in uncertain {
                    let id = owned(get(&message, "messageId"));
                    if id.is_empty() || self.drafts.is_dismissed(&key, &id) {
                        continue;
                    }
                    if self.active_sends.get(&key).is_some_and(|active| active == &id) {
                        continue;
                    }
                    let fresh = self
                        .delivery
                        .entry(host.to_owned())
                        .or_default()
                        .queried
                        .insert(format!("{key}:{id}"));
                    if fresh {
                        self.check_receipt(host, &key, &id, false, None);
                    }
                }
            }
            let draft = self.drafts.draft(&key).clone();
            let queue = maps(get(&draft, "queue"));
            let busy = self.delivery.get(host).is_some_and(|d| d.busy.contains(&key));
            if !queue.is_empty()
                && !non_empty_obj(get(&draft, "pending"))
                && s(get(&queue[0], "state")) == "queued"
                && !busy
                && !self.active_sends.contains_key(&key)
            {
                self.drain(host, &key);
            } else if queue.is_empty()
                && let Some(delivery) = self.delivery.get_mut(host)
                && let Some(observation) = delivery.observations.remove(&key)
            {
                delivery.observed.remove(&observation);
                core.cancel_request(&observation);
            }
        }
    }

    /// Exact-ID receipt query (Flutter `ChatDelivery.check`).
    pub fn check_receipt(&mut self, host: &str, key: &str, id: &str, dismiss: bool, panel: Option<u64>) {
        let Some(core) = self.core.clone() else { return };
        if !self.supports(host, "agentMessageReceipts") {
            self.notify(
                panel,
                PanelNotice::CheckFinished {
                    key: key.to_owned(),
                    message_id: id.to_owned(),
                    error: Some("This host cannot report or cancel message delivery.".into()),
                },
            );
            return;
        }
        let identity = key_identity(key);
        let agent = identity.get(2).cloned().unwrap_or_default();
        let operation = if dismiss { "dismiss" } else { "get" };
        let request = core.request(
            host,
            json!({"type": format!("agent.message_receipt.{operation}.request"), "agentId": agent, "messageId": id}),
            &format!("agent.message_receipt.{operation}.response"),
        );
        self.pending.insert(
            request,
            Pending::Receipt { host: host.to_owned(), key: key.to_owned(), id: id.to_owned(), dismiss, panel },
        );
    }

    pub(super) fn receipt_result(
        &mut self,
        host: &str,
        key: &str,
        id: &str,
        dismiss: bool,
        panel: Option<u64>,
        result: Result<Value, CoreError>,
    ) {
        let agent = key_identity(key).get(2).cloned().unwrap_or_default();
        let request_type = if dismiss { "agent.message_receipt.dismiss.request" } else { "agent.message_receipt.get.request" };
        let outcome = match result {
            Ok(payload) => match paseo_gui_core::response_error(&payload, request_type) {
                Some(error) => Err(error.message),
                None => {
                    let receipt = get(&payload, "receipt").clone();
                    let valid = s(get(&receipt, "agentId")) == agent
                        && s(get(&receipt, "messageId")) == id
                        && get(&receipt, "revision").is_i64()
                        && (!dismiss || get(&receipt, "dismissed") == &Value::Bool(true));
                    if valid { Ok(receipt) } else { Err("Invalid message receipt identity or dismissal".to_owned()) }
                }
            },
            Err(error) => Err(error.message),
        };
        let error = match outcome {
            Ok(receipt) => {
                if dismiss && get(&receipt, "dismissed") == &Value::Bool(true) {
                    self.drafts.dismiss(key, id);
                }
                self.drafts.apply_receipt(key, &receipt);
                if dismiss && !self.drafts.is_dismissed(key, id) {
                    Some("The host has not confirmed dismissal.".to_owned())
                } else {
                    None
                }
            }
            // Keep uncertainty durable; reconnect or an explicit Check retries.
            Err(error) => Some(error),
        };
        if dismiss && error.is_none() && self.active_sends.get(key).is_some_and(|active| active == id) {
            self.active_sends.remove(key);
        }
        if panel.is_some() {
            self.flush_then(super::service::AfterFlush::Nothing { panel: None });
        }
        self.notify(panel, PanelNotice::CheckFinished { key: key.to_owned(), message_id: id.to_owned(), error });
        let _ = host;
    }

    pub(super) fn receipt_observation_result(&mut self, host: &str, request_id: &str, result: Result<Value, CoreError>) {
        let delivery = self.delivery.entry(host.to_owned()).or_default();
        delivery.subscribing = false;
        match result {
            // Exact-ID recovery still works when event subscription is unavailable.
            Ok(_) => delivery.receipt_observation = Some(request_id.to_owned()),
            Err(_) => delivery.receipt_observation = None,
        }
        self.schedule(host);
    }

    /// Flutter `_drain`: steer the first queued message into an idle agent.
    fn drain(&mut self, host: &str, key: &str) {
        let Some(core) = self.core.clone() else { return };
        let delivery = self.delivery.entry(host.to_owned()).or_default();
        if !delivery.busy.insert(key.to_owned()) {
            return;
        }
        let agent = key_identity(key).get(2).cloned().unwrap_or_default();
        match delivery.observations.get(key).cloned() {
            Some(observation) if delivery.observed.contains(&observation) => {
                let request = core.request(host, json!({"type": "fetch_agent_request", "agentId": agent}), "fetch_agent_response");
                self.pending.insert(request, Pending::DrainFetch { host: host.to_owned(), key: key.to_owned() });
            }
            Some(_) => {
                // The subscription snapshot is still pending; it continues the drain.
            }
            None => {
                let observation = core.observe(
                    host,
                    json!({"type": "agent.timeline.set_subscription.request", "agentIds": [agent]}),
                    "agent.timeline.set_subscription.response",
                );
                delivery.observations.insert(key.to_owned(), observation.clone());
                self.pending.insert(observation, Pending::TimelineObservation { host: host.to_owned(), key: key.to_owned() });
            }
        }
    }

    fn block_queue(&mut self, host: &str, key: &str, error: String) {
        let mut queue = maps(&self.drafts.field(key, "queue"));
        if let Some(first) = queue.first_mut() {
            // A failed readiness/observation check sent nothing; preserve queue order.
            set(first, "state", json!("blocked"));
            set(first, "error", json!(error));
        }
        self.drafts.update(key, json!({"queue": queue}));
        if let Some(delivery) = self.delivery.get_mut(host) {
            delivery.busy.remove(key);
        }
    }

    pub(super) fn timeline_observation_result(&mut self, host: &str, key: &str, request_id: &str, result: Result<Value, CoreError>) {
        match result {
            Ok(_) => {
                let Some(core) = self.core.clone() else { return };
                let delivery = self.delivery.entry(host.to_owned()).or_default();
                if delivery.observations.get(key).map(String::as_str) != Some(request_id) {
                    core.cancel_request(request_id);
                    delivery.busy.remove(key);
                    return;
                }
                delivery.observed.insert(request_id.to_owned());
                let agent = key_identity(key).get(2).cloned().unwrap_or_default();
                let request = core.request(host, json!({"type": "fetch_agent_request", "agentId": agent}), "fetch_agent_response");
                self.pending.insert(request, Pending::DrainFetch { host: host.to_owned(), key: key.to_owned() });
            }
            Err(error) => {
                if let Some(delivery) = self.delivery.get_mut(host) {
                    delivery.observations.remove(key);
                }
                self.block_queue(host, key, error.message);
            }
        }
    }

    pub(super) fn drain_fetch_result(&mut self, host: &str, key: &str, result: Result<Value, CoreError>) {
        let payload = match result {
            Ok(payload) => match paseo_gui_core::response_error(&payload, "fetch_agent_request") {
                Some(error) => return self.block_queue(host, key, error.message),
                None => payload,
            },
            Err(error) => return self.block_queue(host, key, error.message),
        };
        let agent = get(&payload, "agent").clone();
        if !non_empty_obj(&agent) {
            return self.block_queue(host, key, "The queued session no longer exists.".into());
        }
        let release = |service: &mut ComposerService| {
            if let Some(delivery) = service.delivery.get_mut(host) {
                delivery.busy.remove(key);
            }
        };
        // A preparing, failed or busy-observed provider is not idle evidence.
        if s(get(&agent, "status")) != "idle"
            || !get(&agent, "activeTurn").is_null()
            || !get(&agent, "preparation").is_null()
            || s(get(&agent, "snapshotAvailability")) == "last_persisted"
            || non_empty_list(get(&agent, "pendingPermissions"))
            || !get(&agent, "archivedAt").is_null()
        {
            return release(self);
        }
        let draft = self.drafts.draft(key).clone();
        let mut queue = maps(get(&draft, "queue"));
        if non_empty_obj(get(&draft, "pending")) || queue.is_empty() || s(get(&queue[0], "state")) != "queued" {
            return release(self);
        }
        // host-runtime.ts drainQueuedAgentMessage: a drained message steers.
        let sending = queued_message_for_delivery(&queue[0], "steer");
        queue[0] = merged(&sending, &json!({"state": "sending"}));
        self.drafts.update(key, json!({"queue": queue}));
        let agent_id = key_identity(key).get(2).cloned().unwrap_or_default();
        self.start_send(SendJob {
            host: host.to_owned(),
            key: key.to_owned(),
            agent_id,
            message: sending,
            origin: SendOrigin::Queue { drain: true },
            panel: None,
        });
    }
}
