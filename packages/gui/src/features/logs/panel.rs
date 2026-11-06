//! `ExecutionLogPanel` (Flutter `execution_log_panel.dart`): the original `TerminalPane`
//! presentation — a 36px `PaneContentToolbar` (title, status, line count, follow, find,
//! copy), the read-only output, the floating `PaneFind` card, the attach spinner overlay and
//! the stream error row.

use makepad_widgets::*;

use super::find;
use super::session::{display_title, scrollback_preference, LogSession};
use super::view::{WaLogView, WaLogViewWidgetExt};
use crate::common::{Route, UiContext, UiEffect};
use crate::features::workspace::host;
use crate::features::workspace::i18n::{l, lf, t, ta};
use crate::features::workspace::kit::{ellipsize, set_optional, WaButtonWidgetExt, WaInputWidgetExt};
use crate::common::PanelTarget;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;
    use crate::features::logs::view::*;

    pub ExecutionLogPanel = {{ExecutionLogPanel}} {
        width: Fill, height: Fill, flow: Down
        wa_root = <Surface> {
            width: Fill, height: Fill, flow: Down
            role: Surface0
            // Original `PaneContentToolbar`: 36px surface0 band with a bottom border.
            toolbar = <View> {
                width: Fill, height: 36., flow: Right, spacing: 12.
                padding: { left: 12., right: 5. }
                align: { y: 0.5 }
                info = <View> {
                    width: Fill, height: Fit, flow: Right, spacing: 8., align: { y: 0.5 }
                    title = <WaSmallFit> {}
                    status = <WaSmallFit> { role: ExtraMuted }
                    lines = <View> { width: Fit, height: Fit, text = <WaSmallFit> { role: ExtraMuted } }
                }
                tools = <View> {
                    width: Fit, height: Fit, flow: Right, spacing: 1., align: { y: 0.5 }
                    follow = <WaToolButton> { icon: "arrow-down-to-line" }
                    toggle_find = <WaToolButton> { icon: "search" }
                    copy = <WaToolButton> { icon: "copy" }
                }
            }
            <WaRule> {}
            body = <View> {
                width: Fill, height: Fill, flow: Overlay
                output = <WaLogView> {}
                empty = <View> {
                    width: Fill, height: Fill, align: { x: 0.5, y: 0.5 }, padding: 24., visible: false
                    text = <WaMuted> { width: Fit }
                }
                find_wrap = <View> {
                    width: Fill, height: Fit, align: { x: 1.0 }, padding: 8., visible: false
                    // Original `PaneFind`: 340px surface1 card, 6px padding, 4px gap, border, 8px radius.
                    find_bar = <Surface> {
                        width: 340., height: Fit, flow: Right, spacing: 4., padding: 6.
                        align: { y: 0.5 }
                        role: Surface1, border_role: Border, radius: 8.
                        field = <Surface> {
                            width: Fill, height: 28., flow: Right, spacing: 8.
                            padding: { left: 8., right: 8. }
                            align: { y: 0.5 }
                            role: Surface2, border_role: BorderAccent, radius: 6., border_width: 0.
                            query = <WaBareInput> { input = { padding: 0. } }
                            match_status = <View> { width: Fit, height: Fit, text = <WaSmallFit> {} }
                        }
                        find_previous = <WaToolButton> { icon: "arrow-up" }
                        find_next = <WaToolButton> { icon: "arrow-down" }
                        find_close = <WaToolButton> { icon: "x" }
                    }
                }
                // Original attach overlay: a dimmed surface with a small muted spinner.
                attach = <View> {
                    width: Fill, height: Fill, visible: false
                    show_bg: true, draw_bg: { color: #00000029 }
                    align: { x: 0.5, y: 0.5 }
                    <WaSpinner> { width: 20., height: 20., stroke: 2. }
                }
            }
            // Original terminal error row: surface1 band with a top border, destructive small text.
            error_row = <Surface> {
                width: Fill, height: Fit, flow: Down, visible: false
                role: Surface1
                <WaRule> {}
                row = <View> {
                    width: Fill, height: Fit, flow: Right, spacing: 8.
                    padding: { left: 12., right: 12., top: 4., bottom: 4. }
                    align: { y: 0.5 }
                    text = <WaText> { size: 12., role: Danger }
                    retry = <WaButtonGhost> { size: Xs }
                    close_tab = <WaButtonGhost> { size: Xs, visible: false }
                }
            }
        }
    }
}

/// What an execution log tab shows (Flutter `PanelTarget` kind `execution_log`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogTarget {
    pub host_id: String,
    pub workspace_id: String,
    pub terminal_id: String,
    pub title: String,
    /// The shell tab key, for Close when the target has no process id.
    pub tab_key: String,
}

