use anyhow::{Result, anyhow, ensure};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    operation: Option<String>,
    active: usize,
}

/// Short admission bookkeeping only; no provider work or IO holds this mutex.
#[derive(Default)]
pub(crate) struct WorkAdmission {
    state: Mutex<State>,
}

pub(crate) struct WorkGuard(Arc<WorkAdmission>);

impl Drop for WorkGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        state.active -= 1;
        drop(state);
    }
}

impl WorkAdmission {
    pub(crate) fn enter(self: &Arc<Self>) -> Result<WorkGuard> {
        let mut state = self.state.lock().map_err(|_| anyhow!("Work admission lock poisoned"))?;
        if let Some(operation) = &state.operation {
            return Err(crate::providers::api::PromptDeliveryError::not_delivered(
                format!("daemon_update_in_progress: {operation}"),
            ));
        }
        state.active += 1;
        Ok(WorkGuard(self.clone()))
    }

    pub(crate) fn pause(&self, operation: &str) -> Result<bool> {
        let mut state = self.state.lock().map_err(|_| anyhow!("Work admission lock poisoned"))?;
        ensure!(state.operation.as_deref().is_none_or(|current| current == operation),
            "A different update owns work admission");
        let fresh = state.operation.is_none();
        state.operation = Some(operation.to_owned());
        Ok(fresh)
    }

    pub(crate) fn release(&self, operation: &str) -> Result<bool> {
        let mut state = self.state.lock().map_err(|_| anyhow!("Work admission lock poisoned"))?;
        if state.operation.as_deref() != Some(operation) {
            return Ok(false);
        }
        state.operation = None;
        Ok(true)
    }

    pub(crate) fn active(&self) -> Result<usize> {
        Ok(self.state.lock().map_err(|_| anyhow!("Work admission lock poisoned"))?.active)
    }

    pub(crate) fn operation(&self) -> Result<Option<String>> {
        Ok(self.state.lock().map_err(|_| anyhow!("Work admission lock poisoned"))?.operation.clone())
    }

}

impl super::AgentManager {
    pub(crate) async fn update_busy_agents(&self, exempt: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let cells = self.agents.read().await.iter()
            .map(|(id, cell)| (id.clone(), cell.clone())).collect::<Vec<_>>();
        let mut busy = Vec::new();
        for (id, cell) in cells {
            if exempt == Some(id.as_str()) { continue; }
            match cell.data.try_lock() {
                Ok(data) if !data.owner_ended && (!data.ready || super::is_busy(&data.snapshot) || data.pending_replacement) => {
                    busy.push(serde_json::json!({"agentId":id,"reason":"active","status":data.snapshot["lifecycle"]}));
                }
                Err(_) => busy.push(serde_json::json!({"agentId":id,"reason":"state_unavailable"})),
                _ => {},
            }
        }
        Ok(busy)
    }

    pub(crate) async fn flush_update_events(&self) -> Result<()> {
        let cells = self.agents.read().await.values().cloned().collect::<Vec<_>>();
        for cell in cells {
            let live = {
                let data = cell.data.lock().await;
                data.ready && !data.owner_ended && data.event_error.is_none()
            };
            if live { self.drain_events(&cell).await?; }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_admission_drains_existing_work_and_only_its_owner_releases_it() {
        let admission = Arc::new(WorkAdmission::default());
        let running = admission.enter().unwrap();
        assert!(admission.pause("update-one").unwrap());
        assert!(admission.enter().is_err());
        assert_eq!(admission.active().unwrap(), 1);
        assert!(admission.pause("update-two").is_err());
        assert!(!admission.release("update-two").unwrap());
        drop(running);
        assert_eq!(admission.active().unwrap(), 0);
        assert_eq!(admission.operation().unwrap().as_deref(), Some("update-one"));
        assert!(admission.release("update-one").unwrap());
        let next = admission.enter().unwrap();
        assert_eq!(admission.active().unwrap(), 1);
        drop(next);
        assert_eq!(admission.active().unwrap(), 0);
    }
}
