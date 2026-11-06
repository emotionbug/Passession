//! The one adapter between these features and the shell's directory/preferences state
//! (`UiContext.state`). Every read of host identity, connection status, server info,
//! projects, workspaces, agents and preferences goes through here, so a different state
//! shape published by CoreTransport is a one-file change.
//!
//! Host identity is the daemon `serverId` (Flutter `HostState.serverId`). Two hosts with
//! equal directories or agent names stay distinct because every list is read per host.

use std::cell::RefCell;
use std::collections::HashMap;

use paseo_gui_core::CoreEvent;
use serde_json::Value;

use super::rpc::{array_of, empty_object, object_of, str_of};

/// Flutter `ConnectionStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Connecting,
    Disconnected,
    Error,
}

impl Connection {
    fn parse(value: &str) -> Self {
        match value {
            "connected" | "online" | "ready" => Self::Connected,
            "connecting" | "reconnecting" | "authenticating" => Self::Connecting,
            "error" | "failed" => Self::Error,
            _ => Self::Disconnected,
        }
    }
}

thread_local! {
    /// server_info and connection facts learned from core events, for hosts whose state
    /// entry does not (yet) carry them.
    static LEARNED: RefCell<HashMap<String, (Connection, Value)>> = RefCell::new(HashMap::new());
}

/// Records `Connected`/`Disconnected` facts from an event batch. Cheap; call once per batch.
pub fn learn(events: &[CoreEvent]) {
    if events.is_empty() {
        return;
    }
    LEARNED.with(|learned| {
        let mut learned = learned.borrow_mut();
        for event in events {
            match event {
                CoreEvent::Connected { host_id, server_info } => {
                    learned.insert(host_id.clone(), (Connection::Connected, server_info.clone()));
                }
                CoreEvent::Disconnected { host_id, error } => {
                    let info = learned.remove(host_id).map(|(_, info)| info).unwrap_or(Value::Null);
                    let status = if error.is_some() { Connection::Error } else { Connection::Disconnected };
                    learned.insert(host_id.clone(), (status, info));
                }
                _ => {}
            }
        }
    });
}

/// One host entry of the directory state.
#[derive(Clone, Copy)]
pub struct Host<'a> {
    value: &'a Value,
}

impl<'a> Host<'a> {
    pub fn raw(&self) -> &'a Value {
        self.value
    }

    /// The daemon server id this host routes to.
    pub fn id(&self) -> &'a str {
        for key in ["serverId", "id", "hostId"] {
            let value = str_of(self.value, key);
            if !value.is_empty() {
                return value;
            }
        }
        ""
    }

    /// `hostDisplayLabel`: the configured label, else the server id.
    pub fn label(&self) -> String {
        for key in ["label", "name", "displayName"] {
            let value = str_of(self.value, key).trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
        let info = self.server_info();
        let hostname = str_of(info, "hostname").trim();
        if !hostname.is_empty() {
            return hostname.to_owned();
        }
        self.id().to_owned()
    }

    pub fn status(&self) -> Connection {
        let status = self
            .value
            .get("status")
            .or_else(|| self.value.get("connectionStatus"))
            .and_then(|s| s.as_str().map(str::to_owned).or_else(|| s.get("status").and_then(Value::as_str).map(str::to_owned)));
        match status {
            Some(status) => Connection::parse(&status),
            None => LEARNED.with(|l| l.borrow().get(self.id()).map(|(c, _)| *c)).unwrap_or(Connection::Disconnected),
        }
    }

    pub fn connected(&self) -> bool {
        self.status() == Connection::Connected
    }

    /// The daemon's `server_info` status payload.
    pub fn server_info(&self) -> &'a Value {
        for key in ["serverInfo", "server_info"] {
            if let Some(info) = self.value.get(key).filter(|v| v.is_object()) {
                return info;
            }
        }
        empty_object()
    }

    /// `PaseoClient.supports(feature)`.
    pub fn supports(&self, feature: &str) -> bool {
        let info = self.server_info();
        if info.as_object().is_some_and(|m| !m.is_empty()) {
            return object_of(info, "features").get(feature) == Some(&Value::Bool(true));
        }
        LEARNED.with(|learned| {
            learned
                .borrow()
                .get(self.id())
                .is_some_and(|(_, info)| info.get("features").and_then(|f| f.get(feature)) == Some(&Value::Bool(true)))
        })
    }

    /// Whether the connection was granted `permission`; hosts that report no permission list
    /// grant everything (schedule form `canManage`).
    pub fn has_permission(&self, permission: &str) -> bool {
        let info = self.server_info();
        match info.get("permissions") {
            None => true,
            Some(Value::Array(list)) => list.iter().any(|p| p.as_str() == Some(permission)),
            Some(_) => false,
        }
    }

    /// Local (desktop-managed) hosts hide their badge.
    pub fn is_local(&self) -> bool {
        self.value.get("isLocal") == Some(&Value::Bool(true))
            || self.value.get("local") == Some(&Value::Bool(true))
            || str_of(self.value, "kind") == "local"
    }

    pub fn agents(&self) -> &'a [Value] {
        array_of(self.value, "agents")
    }

    /// Whether the agent directory has been hydrated at least once. A missing agent only
    /// counts as gone once the directory is known.
    pub fn agents_loaded(&self) -> bool {
        !self.agents().is_empty()
    }

    pub fn projects(&self) -> &'a [Value] {
        array_of(self.value, "projects")
    }

    pub fn workspaces(&self) -> &'a [Value] {
        array_of(self.value, "workspaces")
    }

    /// An agent row, unwrapping the `{agent: {...}}` directory envelope.
    pub fn agent(&self, agent_id: &str) -> Option<&'a Value> {
        self.agents().iter().map(agent_of).find(|agent| str_of(agent, "id") == agent_id)
    }

    pub fn workspace(&self, workspace_id: &str) -> Option<&'a Value> {
        self.workspaces().iter().find(|w| str_of(w, "id") == workspace_id)
    }

    /// The project whose root is `root` (`projectRootPath`, else `repoRoot`).
    pub fn project_by_root(&self, root: &str) -> Option<&'a Value> {
        let root = root.trim();
        if root.is_empty() {
            return None;
        }
        self.projects().iter().find(|p| project_root(p).trim() == root)
    }
}

