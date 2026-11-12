//! `CreationPanel`: the new-session draft pane (Flutter `PanelTarget(kind: 'draft')` in
//! `AgentPanel`: an empty stage above the composer, which switches provider, model, mode and
//! features and creates the agent on submit). The shell allocates the usable viewport;
//! child panels must not reserve system-bar or keyboard space again.
//!
//! The composer rests on the pane bottom at its last drawn height, like the chat panel's.
//! It must not follow a `Fill` sibling: Makepad positions what follows a `Fill` only when
//! the parent turtle ends, and it does not move the editor's absolute clip turtle or
//! anything drawn after it, nor the hit rectangles the composer recorded while drawing.
//! The input and toolbar then stayed at the top of the pane, outside their card.

use crate::common::file_drop::FileDropZone;
use crate::features::composer::controller::ComposerTarget;
use makepad_widgets::*;

use super::panel::{ComposerPanelWidgetExt, ComposerPanelWidgetRefExt};

live_design! {
    use link::theme::*;
    use link::widgets::*;
    use crate::common::file_drop::FileDropZone;
    use crate::features::composer::ui::panel::ComposerPanel;

    pub CreationPanelBase = {{CreationPanel}} {}
    pub CreationPanel = <CreationPanelBase> {
        width: Fill, height: Fill,
        flow: Down,
        composer = <ComposerPanel> {}
        file_drop: <FileDropZone> {}
    }
}

#[derive(Live, LiveHook, Widget)]
pub struct CreationPanel {
    #[deref]
    view: View,
    /// The composer's height as last drawn.
    #[rust]
    composer_h: f64,
    /// composer/draft/workspace-tab.tsx: the whole draft pane takes dropped files.
    #[live]
    file_drop: FileDropZone,
}

impl Widget for CreationPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let composer = self.view.composer_panel(id!(composer));
        if let Some(paths) = self.file_drop.handle_event(cx, event, composer.accepts_file_drop()) {
            composer.drop_files(cx, paths);
        }
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, Layout::flow_down());
        let rect = cx.turtle().rect();
        let composer = self.view.widget(id!(composer));
        // The pane is the composer dock's stationary viewport.
        composer.as_composer_panel().set_viewport_height(rect.size.y);
        let bottom = rect.pos.y + rect.size.y;
        let dock = Walk { abs_pos: Some(dvec2(rect.pos.x, bottom - self.composer_h)), margin: Margin::default(), width: Size::Fixed(rect.size.x), height: Size::Fit };
        cx.begin_turtle(dock, Layout::flow_down());
        let _ = composer.draw_walk(cx, scope, Walk { abs_pos: None, margin: Margin::default(), width: Size::Fill, height: Size::Fit });
        let range = cx.get_turtle_align_range();
        let drawn = cx.end_turtle().size.y;
        if drawn.is_finite() && (drawn - self.composer_h).abs() > 0.5 {
            // A height change moves what was just drawn in the same frame; the composer's
            // hit rectangles still hold the old position, so it draws once more.
            cx.shift_align_range(&range, dvec2(0.0, self.composer_h - drawn));
            self.composer_h = drawn;
            cx.redraw_area_in_draw(composer.area());
        }
        // The drop backdrop covers the stage and the composer.
        self.file_drop.draw(cx, rect);
        cx.end_turtle();
        DrawStep::done()
    }
}

impl CreationPanelRef {
    pub fn set_target(&self, cx: &mut Cx, target: ComposerTarget) {
        if let Some(inner) = self.borrow() {
            inner.view.composer_panel(id!(composer)).set_target(cx, target);
        }
    }
}
