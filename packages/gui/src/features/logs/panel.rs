//! `ExecutionLogPanel` (Flutter `execution_log_panel.dart`): the original `TerminalPane`
//! presentation — a 36px `PaneContentToolbar` (title, status, line count, follow, find,
//! copy), the read-only output, the floating `PaneFind` card, the attach spinner overlay and
//! the stream error row. A finished execution reports success or failure with its exit code
//! when the daemon sends one.

use makepad_widgets::*;

use super::session::{self, display_title, scrollback_preference, LogSession};
use super::view::{WaLogView, WaLogViewWidgetExt};
use crate::common::widgets::PLabelWidgetRefExt;
use crate::common::{PanelTarget, Route, UiContext, UiEffect};
use crate::features::terminal::find::{FindCard, FindCommand, SearchMode};
use crate::features::workspace::host;
use crate::features::workspace::i18n::{l, lf, t, ta};
use crate::features::workspace::kit::{ellipsize, set_optional, WaButtonWidgetExt};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;
    use crate::features::logs::view::*;
    use crate::features::terminal::find::*;

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
                    // Long titles ellipsize; the status after them stays visible.
                    title = <WaSmallLine> {}
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
                find_wrap = <WaPaneFind> {}
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

/// How a finished execution ended, for the toolbar status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Running,
    Live,
    Waiting,
    /// Finished; the daemon did not report the exit code (older hosts).
    Finished,
    Succeeded,
    Failed(i64),
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
    find: FindCard,
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
        self.init_error = None;
        let output = self.output();
        let Some(mut view) = output.borrow_mut() else { return };
        view.clear_selection(cx);
        view.session = None;
        view.find.clear();
        view.follow = true;
        if target.terminal_id.is_empty() {
            self.init_error = Some(l("Execution output requires an existing process identifier").to_owned());
            return;
        }
        let scrollback = scrollback_preference(host::preferences(&ctx.state));
        let mut session = match session::open(&target.host_id, &target.terminal_id, scrollback) {
            Ok(session) => session,
            Err(error) => {
                self.init_error = Some(error);
                return;
            }
        };
        self.connected = host::host(&ctx.state, &target.host_id).is_some_and(|h| h.connected());
        session.set_visible(&ctx.core, !self.hidden, self.connected);
        view.session = Some(session);
        view.redraw(cx);
    }

    fn open_find(&mut self, cx: &mut Cx) {
        let reopened = !self.find.open && !self.find.query.is_empty();
        self.find.open();
        if reopened {
            // Closing cleared the highlights: the kept query searches again.
            self.apply_find(cx, vec![FindCommand::Search(SearchMode::Reset { previous: false })]);
        }
        self.view.redraw(cx);
    }

    fn apply_find(&mut self, cx: &mut Cx, commands: Vec<FindCommand>) {
        let output = self.output();
        for command in commands {
            let Some(mut view) = output.borrow_mut() else { return };
            match command {
                FindCommand::Search(mode) => {
                    view.run_find(cx, &self.find.query, mode);
                    self.find.searched(cx);
                }
                FindCommand::Closed => view.clear_find(cx),
            }
        }
        self.view.redraw(cx);
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
                // A snapshot or restore replaced the output: matches follow it.
                self.find.output_changed(cx);
                self.view.redraw(cx);
            }
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: Option<&mut UiContext>) {
        let output = self.output();
        if output.user_scrolled(actions) {
            // The view paused or resumed following; the toolbar toggle shows it.
            self.view.redraw(cx);
        }
        if self.view.wa_button(id!(follow)).clicked(actions) {
            if let Some(mut view) = output.borrow_mut() {
                view.follow = !view.follow;
                view.redraw(cx);
            }
            self.view.redraw(cx);
        }
        let toggled = self.view.wa_button(id!(toggle_find)).clicked(actions);
        if toggled && self.find.open {
            self.find.close(cx);
            self.apply_find(cx, vec![FindCommand::Closed]);
        } else if toggled || output.find_requested(actions) {
            self.open_find(cx);
        }
        let commands = self.find.handle_actions(cx, &self.view.widget(id!(find_wrap)), actions);
        if !commands.is_empty() {
            self.apply_find(cx, commands);
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
            // Matches follow live output while the find card is open.
            self.find.output_changed(cx);
            self.view.redraw(cx);
        }
        if let Some(mode) = self.find.timer_fired(event) {
            self.apply_find(cx, vec![FindCommand::Search(mode)]);
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event {
            let ctx = scope.data.get_mut::<UiContext>();
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.compact = scope.data.get_mut::<UiContext>().is_some_and(|ctx| ctx.compact);
        let ty = crate::theme::current_typography();
        let toolbar_h = 36.0_f64.max(ty.ui_size(12.0) * 1.4 + 12.0);
        self.view.view(id!(toolbar)).apply_over(cx, live! { height: (toolbar_h) });
        let target_title = self.target.as_ref().map(|t| t.title.clone()).unwrap_or_default();
        let connected = self.connected;
        struct Facts {
            title: String,
            outcome: Outcome,
            lines: Option<String>,
            rows: usize,
            exited: bool,
            attaching: bool,
            error: Option<String>,
            follow: bool,
            total: usize,
            active: Option<usize>,
        }
        let facts = {
            let output = self.output();
            let view = output.borrow();
            let session = view.as_ref().and_then(|v| v.session.as_ref());
            Facts {
                title: display_title(&target_title, session).to_owned(),
                outcome: match session {
                    None => Outcome::Running,
                    Some(s) if s.exited => match s.exit_code {
                        None => Outcome::Finished,
                        Some(0) => Outcome::Succeeded,
                        Some(code) => Outcome::Failed(code),
                    },
                    Some(s) if s.live(connected) => Outcome::Live,
                    Some(_) => Outcome::Waiting,
                },
                lines: session.map(|s| lf("{n} lines", &[("n", &s.line_count().to_string())])),
                rows: session.map(|s| s.line_count()).unwrap_or(0),
                exited: session.is_some_and(|s| s.exited),
                attaching: session.is_some_and(|s| s.attaching),
                error: self.init_error.clone().or_else(|| session.and_then(|s| s.error.clone())),
                follow: view.as_ref().is_some_and(|v| v.follow),
                total: view.as_ref().map(|v| v.find.matches.len()).unwrap_or(0),
                active: view.as_ref().and_then(|v| v.find.active),
            }
        };
        let has_session = facts.lines.is_some();
        self.view.widget(id!(toolbar.info.title)).set_text(cx, &facts.title);
        let palette = crate::theme::current_palette();
        let (status, color) = match facts.outcome {
            Outcome::Running => (String::new(), None),
            Outcome::Live => (l("Live output · read-only").to_owned(), None),
            Outcome::Waiting => (l("Waiting for host · cached output").to_owned(), None),
            Outcome::Finished => (l("Execution finished").to_owned(), None),
            Outcome::Succeeded => (t("workspace.executionLog.succeeded"), Some(palette.success)),
            Outcome::Failed(code) => (ta("workspace.executionLog.failed", &[("code", &code.to_string())]), Some(palette.danger)),
        };
        let info = self.view.widget(id!(toolbar.info));
        info.widget(id!(status)).set_text(cx, &status);
        info.plabel(id!(status)).set_color(cx, color);
        let lines = if self.compact { "" } else { facts.lines.as_deref().unwrap_or("") };
        set_optional(cx, &self.view.widget(id!(toolbar.info.lines)), lines);
        let follow = self.view.wa_button(id!(follow));
        follow.set_selected(cx, facts.follow);
        follow.set_tooltip(cx, if facts.follow { l("Pause following output") } else { l("Follow output") });
        let find = self.view.wa_button(id!(toggle_find));
        find.set_selected(cx, self.find.open);
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
        let body_width = self.view.widget(id!(body)).area().rect(cx).size.x;
        self.find.present(cx, &self.view.widget(id!(find_wrap)), body_width, self.compact, facts.total, facts.active);
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
        self.find.take_focus(cx, &self.view.widget(id!(find_wrap)));
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
            view.clear_selection(cx);
            view.session = None;
            view.find.clear();
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
