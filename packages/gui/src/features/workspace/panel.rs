//! `WorkspacePanel`: one root for the `new_tab`, `setup` and `project_settings` tab kinds.
//! Its DSL lives in `workspace/mod.rs` so `crate::features::workspace::WorkspacePanel`
//! resolves in other modules' `live_design!`.

use makepad_widgets::*;
use serde_json::Value;

use super::host;
use super::new_tab::{NewTabTarget, WaNewTabViewWidgetExt};
use super::project_settings::WaProjectSettingsViewWidgetExt;
use super::rpc::{object_of, str_of};
use super::setup::WaSetupViewWidgetExt;
use crate::common::UiContext;


/// What a workspace tab shows (shell `panels.rs` `workspace_target`).
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceTarget {
    NewTab { host_id: String, workspace_id: Option<String>, cwd: Option<String>, tab_id: String, source: Option<String> },
    Setup { host_id: String, workspace_id: String },
    ProjectSettings { host_id: String, project_key: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Active {
    #[default]
    None,
    Setup,
    Project,
    NewTab,
}

#[derive(Live, LiveHook, Widget)]
pub struct WorkspacePanel {
    #[deref]
    view: View,
    #[rust]
    target: Option<WorkspaceTarget>,
    #[rust]
    active: Active,
    /// The tab's data the last time it was applied (`ui.target`).
    #[rust]
    applied: Option<(String, String, Value)>,
}

impl WorkspacePanel {
    fn child(&self) -> WidgetRef {
        match self.active {
            Active::Setup => self.view.widget(id!(setup)),
            Active::Project => self.view.widget(id!(project)),
            Active::NewTab => self.view.widget(id!(new_tab)),
            Active::None => WidgetRef::empty(),
        }
    }

    /// Applies the target together with the dispatched tab's cwd, key and data.
    fn apply(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let Some(target) = self.target.clone() else { return };
        let tab = ctx.target.as_ref();
        let cwd = tab.map(|t| t.cwd.clone()).unwrap_or_default();
        let tab_key = ctx.tab_key.clone().unwrap_or_default();
        let data = tab.map(|t| t.data.clone()).unwrap_or(Value::Null);
        let applied = (tab_key.clone(), cwd.clone(), data.clone());
        if self.applied.as_ref() == Some(&applied) {
            return;
        }
        self.applied = Some(applied);
        match target {
            WorkspaceTarget::Setup { host_id, workspace_id } => {
                if let Some(mut setup) = self.view.wa_setup_view(id!(setup)).borrow_mut() {
                    setup.set_workspace(cx, &host_id, &workspace_id, &cwd);
                }
            }
            WorkspaceTarget::ProjectSettings { host_id, project_key } => {
                let root = [str_of(&data, "repoRoot"), str_of(object_of(&data, "project"), "projectRootPath"), project_key.as_str(), cwd.as_str()]
                    .into_iter()
                    .find(|value| !value.is_empty())
                    .unwrap_or("")
                    .to_owned();
                let workspace_id = tab.map(|t| t.workspace_id.clone()).unwrap_or_default();
                if let Some(mut project) = self.view.wa_project_settings_view(id!(project)).borrow_mut() {
                    project.set_project(cx, &host_id, &root, &workspace_id, &tab_key);
                }
            }
            WorkspaceTarget::NewTab { host_id, workspace_id, cwd: target_cwd, tab_id, source } => {
                let target = NewTabTarget {
                    host_id,
                    workspace_id: workspace_id.unwrap_or_default(),
                    cwd: target_cwd.unwrap_or(cwd),
                    tab_key: if tab_key.is_empty() { tab_id } else { tab_key },
                    source,
                    data,
                };
                if let Some(mut view) = self.view.wa_new_tab_view(id!(new_tab)).borrow_mut() {
                    view.set_new_tab(cx, target);
                }
            }
        }
    }
}

impl Widget for WorkspacePanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            host::learn(&ctx.events);
            self.apply(cx, ctx);
        }
        let child = self.child();
        if !child.is_empty() {
            scope.with_id(live_id!(workspace), |scope| child.handle_event(cx, event, scope));
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.apply(cx, ctx);
        }
        self.view.draw_walk(cx, scope, walk)
    }
}

impl WorkspacePanelRef {
    /// Mounts the panel on a workspace tab target.
    pub fn set_target(&self, cx: &mut Cx, target: WorkspaceTarget) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.target.as_ref() == Some(&target) {
            return;
        }
        inner.active = match target {
            WorkspaceTarget::Setup { .. } => Active::Setup,
            WorkspaceTarget::ProjectSettings { .. } => Active::Project,
            WorkspaceTarget::NewTab { .. } => Active::NewTab,
        };
        inner.target = Some(target);
        inner.applied = None;
        let active = inner.active;
        inner.view.widget(id!(setup)).set_visible(cx, active == Active::Setup);
        inner.view.widget(id!(project)).set_visible(cx, active == Active::Project);
        inner.view.widget(id!(new_tab)).set_visible(cx, active == Active::NewTab);
        inner.view.redraw(cx);
    }
}