#[derive(Live, LiveHook, Widget)]
pub struct ExecutionLogPanel {
    #[deref]
    view: View,
    #[rust]
    target: Option<LogTarget>,
    #[rust]
    explicit_target: bool,
    #[rust]
    hidden: bool,
    #[rust]
    visibility_dirty: bool,
    #[rust]
    session_opened: bool,
    #[rust]
    init_error: Option<String>,
    #[rust]
    publish_timer: Timer,
    #[rust]
    search_timer: Timer,
    #[rust]
    show_find: bool,
    #[rust]
    query: String,
    #[rust]
    searched_revision: Option<u64>,
    #[rust]
    focus_query: bool,
    #[rust]
    connected: bool,
    #[rust]
    compact: bool,
}

impl ExecutionLogPanel {
    fn output(&self) -> super::view::WaLogViewRef {
        self.view.wa_log_view(id!(output))
    }

    fn with_session<R>(&self, f: impl FnOnce(&mut LogSession, &mut WaLogView) -> R) -> Option<R> {
        let output = self.output();
        let mut view = output.borrow_mut()?;
        let mut session = view.session.take()?;
        let result = f(&mut session, &mut view);
        view.session = Some(session);
        Some(result)
    }

    /// `_openSession`: a fresh observation for the target.
    fn open_session(&mut self, cx: &mut Cx, ctx: &UiContext) {
        self.session_opened = true;
        let Some(target) = self.target.clone() else { return };
        self.searched_revision = None;
        self.init_error = None;
        let output = self.output();
        let Some(mut view) = output.borrow_mut() else { return };
        view.session = None;
        view.matches.clear();
        view.active_match = None;
        view.follow = true;
        if target.terminal_id.is_empty() {
            self.init_error = Some(l("Execution output requires an existing process identifier").to_owned());
            return;
        }
        let scrollback = scrollback_preference(host::preferences(&ctx.state));
        let mut session = LogSession::new(&target.host_id, &target.terminal_id, scrollback);
        self.connected = host::host(&ctx.state, &target.host_id).is_some_and(|h| h.connected());
        session.set_visible(&ctx.core, !self.hidden, self.connected);
        view.session = Some(session);
        view.redraw(cx);
    }

    fn run_find(&mut self, cx: &mut Cx, previous: bool, reset: bool) {
        cx.stop_timer(self.search_timer);
        self.search_timer = Timer::default();
        let query = self.query.clone();
        let outcome = self.with_session(|session, view| {
            if query.is_empty() {
                view.matches.clear();
                view.active_match = None;
                view.matches_revision = session.revision;
                return (session.revision, None);
            }
            let matches = find::find_matches(session, &query);
            let active = if reset {
                find::next_index(None, matches.len(), previous, true)
            } else {
                find::next_index(view.active_match, matches.len(), previous, false)
            };
            view.matches = matches;
            view.active_match = active;
            view.matches_revision = session.revision;
            let target = active.map(|index| view.matches[index]);
            if target.is_some() {
                view.follow = false;
            }
            (session.revision, target)
        });
        if let Some((revision, target)) = outcome {
            self.searched_revision = Some(revision);
            if let Some(target) = target && let Some(mut view) = self.output().borrow_mut() {
                view.reveal(cx, target);
            }
        }
        self.view.redraw(cx);
    }

    fn open_find(&mut self, cx: &mut Cx) {
        self.show_find = true;
        self.focus_query = true;
        self.view.redraw(cx);
    }

