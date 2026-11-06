//! The one adapter between core `State.hosts[]` (core-api.md) and the shell: Flutter
//! `HostState` accessors, `hostDisplayLabel`, agent/workspace/project lookups.

use serde_json::Value;

use crate::common::json as j;

/// A borrowed `state.hosts[i]` record.
#[derive(Clone, Copy)]
pub struct Host<'a>(pub &'a Value);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connected,
    Connecting,
    Disconnected,
    Error,
}

impl<'a> Host<'a> {
    pub fn server_id(&self) -> &'a str {
        j::text(self.0, "serverId")
    }

    pub fn config(&self) -> &'a Value {
        j::object(self.0, "config")
    }

    pub fn connections(&self) -> &'a [Value] {
        j::array(self.0, "connections")
    }

    pub fn status(&self) -> Status {
        match j::text(self.0, "status") {
            "connected" => Status::Connected,
            "connecting" => Status::Connecting,
            "error" => Status::Error,
            _ => Status::Disconnected,
        }
    }

    pub fn connected(&self) -> bool {
        self.status() == Status::Connected
    }

    pub fn loading(&self) -> bool {
        j::boolean(self.0, "loading", false)
    }

    pub fn loading_lists(&self) -> Vec<&'a str> {
        j::array(self.0, "loadingLists").iter().filter_map(Value::as_str).collect()
    }

    /// `listErrors` as (kind, message).
    pub fn list_errors(&self) -> Vec<(&'a str, String)> {
        match self.0.get("listErrors") {
            Some(Value::Object(map)) => map.iter().map(|(k, v)| (k.as_str(), error_text(v))).collect(),
            _ => Vec::new(),
        }
    }

    pub fn error(&self) -> Option<String> {
        match self.0.get("error") {
            None | Some(Value::Null) => None,
            Some(value) => Some(error_text(value)),
        }
    }

    pub fn server_info(&self) -> &'a Value {
        j::object(self.0, "serverInfo")
    }

    pub fn features(&self) -> &'a Value {
        j::object(self.server_info(), "features")
    }

    pub fn supports(&self, feature: &str) -> bool {
        j::boolean(self.features(), feature, false)
    }

    pub fn is_local(&self) -> bool {
        self.config().get("localCredentialPath").is_some_and(|v| !v.is_null())
    }

    pub fn color(&self) -> &'a str {
        j::text_or(self.config(), "color", "none")
    }

    pub fn projects(&self) -> &'a [Value] {
        j::array(self.0, "projects")
    }

    pub fn workspaces(&self) -> &'a [Value] {
        j::array(self.0, "workspaces")
    }

    pub fn agents(&self) -> impl Iterator<Item = &'a Value> {
        j::array(self.0, "agents").iter().map(j::agent_of)
    }

    pub fn agent(&self, id: &str) -> Option<&'a Value> {
        self.agents().find(|agent| j::text(agent, "id") == id)
    }

    pub fn workspace(&self, id: &str) -> Option<&'a Value> {
        self.workspaces().iter().find(|w| j::text(w, "id") == id)
    }

    /// `hostDisplayLabel`: a stored label that only repeats the endpoint or server id is
    /// unnamed, so the daemon hostname wins over it; the endpoint stays the last resort.
    pub fn display_label(&self) -> String {
        let config = self.config();
        let label = j::text(config, "label").trim();
        let endpoint = j::text(config, "endpoint").trim();
        let without_scheme = match endpoint.find("://") {
            Some(i) if endpoint[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
                && endpoint[..i].chars().next().is_some_and(|c| c.is_ascii_alphabetic()) =>
            {
                &endpoint[i + 3..]
            }
            _ => endpoint,
        };
        let address = without_scheme.split('/').next().unwrap_or("");
        let unnamed = label.is_empty()
            || label == self.server_id()
            || label == endpoint
            || label == address
            || (address.contains(':') && label == &address[..address.rfind(':').unwrap_or(0)]);
        if !unnamed {
            return label.to_string();
        }
        let hostname = j::text(self.server_info(), "hostname").trim();
        if !hostname.is_empty() {
            return hostname.to_string();
        }
        if label.is_empty() { self.server_id().to_string() } else { label.to_string() }
    }

    /// `HostState.label`: the configured label or the server id.
    pub fn label(&self) -> String {
        let label = j::text(self.config(), "label");
        if label.is_empty() { self.server_id().to_string() } else { label.to_string() }
    }
}

pub fn error_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            let message = j::text(value, "message");
            if message.is_empty() { j::text(value, "code").to_string() } else { message.to_string() }
        }
        other => other.to_string(),
    }
}

pub fn hosts(state: &Value) -> Vec<Host<'_>> {
    j::array(state, "hosts").iter().map(Host).collect()
}

pub fn host<'a>(state: &'a Value, id: &str) -> Option<Host<'a>> {
    crate::common::host_record(state, id).map(Host)
}

/// `_parentAgentId`: `parentAgentId` or the `paseo.parent-agent-id` label.
pub fn parent_agent_id(agent: &Value) -> String {
    let parent = j::text(agent, "parentAgentId");
    if !parent.is_empty() {
        return parent.to_string();
    }
    j::text(j::object(agent, "labels"), "paseo.parent-agent-id").trim().to_string()
}

pub fn is_archived(agent: &Value) -> bool {
    !j::is_null_or_missing(agent, "archivedAt")
}
