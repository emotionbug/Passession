use super::*;
use crate::providers::api::UnloadReadiness;
use std::sync::atomic::AtomicU64;
use tokio::{
    sync::{Notify, OwnedRwLockReadGuard},
    time::Instant,
};

struct Activity {
    idle_since: Instant,
    viewers: usize,
}

/// Shared by the live cell, incoming work, and timeline subscriptions. Unload
/// only tries the exclusive gate: it never queues ahead of newly admitted work.
pub(super) struct RuntimeActivity {
    pub(super) gate: Arc<RwLock<()>>,
    state: StdMutex<Activity>,
}
impl RuntimeActivity {
    fn new() -> Self {
        Self {
            gate: Arc::new(RwLock::new(())),
            state: StdMutex::new(Activity {
                idle_since: Instant::now(),
                viewers: 0,
            }),
        }
    }
    pub(super) fn touch(&self) {
        self.state
            .lock()
            .expect("Runtime activity poisoned")
            .idle_since = Instant::now();
    }
    fn expired(&self, timeout: Duration) -> bool {
        let state = self.state.lock().expect("Runtime activity poisoned");
        state.viewers == 0 && state.idle_since.elapsed() >= timeout
    }
}

pub(super) struct RuntimeUse {
    _activity: Arc<RuntimeActivity>,
    _resident: OwnedRwLockReadGuard<()>,
}

pub(super) struct TimelineViewer {
    activity: Arc<RuntimeActivity>,
    _resident: OwnedRwLockReadGuard<()>,
}
impl Drop for TimelineViewer {
    fn drop(&mut self) {
        let mut state = self
            .activity
            .state
            .lock()
            .expect("Runtime activity poisoned");
        state.viewers -= 1;
        if state.viewers == 0 {
            state.idle_since = Instant::now();
        }
    }
}