    fn close_find(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.search_timer);
        self.search_timer = Timer::default();
        self.show_find = false;
        self.view.redraw(cx);
    }

    /// `_findStatus`.
    fn find_status(&self, revision: u64, total: usize, active: Option<usize>) -> String {
        if self.query.is_empty() || self.searched_revision.is_none() {
            return String::new();
        }
        if self.searched_revision != Some(revision) {
            return l("Output changed — search again").to_owned();
        }
        if total == 0 {
            return t("paneFind.noMatches");
        }
        match active {
            None => ta("paneFind.total", &[("total", &total.to_string())]),
            Some(index) => ta(
                "paneFind.position",
                &[("current", &(index + 1).to_string()), ("total", &total.to_string())],
            ),
        }
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        host::learn(&ctx.events);
        if !self.explicit_target && let Route::ExecutionLog { host_id, workspace_id, terminal_id } = &ctx.route {
            let title = ctx.target.as_ref().and_then(|t: &PanelTarget| t.title.clone()).unwrap_or_default();
            let next = LogTarget {
                host_id: host_id.clone(),
                workspace_id: workspace_id.clone(),
                terminal_id: terminal_id.clone(),
                title,
                tab_key: ctx.tab_key.clone().unwrap_or_default(),
            };
            if self.target.as_ref() != Some(&next) {
                self.target = Some(next);
                self.session_opened = false;
            }
        }
        let Some(target) = self.target.clone() else { return };
        if !self.session_opened {
            self.open_session(cx, ctx);
        }
        self.connected = host::host(&ctx.state, &target.host_id).is_some_and(|h| h.connected());
        let core = ctx.core.clone();
        if self.visibility_dirty {
            self.visibility_dirty = false;
            let (visible, connected) = (!self.hidden, self.connected);
            self.with_session(|session, _| session.set_visible(&core, visible, connected));
            self.view.redraw(cx);
        }
        let outcome = self.with_session(|session, _| (session.pump(&core, &ctx.events), session.output_pending));
        if let Some((changed, pending)) = outcome {
            if pending && self.publish_timer.is_empty() {
                // Coalesce output bursts (33ms) instead of repainting per chunk.
                self.publish_timer = cx.start_timeout(0.033);
            }
            if changed {
                self.view.redraw(cx);
            }
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: Option<&mut UiContext>) {
        let output = self.output();
        if output.user_scrolled(actions) {
            if let Some(mut view) = output.borrow_mut() {
                view.follow = false;
            }
            self.view.redraw(cx);
        }
        if output.find_requested(actions) {
            self.open_find(cx);
        }
        if self.view.wa_button(id!(follow)).clicked(actions) {
            if let Some(mut view) = output.borrow_mut() {
                view.follow = !view.follow;
                view.redraw(cx);
            }
            self.view.redraw(cx);
        }
        if self.view.wa_button(id!(toggle_find)).clicked(actions) {
            if self.show_find {
                self.close_find(cx)
            } else {
                self.open_find(cx)
            }
        }
        let query = self.view.wa_input(id!(query));
        if let Some(text) = query.changed(actions) {
            self.query = text;
            cx.stop_timer(self.search_timer);
            self.search_timer = cx.start_timeout(0.18);
        }
        if let Some((_, modifiers)) = query.returned(actions) {
            self.run_find(cx, modifiers.shift, false);
            self.focus_query = true;
        }
        if query.escaped(actions) || self.view.wa_button(id!(find_close)).clicked(actions) {
            self.close_find(cx);
        }
        if self.view.wa_button(id!(find_previous)).clicked(actions) {
            self.run_find(cx, true, false);
        }
        if self.view.wa_button(id!(find_next)).clicked(actions) {
            self.run_find(cx, false, false);
        }
        let Some(ctx) = ctx else { return };
        if self.view.wa_button(id!(copy)).clicked(actions) && let Some(text) = self.with_session(|session, _| session.rendered_text()) {
            ctx.effects.push(UiEffect::CopyText { text, toast: Some(t("common.states.copied")) });
        }
        if self.view.wa_button(id!(retry)).clicked(actions) {
            let core = ctx.core.clone();
            let connected = self.connected;
            self.with_session(|session, _| session.retry(&core, connected));
            self.view.redraw(cx);
        }
        if self.view.wa_button(id!(close_tab)).clicked(actions) && let Some(target) = &self.target {
            if target.tab_key.is_empty() {
                ctx.effects.push(UiEffect::Navigate(Route::Workspace {
                    host_id: target.host_id.clone(),
                    workspace_id: Some(target.workspace_id.clone()).filter(|w| !w.is_empty()),
                }));
            } else {
                ctx.effects.push(UiEffect::CloseTab { tab_key: target.tab_key.clone() });
            }
        }
    }
}

