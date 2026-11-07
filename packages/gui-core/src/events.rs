use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::CoreEvent;

/// Ordered core → GUI queue. Adjacent `State` events collapse into the newest one.
/// Only an empty-to-nonempty transition wakes the GUI; a bounded consumer schedules
/// its own continuation until a batch is empty.
pub(crate) struct EventQueue {
    events: Mutex<VecDeque<CoreEvent>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventQueue {
    pub(crate) fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            wake,
        }
    }

    pub(crate) fn push(&self, event: CoreEvent) {
        let notify = {
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(event, CoreEvent::State { .. })
                && matches!(events.back(), Some(CoreEvent::State { .. }))
            {
                *events.back_mut().expect("checked non-empty") = event;
                return;
            }
            let was_empty = events.is_empty();
            events.push_back(event);
            was_empty
        };
        if notify {
            (self.wake)();
        }
    }

    pub(crate) fn drain_into(&self, output: &mut Vec<CoreEvent>, limit: usize) -> usize {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let count = events.len().min(limit);
        output.extend(events.drain(..count));
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn bounded_batches_preserve_order_across_refills() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let observer = wakes.clone();
        let queue = EventQueue::new(Arc::new(move || {
            observer.fetch_add(1, Ordering::SeqCst);
        }));
        let event = |seq| CoreEvent::HostEvent {
            host_id: "host".into(),
            event: json!({"seq": seq}),
        };
        for seq in 0..5 {
            queue.push(event(seq));
        }
        let mut batch = Vec::new();
        assert_eq!(queue.drain_into(&mut batch, 2), 2);
        for seq in 5..8 {
            queue.push(event(seq));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(queue.drain_into(&mut batch, 3), 3);
        assert_eq!(queue.drain_into(&mut batch, 3), 3);
        assert_eq!(queue.drain_into(&mut batch, 3), 0);
        let sequences: Vec<_> = batch.iter().map(|event| match event {
            CoreEvent::HostEvent { event, .. } => event["seq"].as_i64().unwrap(),
            _ => panic!("unexpected event"),
        }).collect();
        assert_eq!(sequences, (0..8).collect::<Vec<_>>());
        queue.push(event(8));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn state_coalescing_does_not_cross_ordered_event_boundaries() {
        let queue = EventQueue::new(Arc::new(|| {}));
        let state = |revision| CoreEvent::State { state: Arc::new(json!({"revision": revision})) };
        queue.push(state(1));
        queue.push(state(2));
        queue.push(CoreEvent::Disconnected { host_id: "host".into(), error: None });
        queue.push(state(3));
        queue.push(state(4));
        let mut batch = Vec::new();
        assert_eq!(queue.drain_into(&mut batch, 2), 2);
        assert!(matches!(&batch[0], CoreEvent::State { state } if state["revision"] == 2));
        assert!(matches!(&batch[1], CoreEvent::Disconnected { host_id, .. } if host_id == "host"));
        batch.clear();
        assert_eq!(queue.drain_into(&mut batch, 2), 1);
        assert!(matches!(&batch[0], CoreEvent::State { state } if state["revision"] == 4));
    }
}
