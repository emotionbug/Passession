use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::CoreEvent;

/// Ordered core → GUI queue. Adjacent `State` events collapse into the newest one, and `wake`
/// runs only when the queue turns non-empty, so the GUI drains it fully on every wake.
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

    pub(crate) fn pop(&self) -> Option<CoreEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}
