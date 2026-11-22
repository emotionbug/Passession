use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

/// These entries have a local kill handler in Claude 2.1.330. Cloud sessions,
/// housekeeping, undeclared tasks, and future task types do not gain authority.
fn task_prefix(kind: &str) -> Option<u8> {
    Some(match kind {
        "local_agent" => b'a',
        "local_bash" => b'b',
        "local_workflow" => b'w',
        "monitor_mcp" => b'm',
        "monitor_ws" => b's',
        "mcp_task" => b'k',
        "in_process_teammate" => b't',
        _ => return None,
    })
}

/// Claude uses a zero-padded base-36 run clock followed by an eight-digit
/// hex nonce. The clock increases when Claude reuses a task identifier.
fn run_clock(run: &str) -> Option<&str> {
    let (clock, nonce) = run.split_once('-')?;
    (clock.len() == 9
        && clock.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        && nonce.len() == 8
        && nonce.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(clock)
}

struct Task {
    run_id: String,
    kind: String,
    description: String,
    background: bool,
    token: Option<Uuid>,
}

/// One CLI process and one native session. The exact `task_id` never comes
/// from the client: it is resolved from a one-use token for a live `run_id`.
#[derive(Default)]
pub(super) struct TaskControls {
    session: Option<String>,
    tasks: HashMap<String, Task>,
}

impl TaskControls {
    pub fn observe(&mut self, frame: &Value) -> bool {
        if frame["isReplay"] == true {
            return false;
        }
        if frame["type"] != "system" {
            if self.session.is_some()
                && frame["parent_tool_use_id"].is_null()
                && matches!(frame["type"].as_str(), Some("assistant" | "user" | "result" | "stream_event"))
                && let Some(session) = frame["session_id"].as_str().filter(|id| !id.is_empty())
                && self.session.as_deref() != Some(session)
            {
                let changed = self.invalidate(false);
                *self = Self::default();
                return changed;
            }
            return false;
        }
        if frame["subtype"] == "init" && frame["parent_tool_use_id"].is_null() {
            let session = frame["session_id"].as_str().filter(|id| !id.is_empty());
            let version = frame["claude_code_version"]
                .as_str()
                .and_then(|version| semver::Version::parse(version).ok());
            let session = session.filter(|_| {
                version.is_some_and(|v| {
                    v.major == 2 && v.pre.is_empty() && (v.minor, v.patch) >= (1, 330)
                })
            });
            if self.session.as_deref() == session {
                return false;
            }
            let changed = self.invalidate(false);
            self.tasks.clear();
            self.session = session.map(str::to_owned);
            return changed;
        }
        let Some(session) = &self.session else {
            return false;
        };
        if frame["session_id"] != session.as_str() {
            return false;
        }
        let Some(id) = frame["task_id"].as_str() else {
            return false;
        };
        let kind = frame["subtype"].as_str();
        let run = frame["run_id"].as_str().filter(|run| run_clock(run).is_some());
        let Some(run) = run else {
            // An incomplete lifecycle update cannot prove the old offer is
            // still current. It can revoke authority, never create it.
            return matches!(kind, Some("task_started" | "task_updated" | "task_notification"))
                && self.tasks.get_mut(id).is_some_and(|task| task.token.take().is_some());
        };
        let status = frame["patch"].get("status").or_else(|| frame.get("status"));
        match kind {
            Some("task_started") => {
                if self.tasks.get(id).is_some_and(|task| run_clock(&task.run_id) >= run_clock(run)) {
                    return false;
                }
                let already_visible = self.tasks.get(id).is_some_and(|task| task.token.is_some());
                let task_kind = frame["task_type"].as_str().unwrap_or_default();
                // Native-shaped identifiers only. Claude can fall back to name
                // lookup for non-identifiers; never expose that lookup to RPCs.
                if task_prefix(task_kind).is_none_or(|prefix| {
                    (id.len() != 9 && !(task_kind == "local_agent" && id.len() == 17))
                        || id.as_bytes()[0] != prefix
                        || !id.as_bytes()[1..]
                            .iter()
                            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
                }) || frame["skip_transcript"] == true
                    || frame["ambient"] == true
                {
                    if let Some(task) = self.tasks.get_mut(id) {
                        task.run_id = run.to_owned();
                        return task.token.take().is_some();
                    }
                    return false;
                }
                let token = status.is_none_or(|status| status == "running").then(Uuid::new_v4);
                self.tasks.insert(
                    id.to_owned(),
                    Task {
                        run_id: run.to_owned(),
                        kind: task_kind.to_owned(),
                        description: frame["description"].as_str().unwrap_or_default().to_owned(),
                        background: frame["is_backgrounded"] == true,
                        token,
                    },
                );
                already_visible || token.is_some()
            }
            Some("task_updated" | "task_progress" | "task_notification") => {
                let Some(task) = self.tasks.get_mut(id) else {
                    return false;
                };
                if task.run_id != run {
                    if run_clock(run) > run_clock(&task.run_id) {
                        task.run_id = run.to_owned();
                        return task.token.take().is_some();
                    }
                    return false;
                }
                if let Some(background) = frame["patch"]["is_backgrounded"].as_bool() {
                    task.background = background;
                }
                if frame["subtype"] == "task_notification"
                    || status.is_some_and(|status| status != "running")
                {
                    return task.token.take().is_some();
                }
                if let Some(description) = frame["patch"]["description"].as_str()
                    && task.token.is_some()
                    && task.description != description
                {
                    task.description = description.to_owned();
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    pub fn invalidate(&mut self, foreground_only: bool) -> bool {
        let mut changed = false;
        for task in self.tasks.values_mut() {
            if !foreground_only || !task.background {
                changed |= task.token.take().is_some();
            }
        }
        changed
    }

    pub fn list(&self) -> Vec<Value> {
        let mut tasks: Vec<_> = self.tasks.iter().filter(|(_, task)| task.token.is_some()).collect();
        tasks.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        tasks
            .into_iter()
            .map(|(_, task)| json!({"token":task.token,"kind":task.kind,"description":task.description}))
            .collect()
    }

    pub fn take(&mut self, token: Uuid) -> Option<&str> {
        let (id, task) = self.tasks.iter_mut().find(|(_, task)| task.token == Some(token))?;
        task.token = None;
        Some(id)
    }

    pub fn claim_stop(&mut self, token: Uuid) -> Option<Value> {
        let id = self.take(token)?;
        Some(json!({"subtype":"stop_task","task_id":id}))
    }
}

pub(super) fn changed(provider: &str) -> Value {
    json!({"type":"provider_tasks_changed","provider":provider})
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "00abcdef0-01234567";
    const SECOND: &str = "00abcdef1-fedcba98";

    fn init(version: &str) -> Value {
        json!({"type":"system","subtype":"init","session_id":"s","claude_code_version":version})
    }

    fn start(id: &str, kind: &str, run: &str) -> Value {
        json!({"type":"system","subtype":"task_started","session_id":"s","task_id":id,
            "task_type":kind,"run_id":run,"description":"work","is_backgrounded":true})
    }

    fn token(controls: &TaskControls) -> Uuid {
        controls.list()[0]["token"].as_str().unwrap().parse().unwrap()
    }

    #[test]
    fn stop_tokens_are_live_and_bound_to_one_native_run() {
        let mut controls = TaskControls::default();
        let first = start("b12345678", "local_bash", FIRST);
        controls.observe(&init("2.1.329"));
        assert!(!controls.observe(&first));
        controls.observe(&init("2.1.330"));
        let mut replay = first.clone();
        replay["isReplay"] = json!(true);
        assert!(!controls.observe(&replay));
        let mut foreign = first.clone();
        foreign["session_id"] = json!("different");
        assert!(!controls.observe(&foreign));
        assert!(!controls.observe(&start("anything", "local_bash", FIRST)));
        assert!(!controls.observe(&start("r12345678", "remote_agent", FIRST)));
        assert!(controls.observe(&first));
        let old = token(&controls);
        assert!(!controls.observe(&first));
        let finished = json!({"type":"system","subtype":"task_notification","session_id":"s",
            "task_id":"b12345678","run_id":FIRST,"status":"completed"});
        assert!(controls.observe(&finished));
        assert!(controls.take(old).is_none());
        assert!(!controls.observe(&first));
        assert!(controls.observe(&start("b12345678", "local_bash", SECOND)));
        assert!(!controls.observe(&first));
        assert!(!controls.observe(&finished));
        let current = token(&controls);
        assert_ne!(old, current);
        assert!(controls.take(old).is_none());
        assert!(TaskControls::default().take(current).is_none());
        assert_eq!(controls.take(current), Some("b12345678"));
        assert!(controls.take(current).is_none());
        assert!(controls.observe(&start("a12345678", "local_agent", FIRST)));
        let left_behind = token(&controls);
        let mut reconnected = init("2.1.330");
        reconnected["session_id"] = json!("new-session");
        assert!(controls.observe(&reconnected));
        assert!(!controls.observe(&first));
        assert!(controls.claim_stop(left_behind).is_none());
        assert!(controls.list().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_task_uses_native_request_and_only_consumes_the_selected_offer() -> anyhow::Result<()> {
        use super::super::ClaudeProvider;
        use crate::providers::{api::{EventSink, LaunchContext, ProviderSpec}, registry::client::wrap_client};
        use std::{sync::Arc, time::Duration};
        use tokio_util::sync::CancellationToken;

        let script = r#"
while IFS= read -r line; do
    id=${line#*'"request_id":"'}
    id=${id%%'"'*}
    case "$line" in
        *'"type":"user"'*)
            printf '%s\n' '{"type":"system","subtype":"init","claude_code_version":"2.1.330","session_id":"s"}'
            for task in b12345678 b87654321; do
                printf '{"type":"system","subtype":"task_started","session_id":"s","task_id":"%s","description":"%s","task_type":"local_bash","run_id":"00abcdef0-01234567","is_backgrounded":true}\n' "$task" "$task"
            done
            continue ;;
        *'"subtype":"initialize"'*|*'"subtype":"apply_flag_settings"'*)
            printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
            continue ;;
        *'"subtype":"stop_task"'*) ;;
        *) exit 1 ;;
    esac
    case "$line" in
        *'"subtype":"stop_task"'*'"task_id":"b12345678"'*)
            printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
            ;;
        *)
            printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"wrong stop target"}}\n' "$id"
            ;;
    esac
done
"#;
        let root = tempfile::tempdir()?;
        let cancel = CancellationToken::new();
        let (sink, mut events) = EventSink::channel(64, cancel.child_token());
        let provider = wrap_client(Arc::new(ClaudeProvider::new(ProviderSpec {
            id: "claude".into(), family: "claude".into(),
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: Default::default(), options: json!({}), defaults: json!({}),
            runtime_settings: json!({}), configured_models: Vec::new(),
        })), "custom-claude".into(), vec![], vec![], false, json!({}))?;
        let session = provider.create_session(json!({"cwd":root.path()}), LaunchContext {
            agent_id: "task-test".into(), home: root.path().into(),
            executable: std::env::current_exe()?, env: Default::default(),
            mcp_servers: json!({}), tool_catalog: json!([]), tool_manifest: json!([]),
            tool_executor: None, cancel: cancel.child_token(),
        }, json!({}), sink).await?;
        session.start_turn(json!("fixture"), json!({})).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut count = 0;
            while count < 2 {
                let event = events.recv().await.expect("task stream closed");
                count += usize::from(event["type"] == "provider_tasks_changed");
            }
        }).await?;
        let offers = session.provider_tasks();
        assert_eq!(offers.len(), 2);
        let offer = offers[0]["token"].as_str().unwrap().parse()?;
        session.stop_provider_task(offer).await?;
        assert!(session.stop_provider_task(offer).await.is_err());
        assert_eq!(session.provider_tasks(), offers[1..]);
        session.close().await?;
        Ok(())
    }
}
