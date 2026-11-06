//! Native composer surfaces: the inline composer (`ComposerPanel`), the new-session draft
//! pane (`CreationPanel`) and their overlays (autocomplete, model sheet, forge picker,
//! image preview, fullscreen editor, context window details, tooltips).

pub mod creation_panel;
pub mod draw;
pub mod layout;
pub mod look;
pub mod overlays;
pub mod panel;
pub mod sheet;

use makepad_widgets::Cx;

pub fn live_design(cx: &mut Cx) {
    draw::live_design(cx);
    panel::live_design(cx);
    creation_panel::live_design(cx);
}
