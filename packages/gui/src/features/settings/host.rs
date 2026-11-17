//! Adapter over `UiContext.state` and `CoreHandle` for settings. Every read of
//! the core state shape and every host-directory mutation goes through here so
//! a different core shape is a one-file change (see SettingsProviders-needs.md).

use serde_json::{Value, json};

use super::json::{arr, as_obj, get, str_of};
use paseo_gui_core::{CoreHandle, CoreState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Connected,
    Connecting,
    Error,
    Disconnected,
}

impl Status {
    fn parse(value: &str) -> Self {
        match value {
            "connected" | "online" => Status::Connected,
            "connecting" => Status::Connecting,
            "error" => Status::Error,
            _ => Status::Disconnected,
        }
    }
}

/// One saved host as settings needs it.
#[derive(Clone, Debug)]
pub struct HostView {
    pub server_id: String,
    pub label: String,
    pub status: Status,
    pub server_info: Value,
    pub error: Option<String>,
    pub config: Value,
    pub connections: Vec<Value>,
    pub projects: Vec<std::sync::Arc<Value>>,
    pub loading_projects: bool,
    pub project_error: Option<String>,
    pub maintenance_operation: Option<String>,
    pub local: bool,
}

impl HostView {
    pub fn connected(&self) -> bool {
        self.status == Status::Connected
    }

    /// Flutter `manage`: no permissions list means full access.
    pub fn can_manage(&self) -> bool {
        match self
            .server_info
            .get("permissions")
            .and_then(Value::as_array)
        {
            None => true,
            Some(list) => list.iter().any(|p| p.as_str() == Some("daemon.manage")),
        }
    }

    pub fn feature(&self, name: &str) -> bool {
        super::json::feature(&self.server_info, name)
    }

    pub fn version(&self) -> String {
        str_of(&self.server_info, "version").trim().to_string()
    }

    /// The server id the live connection reports (identity check after replies).
    pub fn reported_server_id(&self) -> String {
        let reported = str_of(&self.server_info, "serverId");
        if reported.is_empty() {
            self.server_id.clone()
        } else {
            reported.to_string()
        }
    }
}

fn error_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Object(map) => map.get("message").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

/// Saved hosts, local hosts first (Flutter `_sortedHosts`).
pub fn hosts(state: &CoreState) -> Vec<HostView> {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for entry in arr(state, "hosts") {
        let config = entry.get("config").cloned().unwrap_or_else(|| json!({}));
        let server_id = [
            str_of(entry, "serverId"),
            str_of(entry, "id"),
            str_of(&config, "expectedServerId"),
            str_of(&config, "id"),
        ]
        .into_iter()
        .find(|id| !id.is_empty())
        .unwrap_or("")
        .to_string();
        let label = [str_of(entry, "label"), str_of(&config, "label")]
            .into_iter()
            .find(|l| !l.is_empty())
            .map(String::from)
            .unwrap_or_else(|| server_id.clone());
        let server_info = entry
            .get("serverInfo")
            .or_else(|| entry.get("server_info"))
            .cloned()
            .unwrap_or(Value::Null);
        let mut connections: Vec<Value> = arr(entry, "connections").to_vec();
        if connections.is_empty() && config.is_object() {
            connections.push(config.clone());
        }
        let loading_lists = arr(entry, "loadingLists");
        let view = HostView {
            label,
            status: Status::parse(str_of(entry, "status")),
            server_info,
            error: error_text(get(entry, "error")),
            local: !config
                .get("localCredentialPath")
                .map(Value::is_null)
                .unwrap_or(true),
            connections,
            projects: state
                .directory(entry)
                .map(|directory| directory.projects.clone())
                .unwrap_or_default(),
            loading_projects: super::json::is_true(entry, "loading")
                || loading_lists.iter().any(|v| v.as_str() == Some("project")),
            project_error: error_text(get(get(entry, "listErrors"), "project")),
            maintenance_operation: Some(str_of(entry, "maintenanceOperationId").to_string())
                .filter(|s| !s.is_empty()),
            config,
            server_id,
        };
        if view.local {
            local.push(view)
        } else {
            remote.push(view)
        }
    }
    local.extend(remote);
    local
}

/// The whole preferences object (unknown keys included).
pub fn preferences(state: &Value) -> Value {
    match state.get("preferences") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({}),
    }
}

