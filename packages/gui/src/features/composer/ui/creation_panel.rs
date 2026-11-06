//! `CreationPanel`: the new-session draft pane (Flutter `PanelTarget(kind: 'draft')` in
//! `AgentPanel`: an empty stage above the composer, which switches provider, model, mode and
//! features and creates the agent on submit). The shell allocates the usable viewport;
//! child panels must not reserve system-bar or keyboard space again.

use crate::features::composer::controller::ComposerTarget;
use makepad_widgets::*;

use super::panel::ComposerPanelWidgetExt;

live_design! {
    use link::theme::*;
    use link::widgets::*;
    use crate::features::composer::ui::panel::ComposerPanel;

    pub CreationPanelBase = {{CreationPanel}} {}
    pub CreationPanel = <CreationPanelBase> {
        width: Fill, height: Fill,
        flow: Down,
        stage = <View> { width: Fill, height: Fill }
        composer = <ComposerPanel> {}
    }
}

#[derive(Live, LiveHook, Widget)]
pub struct CreationPanel {
    #[deref]
    view: View,
}

impl Widget for CreationPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl CreationPanelRef {
    pub fn set_target(&self, cx: &mut Cx, target: ComposerTarget) {
        if let Some(inner) = self.borrow() {
            inner.view.composer_panel(id!(composer)).set_target(cx, target);
        }
    }
}
