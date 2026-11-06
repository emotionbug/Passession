//! Client-boundary compatibility with older daemons (docs/protocol-compatibility.md):
//! pre-registry workspace grouping (`legacy_workspaces.dart`) and the legacy observation
//! rules of `PaseoClient._startObservation` / `_legacyMatches` / `_releaseObservation`.
use serde_json::{Map, Value, json};

/// Compatibility with daemons predating the workspace registry. IDs are opaque: a Windows
/// drive root is never converted back into a filesystem path.
#[derive(Default)]
pub(crate) struct LegacyWorkspaces {
    agents: Map<String, Value>,
}

fn workspace_id(path: &str) -> String {
    let value = path.trim().replace('\\', "/");
    let value = value.trim_end_matches('/');
    if value.is_empty() {
        "/".to_owned()
    } else {
        value.to_owned()
    }
}

fn stamp(agent: &Value) -> Value {
    let mut agent = agent.clone();
    if agent["workspaceId"].is_null()
        && let Some(cwd) = agent["cwd"].as_str() {
            agent["workspaceId"] = Value::String(workspace_id(cwd));
        }
    agent
}

fn trimmed(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

impl LegacyWorkspaces {
    pub fn normalize(&self, mut message: Value) -> Value {
        if !message["payload"].is_object() {
            return message;
        }
        let kind = message["type"].as_str().unwrap_or_default().to_owned();
        if kind == "fetch_agents_response" {
            if let Some(entries) = message["payload"]["entries"].as_array_mut() {
                for entry in entries {
                    if entry["agent"].is_object() {
                        entry["agent"] = stamp(&entry["agent"]);
                    }
                }
            }
            return message;
        }
        let status = message["payload"]["status"].as_str().unwrap_or_default();
        let stamped = matches!(
            kind.as_str(),
            "agent_update" | "fetch_agent_response" | "fetch_agent_timeline_response" | "cancel_agent_response"
        ) || (kind == "status" && matches!(status, "agent_created" | "agent_resumed"));
        if stamped && message["payload"]["agent"].is_object() {
            let agent = stamp(&message["payload"]["agent"]);
            message["payload"]["agent"] = agent;
        }
        message
    }

    /// Workspaces of one agent page; a first page resets the grouping.
    pub fn read(&mut self, entries: &[Value], reset: bool) -> Vec<Value> {
        if reset {
            self.agents.clear();
        }
        let mut ids = Vec::new();
        for entry in entries {
            let Some(agent_id) = entry["agent"]["id"].as_str() else {
                continue;
            };
            self.agents.insert(agent_id.to_owned(), entry.clone());
            let id = entry["agent"]["workspaceId"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    entry["project"]["checkout"]["cwd"]
                        .as_str()
                        .map(workspace_id)
                });
            if let Some(id) = id {
                ids.push(id);
            }
        }
        self.workspaces()
            .into_iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(_, workspace)| workspace)
            .collect()
    }

    /// Synthesized `workspace_update` events caused by one `agent_update`.
    pub fn update(&mut self, message: &Value) -> Vec<Value> {
        if message["type"] != "agent_update" {
            return Vec::new();
        }
        let before = self.workspaces();
        let payload = &message["payload"];
        if payload["kind"] == "remove" {
            if let Some(id) = payload["agentId"].as_str() {
                self.agents.remove(id);
            }
        } else if let Some(agent_id) = payload["agent"]["id"].as_str() {
            let project = if payload["project"].is_null() {
                self.agents
                    .get(agent_id)
                    .map(|entry| entry["project"].clone())
                    .unwrap_or(Value::Null)
            } else {
                payload["project"].clone()
            };
            if !payload["agent"]["archivedAt"].is_null() {
                self.agents.remove(agent_id);
            } else if !project.is_null() {
                self.agents.insert(
                    agent_id.to_owned(),
                    json!({"agent": payload["agent"], "project": project}),
                );
            }
        }
        let after = self.workspaces();
        let mut events = Vec::new();
        for (id, workspace) in &after {
            if before.get(id) != Some(workspace) {
                events.push(json!({"type":"workspace_update","payload":{"kind":"upsert","workspace":workspace}}));
            }
        }
        for id in before.keys() {
            if !after.contains_key(id) {
                events.push(json!({"type":"workspace_update","payload":{"kind":"remove","id":id}}));
            }
        }
        events
    }

    fn workspaces(&self) -> Map<String, Value> {
        fn priority(status: &str) -> u8 {
            match status {
                "needs_input" => 0,
                "failed" => 1,
                "running" => 2,
                "attention" => 3,
                _ => 4,
            }
        }
        let mut result = Map::new();
        for entry in self.agents.values() {
            let agent = &entry["agent"];
            let project = &entry["project"];
            let checkout = &project["checkout"];
            let Some(id) = agent["workspaceId"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| checkout["cwd"].as_str().map(workspace_id))
            else {
                continue;
            };
            let pending = agent["pendingPermissions"]
                .as_array()
                .is_some_and(|p| !p.is_empty());
            let status = if pending || agent["attentionReason"] == "permission" {
                "needs_input"
            } else if agent["status"] == "error" || agent["attentionReason"] == "error" {
                "failed"
            } else if agent["status"] == "running" {
                "running"
            } else if agent["requiresAttention"] == true {
                "attention"
            } else {
                "done"
            };
            if let Some(existing) = result.get(&id)
                && priority(existing["status"].as_str().unwrap_or_default()) <= priority(status) {
                    continue;
                }
            let is_git = checkout["isGit"] == true;
            let name = trimmed(&project["workspaceName"])
                .map(str::to_owned)
                .or_else(|| {
                    trimmed(&checkout["currentBranch"])
                        .filter(|branch| *branch != "HEAD")
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| id.rsplit('/').next().unwrap_or_default().to_owned());
            let root = [&checkout["mainRepoRoot"], &checkout["worktreeRoot"], &checkout["cwd"]]
                .into_iter()
                .find(|v| !v.is_null())
                .cloned()
                .unwrap_or(Value::Null);
            let entered = if agent["attentionTimestamp"].is_null() {
                agent["updatedAt"].clone()
            } else {
                agent["attentionTimestamp"].clone()
            };
            let workspace = json!({
                "id": id,
                "projectId": project["projectKey"],
                "projectDisplayName": project["projectName"],
                "projectCustomName": null,
                "projectRootPath": root,
                "workspaceDirectory": checkout["cwd"],
                "projectKind": if is_git { "git" } else { "non_git" },
                "workspaceKind": if !is_git { "directory" } else if checkout["isPaseoOwnedWorktree"] == true { "worktree" } else { "checkout" },
                "name": name,
                "title": null,
                "status": status,
                "statusEnteredAt": entered,
                "activityAt": agent["updatedAt"],
                "archivingAt": null,
                "diffStat": null,
                "scripts": [],
                "githubRuntime": null,
                "project": project,
                "gitRuntime": if !is_git { Value::Null } else { json!({
                    "currentBranch": checkout["currentBranch"],
                    "remoteUrl": checkout["remoteUrl"],
                    "isPaseoOwnedWorktree": checkout["isPaseoOwnedWorktree"],
                    "isDirty": null,
                    "aheadBehind": null,
                    "aheadOfOrigin": null,
                    "behindOfOrigin": null,
                }) },
            });
            result.insert(id, workspace);
        }
        result
    }
}

