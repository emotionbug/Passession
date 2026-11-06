//! Shared UI boundary between the shell and feature roots (see `ui-api.md`).
//!
//! The frozen contract items (`UiContext` core fields, `Route`, `UiEffect` first variants)
//! come first; additions follow and never change the frozen shapes.

pub mod clock;
pub mod i18n;
pub mod icons;
pub mod json;
pub mod overlay;
pub mod paint;
pub mod provider_icon;
pub mod widgets;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use makepad_widgets::*;
use paseo_gui_core::{CoreError, CoreEvent, CoreHandle};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value, json};

pub use i18n::{locale_tag, tr, tr_args, tr_count};
pub use paseo_gui_core::RequestId;

pub fn live_design(cx: &mut Cx) {
    paint::live_design(cx);
    widgets::live_design(cx);
    provider_icon::live_design(cx);
}

/// User input has a visible/topmost owner. Focus transitions and background work
/// remain broadcast so retained roots can release state and finish their work.
pub(crate) fn is_user_input(event: &Event) -> bool {
    matches!(event,
        Event::MouseDown(_) | Event::MouseMove(_) | Event::MouseUp(_) | Event::MouseLeave(_)
        | Event::TouchUpdate(_) | Event::LongPress(_) | Event::Scroll(_)
        | Event::KeyDown(_) | Event::KeyUp(_) | Event::TextInput(_) | Event::TextIme(_)
        | Event::TextCopy(_) | Event::TextCut(_) | Event::TextSelectAll | Event::BackPressed { .. }
        | Event::Drag(_) | Event::Drop(_))
}

/// Per-dispatch UI context handed to feature widgets as Makepad `Scope` data.
pub struct UiContext {
    pub core: CoreHandle,
    pub state: Arc<Value>,
    pub events: Vec<CoreEvent>,
    pub route: Route,
    pub effects: Vec<UiEffect>,
    // Additions (GuiShell).
    pub target: Option<PanelTarget>,
    pub tab_key: Option<String>,
    pub activity: PanelActivity,
    pub compact: bool,
    pub window_id: String,
    pub commands: Vec<UiCommand>,
    pub layout: Rc<LayoutSnapshot>,
    pub server_infos: Rc<HashMap<String, Value>>,
}

impl UiContext {
    pub fn new(core: CoreHandle) -> Self {
        Self {
            core,
            state: Arc::new(Value::Null),
            events: Vec::new(),
            route: Route::Welcome,
            effects: Vec::new(),
            target: None,
            tab_key: None,
            activity: PanelActivity::default(),
            compact: false,
            window_id: MAIN_WINDOW_ID.to_string(),
            commands: Vec::new(),
            layout: Rc::new(LayoutSnapshot::default()),
            server_infos: Rc::new(HashMap::new()),
        }
    }

    pub fn push(&mut self, effect: UiEffect) {
        self.effects.push(effect);
    }

    pub fn toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.effects.push(UiEffect::Toast { message: message.into(), is_error });
    }

    pub fn preferences(&self) -> &Value {
        self.state.get("preferences").unwrap_or(&Value::Null)
    }

    pub fn preference(&self, key: &str) -> Option<&Value> {
        self.preferences().get(key)
    }

    /// `state.hosts[i]` by server id, config id or any saved connection id (Flutter `hostFor`).
    pub fn host(&self, host_id: &str) -> Option<&Value> {
        host_record(&self.state, host_id)
    }

    /// Host events of the current batch, in wire order.
    pub fn host_events(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.events.iter().filter_map(|event| match event {
            CoreEvent::HostEvent { host_id, event } => Some((host_id.as_str(), event)),
            _ => None,
        })
    }

    pub fn response(&self, request_id: &str) -> Option<&Result<Value, CoreError>> {
        self.events.iter().find_map(|event| match event {
            CoreEvent::Response { request_id: id, result, .. } if id == request_id => Some(result),
            _ => None,
        })
    }
}

/// Window id of the primary window (Flutter `windowId` default on every platform).
pub const MAIN_WINDOW_ID: &str = "main";

/// `state.hosts[i]` lookup shared by the shell and `UiContext::host`.
pub fn host_record<'a>(state: &'a Value, host_id: &str) -> Option<&'a Value> {
    let hosts = state.get("hosts")?.as_array()?;
    hosts.iter().find(|host| {
        json::text(host, "serverId") == host_id
            || json::text(json::object(host, "config"), "id") == host_id
            || json::array(host, "connections")
                .iter()
                .any(|c| json::text(c, "id") == host_id)
    })
}