impl Widget for ExecutionLogPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
        }
        if self.publish_timer.is_event(event).is_some() {
            self.publish_timer = Timer::default();
            self.with_session(|session, view| {
                session.publish();
                view.redraw(cx);
            });
            self.view.redraw(cx);
        }
        if self.search_timer.is_event(event).is_some() {
            self.search_timer = Timer::default();
            self.run_find(cx, false, true);
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event {
            let ctx = scope.data.get_mut::<UiContext>();
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.compact = scope.data.get_mut::<UiContext>().is_some_and(|ctx| ctx.compact);
        let target_title = self.target.as_ref().map(|t| t.title.clone()).unwrap_or_default();
        let connected = self.connected;
        struct Facts {
            title: String,
            status: String,
            lines: Option<String>,
            rows: usize,
            exited: bool,
            attaching: bool,
            error: Option<String>,
            follow: bool,
            revision: u64,
            total: usize,
            active: Option<usize>,
        }
        let facts = {
            let output = self.output();
            let view = output.borrow();
            let session = view.as_ref().and_then(|v| v.session.as_ref());
            Facts {
                title: display_title(&target_title, session).to_owned(),
                status: session
                    .map(|s| {
                        if s.exited {
                            l("Execution finished")
                        } else if s.live(connected) {
                            l("Live output · read-only")
                        } else {
                            l("Waiting for host · cached output")
                        }
                    })
                    .unwrap_or("")
                    .to_owned(),
                lines: session.map(|s| lf("{n} lines", &[("n", &s.line_count().to_string())])),
                rows: session.map(|s| s.line_count()).unwrap_or(0),
                exited: session.is_some_and(|s| s.exited),
                attaching: session.is_some_and(|s| s.attaching),
                error: self.init_error.clone().or_else(|| session.and_then(|s| s.error.clone())),
                follow: view.as_ref().is_some_and(|v| v.follow),
                revision: session.map(|s| s.revision).unwrap_or(0),
                total: view.as_ref().map(|v| v.matches.len()).unwrap_or(0),
                active: view.as_ref().and_then(|v| v.active_match),
            }
        };
        let has_session = facts.lines.is_some();
        self.view.widget(id!(toolbar.info.title)).set_text(cx, &ellipsize(&facts.title, 80));
        self.view.widget(id!(toolbar.info.status)).set_text(cx, &facts.status);
        let lines = if self.compact { "" } else { facts.lines.as_deref().unwrap_or("") };
        set_optional(cx, &self.view.widget(id!(toolbar.info.lines)), lines);
        let follow = self.view.wa_button(id!(follow));
        follow.set_selected(cx, facts.follow);
        follow.set_tooltip(cx, if facts.follow { l("Pause following output") } else { l("Follow output") });
        let find = self.view.wa_button(id!(toggle_find));
        find.set_selected(cx, self.show_find);
        find.set_tooltip(cx, &t("paneFind.title"));
        let copy = self.view.wa_button(id!(copy));
        copy.set_enabled(cx, has_session);
        copy.set_tooltip(cx, l("Copy all output"));
        let empty = self.view.widget(id!(body.empty));
        let show_empty = has_session && facts.rows == 0;
        empty.set_visible(cx, show_empty);
        if show_empty {
            let text = if facts.exited { l("Execution finished without output") } else { l("No output yet") };
            empty.widget(id!(text)).set_text(cx, text);
        }
        self.view.widget(id!(find_wrap)).set_visible(cx, self.show_find);
        if self.show_find {
            self.view.wa_input(id!(query)).set_empty_text(cx, t("paneFind.placeholder"));
            let status = self.find_status(facts.revision, facts.total, facts.active);
            set_optional(cx, &self.view.widget(id!(match_status)), &status);
            let can_navigate = facts.total > 0;
            let previous = self.view.wa_button(id!(find_previous));
            previous.set_enabled(cx, can_navigate);
            previous.set_tooltip(cx, &t("paneFind.previous"));
            let next = self.view.wa_button(id!(find_next));
            next.set_enabled(cx, can_navigate);
            next.set_tooltip(cx, &t("paneFind.next"));
            self.view.wa_button(id!(find_close)).set_tooltip(cx, &t("paneFind.close"));
            // The query field's borderAccent outline while focused or hovered.
            let active = self.view.wa_input(id!(query)).key_focus(cx);
            self.view.widget(id!(find_bar.field)).apply_over(cx, live! { border_width: (if active { 1.0 } else { 0.0 }) });
        }
        self.view.widget(id!(attach)).set_visible(cx, facts.attaching);
        let error_row = self.view.widget(id!(error_row));
        error_row.set_visible(cx, facts.error.is_some());
        if let Some(error) = &facts.error {
            error_row.widget(id!(text)).set_text(cx, &ellipsize(error, 240));
            let retry = self.view.wa_button(id!(retry));
            retry.set_visible(cx, has_session);
            retry.set_text(cx, &t("common.actions.retry"));
            let close = self.view.wa_button(id!(close_tab));
            close.set_visible(cx, !has_session);
            close.set_text(cx, &t("common.actions.close"));
        }
        let step = self.view.draw_walk(cx, scope, walk);
        if self.focus_query {
            self.focus_query = false;
            self.view.wa_input(id!(query)).set_key_focus(cx);
        }
        step
    }
}

impl ExecutionLogPanelRef {
    /// Mounts the panel on an execution (shell tab kind `execution_log`).
    pub fn set_target(&self, cx: &mut Cx, target: LogTarget) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.explicit_target = true;
        if inner.target.as_ref() == Some(&target) {
            return;
        }
        inner.target = Some(target);
        inner.session_opened = false;
        inner.init_error = None;
        if let Some(mut view) = inner.output().borrow_mut() {
            view.session = None;
        }
        inner.view.redraw(cx);
    }

    /// Hidden tabs release their observation; visible ones attach (`setVisible`). Applied on
    /// the next dispatch, which carries the core handle.
    pub fn set_visible_log(&self, cx: &mut Cx, visible: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.hidden != visible && inner.session_opened {
            return;
        }
        inner.hidden = !visible;
        inner.visibility_dirty = true;
        inner.view.redraw(cx);
    }
}