/// Session event names an older explicit-event daemon accepts (Flutter legacy filter).
pub(crate) const LEGACY_EVENTS: [&str; 5] = [
    "project.update",
    "providers_snapshot_update",
    "agent_attention_required",
    "agent_permission_request",
    "agent_permission_resolved",
];

/// Flutter `_legacyMatches`: which broadcast message a legacy observation receives.
pub(crate) fn legacy_matches(request_type: &str, fields: &Value, message: &Value) -> bool {
    let payload = if message["payload"].is_object() {
        &message["payload"]
    } else {
        message
    };
    let kind = message["type"].as_str().unwrap_or_default();
    let contains = |list: &Value, value: &Value| {
        list.as_array().is_some_and(|items| items.contains(value))
    };
    match request_type {
        "fetch_agents_request" => kind == "agent_update",
        "fetch_workspaces_request" => kind == "workspace_update",
        "workspace.label.list.request" => kind == "workspace.label.update",
        "subscribe_terminals_request" => kind == "terminals_changed" && payload["cwd"] == fields["cwd"],
        "subscribe_terminal_request" => {
            kind == "terminal_stream_exit" && payload["terminalId"] == fields["terminalId"]
        }
        "agent.timeline.set_subscription.request" => {
            (kind == "agent_stream" || kind == "agent.timeline.replacement")
                && contains(&fields["agentIds"], &payload["agentId"])
        }
        "session.events.set_subscription.request" => {
            let name = if kind == "status" {
                Value::String(format!("status.{}", payload["status"].as_str().unwrap_or("null")))
            } else {
                Value::String(kind.to_owned())
            };
            contains(&fields["events"], &name)
        }
        _ => false,
    }
}

pub(crate) fn sorted_union<'a>(lists: impl Iterator<Item = &'a Value>) -> Vec<Value> {
    let mut members: Vec<String> = lists
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    members.sort();
    members.dedup();
    members.into_iter().map(Value::String).collect()
}

pub(crate) fn legacy_event_filter(events: Vec<Value>) -> Vec<Value> {
    events
        .into_iter()
        .filter(|e| e.as_str().is_some_and(|name| LEGACY_EVENTS.contains(&name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, cwd: &str, status: &str) -> Value {
        json!({"agent":{"id":id,"cwd":cwd,"status":status,"updatedAt":"t"},"project":{"projectKey":"p","projectName":"P","checkout":{"cwd":cwd,"isGit":false}}})
    }

    #[test]
    fn agents_group_into_workspaces_by_priority() {
        let mut legacy = LegacyWorkspaces::default();
        let rows = legacy.read(&[entry("a", "/w/", "idle"), entry("b", "/w", "running")], true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "/w");
        assert_eq!(rows[0]["status"], "running");
        assert_eq!(rows[0]["name"], "w");
        let events = legacy.update(&json!({"type":"agent_update","payload":{"kind":"remove","agentId":"b"}}));
        assert_eq!(events[0]["payload"]["workspace"]["status"], "done");
        let events = legacy.update(&json!({"type":"agent_update","payload":{"kind":"remove","agentId":"a"}}));
        assert_eq!(events[0]["payload"], json!({"kind":"remove","id":"/w"}));
    }
}