/// Flutter `PanelActivity`: presentation of the dispatched panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelActivity {
    pub visible: bool,
    pub focused: bool,
    pub app_active: bool,
}

impl Default for PanelActivity {
    /// Standalone panels (pages, overlays) are shown, focused and foreground.
    fn default() -> Self {
        Self { visible: true, focused: true, app_active: true }
    }
}

/// A shell command offered to one tab root (Flutter `PaseoCommands.invoke`).
#[derive(Clone, Debug)]
pub struct UiCommand {
    pub tab_key: String,
    pub action: String,
    pub args: Value,
    pub handled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LayoutSnapshot {
    pub tabs: Vec<PanelTarget>,
    pub panes: Vec<PaneSnapshot>,
    pub focused_pane: String,
    pub selected: Option<String>,
    pub explorer_tabs: Vec<String>,
    pub selected_explorer: Option<String>,
}

impl LayoutSnapshot {
    pub fn target(&self, key: &str) -> Option<&PanelTarget> {
        self.tabs.iter().find(|tab| tab.key() == key)
    }

    pub fn selected_target(&self) -> Option<&PanelTarget> {
        self.selected.as_deref().and_then(|key| self.target(key))
    }
}

#[derive(Clone, Debug, Default)]
pub struct PaneSnapshot {
    pub id: String,
    pub tabs: Vec<String>,
    pub selected: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    Welcome,
    Session { host_id: String, agent_id: String },
    Files { host_id: String, workspace_id: String, path: Option<String> },
    Git { host_id: String, workspace_id: String },
    Settings { host_id: Option<String>, page: String },
    Workspace { host_id: String, workspace_id: Option<String> },
    Schedules { host_id: String },
    CommandCenter,
    // Additions.
    Subagent { host_id: String, parent_agent_id: String, subagent_id: String },
    Draft { host_id: String, workspace_id: String, draft_id: String },
    Setup { host_id: String, workspace_id: String },
    ProjectSettings { host_id: String, project_key: String },
    ExecutionLog { host_id: String, workspace_id: String, terminal_id: String },
    Sessions { host_id: Option<String> },
}

impl Route {
    /// The route a tab target presents.
    pub fn from_target(target: &PanelTarget) -> Route {
        let host_id = target.server_id.clone();
        let workspace_id = target.workspace_id.clone();
        let id = target.id.clone().unwrap_or_default();
        match target.kind.as_str() {
            "agent" => Route::Session { host_id, agent_id: id },
            "provider_subagent" => Route::Subagent {
                host_id,
                parent_agent_id: json::text(&target.data, "parentAgentId").to_string(),
                subagent_id: id,
            },
            "draft" => Route::Draft { host_id, workspace_id, draft_id: id },
            "files" => Route::Files { host_id, workspace_id, path: None },
            "file" => {
                let path = json::text(&target.data, "path");
                Route::Files {
                    host_id,
                    workspace_id,
                    path: Some(if path.is_empty() { id } else { path.to_string() }),
                }
            }
            "changes_tree" | "working_diff" | "commit_diff" | "pull_request" => {
                Route::Git { host_id, workspace_id }
            }
            "execution_log" => Route::ExecutionLog { host_id, workspace_id, terminal_id: id },
            "setup" => Route::Setup { host_id, workspace_id },
            "project_settings" => Route::ProjectSettings {
                host_id,
                project_key: {
                    let key = json::text(&target.data, "projectKey");
                    if key.is_empty() { target.cwd.clone() } else { key.to_string() }
                },
            },
            _ => Route::Workspace {
                host_id,
                workspace_id: (!workspace_id.is_empty()).then_some(workspace_id),
            },
        }
    }
}

/// Where an opened tab goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabPlacement {
    /// The focused pane (Flutter `open`).
    Main,
    /// A new split right of the focused pane (Flutter `open(beside: true)`).
    Beside,
    /// The Explorer column, focused (Flutter `openExplorer`).
    Explorer,
    /// The Explorer column without moving the compact destination (`openExplorer(focus: false)`).
    ExplorerBackground,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiEffect {
    Navigate(Route),
    OpenSession { host_id: String, agent_id: String },
    CloseTab { tab_key: String },
    Toast { message: String, is_error: bool },
    OpenExternal { url: String },
    OpenFile { host_id: String, workspace_id: String, path: String },
    SetCloseGuard { owner: String, dirty: bool },
    // Additions.
    OpenTab { target: PanelTarget, placement: TabPlacement },
    OpenDraft {
        host_id: String,
        workspace_id: String,
        cwd: String,
        draft_id: String,
        title: String,
        data: Value,
    },
    OpenSessionWithError { host_id: String, agent_id: String, error: String },
    OpenFileBeside { host_id: String, workspace_id: String, path: String },
    ReplaceTab { tab_key: String, target: PanelTarget },
    SetPreference { key: String, value: Value },
    SetFocusScope { owner: WidgetUid, scope: Option<String> },
    RunCommand { action: String, args: Value },
    CopyText { text: String, toast: Option<String> },
    OpenHostDialog,
    OpenHostPairing { host_id: String },
    CloseSettings,
    /// Flutter `removeHost`: closes the host's tabs through their close guards, revokes its push
    /// registration, then removes the saved host. Use this, never `CoreHandle::remove_host`.
    RemoveHost { host_id: String },
    Redraw,
}

/// Flutter `PanelTarget` (`lib/core/target.dart`); `key()` is byte-identical so persisted
/// layouts and close guards keep their identity.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelTarget {
    pub kind: String,
    pub server_id: String,
    pub workspace_id: String,
    pub cwd: String,
    pub id: Option<String>,
    pub title: Option<String>,
    pub data: Value,
}