pub(super) struct IdleUnload {
    activity: StdMutex<HashMap<String, Weak<RuntimeActivity>>>,
    timeout_ms: AtomicU64,
    changed: Notify,
    stop: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Default for IdleUnload {
    fn default() -> Self {
        Self {
            activity: StdMutex::new(HashMap::new()),
            timeout_ms: AtomicU64::new(3_600_000),
            changed: Notify::new(),
            stop: CancellationToken::new(),
            task: Mutex::new(None),
        }
    }
}

impl AgentManager {
    pub(super) fn runtime_activity(&self, id: &str) -> Arc<RuntimeActivity> {
        let mut activities = self
            .idle_unload
            .activity
            .lock()
            .expect("Runtime activity poisoned");
        if let Some(activity) = activities.get(id).and_then(Weak::upgrade) {
            return activity;
        }
        activities.retain(|_, activity| activity.strong_count() > 0);
        let activity = Arc::new(RuntimeActivity::new());
        activities.insert(id.to_owned(), Arc::downgrade(&activity));
        activity
    }
    pub(super) async fn keep_runtime(&self, id: &str) -> RuntimeUse {
        let activity = self.runtime_activity(id);
        let resident = activity.gate.clone().read_owned().await;
        activity.touch();
        RuntimeUse {
            _activity: activity,
            _resident: resident,
        }
    }
    pub(super) async fn watch_runtime(&self, id: &str) -> TimelineViewer {
        let activity = self.runtime_activity(id);
        let resident = activity.gate.clone().read_owned().await;
        activity
            .state
            .lock()
            .expect("Runtime activity poisoned")
            .viewers += 1;
        TimelineViewer {
            activity,
            _resident: resident,
        }
    }
    pub(crate) fn configure_idle_runtime_timeout(&self, timeout_ms: u64) {
        self.idle_unload
            .timeout_ms
            .store(timeout_ms, Ordering::Release);
        self.idle_unload.changed.notify_one();
    }
    pub(super) async fn start_idle_unload(&self) {
        let mut task = self.idle_unload.task.lock().await;
        if task.is_some() {
            return;
        }
        let app = self.app.clone();
        let stop = self.idle_unload.stop.clone();
        *task = Some(tokio::spawn(async move {
            loop {
                let Some(app) = app.upgrade() else {
                    break;
                };
                let manager = app.agents.clone();
                let daemon_stop = app.cancel.clone();
                drop(app);
                let timeout = manager.idle_unload.timeout_ms.load(Ordering::Acquire);
                let period = if timeout == 0 {
                    60_000
                } else {
                    (timeout / 4).clamp(1, 60_000)
                };
                let changed = manager.idle_unload.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = daemon_stop.cancelled() => break,
                    _ = &mut changed => continue,
                    _ = tokio::time::sleep(Duration::from_millis(period)) => {}
                }
                if timeout != 0 {
                    manager.sweep_idle_runtimes().await;
                }
            }
        }));
    }
    pub(super) async fn stop_idle_unload(&self) -> Result<()> {
        self.idle_unload.stop.cancel();
        if let Some(task) = self.idle_unload.task.lock().await.take() {
            task.await?;
        }
        Ok(())
    }
    async fn sweep_idle_runtimes(&self) {
        let timeout = self.idle_unload.timeout_ms.load(Ordering::Acquire);
        if timeout == 0 {
            return;
        }
        let candidates = self
            .agents
            .read()
            .await
            .iter()
            .filter(|(_, cell)| cell.activity.expired(Duration::from_millis(timeout)))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in candidates {
            if self.idle_unload.stop.is_cancelled() || self.closing.load(Ordering::Acquire) {
                break;
            }
            if let Err(error) = self.unload_idle_runtime(&id).await {
                tracing::debug!(agent_id = id, %error, "Retaining idle agent runtime");
            }
        }
    }
    fn idle_timeout(&self, cell: &AgentCell) -> bool {
        let timeout = self.idle_unload.timeout_ms.load(Ordering::Acquire);
        timeout != 0
            && !self.closing.load(Ordering::Acquire)
            && cell.activity.expired(Duration::from_millis(timeout))
    }
    /// No snapshot commit, hooks, or finish notifications: residency is not an
    /// agent lifecycle transition, and the durable idle record is authoritative.
    pub(super) async fn unload_idle_runtime(&self, id: &str) -> Result<bool> {
        let lane = self.lane(id).await;
        let _lane = lane.lock().await;
        let Some(cell) = self.agents.read().await.get(id).cloned() else {
            return Ok(false);
        };
        let _mutation = cell.mutation.lock().await;
        if !self.idle_timeout(&cell) || !idle_snapshot(&*cell.data.lock().await) {
            return Ok(false);
        }
        if cell.stage.lock().await.is_some() {
            return Ok(false);
        }
        // Flush coalesced and queued provider output before taking the gate. The
        // event owner shares this gate, so no event application remains in flight.
        self.drain_events(&cell).await?;
        let Ok(residency) = cell.activity.gate.clone().try_write_owned() else {
            return Ok(false);
        };
        let commit = cell.commit.lock().await;
        if !self.idle_timeout(&cell)
            || !idle_snapshot(&*cell.data.lock().await)
            || cell.stage.lock().await.is_some()
            || self.message_delivery.has_pending_delivery(id).await
            || self.notifications.watches_local_child(id).await
        {
            return Ok(false);
        }
        let store = self.subagent_store.clone();
        let root = self.app()?.home.join("agent-subagents");
        let parent = id.to_owned();
        let safe_children = tokio::task::spawn_blocking(move || -> Result<bool> {
            let (children, unreadable) = store
                .lock()
                .map_err(|_| anyhow!("Subagent lock poisoned"))?
                .list_persisted(&root, &parent)?;
            Ok(unreadable.is_empty() && !children.iter().any(|child| child["status"] == "running"))
        })
        .await??;
        if !safe_children {
            return Ok(false);
        }
        // A missing durable record/handle must never turn an unload into deletion.
        let Some(record) = self.storage()?.get(id).await? else {
            return Ok(false);
        };
        if record["lastStatus"] != "idle"
            || record["archivedAt"].is_string()
            || !record["persistence"].is_object()
        {
            return Ok(false);
        }
        let session = cell
            .data
            .lock()
            .await
            .session
            .clone()
            .context("Agent has no session")?;
        if session.unload_readiness().await != UnloadReadiness::Ready || !self.idle_timeout(&cell) {
            return Ok(false);
        }
        // New work, viewers and event applications cannot cross the exclusive
        // gate. A failed close leaves the same owner installed for a later retry.
        session.close().await?;
        cell.unloaded.store(true, Ordering::Release);
        {
            let mut data = cell.data.lock().await;
            data.owner_ended = true;
            data.session = None;
        }
        drop(commit);
        drop(residency);
        self.teardown_cell(id, &cell).await?;
        Ok(true)
    }
}

