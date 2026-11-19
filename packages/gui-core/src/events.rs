use std::{
    collections::{HashSet, VecDeque},
    mem::size_of,
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::CoreEvent;

// At 32 events per chunk, 256 events allow eight chunks of live data before
// falling back to history. The separate heap budget also catches large rows.
const LIVE_EVENTS: usize = 256;
const LIVE_BYTES: usize = 8 * 1024 * 1024;

/// Ordered core → GUI queue. The latest undelivered `State` is a complete
/// snapshot and keeps its own arrival position. Other events remain FIFO,
/// except that replayable live data can be replaced by a host recovery marker.
/// Only an empty-to-nonempty transition wakes the GUI; a budgeted consumer
/// schedules its own continuation if events remain.
pub(crate) struct EventQueue {
    pending: Mutex<PendingEvents>,
    trace_ages: bool,
    wake: Arc<dyn Fn() + Send + Sync>,
}

struct QueuedEvent {
    event: CoreEvent,
    queued_at: Option<Instant>,
    live_bytes: usize,
}

#[derive(Default)]
struct PendingEvents {
    ordered: VecDeque<QueuedEvent>,
    live_count: usize,
    live_bytes: usize,
    resyncing: HashSet<String>,
    // Number of ordered events ahead of this snapshot. Replacing it is O(1)
    // regardless of how much ordered work has accumulated.
    state: Option<(usize, QueuedEvent)>,
}

impl PendingEvents {
    fn first(&self) -> Option<&QueuedEvent> {
        match &self.state {
            Some((0, state)) => Some(state),
            _ => self.ordered.front(),
        }
    }

    fn pop(&mut self) -> Option<QueuedEvent> {
        if let Some((ahead, _)) = &mut self.state {
            if *ahead == 0 {
                return self.state.take().map(|(_, state)| state);
            }
            *ahead -= 1;
        }
        let queued = self.ordered.pop_front()?;
        if queued.live_bytes != 0 {
            self.live_count -= 1;
            self.live_bytes -= queued.live_bytes;
        } else if let CoreEvent::HostResync { host_id } = &queued.event {
            self.resyncing.remove(host_id);
        }
        Some(queued)
    }

    /// Put the marker at the first omission, without moving the other hosts,
    /// lossless events, or the latest state. Drop retired JSON outside the lock.
    fn collapse(&mut self, host_id: &str, queued_at: Option<Instant>) -> Vec<Value> {
        self.resyncing.insert(host_id.to_owned());
        let mut retired = Vec::new();
        let mut have_marker = false;
        let state_ahead = self.state.as_ref().map_or(0, |(ahead, _)| *ahead);
        let mut index = 0;
        let mut removed_before_state = 0;
        let mut retired_bytes = 0;
        self.ordered.retain_mut(|queued| {
            let before_state = index < state_ahead;
            index += 1;
            if queued.live_bytes == 0 {
                return true;
            }
            let CoreEvent::HostEvent { host_id: id, event } = &mut queued.event else {
                unreachable!("only host events are charged")
            };
            if id.as_str() != host_id {
                return true;
            }
            retired.push(std::mem::take(event));
            retired_bytes += std::mem::take(&mut queued.live_bytes);
            if have_marker {
                removed_before_state += usize::from(before_state);
                return false;
            }
            have_marker = true;
            queued.event = CoreEvent::HostResync {
                host_id: std::mem::take(id),
            };
            true
        });
        if !have_marker {
            self.ordered.push_back(QueuedEvent {
                event: CoreEvent::HostResync {
                    host_id: host_id.to_owned(),
                },
                queued_at,
                live_bytes: 0,
            });
        }
        if let Some((ahead, _)) = &mut self.state {
            *ahead -= removed_before_state;
        }
        self.live_count -= retired.len();
        self.live_bytes -= retired_bytes;
        retired
    }
}

impl EventQueue {
    pub(crate) fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            pending: Mutex::default(),
            trace_ages: std::env::var_os("PASEO_FRAME_TRACE").as_deref()
                == Some(std::ffi::OsStr::new("1")),
            wake,
        }
    }

    pub(crate) fn push(&self, event: CoreEvent) {
        let live_bytes = match &event {
            CoreEvent::HostEvent { host_id, event } if replayable(event) => {
                size_of::<QueuedEvent>() + host_id.capacity() + json_heap_bytes(event)
            }
            _ => 0,
        };
        let queued = QueuedEvent {
            event,
            queued_at: self.trace_ages.then(Instant::now),
            live_bytes,
        };
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let notify = pending.first().is_none();
        if live_bytes != 0
            && let CoreEvent::HostEvent { host_id, .. } = &queued.event
        {
            if pending.resyncing.contains(host_id) {
                return;
            }
            if pending.live_count == LIVE_EVENTS || live_bytes > LIVE_BYTES - pending.live_bytes {
                let retired = pending.collapse(host_id, queued.queued_at);
                drop(pending);
                if notify {
                    (self.wake)();
                }
                eprintln!(
                    "Paseo core event queue full for host {host_id:?}; skipped {} live updates and queued a refresh",
                    retired.len() + 1
                );
                return;
            }
            pending.live_count += 1;
            pending.live_bytes += live_bytes;
        }
        let superseded = if matches!(queued.event, CoreEvent::State { .. }) {
            let ahead = pending.ordered.len();
            pending.state.replace((ahead, queued))
        } else {
            pending.ordered.push_back(queued);
            None
        };
        drop(pending);
        drop(superseded);
        if notify {
            (self.wake)();
        }
    }

    pub(crate) fn stage(&self, output: &mut Vec<CoreEvent>) -> bool {
        // The GUI yields on time, not count. This staging chunk is the unit
        // between budget checks, with one acquisition and one UI dispatch.
        const CHUNK: usize = 32;
        let mut pending = match self.pending.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            // Compaction is producer work. Yield and re-arm on contention.
            Err(TryLockError::WouldBlock) => return true,
        };
        for _ in 0..CHUNK {
            let Some(queued) = pending.pop() else { break };
            output.push(queued.event);
        }
        pending.first().is_some()
    }

    pub(crate) fn backlog(&self) -> (usize, Duration) {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let count = pending.ordered.len() + usize::from(pending.state.is_some());
        let age = pending
            .first()
            .and_then(|first| first.queued_at)
            .map_or(Duration::ZERO, |queued_at| queued_at.elapsed());
        (count, age)
    }
}

