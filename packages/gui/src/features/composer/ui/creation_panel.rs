//! `CreationPanel`: the new-session draft pane (Flutter `PanelTarget(kind: 'draft')` in
//! `AgentPanel`: an empty stage above the composer, which switches provider, model, mode and
//! features and creates the agent on submit). On Android/iOS the composer sits above the
//! safe area and the software keyboard.

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
    #[rust]
    bottom_inset: f64,
}

impl Widget for CreationPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let visible = scope.data.get::<crate::common::UiContext>().is_some_and(|ui| ui.activity.visible);
        if visible && let Event::Actions(actions) = event {
            for action in actions.iter() {
                if let Some(paseo_gui_platform::PlatformAction::Event(paseo_gui_platform::PlatformEvent::Insets(_))) =
                    action.downcast_ref::<paseo_gui_platform::PlatformAction>()
                {
                    self.view.redraw(cx);
                }
            }
        }
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let inset = paseo_gui_platform::platform()
            .map(|platform| {
                let insets = platform.safe_insets();
                if insets.keyboard_visible { insets.keyboard.max(insets.bottom) } else { insets.bottom }
            })
            .unwrap_or(0.0);
        if (inset - self.bottom_inset).abs() > 0.5 {
            self.bottom_inset = inset;
            self.view.apply_over(cx, live! { padding: { bottom: (inset) } });
        }
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
