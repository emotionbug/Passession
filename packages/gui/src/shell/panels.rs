//! The one integration file between the shell and feature roots: DSL templates per tab
//! kind, instantiation (`set_target`), presentation (`set_visible`/`set_focused`) and
//! release on close. Flutter `_panel(target)` + `_RetainedPanel`.

use makepad_widgets::*;

use crate::common::{PanelTarget, UiContext, json as j};
use crate::features::{command_center, composer, files, logs, timeline, workspace};
use crate::features::git::{GitActionsButtonWidgetRefExt, GitPanelWidgetRefExt, OpenInButtonWidgetRefExt};
use crate::features::command_center::CommandCenterPanelWidgetRefExt;
use crate::features::composer::CreationPanelWidgetRefExt;
use crate::features::files::FilesPanelWidgetRefExt;
use crate::features::logs::ExecutionLogPanelWidgetRefExt;
use crate::features::timeline::ChatPanelWidgetRefExt;
use crate::features::workspace::WorkspacePanelWidgetRefExt;

live_design! {
    use link::theme::*;
    use link::widgets::*;
    use crate::features::timeline::ChatPanel;
    use crate::features::composer::ui::creation_panel::CreationPanel;
    use crate::features::files::FilesPanel;
    use crate::features::git::GitPanel;
    use crate::features::git::GitActionsButton;
    use crate::features::git::OpenInButton;
    use crate::features::logs::panel::ExecutionLogPanel;
    use crate::features::workspace::WorkspacePanel;
    use crate::features::settings::panel::SettingsPanel;
    use crate::features::schedules::SchedulesPanel;
    use crate::features::command_center::CommandCenterPanel;

    pub PanelTemplates = {{PanelTemplates}} {
        chat: <ChatPanel> {}
        creation: <CreationPanel> {}
        files: <FilesPanel> {}
        git: <GitPanel> {}
        logs: <ExecutionLogPanel> {}
        workspace: <WorkspacePanel> {}
        settings: <SettingsPanel> {}
        schedules: <SchedulesPanel> {}
        command_center: <CommandCenterPanel> {}
        git_actions: <GitActionsButton> {}
        open_in: <OpenInButton> {}
    }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct PanelTemplates {
    #[live]
    pub chat: Option<LivePtr>,
    #[live]
    pub creation: Option<LivePtr>,
    #[live]
    pub files: Option<LivePtr>,
    #[live]
    pub git: Option<LivePtr>,
    #[live]
    pub logs: Option<LivePtr>,
    #[live]
    pub workspace: Option<LivePtr>,
    #[live]
    pub settings: Option<LivePtr>,
    #[live]
    pub schedules: Option<LivePtr>,
    #[live]
    pub command_center: Option<LivePtr>,
    #[live]
    pub git_actions: Option<LivePtr>,
    #[live]
    pub open_in: Option<LivePtr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Chat,
    Creation,
    Files,
    Git,
    Logs,
    Workspace,
}

/// Flutter `_panel(target)` by tab kind; `None` for removed/unknown kinds.
pub fn root_kind(kind: &str) -> Option<RootKind> {
    Some(match kind {
        "agent" | "provider_subagent" => RootKind::Chat,
        "draft" => RootKind::Creation,
        "files" | "file" => RootKind::Files,
        "changes_tree" | "working_diff" | "commit_diff" | "pull_request" => RootKind::Git,
        "execution_log" => RootKind::Logs,
        "new_tab" | "setup" | "project_settings" => RootKind::Workspace,
        _ => return None,
    })
}

/// A mounted tab root.
pub struct Panel {
    pub widget: WidgetRef,
    pub kind: RootKind,
    pub target: PanelTarget,
    pub visible: bool,
    pub focused: bool,
}

fn chat_target(target: &PanelTarget) -> timeline::ChatTarget {
    let subagent = target.kind == "provider_subagent";
    timeline::ChatTarget {
        host_id: target.server_id.clone(),
        agent_id: if subagent { j::text(&target.data, "parentAgentId").to_string() } else { target.id.clone().unwrap_or_default() },
        subagent_id: if subagent { target.id.clone() } else { None },
        workspace_id: target.workspace_id.clone(),
        cwd: (!target.cwd.is_empty()).then(|| target.cwd.clone()),
        tab_key: target.key(),
        sheet: j::text(&target.data, "paneHost") == "sheet",
        title: target.title.clone(),
    }
}

fn composer_target(target: &PanelTarget) -> composer::ComposerTarget {
    composer::ComposerTarget {
        kind: composer::ComposerKind::Draft,
        host_id: target.server_id.clone(),
        workspace_id: target.workspace_id.clone(),
        cwd: target.cwd.clone(),
        id: target.id.clone().unwrap_or_else(|| "draft".to_string()),
        tab_key: target.key(),
        data: target.data.clone(),
    }
}

fn workspace_target(target: &PanelTarget) -> workspace::WorkspaceTarget {
    match target.kind.as_str() {
        "setup" => workspace::WorkspaceTarget::Setup {
            host_id: target.server_id.clone(),
            workspace_id: target.workspace_id.clone(),
        },
        "project_settings" => workspace::WorkspaceTarget::ProjectSettings {
            host_id: target.server_id.clone(),
            project_key: {
                let root = j::text(j::object(&target.data, "project"), "projectRootPath");
                if root.is_empty() { target.cwd.clone() } else { root.to_string() }
            },
        },
        _ => workspace::WorkspaceTarget::NewTab {
            host_id: target.server_id.clone(),
            workspace_id: (!target.workspace_id.is_empty()).then(|| target.workspace_id.clone()),
            cwd: (!target.cwd.is_empty()).then(|| target.cwd.clone()),
            tab_id: target.key(),
            source: target.data.get("source").and_then(|v| v.as_str()).map(str::to_string),
        },
    }
}

fn log_target(target: &PanelTarget) -> logs::LogTarget {
    logs::LogTarget {
        host_id: target.server_id.clone(),
        workspace_id: target.workspace_id.clone(),
        terminal_id: target.id.clone().unwrap_or_default(),
        title: target.title.clone().unwrap_or_default(),
        tab_key: target.key(),
    }
}

/// Instantiates the root for `target` and hands it its target.
pub fn mount(cx: &mut Cx, templates: &PanelTemplates, target: &PanelTarget) -> Option<Panel> {
    let kind = root_kind(&target.kind)?;
    let ptr = match kind {
        RootKind::Chat => templates.chat,
        RootKind::Creation => templates.creation,
        RootKind::Files => templates.files,
        RootKind::Git => templates.git,
        RootKind::Logs => templates.logs,
        RootKind::Workspace => templates.workspace,
    };
    let widget = WidgetRef::new_from_ptr(cx, ptr);
    let panel = Panel { widget, kind, target: target.clone(), visible: false, focused: false };
    retarget(cx, &panel);
    Some(panel)
}

/// Re-applies the target after an in-place update (rename, data change).
pub fn retarget(cx: &mut Cx, panel: &Panel) {
    let target = &panel.target;
    match panel.kind {
        RootKind::Chat => panel.widget.as_chat_panel().set_target(cx, chat_target(target)),
        RootKind::Creation => panel.widget.as_creation_panel().set_target(cx, composer_target(target)),
        RootKind::Files => panel.widget.as_files_panel().set_target(cx, files::FilesTarget::from_panel(target)),
        RootKind::Git => panel.widget.as_git_panel().set_target(cx, target.clone()),
        RootKind::Logs => panel.widget.as_execution_log_panel().set_target(cx, log_target(target)),
        RootKind::Workspace => panel.widget.as_workspace_panel().set_target(cx, workspace_target(target)),
    }
}

/// Presentation changes (`PanelActivity` visible/focused), applied only on change.
pub fn set_activity(cx: &mut Cx, panel: &mut Panel, visible: bool, focused: bool) {
    if panel.visible != visible {
        panel.visible = visible;
        match panel.kind {
            RootKind::Chat => panel.widget.as_chat_panel().set_visible(cx, visible),
            RootKind::Files => panel.widget.as_files_panel().set_visible(cx, visible),
            RootKind::Logs => panel.widget.as_execution_log_panel().set_visible_log(cx, visible),
            RootKind::Git => panel.widget.as_git_panel().set_visible(cx, visible),
            RootKind::Creation | RootKind::Workspace => {}
        }
    }
    if panel.focused != focused {
        panel.focused = focused;
        if panel.kind == RootKind::Chat {
            panel.widget.as_chat_panel().set_focused(cx, focused);
        }
    }
}

/// Tab close: ends the selective subscription and create handoff of chat tabs.
pub fn release(cx: &mut Cx, ui: &UiContext, panel: &Panel) {
    let target = &panel.target;
    if panel.kind != RootKind::Chat {
        return;
    }
    let id = target.id.clone().unwrap_or_default();
    if target.kind == "provider_subagent" {
        timeline::release_subagent(cx, ui, &target.server_id, j::text(&target.data, "parentAgentId"), &id);
    } else {
        timeline::release_session(cx, ui, &target.server_id, &id);
    }
}

/// Conversation menu items offered by an agent tab (Flutter `ConversationMenus`).
pub fn conversation_actions(target: &PanelTarget) -> Vec<(String, String)> {
    if target.kind != "agent" {
        return Vec::new();
    }
    timeline::conversation_actions(&target.server_id, target.id.as_deref().unwrap_or(""))
}

/// Forwards a picked conversation action to the mounted chat root.
pub fn run_conversation_action(cx: &mut Cx, panel: &Panel, action: &str) {
    if panel.kind == RootKind::Chat {
        panel.widget.as_chat_panel().run_conversation_action(cx, action);
    }
}

/// `files::close_requested` flow for dirty `file` tabs.
pub fn file_close_requested(tab_key: &str) -> bool {
    matches!(files::close_requested(tab_key), files::CloseCheck::Dirty)
}

pub fn file_close_confirmed(tab_key: &str) {
    files::close_confirmed(tab_key);
}

pub fn file_close_cancelled(tab_key: &str) {
    files::close_cancelled(tab_key);
}

/// Shell header git widgets (`WorkspaceOpenInButton`, `WorkspaceGitActionsButton`).
pub fn header_widgets(cx: &mut Cx, templates: &PanelTemplates) -> (WidgetRef, WidgetRef) {
    (WidgetRef::new_from_ptr(cx, templates.git_actions), WidgetRef::new_from_ptr(cx, templates.open_in))
}

pub fn set_header_target(cx: &mut Cx, git_actions: &WidgetRef, open_in: &WidgetRef, target: &PanelTarget) {
    let file = (target.kind == "file").then(|| {
        let path = j::text(&target.data, "path");
        if path.is_empty() { target.id.clone().unwrap_or_default() } else { path.to_string() }
    });
    git_actions.as_git_actions_button().set_workspace(cx, &target.server_id, &target.workspace_id, &target.cwd);
    open_in.as_open_in_button().set_workspace(cx, &target.server_id, &target.cwd, file.as_deref());
}

pub fn page_widget(cx: &mut Cx, templates: &PanelTemplates, page: &super::Page) -> WidgetRef {
    match page {
        super::Page::Settings { .. } => WidgetRef::new_from_ptr(cx, templates.settings),
        super::Page::Schedules { .. } => WidgetRef::new_from_ptr(cx, templates.schedules),
        super::Page::Sessions { .. } => WidgetRef::default(),
    }
}

pub fn command_center(cx: &mut Cx, templates: &PanelTemplates) -> WidgetRef {
    WidgetRef::new_from_ptr(cx, templates.command_center)
}

pub fn open_command_center(cx: &mut Cx, widget: &WidgetRef, files: bool) {
    widget.as_command_center_panel().open(cx, files);
}

pub fn close_command_center(cx: &mut Cx, widget: &WidgetRef) {
    widget.as_command_center_panel().close(cx);
}

pub fn command_center_open(widget: &WidgetRef) -> bool {
    widget.as_command_center_panel().is_open()
}

/// Shell commands raised by the command center (`onCommand: _execute`).
pub fn command_center_commands(actions: &Actions) -> Vec<(String, serde_json::Value)> {
    actions
        .iter()
        .filter_map(|action| action.as_widget_action())
        .filter_map(|action| match action.cast_ref::<command_center::CommandCenterAction>() {
            command_center::CommandCenterAction::Command { action, args } => Some((action.clone(), args.clone())),
            _ => None,
        })
        .collect()
}