/// Keep unknown and one-shot events lossless. A timeline row has a durable
/// position, and directory upserts have already been applied by the core.
fn replayable(event: &Value) -> bool {
    let payload = &event["payload"];
    if ["requestId", "observationId", "receipt"]
        .iter()
        .any(|key| !event[*key].is_null() || !payload[*key].is_null())
    {
        return false;
    }
    let positioned = || {
        payload["seq"].as_i64().is_some_and(|seq| seq >= 0)
            && payload["epoch"].as_str().is_some_and(|id| !id.is_empty())
    };
    match event["type"].as_str() {
        Some("agent_stream") => {
            payload["agentId"].is_string()
                && payload["event"]["type"] == "timeline"
                && payload["event"]["item"].is_object()
                && positioned()
        }
        Some("agent.provider_subagents.update") => {
            payload["kind"] == "timeline"
                && payload["parentAgentId"].is_string()
                && payload["subagentId"].is_string()
                && payload["item"].is_object()
                && positioned()
        }
        Some("agent_update") => payload["kind"] == "upsert" && payload["agent"]["id"].is_string(),
        _ => false,
    }
}

/// Estimate retained allocations without reserializing the JSON. Object entries
/// include an allowance for the map's spare slots and node links.
fn json_heap_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(items) => {
            items.capacity() * size_of::<Value>() + items.iter().map(json_heap_bytes).sum::<usize>()
        }
        Value::Object(fields) => {
            const ENTRY: usize = 2 * size_of::<(String, Value)>();
            fields
                .iter()
                .map(|(key, value)| ENTRY + key.capacity() + json_heap_bytes(value))
                .sum()
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn timeline(seq: usize, text: &str) -> Value {
        json!({
            "type": "agent_stream",
            "payload": {
                "agentId": "agent", "seq": seq, "epoch": "current",
                "event": {"type": "timeline", "item": {"type": "assistant_message", "text": text}}
            }
        })
    }

    fn live(host_id: &str, seq: usize, text: &str) -> CoreEvent {
        CoreEvent::HostEvent {
            host_id: host_id.into(),
            event: timeline(seq, text),
        }
    }

    fn state(revision: usize) -> CoreEvent {
        CoreEvent::State {
            state: Arc::new(crate::CoreState {
                value: json!({"revision": revision}),
                ..crate::CoreState::default()
            }),
        }
    }

    #[test]
    fn stages_preserve_order_across_refills() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let observer = wakes.clone();
        let queue = EventQueue::new(Arc::new(move || {
            observer.fetch_add(1, Ordering::SeqCst);
        }));
        let event = |seq| live("host", seq, "");
        for seq in 0..80 {
            queue.push(event(seq));
        }
        let mut batch = Vec::new();
        assert!(queue.stage(&mut batch));
        assert!(!batch.is_empty());
        assert_eq!(queue.backlog().0, 80 - batch.len());
        for seq in 80..100 {
            queue.push(event(seq));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        while queue.stage(&mut batch) {}
        assert_eq!(queue.backlog(), (0, Duration::ZERO));
        let sequences: Vec<_> = batch
            .iter()
            .map(|event| match event {
                CoreEvent::HostEvent { event, .. } => event["payload"]["seq"].as_i64().unwrap(),
                _ => panic!("unexpected event"),
            })
            .collect();
        assert_eq!(sequences, (0..100).collect::<Vec<_>>());
        queue.push(event(100));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn latest_state_stays_at_its_own_position_in_the_fifo() {
        let queue = EventQueue::new(Arc::new(|| {}));
        queue.push(state(1));
        queue.push(state(2));
        queue.push(CoreEvent::Disconnected {
            host_id: "host".into(),
            error: None,
        });
        queue.push(state(3));
        queue.push(state(4));
        queue.push(CoreEvent::Connected {
            host_id: "host".into(),
            server_info: json!({}),
        });
        assert_eq!(queue.backlog().0, 3);
        let mut batch = Vec::new();
        assert!(!queue.stage(&mut batch));
        assert!(matches!(&batch[0], CoreEvent::Disconnected { host_id, .. } if host_id == "host"));
        assert!(matches!(&batch[1], CoreEvent::State { state } if state["revision"] == 4));
        assert!(matches!(&batch[2], CoreEvent::Connected { host_id, .. } if host_id == "host"));
    }

    #[test]
    fn slow_consumer_collapses_only_the_overflowing_host_and_keeps_fifo() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = wakes.clone();
        let queue = Arc::new(EventQueue::new(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        })));
        let producer_queue = queue.clone();
        let (done, finished) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            let queue = producer_queue;
            queue.push(CoreEvent::Connected {
                host_id: "busy".into(),
                server_info: json!({}),
            });
            queue.push(live("busy", 0, ""));
            queue.push(live("other", 0, ""));
            queue.push(live("busy", 1, ""));
            queue.push(state(1));
            queue.push(CoreEvent::Response {
                host_id: "busy".into(),
                request_id: "first".into(),
                result: Ok(json!({"value": 42})),
            });
            queue.push(live("busy", 2, ""));
            queue.push(state(2));
            queue.push(live("other", 1, ""));
            for seq in 3..LIVE_EVENTS - 2 {
                queue.push(live("busy", seq, ""));
            }
            assert_eq!(queue.backlog().0, LIVE_EVENTS + 3);
            for seq in LIVE_EVENTS..LIVE_EVENTS * 16 {
                queue.push(live("busy", seq, ""));
            }
            queue.push(CoreEvent::HostEvent {
                host_id: "busy".into(),
                event: json!({"type": "agent_permission_request"}),
            });
            queue.push(CoreEvent::Disconnected {
                host_id: "busy".into(),
                error: None,
            });
            queue.push(CoreEvent::Response {
                host_id: "busy".into(),
                request_id: "second".into(),
                result: Err(crate::CoreError::new("ERROR", "failure", false)),
            });
            queue.push(CoreEvent::Error {
                host_id: Some("busy".into()),
                error: crate::CoreError::new("ERROR", "failure", false),
            });
            done.send(()).unwrap();
        });
        // The producer finishes without the consumer staging a single event.
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        producer.join().unwrap();
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(queue.backlog().0, 10);
        let mut batch = Vec::new();
        assert!(!queue.stage(&mut batch));
        assert!(matches!(
            batch.as_slice(),
            [
                CoreEvent::Connected { host_id: connected, .. },
                CoreEvent::HostResync { host_id: resync },
                CoreEvent::HostEvent { host_id: first_other, event: zero },
                CoreEvent::Response { request_id: first, result: Ok(result), .. },
                CoreEvent::State { state },
                CoreEvent::HostEvent { host_id: second_other, event: one },
                CoreEvent::HostEvent { event: permission, .. },
                CoreEvent::Disconnected { host_id: disconnected, .. },
                CoreEvent::Response { request_id: second, result: Err(_), .. },
                CoreEvent::Error { .. },
            ] if connected == "busy" && resync == "busy" && disconnected == "busy"
                && first_other == "other" && zero["payload"]["seq"] == 0
                && second_other == "other" && one["payload"]["seq"] == 1
                && first == "first" && result["value"] == 42 && second == "second"
                && state["revision"] == 2 && permission["type"] == "agent_permission_request"
        ));
        queue.push(live("busy", 9999, ""));
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        batch.clear();
        assert!(!queue.stage(&mut batch));
        assert!(matches!(&batch[0], CoreEvent::HostEvent { event, .. }
            if event["payload"]["seq"] == 9999));
    }

    #[test]
    fn large_timeline_rows_fill_the_byte_budget_before_the_count_budget() {
        let queue = EventQueue::new(Arc::new(|| {}));
        let text = "x".repeat(LIVE_BYTES / 2);
        queue.push(live("busy", 0, &text));
        assert_eq!(queue.pending.lock().unwrap().live_count, 1);
        queue.push(live("busy", 1, &text));
        let pending = queue.pending.lock().unwrap();
        assert_eq!((pending.live_count, pending.live_bytes), (0, 0));
        drop(pending);
        let mut batch = Vec::new();
        assert!(!queue.stage(&mut batch));
        assert!(
            matches!(batch.as_slice(), [CoreEvent::HostResync { host_id }] if host_id == "busy")
        );
    }

    #[test]
    fn only_reconstructible_host_updates_are_replayable() {
        let modern = timeline(1, "row");
        assert!(replayable(&modern));
        assert!(replayable(&json!({
            "type": "agent.provider_subagents.update",
            "payload": {"kind": "timeline", "parentAgentId": "a", "subagentId": "b",
                "seq": 1, "epoch": "current", "item": {"type": "assistant_message"}}
        })));
        assert!(replayable(&json!({
            "type": "agent_update", "payload": {"kind": "upsert", "agent": {"id": "a"}}
        })));
        for kind in [
            "agent_permission_request",
            "agent_attention_required",
            "terminal.frame",
            "daemon.update.maintenance",
            "subscription.restored",
            "unknown_future_event",
        ] {
            assert!(!replayable(
                &json!({"type": kind, "payload": modern["payload"]})
            ));
        }
        let mut nonreplayable = modern.clone();
        nonreplayable["payload"]["epoch"] = Value::Null;
        assert!(!replayable(&nonreplayable));
        nonreplayable = modern.clone();
        nonreplayable["payload"]["event"]["type"] = json!("turn_completed");
        assert!(!replayable(&nonreplayable));
        nonreplayable = modern.clone();
        nonreplayable["payload"]["requestId"] = json!("request");
        assert!(!replayable(&nonreplayable));
        nonreplayable = modern;
        nonreplayable["payload"]["receipt"] = json!({"id": "receipt"});
        assert!(!replayable(&nonreplayable));
    }
}