/// SharedPreferences key of the operation a host is updating (Flutter
/// `DaemonUpdateOperationStore._key`), stored by the core under `prefs:<key>`.
pub fn update_operation_key(server_id: &str) -> String {
    format!("paseo.daemonUpdate.operation.{server_id}")
}

/// The core keeps objects as their JSON text (Flutter key-value convention), so
/// a string is a saved submit request when it parses to an object and an old
/// ID-only record otherwise.
fn stored_update_record(state: &CoreState, server_id: &str) -> Option<Value> {
    let record = state.prefs.get(&update_operation_key(server_id))?;
    if let Some(text) = record.as_str()
        && let Ok(object @ Value::Object(_)) = serde_json::from_str(text)
    {
        return Some(object);
    }
    Some(record.clone())
}

/// The saved operation ID, or `None` when it is missing or not a UUID the
/// daemon can observe (such a record can only fail; drop it).
pub fn stored_update_operation(state: &CoreState, server_id: &str) -> Option<String> {
    let record = stored_update_record(state, server_id)?;
    record
        .as_str()
        .or_else(|| record["operationId"].as_str())
        .filter(|id| uuid::Uuid::parse_str(id.trim()).is_ok_and(|id| !id.is_nil()))
        .map(String::from)
}

/// Whether a saved record exists that [`stored_update_operation`] rejects.
pub fn has_unusable_update_record(state: &CoreState, server_id: &str) -> bool {
    state
        .prefs
        .get(&update_operation_key(server_id))
        .is_some_and(|record| !record.is_null())
        && stored_update_operation(state, server_id).is_none()
}

pub fn stored_update_request(state: &CoreState, server_id: &str) -> Option<Value> {
    stored_update_operation(state, server_id)?;
    let record = stored_update_record(state, server_id)?;
    if !record.is_object()
        || record["operationId"].as_str().is_none_or(str::is_empty)
        || record["source"] != "release"
    {
        return None;
    }
    if record["targetKind"] == "app" {
        let selected = record["selectedComponents"].as_array()?;
        if selected.is_empty()
            || selected.len() > 4
            || selected.iter().enumerate().any(|(index, id)| {
                id.as_str()
                    .is_none_or(|id| !super::model::daemon_update::COMPONENT_IDS.contains(&id))
                    || selected[..index].contains(id)
            })
            || record["expectedRelease"]["version"]
                .as_str()
                .is_none_or(str::is_empty)
            || record["expectedRelease"]["manifestSha512"]
                .as_str()
                .is_none_or(|pin| pin.len() != 88)
            || !record["forceRunning"].is_boolean()
            || record["idleTimeoutMs"]
                .as_u64()
                .is_none_or(|timeout| timeout == 0)
        {
            return None;
        }
    }
    Some(record.clone())
}

/// Flutter `DaemonUpdateOperationStore.clear`.
pub fn clear_update_operation(core: &CoreHandle, server_id: &str) {
    core.save_state(
        &format!("prefs:{}", update_operation_key(server_id)),
        Value::Null,
    );
}

/// Flutter `updateHost`: an existing connection id updates that connection.
pub fn update_host_config(core: &CoreHandle, config: Value) {
    core.connect_host(config);
}

/// Flutter `removeConnection` (the last connection removes the host).
pub fn remove_connection(core: &CoreHandle, server_id: &str, connection_id: &str) {
    core.remove_connection(server_id, connection_id);
}

// ---------------------------------------------------------------------------
// Endpoint labels (Flutter `HostConfig.resolve` reduced to what labels need).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndpointKind {
    Relay,
    Local,
    Ssh,
    Tcp,
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub kind: EndpointKind,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub user: String,
}

