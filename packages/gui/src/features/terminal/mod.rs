//! Interactive terminals (original `TerminalPane`) and the terminal core shared with the
//! read-only execution logs: the `subscribe_terminal_request` stream, the Ghostty screen
//! (`paseo-terminal`), the snapshot renderer and the cell grid. `TerminalPanel` shows a
//! daemon-owned shell (tab kind `terminal`); input and resizes go to the daemon as
//! `terminal_input` session messages.

pub mod drop;
pub mod find;
pub mod grid;
pub mod input;
pub mod keybar;
pub mod links;
pub mod panel;
pub mod screen;
pub mod session;
pub mod snapshot;
pub mod stream;
pub mod view;

pub use panel::{TerminalPanel, TerminalPanelRef, TerminalPanelWidgetExt, TerminalPanelWidgetRefExt, TerminalTarget};

/// Registers the grid and the find card before the logs view and the terminal panel that
/// use them.
pub fn live_design(cx: &mut makepad_widgets::Cx) {
    grid::live_design(cx);
    find::live_design(cx);
    view::live_design(cx);
    keybar::live_design(cx);
    panel::live_design(cx);
}
