//! `TerminalPanel`: the interactive terminal tab (original `panels/terminal-panel.tsx` with
//! `TerminalPane`): a 36px pane toolbar like the execution log's (title, status, copy), the
//! terminal surface, the attach spinner overlay, the "Host is not connected" state, the
//! stream error row and, on mobile or compact layouts, the virtual key bar.

use makepad_widgets::*;
use paseo_gui_platform::{PlatformAction, PlatformValue};

use super::keybar::{Control, Modifier, WaTerminalKeyBarWidgetExt};
use super::session::{self, display_title, scrollback_preference};
use super::stream::Stream;
use super::view::{WaTerminalViewRef, WaTerminalViewWidgetExt};
use crate::common::{UiContext, UiEffect};
use crate::features::workspace::host;
use crate::features::workspace::i18n::t;
use crate::features::workspace::kit::{ellipsize, WaButtonWidgetExt};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;
    use crate::features::terminal::view::*;
    use crate::features::terminal::keybar::*;

    pub TerminalPanel = {{TerminalPanel}} {
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
                }
                tools = <View> {
                    width: Fit, height: Fit, flow: Right, spacing: 1., align: { y: 0.5 }
                    copy = <WaToolButton> { icon: "copy" }
                }
            }
            <WaRule> {}
            body = <View> {
                width: Fill, height: Fill, flow: Overlay
                output = <WaTerminalView> {}
                // Original `centerState`: muted, centred, 16px side padding.
                disconnected = <Surface> {
                    width: Fill, height: Fill, visible: false
                    role: Surface0
                    align: { x: 0.5, y: 0.5 }
                    padding: { left: 16., right: 16. }
                    text = <WaMuted> { width: Fit }
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
            keys = <View> {
                width: Fill, height: Fit, visible: false
                keybar = <WaTerminalKeyBar> {}
            }
        }
    }
}

/// What a terminal tab shows (tab kind `terminal`, `PanelTarget.id` = daemon terminal id).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalTarget {
    pub host_id: String,
    pub workspace_id: String,
    pub terminal_id: String,
    pub title: String,
    /// The shell tab key, for Close.
    pub tab_key: String,
}

#[derive(Live, LiveHook, Widget)]
pub struct TerminalPanel {
    #[deref]
    view: View,
    #[rust]
    target: Option<TerminalTarget>,
    #[rust]
    hidden: bool,
    #[rust]
    visibility_dirty: bool,
    #[rust]
    focused: bool,
    #[rust]
    focus_dirty: bool,
    #[rust]
    session_opened: bool,
    #[rust]
    init_error: Option<String>,
    #[rust]
    connected: bool,
    #[rust]
    compact: bool,
    /// The key bar's pending clipboard read.
    #[rust]
    paste_request: Option<u64>,
}

impl TerminalPanel {
    fn output(&self) -> WaTerminalViewRef {
        self.view.wa_terminal_view(id!(output))
    }