impl Endpoint {
    fn address(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    /// Standard web ports dropped (original `formatActiveConnectionLabel`).
    fn short_address(&self) -> String {
        match self.port {
            Some(port) if port != 80 && port != 443 => format!("{}:{port}", self.host),
            _ => self.host.clone(),
        }
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            out.push(value);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut count = 0;
    let mut out = Vec::new();
    for ch in input.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        bits = (bits << 6) | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

/// Splits `scheme://[user@]host[:port][/path]`.
fn split_url(input: &str) -> Option<(String, String, String, Option<u16>, String)> {
    let (scheme, rest) = input.split_once("://")?;
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, host)) => (user.to_string(), host),
        None => (String::new(), authority),
    };
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        let end = stripped.find(']')?;
        let port = stripped[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok());
        (format!("[{}]", &stripped[..end]), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(port) => (host.to_string(), Some(port)),
                Err(_) => return None,
            },
            None => (hostport.to_string(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("").to_string();
    Some((scheme.to_ascii_lowercase(), user, host, port, path))
}

pub fn resolve_endpoint(endpoint: &str) -> Option<Endpoint> {
    let mut input = endpoint.trim().to_string();
    if let Some(at) = input.find("#connect=") {
        input = percent_decode(&input[at + 9..]);
    }
    if let Some(at) = input.find("#offer=") {
        let bytes = base64url_decode(&input[at + 7..])?;
        let offer: Value = serde_json::from_slice(&bytes).ok()?;
        let relay = offer.get("relay")?;
        let address = relay.get("endpoint")?.as_str()?;
        let (host, port) = match address.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().ok()),
            None => (address.to_string(), None),
        };
        return Some(Endpoint {
            kind: EndpointKind::Relay,
            host,
            port,
            path: String::new(),
            user: String::new(),
        });
    }
    if !input.is_empty() && input.bytes().all(|b| b.is_ascii_digit()) {
        input = format!("ws://localhost:{input}/ws");
    }
    for prefix in ["unix://", "pipe://"] {
        if let Some(path) = input.strip_prefix(prefix) {
            if path.is_empty() || path == "~" {
                return None;
            }
            return Some(Endpoint {
                kind: EndpointKind::Local,
                host: String::new(),
                port: None,
                path: path.to_string(),
                user: String::new(),
            });
        }
    }
    if input.starts_with(r"\\.\pipe\") || input.starts_with('/') || input.starts_with("~/") {
        return Some(Endpoint {
            kind: EndpointKind::Local,
            host: String::new(),
            port: None,
            path: input,
            user: String::new(),
        });
    }
    if !input.contains("://") {
        input = format!("ws://{input}/ws");
    }
    let (scheme, user, host, port, path) = split_url(&input)?;
    let kind = match scheme.as_str() {
        "ssh" => EndpointKind::Ssh,
        "relay" => EndpointKind::Relay,
        "tcp" | "ws" | "wss" => EndpointKind::Tcp,
        _ => return None,
    };
    Some(Endpoint {
        kind,
        host,
        port,
        path,
        user,
    })
}

fn endpoint_of(config: &Value) -> Option<Endpoint> {
    resolve_endpoint(str_of(config, "endpoint"))
}

/// Flutter `hostConnectionLabel`.
pub fn connection_label(config: &Value) -> String {
    use super::i18n::t;
    let label = match endpoint_of(config) {
        Some(e) => match e.kind {
            EndpointKind::Relay => format!("{} ({})", t("settings.host.badges.relay"), e.address()),
            EndpointKind::Local => format!("{} ({})", t("settings.host.badges.local"), e.path),
            EndpointKind::Ssh => {
                let user = if e.user.is_empty() {
                    String::new()
                } else {
                    format!("{}@", e.user)
                };
                format!("{} ({user}{})", t("settings.host.badges.remoteSsh"), e.host)
            }
            EndpointKind::Tcp => format!("TCP ({})", e.address()),
        },
        None => t("settingsNative.invalidEndpoint"),
    };
    let mode = t(if str_of(config, "networkMode") == "tailnet" {
        "tailnet.embedded"
    } else {
        "tailnet.normal"
    });
    format!("{mode} · {label}")
}

/// Settings host picker hint (original `formatActiveConnectionLabel`).
pub fn active_connection_label(host: &HostView) -> String {
    if host.local {
        return super::i18n::t("settings.hostPicker.local");
    }
    match endpoint_of(&host.config) {
        Some(e) if e.kind == EndpointKind::Local => super::i18n::t("settings.hostPicker.local"),
        Some(e) => e.short_address(),
        None => String::new(),
    }
}

/// Host overview connection pill (original `formatActiveConnectionBadge`):
/// `(remote, label)`; remote picks the globe glyph.
pub fn connection_badge(host: &HostView) -> Option<(bool, String)> {
    use super::i18n::t;
    if str_of(&host.config, "networkMode") == "tailnet" {
        return Some((true, t("tailnet.embedded")));
    }
    let e = endpoint_of(&host.config)?;
    Some(match e.kind {
        EndpointKind::Relay => (true, t("settings.host.badges.relay")),
        EndpointKind::Local => (false, t("settings.host.badges.local")),
        EndpointKind::Ssh => (true, t("settings.host.badges.remoteSsh")),
        EndpointKind::Tcp => (false, e.short_address()),
    })
}