/// Dart `Uri.encodeComponent`: everything but `A-Za-z0-9-_.!~*'()` is percent-encoded.
const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

pub fn encode_component(value: &str) -> String {
    utf8_percent_encode(value, URI_COMPONENT).to_string()
}

impl PanelTarget {
    pub fn new(kind: &str, server_id: &str, workspace_id: &str, cwd: &str) -> Self {
        Self {
            kind: kind.to_string(),
            server_id: server_id.to_string(),
            workspace_id: workspace_id.to_string(),
            cwd: cwd.to_string(),
            id: None,
            title: None,
            data: Value::Object(Map::new()),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    pub fn agent(host_id: &str, workspace_id: &str, cwd: &str, agent_id: &str, title: &str) -> Self {
        let mut target = Self::new("agent", host_id, workspace_id, cwd).with_id(agent_id);
        if !title.is_empty() {
            target.title = Some(title.to_string());
        }
        target
    }

    pub fn key(&self) -> String {
        [
            self.server_id.as_str(),
            self.workspace_id.as_str(),
            self.kind.as_str(),
            self.id.as_deref().unwrap_or(""),
        ]
        .iter()
        .map(|part| encode_component(part))
        .collect::<Vec<_>>()
        .join("/")
    }

    /// Layout v2 tab JSON (`AppStore._targetJson`).
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "serverId": self.server_id,
            "workspaceId": self.workspace_id,
            "cwd": self.cwd,
            "id": self.id,
            "title": self.title,
            "data": if self.data.is_object() { self.data.clone() } else { json!({}) },
        })
    }

    pub fn from_json(value: &Value) -> Self {
        let optional = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            kind: json::text(value, "kind").to_string(),
            server_id: json::text(value, "serverId").to_string(),
            workspace_id: json::text(value, "workspaceId").to_string(),
            cwd: json::text(value, "cwd").to_string(),
            id: optional("id"),
            title: optional("title"),
            data: match value.get("data") {
                Some(Value::Object(map)) => Value::Object(map.clone()),
                _ => Value::Object(Map::new()),
            },
        }
    }

    pub fn copy_with_server(&self, server_id: &str) -> Self {
        Self { server_id: server_id.to_string(), ..self.clone() }
    }
}

/// Original `isCompactLayout`: width below 600 logical pixels.
pub const COMPACT_WIDTH: f64 = 600.0;

pub fn is_compact_width(width: f64) -> bool {
    width < COMPACT_WIDTH
}

thread_local! {
    static COMPACT: std::cell::Cell<bool> = const { std::cell::Cell::new(theme_mobile_default()) };
}

const fn theme_mobile_default() -> bool {
    crate::theme::MOBILE
}

/// Whether the window currently dispatched/drawn is compact. The shell updates it per window.
pub fn is_compact_layout(_cx: &Cx) -> bool {
    COMPACT.with(|compact| compact.get())
}

pub(crate) fn set_compact_layout(compact: bool) {
    COMPACT.with(|value| value.set(compact));
}

/// Original `errorMessage(Object)` for core errors.
pub fn error_message(error: &CoreError) -> String {
    let message = error.message.trim();
    if message.is_empty() { error.code.clone() } else { message.to_string() }
}

/// Label of the shortcut bound to a keyboard.json `help.id` on this platform, with user
/// overrides applied (Flutter `shortcutLabel`).
pub fn shortcut_label(ui: &UiContext, help_id: &str) -> Option<String> {
    crate::shell::shortcuts::label_for_help_id(ui.preferences(), help_id)
}
