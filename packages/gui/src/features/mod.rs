//! Feature modules (owners in contract.md); each `mod.rs` defines its root DSL widget(s).

pub mod command_center;
pub mod composer;
pub mod files;
pub mod git;
pub mod logs;
pub mod schedules;
pub mod settings;
pub mod timeline;
pub mod workspace;

use makepad_widgets::Cx;

/// Registered before the shell DSL, which references the feature roots. `workspace` comes
/// before `settings`, `schedules`, `logs` and `command_center`: their DSL imports
/// `crate::features::workspace::kit::*` and the settings Projects page embeds `WorkspacePanel`.
pub fn live_design(cx: &mut Cx) {
    composer::live_design(cx);
    timeline::live_design(cx);
    files::live_design(cx);
    git::paint::live_design(cx);
    git::live_design(cx);
    workspace::register_modules(cx);
    workspace::live_design(cx);
    settings::live_design(cx);
    schedules::register_modules(cx);
    schedules::live_design(cx);
    logs::live_design(cx);
    command_center::register_modules(cx);
    command_center::live_design(cx);
}