/// `config` with `label`/`color` applied (`None` color removes it).
pub fn config_with(config: &Value, label: Option<&str>, color: Option<Option<&str>>) -> Value {
    let mut next = as_obj(config).clone();
    if let Some(label) = label {
        next.insert("label".into(), Value::String(label.to_string()));
    }
    if let Some(color) = color {
        next.remove("color");
        if let Some(color) = color {
            next.insert("color".into(), Value::String(color.to_string()));
        }
    }
    Value::Object(next)
}

/// Identity of a project row (Flutter `projectRootPath` → `repoRoot` → `cwd`).
pub fn project_root(project: &Value) -> String {
    for key in ["projectRootPath", "repoRoot", "cwd"] {
        let value = str_of(project, key);
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

pub fn project_name(project: &Value) -> String {
    let name = str_of(project, "name");
    if !name.is_empty() {
        return name.to_string();
    }
    project_root(project)
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod update_restore_tests {
    use super::*;

    const OP_A: &str = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
    const OP_B: &str = "6d1f3a2b-8c4e-4f1a-9b7d-2e5c8a0f4d33";

    /// Preferences hold objects as JSON text, exactly as the core stores them.
    fn saved(record: &Value) -> Value {
        json!(record.to_string())
    }

    #[test]
    fn restores_exact_host_request_and_legacy_ids_without_cross_wiring() {
        let request = json!({"operationId":OP_A,"source":"release","targetKind":"app",
            "forceRunning":false,"idleTimeoutMs":300_000,
            "selectedComponents":["daemon","update-helper"],
            "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)}});
        let mut state = CoreState::default();
        let prefs = std::sync::Arc::make_mut(&mut state.prefs);
        prefs.insert(update_operation_key("host-a"), saved(&request));
        prefs.insert(update_operation_key("host-b"), json!(OP_B));
        prefs.insert(update_operation_key("host-d"), json!("op-d"));
        assert_eq!(
            stored_update_operation(&state, "host-a").as_deref(),
            Some(OP_A)
        );
        assert_eq!(stored_update_request(&state, "host-a"), Some(request));
        assert_eq!(
            stored_update_operation(&state, "host-b").as_deref(),
            Some(OP_B)
        );
        assert_eq!(stored_update_request(&state, "host-b"), None);
        assert_eq!(stored_update_operation(&state, "host-c"), None);
        assert!(!has_unusable_update_record(&state, "host-c"));
        // A non-UUID ID can never be observed by the daemon: drop it.
        assert_eq!(stored_update_operation(&state, "host-d"), None);
        assert!(has_unusable_update_record(&state, "host-d"));
        assert!(!has_unusable_update_record(&state, "host-a"));
    }

    #[test]
    fn incomplete_desktop_approval_keeps_id_for_observation_but_never_reconstructs_submit() {
        let request = json!({"operationId":OP_A,"source":"release","targetKind":"app",
            "forceRunning":true,"idleTimeoutMs":300_000,"selectedComponents":["daemon"],
            "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)}});
        for field in [
            "selectedComponents",
            "expectedRelease",
            "forceRunning",
            "idleTimeoutMs",
        ] {
            let mut partial = request.clone();
            partial.as_object_mut().unwrap().remove(field);
            let mut state = CoreState::default();
            std::sync::Arc::make_mut(&mut state.prefs)
                .insert(update_operation_key("host"), saved(&partial));
            assert_eq!(
                stored_update_operation(&state, "host").as_deref(),
                Some(OP_A)
            );
            assert!(stored_update_request(&state, "host").is_none(), "{field}");
        }
        for selected in [json!([]), json!(["daemon", "daemon"]), json!(["unknown"])] {
            let mut invalid = request.clone();
            invalid["selectedComponents"] = selected;
            let mut state = CoreState::default();
            std::sync::Arc::make_mut(&mut state.prefs)
                .insert(update_operation_key("host"), saved(&invalid));
            assert!(stored_update_request(&state, "host").is_none());
        }
    }
}
