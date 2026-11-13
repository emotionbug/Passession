//! `GitPanel`: the root of the `changes_tree`, `working_diff`, `commit_diff`,
//! `pull_request` and `git_log` tabs. One instance per tab; each binds its own view to
//! the shared window git store and releases it when dropped or retargeted.

use super::changes::{ChangesView, OverlayParts};
use super::log_view::{LogInputs, LogView};
use super::paint::GitPaint;
use super::pr_view::PrView;
use super::store::{GitStoreChanged, with_store};
use crate::common::{PanelTarget, UiContext, is_compact_layout};
use crate::features::timeline::canvas::ChatKit;
use crate::theme::DrawBox;
use makepad_widgets::*;

enum View {
    Changes(Box<ChangesView>),
    Pr(Box<PrView>),
    Log(Box<LogView>),
}

impl View {
    fn dispose(&mut self, store: &mut super::store::GitStore) {
        match self {
            View::Changes(view) => view.dispose(store),
            View::Pr(view) => view.dispose(store),
            View::Log(view) => view.dispose(store),
        }
    }
}

#[derive(Live, LiveHook, Widget)]
pub struct GitPanel {
    #[walk]
    walk: Walk,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    paint: GitPaint,
    #[live]
    overlay_bg: DrawBox,
    #[live]
    input: TextInput,
    /// The Git Log's message search and branches-panel search fields.
    #[live]
    search_input: TextInput,
    #[live]
    branch_input: TextInput,
    /// Draw primitives of the native Markdown painter (pull request bodies).
    #[live]
    chat_kit: ChatKit,
    #[rust(DrawList2d::new(cx))]
    overlay_list: DrawList2d,
    #[rust]
    target: Option<PanelTarget>,
    #[rust]
    visible: bool,
    #[rust]
    view: Option<View>,
    #[rust]
    next_frame: NextFrame,
    /// The visibility last applied to the view (hidden views release subscriptions).
    #[rust]
    applied_visible: Option<bool>,
    /// `(files::revision(), timeline images::version())` at the last check.
    #[rust]
    media_revision: (u64, u64),
}

impl Drop for GitPanel {
    fn drop(&mut self) {
        if let Some(mut view) = self.view.take() {
            with_store(|store| view.dispose(store));
        }
    }
}

impl GitPanel {
    /// Binds the view for the current target once a `UiContext` is available.
    fn ensure_view(&mut self, cx: &mut Cx, ui: &UiContext) {
        if self.view.is_some() {
            return;
        }
        let Some(target) = self.target.clone() else { return };
        let view = with_store(|store| {
            if target.kind == "pull_request" {
                View::Pr(Box::new(PrView::new(cx, ui, store, target)))
            } else if target.kind == "git_log" {
                View::Log(Box::new(LogView::new(cx, ui, store, target)))
            } else {
                View::Changes(Box::new(ChangesView::new(cx, ui, store, target)))
            }
        });
        self.view = Some(view);
        self.redraw(cx);
    }

    fn release_view(&mut self, cx: &mut Cx) {
        self.applied_visible = None;
        if let Some(mut view) = self.view.take() {
            match &mut view {
                View::Changes(changes) => changes.close_overlay(cx),
                View::Pr(pr) => pr.close_overlay(cx),
                View::Log(log) => log.close_overlay(cx),
            }
            with_store(|store| view.dispose(store));
        }
    }
}

/// Whether an event can change this panel's state (core batches, store
/// changes, timers, visibility). Pointer events go to input handling only.
fn processes(event: &Event, ui: &UiContext) -> bool {
    !ui.events.is_empty()
        || matches!(event, Event::Timer(_) | Event::Signal)
        || matches!(event, Event::Actions(actions) if actions.iter().any(|a| a.downcast_ref::<GitStoreChanged>().is_some()))
}

