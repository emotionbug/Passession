//! Workspace surfaces (Flutter `features/workspace/`): the new session / new workspace /
//! add project tab (`new_tab`), workspace setup with its scripts (`setup`) and project
//! settings (`project_settings`), mounted through one root, `WorkspacePanel`.
//!
//! The shared modules here (`rpc`, `host`, `observe`, `kit`, `select`, `dialog`, `i18n`,
//! `identity`, `scripts`) also serve the schedules, logs and command-center features.

pub mod dialog;
pub mod host;
pub mod i18n;
pub mod identity;
pub mod kit;
pub mod new_tab;
pub mod new_tab_model;
pub mod observe;
pub mod panel;
pub mod project_model;
pub mod project_settings;
pub mod rpc;
pub mod scripts;
pub mod select;
pub mod setup;

use makepad_widgets::*;

pub use panel::{WorkspacePanel, WorkspacePanelRef, WorkspacePanelWidgetExt, WorkspacePanelWidgetRefExt, WorkspaceTarget};

/// Registers the submodule DSL (shared kit, sub-views). The shell calls this before the
/// module's macro-generated `live_design`, which defines the `WorkspacePanel` root.
pub fn register_modules(cx: &mut Cx) {
    kit::live_design(cx);
    select::live_design(cx);
    dialog::live_design(cx);
    setup::live_design(cx);
    project_settings::live_design(cx);
    new_tab::live_design(cx);
}

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::features::workspace::kit::*;
    use crate::features::workspace::setup::WaSetupView;
    use crate::features::workspace::project_settings::WaProjectSettingsView;
    use crate::features::workspace::new_tab::WaNewTabView;

    pub WorkspacePanel = {{WorkspacePanel}} {
        width: Fill, height: Fill, flow: Overlay
        setup = <WaSetupView> { visible: false }
        project = <WaProjectSettingsView> { visible: false }
        new_tab = <WaNewTabView> { visible: false }
    }
}
