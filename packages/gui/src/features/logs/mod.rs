//! Read-only execution logs (Flutter `features/logs/`): `ExecutionLogPanel` shows the
//! output of an existing setup, script or attention execution. Opening it never starts a
//! process; the target carries the daemon terminal id.

pub mod find;
pub mod panel;
pub mod session;
pub mod snapshot;
pub mod view;

pub use panel::{ExecutionLogPanel, ExecutionLogPanelRef, ExecutionLogPanelWidgetExt, ExecutionLogPanelWidgetRefExt, LogTarget};

pub fn live_design(cx: &mut makepad_widgets::Cx) {
    view::live_design(cx);
    panel::live_design(cx);
}