fn idle_snapshot(data: &AgentData) -> bool {
    data.serves_runtime()
        && data.snapshot["internal"] != true
        && !data.snapshot["archivedAt"].is_string()
        && data.snapshot["lifecycle"] == "idle"
        && !data.snapshot["activeTurnId"].is_string()
        && !data.snapshot["activeTurn"].is_object()
        && !data.snapshot["activeForegroundTurnId"].is_string()
        && !data.pending_replacement
        && data.snapshot["pendingPermissions"]
            .as_array()
            .is_some_and(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::api::{AgentProvider, LaunchContext};

    const ID: &str = "00000000-0000-4000-8000-0000000000e1";

    /// A live idle agent whose stored record is what a restart would load.
    async fn fixture() -> (tempfile::TempDir, Arc<AppState>, Arc<AgentCell>) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let app = AppState::new(
            home.clone(),
            root.path().into(),
            std::env::current_exe().unwrap(),
        );
        app.agents
            .storage
            .get_or_init(|| async { AgentStorage::new(home) })
            .await;
        let record = json!({
            "id":ID,"provider":"mock","cwd":root.path(),"config":{},"internal":false,
            "lastStatus":"idle","createdAt":"2026-01-01T00:00:00.000Z",
            "updatedAt":"2026-01-02T00:00:00.000Z","lastActivityAt":"2026-01-01T00:00:00.000Z",
            "title":"Keep this title","labels":{"custom":"retained"},
            "requiresAttention":true,"attentionReason":"finished",
            "attentionTimestamp":"2026-01-01T00:00:00.000Z",
            "persistence":{"provider":"mock","sessionId":"session"}
        });
        app.agents
            .storage()
            .unwrap()
            .upsert(record.clone())
            .await
            .unwrap();
        let (cell, receiver) = app.agents.make_cell(ID, record, true).await.unwrap();
        let session = crate::providers::development::DevelopmentProvider::new("mock")
            .unwrap()
            .create_session(
                json!({}),
                LaunchContext {
                    agent_id: ID.into(),
                    home: app.home.clone(),
                    executable: app.executable.clone(),
                    env: BTreeMap::new(),
                    mcp_servers: json!({}),
                    tool_catalog: json!([]),
                    tool_manifest: json!([]),
                    tool_executor: None,
                    cancel: cell.cancel.child_token(),
                },
                json!({}),
                cell.sink.clone(),
            )
            .await
            .unwrap();
        {
            let mut data = cell.data.lock().await;
            data.session = Some(session);
            data.ready = true;
            data.snapshot["lifecycle"] = json!("idle");
            data.snapshot["status"] = json!("idle");
        }
        *cell.task.lock().await = Some(app.agents.start_events(ID, &cell, receiver));
        app.agents.configure_idle_runtime_timeout(1_000);
        (root, app, cell)
    }

    fn expire(cell: &AgentCell) {
        cell.activity.state.lock().unwrap().idle_since = Instant::now() - Duration::from_secs(2);
    }

    #[tokio::test]
    async fn idle_unload_leaves_the_restart_record_and_publishes_nothing() {
        let (_root, app, cell) = fixture().await;
        let manager = &app.agents;
        let before = manager.storage().unwrap().get(ID).await.unwrap().unwrap();
        let mut events = manager.subscribe_events();
        expire(&cell);
        assert!(manager.unload_idle_runtime(ID).await.unwrap());
        assert!(manager.cell(ID).await.is_err());
        assert!(cell.task.lock().await.is_none());
        assert_eq!(
            manager.storage().unwrap().get(ID).await.unwrap().unwrap(),
            before
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let observed = manager.observe_agent_snapshot(ID).await.unwrap().unwrap();
        assert_eq!(observed["status"], "idle");
        assert_eq!(observed["attentionTimestamp"], before["attentionTimestamp"]);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_unload_keeps_viewers_and_provider_busy_resident() {
        let (_root, app, cell) = fixture().await;
        let manager = &app.agents;
        let viewer = manager.watch_runtime(ID).await;
        expire(&cell);
        assert!(!manager.unload_idle_runtime(ID).await.unwrap());
        drop(viewer);
        assert!(!manager.idle_timeout(&cell));
        expire(&cell);
        let session = manager.session(ID).await.unwrap().unwrap();
        session
            .start_turn(json!("delay synthetic user message by 100000ms"), json!({}))
            .await
            .unwrap();
        assert_eq!(session.unload_readiness().await, UnloadReadiness::Busy);
        assert!(!manager.unload_idle_runtime(ID).await.unwrap());
        assert!(manager.cell(ID).await.is_ok());
        manager.shutdown().await.unwrap();
    }
}
