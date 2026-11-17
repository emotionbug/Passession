use super::UnloadReadiness;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy)]
enum TaskStatus {
    Running,
    Terminal,
}

struct Task {
    declared: bool,
    status: TaskStatus,
}

/// Native lifetimes, not timeline cards: ambient, hidden and background tasks
/// are just as capable of doing work after the foreground result.
#[derive(Default)]
pub(super) struct TaskLedger {
    tasks: HashMap<String, Task>,
    uncertain: bool,
}

impl TaskLedger {
    /// Called only by the live transport consumer, never by transcript projection.
    pub fn observe(&mut self, frame: &Value) {
        if frame["type"] != "system" || frame["isReplay"] == true {
            return;
        }
        let Some(kind @ ("task_started" | "task_updated" | "task_progress" | "task_notification")) =
            frame["subtype"].as_str()
        else {
            return;
        };
        let Some(id) = frame["task_id"].as_str().filter(|id| !id.is_empty()) else {
            self.uncertain = true;
            return;
        };
        if !self.tasks.contains_key(id) {
            self.tasks.insert(
                id.to_owned(),
                Task {
                    declared: false,
                    status: TaskStatus::Running,
                },
            );
        }
        let task = self.tasks.get_mut(id).expect("recorded native task");
        if kind == "task_started" {
            task.declared = true;
        }
        let status = frame["patch"].get("status").or_else(|| frame.get("status"));
        match status {
            Some(status) => match status.as_str() {
                Some("pending" | "running" | "paused") => task.status = TaskStatus::Running,
                Some("completed" | "failed" | "killed" | "stopped") => {
                    task.status = TaskStatus::Terminal;
                }
                _ => self.uncertain = true,
            },
            None if kind == "task_notification" => self.uncertain = true,
            None => {}
        }
    }

    pub fn readiness(&self) -> UnloadReadiness {
        if self.uncertain {
            return UnloadReadiness::Unknown;
        }
        let mut running = false;
        for task in self.tasks.values() {
            if !task.declared {
                return UnloadReadiness::Unknown;
            }
            running |= matches!(task.status, TaskStatus::Running);
        }
        if running {
            UnloadReadiness::Busy
        } else {
            UnloadReadiness::Ready
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filtered_native_tasks_require_matched_terminal_notifications() {
        let mut ledger = TaskLedger::default();
        ledger.observe(&json!({"type":"system","subtype":"task_started","task_id":"shell","task_type":"local_bash","skip_transcript":true,"is_backgrounded":true}));
        assert_eq!(ledger.readiness(), UnloadReadiness::Busy);
        ledger.observe(&json!({"type":"system","subtype":"task_notification","task_id":"shell","status":"completed"}));
        assert_eq!(ledger.readiness(), UnloadReadiness::Ready);
        ledger.observe(&json!({"type":"system","subtype":"task_notification","task_id":"unknown","status":"completed"}));
        assert_eq!(ledger.readiness(), UnloadReadiness::Unknown);
    }
}