impl Widget for GitPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return;
        };
        with_store(|store| store.begin_event(cx, event, ui));
        self.ensure_view(cx, ui);
        if self.next_frame.is_event(event).is_some() {
            self.redraw(cx);
        }
        // Decoded file icons and avatar images bump their caches' revisions
        // (on Signal, possibly after this widget saw it), so compare on every event.
        let revision = (crate::features::files::revision(), crate::features::timeline::images::version());
        if revision != self.media_revision {
            self.media_revision = revision;
            let pending = match &self.view {
                Some(View::Changes(changes)) => changes.icons_pending,
                Some(View::Pr(pr)) => pr.images_pending,
                Some(View::Log(log)) => log.icons_pending,
                None => false,
            };
            if pending {
                self.redraw(cx);
            }
        }
        let compact = is_compact_layout(cx);
        let visible = self.visible;
        let process = processes(event, ui) || self.applied_visible != Some(visible);
        self.applied_visible = Some(visible);
        let Self { view, overlay_list, overlay_bg, input, search_input, branch_input, area, .. } = self;
        let Some(view) = view.as_mut() else { return };
        let area = *area;
        let mut parts = OverlayParts { list: overlay_list, bg: overlay_bg, input };
        let redraw = with_store(|store| match view {
            View::Changes(changes) => {
                let mut redraw = process && changes.process(cx, ui, store, visible);
                if changes.handle_overlay(cx, event, ui, &mut parts, store, compact) {
                    redraw = true;
                } else if visible {
                    redraw |= changes.handle_pointer(cx, event, area, ui, &mut parts, store, compact);
                }
                // Menus, confirmations and selects opened above go to the shell.
                changes.flush_overlay(ui);
                redraw
            }
            View::Pr(pr) => {
                let mut redraw = process && pr.process(cx, event, ui, store, visible);
                redraw |= pr.handle_overlay(ui);
                if visible {
                    redraw |= pr.handle_pointer(cx, event, area, ui, store);
                }
                redraw
            }
            View::Log(log) => {
                let mut inputs = LogInputs { search: search_input, branch: branch_input };
                let mut redraw = process && log.process(cx, event, ui, store, visible);
                redraw |= log.handle_overlay(ui);
                if visible {
                    redraw |= log.handle_inputs(cx, event, ui, &mut inputs);
                    redraw |= log.handle_pointer(cx, event, area, ui, &mut inputs);
                }
                // Menus and selects opened above go to the shell.
                log.flush_overlay(ui);
                redraw
            }
        });
        // Processing a timer/signal is not itself a visual change.
        if redraw {
            self.redraw(cx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return DrawStep::done();
        };
        self.ensure_view(cx, ui);
        let rect = cx.walk_turtle_with_area(&mut self.area, walk);
        let compact = is_compact_layout(cx);
        let Self { view, overlay_list, overlay_bg, input, search_input, branch_input, paint, chat_kit, .. } = self;
        let Some(view) = view.as_mut() else { return DrawStep::done() };
        let mut parts = OverlayParts { list: overlay_list, bg: overlay_bg, input };
        paint.begin_clip(cx, rect);
        let spinning = with_store(|store| match view {
            View::Changes(changes) => {
                changes.draw(cx, ui, store, paint, &mut parts, rect, compact);
                changes.spinning
            }
            View::Pr(pr) => {
                pr.draw(cx, ui, store, paint, rect, chat_kit);
                pr.spinning
            }
            View::Log(log) => {
                let mut inputs = LogInputs { search: search_input, branch: branch_input };
                log.draw(cx, ui, paint, &mut inputs, rect, compact);
                log.spinning
            }
        });
        paint.end_clip(cx);
        // Spinners animate only while something is pending and the tab is drawn.
        if spinning {
            self.next_frame = cx.new_next_frame();
        }
        DrawStep::done()
    }
}

impl GitPanelRef {
    /// Binds the panel to a tab target. Reopening a tab replaces its target
    /// under the same key; another checkout directory or commit is another view.
    pub fn set_target(&self, cx: &mut Cx, target: PanelTarget) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let sha = |t: &PanelTarget| {
            t.data
                .get("sha")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| t.id.clone())
                .unwrap_or_default()
        };
        let same_view = inner.target.as_ref().is_some_and(|current| {
            current.kind == target.kind
                && current.server_id == target.server_id
                && current.cwd == target.cwd
                && sha(current) == sha(&target)
        });
        if !same_view {
            inner.release_view(cx);
        } else if let Some(View::Changes(changes)) = inner.view.as_mut() {
            changes.target.title = target.title.clone();
            // A new `focusRequestId` refocuses the same diff tab.
            if target.data.get("focusRequestId") != changes.target.data.get("focusRequestId")
                && let Some(path) = target.data.get("focusPath").and_then(|v| v.as_str())
            {
                changes.pending_focus = Some(path.to_owned());
            }
            changes.target.data = target.data.clone();
        }
        inner.target = Some(target);
        inner.redraw(cx);
    }

    /// The tab became shown or hidden (original `PanelActivity.visible`).
    pub fn set_visible(&self, cx: &mut Cx, visible: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.visible != visible {
            inner.visible = visible;
            if !visible {
                match inner.view.as_mut() {
                    Some(View::Changes(changes)) => changes.close_overlay(cx),
                    Some(View::Pr(pr)) => pr.close_overlay(cx),
                    Some(View::Log(log)) => log.close_overlay(cx),
                    None => {}
                }
            }
            inner.redraw(cx);
        }
    }
}