    /// A fresh stream for the target.
    fn open_session(&mut self, cx: &mut Cx, ctx: &UiContext) {
        self.session_opened = true;
        self.init_error = None;
        let Some(target) = self.target.clone() else { return };
        let output = self.output();
        let Some(mut view) = output.borrow_mut() else { return };
        view.clear_selection(cx);
        view.session = None;
        view.follow = true;
        view.reset_size();
        if target.terminal_id.is_empty() {
            self.init_error = Some(t("workspace.terminal.missingId"));
            return;
        }
        let scrollback = scrollback_preference(host::preferences(&ctx.state));
        let mut stream = match session::open(&target.host_id, &target.terminal_id, scrollback) {
            Ok(stream) => stream,
            Err(error) => {
                self.init_error = Some(error);
                return;
            }
        };
        self.connected = host::host(&ctx.state, &target.host_id).is_some_and(|h| h.connected());
        stream.set_visible(&ctx.core, !self.hidden, self.connected);
        view.connected = self.connected;
        view.session = Some(stream);
        self.focus_dirty = true;
        view.redraw(cx);
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        host::learn(&ctx.events);
        let Some(target) = self.target.clone() else { return };
        if !self.session_opened {
            self.open_session(cx, ctx);
        }
        self.connected = host::host(&ctx.state, &target.host_id).is_some_and(|h| h.connected());
        let core = ctx.core.clone();
        let output = self.output();
        // The panel's own redraw walks its children, including `output`: it runs only after
        // the view borrow below has ended.
        let mut redraw_panel = false;
        if let Some(mut view) = output.borrow_mut() {
            view.connected = self.connected;
            let visible = !self.hidden;
            if std::mem::take(&mut self.visibility_dirty) {
                let connected = self.connected;
                if let Some(session) = view.session.as_mut() {
                    session.set_visible(&core, visible, connected);
                }
                if !visible {
                    view.blur(cx);
                }
                self.focus_dirty = true;
                redraw_panel = true;
            }
            if std::mem::take(&mut self.focus_dirty) {
                let focused = self.focused && visible;
                view.set_claim(focused);
                if focused {
                    view.focus(cx);
                } else {
                    view.blur(cx);
                }
                view.sync_size(&core, true);
            }
            if let Some(session) = view.session.as_mut() {
                let changed = session.pump(&core, &ctx.events);
                let attached = session.take_attached();
                let output_pending = std::mem::take(&mut session.output_pending);
                if output_pending {
                    session.publish();
                }
                if attached {
                    // A new subscription or snapshot: the screen has the PTY's size until this
                    // pane's own size is applied and sent again.
                    view.reset_size();
                    view.sync_size(&core, true);
                }
                if output_pending || attached {
                    view.redraw(cx);
                }
                redraw_panel |= changed || attached;
            }
        }
        if redraw_panel {
            self.view.redraw(cx);
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        let output = self.output();
        let keybar = self.view.wa_terminal_key_bar(id!(keybar));
        if output.modifiers_consumed(actions) {
            self.view.redraw(cx);
        }
        if let Some(control) = keybar.pressed(actions) {
            if let Some(mut view) = output.borrow_mut() {
                match control {
                    Control::Key(key) => {
                        view.key(cx, &ctx.core, key, paseo_terminal::Modifiers::default());
                    }
                    Control::Modifier(modifier) => {
                        let mut sticky = view.sticky();
                        match modifier {
                            Modifier::Ctrl => sticky.control = !sticky.control,
                            Modifier::Alt => sticky.alt = !sticky.alt,
                            Modifier::Shift => sticky.shift = !sticky.shift,
                        }
                        view.set_sticky(sticky);
                    }
                    Control::Paste => {
                        if let Some(platform) = paseo_gui_platform::platform() {
                            self.paste_request = Some(platform.read_clipboard(false));
                        }
                    }
                    Control::KeyboardToggle => {
                        if view.keyboard_requested() {
                            view.hide_keyboard(cx);
                        } else {
                            view.show_keyboard(cx);
                        }
                    }
                }
            }
            self.view.redraw(cx);
        }
        for action in actions.iter() {
            let Some(PlatformAction::Completed { request, result }) = action.downcast_ref::<PlatformAction>() else { continue };
            if self.paste_request != Some(*request) {
                continue;
            }
            self.paste_request = None;
            if let Ok(PlatformValue::Clipboard(content)) = result
                && let Some(text) = content.text.as_deref().filter(|text| !text.is_empty())
                && let Some(mut view) = output.borrow_mut()
            {
                view.paste(cx, &ctx.core, text);
            }
        }
        if self.view.wa_button(id!(copy)).clicked(actions)
            && let Some(text) = output.borrow_mut().map(|mut view| view.copy_text())
            && !text.is_empty()
        {
            ctx.effects.push(UiEffect::CopyText { text, toast: Some(t("common.states.copied")) });
        }
        if self.view.wa_button(id!(retry)).clicked(actions) {
            if let Some(mut view) = output.borrow_mut() {
                let connected = self.connected;
                view.reset_size();
                if let Some(session) = view.session.as_mut() {
                    session.retry(&ctx.core, connected);
                }
            }
            self.view.redraw(cx);
        }
        if self.view.wa_button(id!(close_tab)).clicked(actions)
            && let Some(target) = &self.target
            && !target.tab_key.is_empty()
        {
            ctx.effects.push(UiEffect::CloseTab { tab_key: target.tab_key.clone() });
        }
    }
}

impl Widget for TerminalPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event
            && let Some(ctx) = scope.data.get_mut::<UiContext>()
        {
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
            status: String,
            has_session: bool,
            attaching: bool,
            exited: bool,
            error: Option<String>,
            sticky: paseo_terminal::Modifiers,
            keyboard: bool,
        }
        let facts = {
            let output = self.output();
            let view = output.borrow();
            let session: Option<&Stream> = view.as_ref().and_then(|v| v.session.as_ref());
            Facts {
                title: display_title(&target_title, session),
                status: session
                    .map(|s| {
                        if s.exited {
                            t("workspace.terminal.exited")
                        } else if s.live(connected) || s.error.is_some() {
                            // A failed stream shows its error row, not a connecting status.
                            String::new()
                        } else if !connected {
                            t("workspace.terminal.hostDisconnected")
                        } else {
                            t("workspace.terminal.connecting")
                        }
                    })
                    .unwrap_or_default(),
                has_session: session.is_some(),
                attaching: session.is_some_and(|s| s.attaching && connected),
                exited: session.is_some_and(|s| s.exited),
                error: self.init_error.clone().or_else(|| session.and_then(|s| s.error.clone())),
                sticky: view.as_ref().map(|v| v.sticky()).unwrap_or_default(),
                keyboard: view.as_ref().is_some_and(|v| v.keyboard_requested()),
            }
        };
        self.view.widget(id!(toolbar.info.title)).set_text(cx, &ellipsize(&facts.title, 80));
        self.view.widget(id!(toolbar.info.status)).set_text(cx, &facts.status);
        let copy = self.view.wa_button(id!(copy));
        copy.set_enabled(cx, facts.has_session);
        copy.set_tooltip(cx, &t("workspace.terminal.copy"));
        let disconnected = self.view.widget(id!(body.disconnected));
        let show_disconnected = facts.has_session && !connected;
        disconnected.set_visible(cx, show_disconnected);
        if show_disconnected {
            disconnected.widget(id!(text)).set_text(cx, &t("workspace.terminal.hostDisconnected"));
        }
        self.view.widget(id!(attach)).set_visible(cx, facts.attaching && !show_disconnected);
        // An exit is shown in the error row like a stream error (`Terminal exited`).
        let message = facts.error.clone().or_else(|| facts.exited.then(|| t("workspace.terminal.exited")));
        let error_row = self.view.widget(id!(error_row));
        error_row.set_visible(cx, message.is_some());
        if let Some(message) = &message {
            error_row.widget(id!(text)).set_text(cx, &ellipsize(message, 240));
            let retry = self.view.wa_button(id!(retry));
            retry.set_visible(cx, facts.has_session && !facts.exited);
            retry.set_text(cx, &t("common.actions.retry"));
            // A failed or exited terminal can always leave its tab.
            let close = self.view.wa_button(id!(close_tab));
            close.set_visible(cx, self.target.as_ref().is_some_and(|t| !t.tab_key.is_empty()));
            close.set_text(cx, &t("common.actions.close"));
        }
        // `shouldShowTerminalVirtualKeyBar`: native (mobile) or compact layouts.
        let show_keys = facts.has_session && !facts.exited && (cfg!(any(target_os = "android", target_os = "ios")) || self.compact);
        self.view.widget(id!(keys)).set_visible(cx, show_keys);
        if show_keys {
            self.view.wa_terminal_key_bar(id!(keybar)).set_state(cx, facts.sticky, facts.keyboard, self.compact);
        }
        self.view.draw_walk(cx, scope, walk)
    }
}