/// `_agentOf`: directory rows may wrap the agent.
pub fn agent_of(entry: &Value) -> &Value {
    match entry.get("agent") {
        Some(inner) if inner.as_object().is_some_and(|m| !m.is_empty()) => inner,
        _ => entry,
    }
}

/// `_projectRoot`: `projectRootPath`, else `repoRoot`, else `cwd`.
pub fn project_root(project: &Value) -> &str {
    for key in ["projectRootPath", "repoRoot", "cwd"] {
        let value = str_of(project, key);
        if !value.is_empty() {
            return value;
        }
    }
    ""
}

/// `_projectName`: display name, name, else the last path segment.
pub fn project_name(project: &Value) -> String {
    for key in ["projectDisplayName", "name"] {
        let value = str_of(project, key);
        if !value.is_empty() {
            return value.to_owned();
        }
    }
    last_segment(project_root(project))
}

pub fn last_segment(path: &str) -> String {
    path.split(['/', '\\']).rfind(|s| !s.is_empty()).unwrap_or(path).to_owned()
}

/// Every host of the directory state, in directory order.
pub fn hosts(state: &Value) -> Vec<Host<'_>> {
    let list = match state.get("hosts") {
        Some(Value::Array(list)) => list.iter().collect::<Vec<_>>(),
        Some(Value::Object(map)) => map.values().collect(),
        _ => Vec::new(),
    };
    list.into_iter().map(|value| Host { value }).filter(|h| !h.id().is_empty()).collect()
}

pub fn host<'a>(state: &'a Value, host_id: &str) -> Option<Host<'a>> {
    hosts(state).into_iter().find(|h| h.id() == host_id)
}

/// The preferences object (Flutter `AppStore.preferences`).
pub fn preferences(state: &Value) -> &Value {
    object_of(state, "preferences")
}

/// Writes one preference through the shell, the single preferences writer (`ui-api.md`
/// §12): `Null` removes the key; unknown keys survive.
pub fn set_preference(effects: &mut Vec<crate::common::UiEffect>, key: &str, value: Value) {
    effects.push(crate::common::UiEffect::SetPreference { key: key.to_owned(), value });
}

/// Whether the shell's current palette is dark (stored theme, `auto` following the system).
pub fn is_dark(_state: &Value) -> bool {
    crate::theme::current_palette().is_dark
}

/// Phone layout (Flutter `isCompactLayout`, width below 600 logical pixels).
pub fn is_compact(width: f64) -> bool {
    width < 600.0
}

/// Whether the build is a mobile target (Flutter `Platform.isAndroid || Platform.isIOS`).
pub const fn is_mobile() -> bool {
    cfg!(any(target_os = "android", target_os = "ios"))
}

pub const fn is_apple() -> bool {
    cfg!(any(target_os = "macos", target_os = "ios"))
}
