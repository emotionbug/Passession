use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::CoreEvent;

/// Ordered core → GUI queue. `State` replaces the preceding undelivered
/// `State`, even across ordered events: it is a complete presentation
/// snapshot, not a delta or response. The latest snapshot retains its own
/// arrival position. All other variants are delivered in FIFO order.
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
}

#[derive(Default)]
struct PendingEvents {
    ordered: VecDeque<QueuedEvent>,
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
        self.ordered.pop_front()
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
        let (notify, superseded) = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let was_empty = pending.first().is_none();
            let queued = QueuedEvent {
                event,
                queued_at: self.trace_ages.then(Instant::now),
            };
            let superseded = if matches!(queued.event, CoreEvent::State { .. }) {
                let ahead = pending.ordered.len();
                pending.state.replace((ahead, queued))
            } else {
                pending.ordered.push_back(queued);
                None
            };
            (was_empty, superseded)
        };
        drop(superseded);
        if notify {
            (self.wake)();
        }
    }

    pub(crate) fn stage(&self, output: &mut Vec<CoreEvent>) -> bool {
        // The GUI yields on time, not count. This staging chunk is the unit
        // between budget checks, with one acquisition and one UI dispatch.
        const CHUNK: usize = 32;
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn stages_preserve_order_across_refills() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let observer = wakes.clone();
        let queue = EventQueue::new(Arc::new(move || {
            observer.fetch_add(1, Ordering::SeqCst);
        }));
        let event = |seq| CoreEvent::HostEvent {
            host_id: "host".into(),
            event: json!({"seq": seq}),
        };
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
                CoreEvent::HostEvent { event, .. } => event["seq"].as_i64().unwrap(),
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
        let state = |revision| CoreEvent::State {
            state: Arc::new(crate::CoreState {
                value: json!({"revision": revision}),
                ..crate::CoreState::default()
            }),
        };
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
}