impl TerminalPanelRef {
    /// Mounts the panel on a daemon terminal (shell tab kind `terminal`).
    pub fn set_target(&self, cx: &mut Cx, target: TerminalTarget) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.target.as_ref() == Some(&target) {
            return;
        }
        // A rename keeps the stream; another terminal starts a new one.
        let same_terminal = inner.target.as_ref().is_some_and(|t| t.host_id == target.host_id && t.terminal_id == target.terminal_id);
        inner.target = Some(target);
        if !same_terminal {
            inner.session_opened = false;
            inner.init_error = None;
            if let Some(mut view) = inner.output().borrow_mut() {
                view.clear_selection(cx);
                view.session = None;
            }
        }
        inner.view.redraw(cx);
    }

    /// Hidden tabs release their stream; visible ones attach. Applied on the next dispatch,
    /// which carries the core handle.
    pub fn set_visible_terminal(&self, cx: &mut Cx, visible: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.hidden != visible && inner.session_opened {
            return;
        }
        inner.hidden = !visible;
        inner.visibility_dirty = true;
        inner.view.redraw(cx);
    }

    /// The focused pane takes keyboard focus and claims the PTY size; others only update it.
    pub fn set_focused_terminal(&self, cx: &mut Cx, focused: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.focused == focused {
            return;
        }
        inner.focused = focused;
        inner.focus_dirty = true;
        inner.view.redraw(cx);
    }
}
