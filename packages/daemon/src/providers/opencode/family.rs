//! Live parent sessions by native id. An opened child of a live parent is replayed
//! in that parent's own event stream (`SubagentReplay`): ordered with the parent's
//! live handling, the native snapshot is read and published as a replay marker.
//! Only that child's native events are then held; the parent and its other
//! children keep serving, since the daemon may apply the marker only after a
//! parent stage (history replay, turn admission) that needs the session. A
//! watcher posts the outcome back: applied continues from the snapshot, retained
//! (or none) from the previous state, and the held events are translated once.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::providers::api::{
    HistoryCoverage, ReplayOutcome, SubagentHistoryRequest, SubagentReplay,
};

#[async_trait]
pub(super) trait LiveFamily: Send + Sync {
    fn is_live(&self) -> bool;
    /// Publishes `request.child_id`'s replay marker on the parent's live sink and
    /// holds that child's live events until the daemon settles it. Returns once
    /// published; never waits for the outcome.
    async fn publish_replay(
        self: Arc<Self>,
        provider: &str,
        request: SubagentHistoryRequest,
        directory: &str,
        cancel: CancellationToken,
    ) -> Result<()>;
}

/// The in-stream marker for one replay read; coverage settled by the request.
pub(super) fn marker(
    request: &SubagentHistoryRequest,
    provider: &str,
    mut events: Vec<Value>,
    read: HistoryCoverage,
) -> Value {
    let coverage = request.settle(&mut events, read);
    request
        .replay
        .marker(provider, &request.child_id, events, coverage)
}

/// Waits for the daemon's decision on a published replay. True only when it was
/// applied: live collection then continues from the snapshot. A retained replay,
/// a closed event owner or a closed session keeps the previous live state.
pub(super) async fn applied(replay: &SubagentReplay, closed: &CancellationToken) -> bool {
    tokio::select! {
        outcome = replay.outcome() => outcome == Some(ReplayOutcome::Applied),
        _ = closed.cancelled() => false,
    }
}

/// Watches a published replay off the session's own handling and posts the
/// decision exactly once to `settle`.
pub(super) fn on_outcome<F, Fut>(replay: SubagentReplay, closed: CancellationToken, settle: F)
where
    F: FnOnce(bool) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let applied = applied(&replay, &closed).await;
        settle(applied).await;
    });
}

/// Native events of children whose replay awaits the daemon's outcome.
#[derive(Default)]
pub(super) struct HeldEvents {
    children: HashMap<String, Vec<Value>>,
    /// Sessions a held child spawned during its hold, by that child: their
    /// detection links the child's task row, so all their events wait with it.
    spawned: HashMap<String, String>,
}

impl HeldEvents {
    pub fn hold(&mut self, child: &str) {
        self.children.entry(child.to_owned()).or_default();
    }

    pub fn is_held(&self, child: &str) -> bool {
        self.children.contains_key(child)
    }

    /// Whether this session's events currently wait in some child's hold.
    pub fn covers(&self, session: &str) -> bool {
        self.owner(session).is_some()
    }

    /// Buffers an event of a held child, or of a session spawned beneath it
    /// during the hold. False: handle it now.
    pub fn intercept(&mut self, session: &str, spawner: &str, event: &Value) -> bool {
        if session.is_empty() {
            return false;
        }
        let owner = match self.owner(session) {
            Some(owner) => owner,
            None => match self.owner(spawner) {
                Some(owner) => {
                    self.spawned.insert(session.to_owned(), owner.clone());
                    owner
                }
                None => return false,
            },
        };
        self.children.get_mut(&owner).unwrap().push(event.clone());
        true
    }

    fn owner(&self, session: &str) -> Option<String> {
        if self.children.contains_key(session) {
            Some(session.to_owned())
        } else {
            self.spawned.get(session).cloned()
        }
    }

    /// Ends the hold, returning the buffered events in arrival order.
    pub fn release(&mut self, child: &str) -> Vec<Value> {
        self.spawned.retain(|_, owner| owner != child);
        self.children.remove(child).unwrap_or_default()
    }
}

#[derive(Default)]
pub(super) struct Families {
    parents: Mutex<HashMap<String, Weak<dyn LiveFamily>>>,
}

impl Families {
    pub fn register(&self, parent: &str, family: Weak<dyn LiveFamily>) {
        let mut parents = self.parents.lock().unwrap_or_else(|p| p.into_inner());
        parents.retain(|_, family| family.strong_count() != 0);
        parents.insert(parent.to_owned(), family);
    }

    pub fn live(&self, parent: &str) -> Option<Arc<dyn LiveFamily>> {
        self.parents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(parent)
            .and_then(Weak::upgrade)
            .filter(|family| family.is_live())
    }
}
