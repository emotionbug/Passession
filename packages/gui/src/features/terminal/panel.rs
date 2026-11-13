//! `TerminalPanel`: the interactive terminal tab (original `panels/terminal-panel.tsx` with
//! `TerminalPane`): a 36px pane toolbar like the execution log's (title, status, find, copy),
//! the terminal surface, the find card, the attach spinner overlay, the "Host is not
//! connected" state, the stream error row, the neutral exit row (New terminal, Close) and, on
//! mobile or compact layouts, the virtual key bar.

use std::collections::VecDeque;
use std::path::PathBuf;

use makepad_widgets::*;
use paseo_gui_platform::{PlatformAction, PlatformValue};
use serde_json::{json, Value};

use super::drop::{self, FILE_TOO_LARGE};
use super::find::{FindCard, FindCommand};
use super::keybar::{Control, Modifier, WaTerminalKeyBarWidgetExt};
use super::session::{self, display_title, scrollback_preference};
use super::stream::Stream;
use super::view::{WaTerminalViewRef, WaTerminalViewWidgetExt};
use crate::common::file_drop::FileDropZone;
use crate::common::{PanelTarget, UiContext, UiEffect};
use crate::features::composer::ops;
use crate::features::workspace::host;
use crate::features::workspace::i18n::{t, ta};
use crate::features::workspace::kit::{ellipsize, WaButtonWidgetExt};
use crate::features::workspace::rpc::{object_of, str_of, str_or, Pending};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::widgets::*;
    use crate::common::file_drop::FileDropZone;
    use crate::features::workspace::kit::*;
    use crate::features::terminal::view::*;
    use crate::features::terminal::keybar::*;
    use crate::features::terminal::find::*;

    pub TerminalPanel = {{TerminalPanel}} {
        width: Fill, height: Fill, flow: Down
        file_drop: <FileDropZone> {}
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
                    // Long shell titles ellipsize; the status after them stays visible.
                    title = <WaSmallLine> {}
                    status = <WaSmallFit> { role: ExtraMuted }
                }
                tools = <View> {
                    width: Fit, height: Fit, flow: Right, spacing: 1., align: { y: 0.5 }
                    toggle_find = <WaToolButton> { icon: "search" }
                    copy = <WaToolButton> { icon: "copy" }
                }
            }
            <WaRule> {}
            body = <View> {
                width: Fill, height: Fill, flow: Overlay
                output = <WaTerminalView> {}
                find_wrap = <WaPaneFind> {}
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
            // The process ended: neutral surface1 band with muted text, never the error row.
            exit_row = <Surface> {
                width: Fill, height: Fit, flow: Down, visible: false
                role: Surface1
                <WaRule> {}
                row = <View> {
                    width: Fill, height: Fit, flow: Right, spacing: 8.
                    padding: { left: 12., right: 12., top: 4., bottom: 4. }
                    align: { y: 0.5 }
                    text = <WaText> { size: 12., role: Muted }
                    new_terminal = <WaButtonGhost> { size: Xs, icon: "plus" }
                    exit_close = <WaButtonGhost> { size: Xs }
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
    /// terminal-pane.tsx `FileDropZone`: the output takes dropped files.
    #[live]
    file_drop: FileDropZone,
    /// Dropped files still to store on a host that cannot read this machine's disk, in
    /// drop order (`TerminalFilePasteQueue`).
    #[rust]
    upload_queue: VecDeque<PathBuf>,
    /// The upload in flight.
    #[rust]
    upload_request: Option<String>,
    #[rust]
    find: FindCard,
    /// "New terminal" after an exit: the `create_terminal_request` in flight (key: cwd).
    #[rust]
    create_pending: Pending<String>,
    /// The workspace directory a replacement terminal starts in, while its host is connected.
    #[rust]
    new_terminal_cwd: Option<String>,
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
        view.find.clear();
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
        self.new_terminal_cwd = host::host(&ctx.state, &target.host_id)
            .filter(|h| h.connected())
            .and_then(|h| h.workspace(&target.workspace_id))
            .map(|w| str_of(w, "workspaceDirectory").trim().to_owned())
            .filter(|dir| !dir.is_empty());
        self.handle_created(cx, ctx, &target);
        let core = ctx.core.clone();
        let output = self.output();
        // The panel's own redraw walks its children, including `output`: it runs only after
        // the view borrow below has ended.
        let mut redraw_panel = false;
        let mut output_changed = false;
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
                output_changed = output_pending || attached;
                redraw_panel |= changed || attached;
            }
        }
        if output_changed {
            // Matches follow new output while the find card is open.
            self.find.output_changed(cx);
        }
        if redraw_panel {
            self.view.redraw(cx);
        }
    }

    /// "New terminal" on the exit row: a fresh shell in the workspace replaces this tab.
    fn create_terminal(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let (Some(target), Some(cwd)) = (self.target.clone(), self.new_terminal_cwd.clone()) else { return };
        if !self.create_pending.is_empty() {
            return;
        }
        let fields = json!({ "cwd": cwd, "workspaceId": target.workspace_id });
        self.create_pending.send(&ctx.core, &target.host_id, "create_terminal_request", fields, cwd);
        self.view.redraw(cx);
    }

    fn handle_created(&mut self, cx: &mut Cx, ctx: &mut UiContext, target: &TerminalTarget) {
        for (cwd, result) in self.create_pending.take(&ctx.events) {
            self.view.redraw(cx);
            let terminal = match result {
                Ok(body) => object_of(&body, "terminal").clone(),
                Err(error) => {
                    let message = ta("workspace.terminal.createFailed", &[("reason", &error.to_string())]);
                    ctx.effects.push(UiEffect::Toast { message, is_error: true });
                    continue;
                }
            };
            let terminal_id = str_of(&terminal, "id");
            if terminal_id.is_empty() {
                let message = ta("workspace.terminal.createFailed", &[("reason", &t("workspace.terminal.unableToSubscribe"))]);
                ctx.effects.push(UiEffect::Toast { message, is_error: true });
                continue;
            }
            let next = PanelTarget::new("terminal", &target.host_id, str_or(&terminal, "workspaceId", &target.workspace_id), str_or(&terminal, "cwd", &cwd))
                .with_id(terminal_id);
            ctx.effects.push(UiEffect::ReplaceTab { tab_key: target.tab_key.clone(), target: next });
        }
    }

    /// terminal-pane.tsx `handleDroppedFiles` (`routeTerminalDrop`): on the host's own
    /// machine the files' paths are pasted as one input; any other host is sent the bytes
    /// and each host path is pasted as its upload finishes.
    fn drop_files(&mut self, cx: &mut Cx, ctx: &mut UiContext, paths: Vec<PathBuf>) {
        let Some(target) = self.target.clone() else { return };
        let output = self.output();
        if host::host(&ctx.state, &target.host_id).is_some_and(|h| h.is_local()) {
            if let Some(mut view) = output.borrow_mut() {
                view.paste(cx, &ctx.core, &drop::prepare_dropped_paths(&paths));
            }
        } else {
            self.upload_queue.extend(paths);
            self.upload_next(ctx);
        }
        // requestTerminalFocus
        if let Some(mut view) = output.borrow_mut() {
            view.focus(cx);
        }
        self.view.redraw(cx);
    }

    /// Starts the next queued upload; one file is stored at a time.
    fn upload_next(&mut self, ctx: &mut UiContext) {
        if self.upload_request.is_some() {
            return;
        }
        if !self.connected {
            // The upload that just failed already reported the lost host.
            self.upload_queue.clear();
            return;
        }
        let (Some(path), Some(target)) = (self.upload_queue.pop_front(), self.target.as_ref()) else { return };
        self.upload_request = Some(ctx.core.operation(&target.host_id, move |client| drop::upload_dropped_file(client, path)));
    }

    /// The upload in flight finished: its host path is pasted (`prepareTerminalPath`), a
    /// failure is reported (`reportFilePasteFailure`), and the next file starts.
    fn handle_upload(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        let Some(result) = self.upload_request.as_deref().and_then(|id| ctx.response(id)) else { return };
        let outcome: Result<String, String> = match result.as_ref().map(ops::open_envelope) {
            Ok(Ok(body)) => body
                .pointer("/files/0/path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| t("composer.errors.uploadFailed")),
            Ok(Err(error)) => {
                let message = error.get("message").and_then(Value::as_str).unwrap_or_default();
                Err(if error.get("code").and_then(Value::as_str) == Some(FILE_TOO_LARGE) {
                    ta("composer.errors.fileTooLarge", &[("size", "50MB"), ("fileName", message)])
                } else {
                    message.to_owned()
                })
            }
            Err(error) => Err(error.to_string()),
        };
        self.upload_request = None;
        match outcome {
            Ok(path) => {
                if let Some(mut view) = self.output().borrow_mut() {
                    view.paste(cx, &ctx.core, &drop::prepare_terminal_path(&path));
                }
                ctx.effects.push(UiEffect::Toast { message: t("workspace.terminal.fileUploaded"), is_error: false });
            }
            Err(reason) => {
                let message = ta("workspace.terminal.pasteFileFailed", &[("reason", &reason)]);
                ctx.effects.push(UiEffect::Toast { message, is_error: true });
            }
        }
        self.upload_next(ctx);
        self.view.redraw(cx);
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
        if (self.view.wa_button(id!(close_tab)).clicked(actions) || self.view.wa_button(id!(exit_close)).clicked(actions))
            && let Some(target) = &self.target
            && !target.tab_key.is_empty()
        {
            ctx.effects.push(UiEffect::CloseTab { tab_key: target.tab_key.clone() });
        }
        if self.view.wa_button(id!(new_terminal)).clicked(actions) {
            self.create_terminal(cx, ctx);
        }
        // Find: the toolbar toggle, Ctrl+Shift+F / Cmd+F in the terminal, and the card.
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
    }

    fn open_find(&mut self, cx: &mut Cx) {
        let reopened = !self.find.open && !self.find.query.is_empty();
        self.find.open();
        if reopened {
            // Closing cleared the highlights: the kept query searches again.
            self.apply_find(cx, vec![FindCommand::Search(super::find::SearchMode::Reset { previous: false })]);
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
                FindCommand::Closed => {
                    view.clear_find(cx);
                    // Typing goes back to the shell.
                    view.focus(cx);
                }
            }
        }
        self.view.redraw(cx);
    }
}

impl Widget for TerminalPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(mode) = self.find.timer_fired(event) {
            self.apply_find(cx, vec![FindCommand::Search(mode)]);
        }
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
            self.handle_upload(cx, ctx);
            // terminal-pane.tsx: the output of a connected terminal takes dropped files.
            let drop_ready = !self.hidden && self.connected && self.output().borrow().is_some_and(|view| view.session.is_some());
            if let Some(paths) = self.file_drop.handle_event(cx, event, drop_ready) {
                self.drop_files(cx, ctx, paths);
            }
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
            exit_code: Option<i64>,
            error: Option<String>,
            sticky: paseo_terminal::Modifiers,
            keyboard: bool,
            matches: usize,
            active: Option<usize>,
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
                            // The exit row says it; the toolbar does not repeat it.
                            String::new()
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
                exit_code: session.and_then(|s| s.exit_code),
                error: self.init_error.clone().or_else(|| session.and_then(|s| s.error.clone())),
                sticky: view.as_ref().map(|v| v.sticky()).unwrap_or_default(),
                keyboard: view.as_ref().is_some_and(|v| v.keyboard_requested()),
                matches: view.as_ref().map(|v| v.find.matches.len()).unwrap_or(0),
                active: view.as_ref().and_then(|v| v.find.active),
            }
        };
        self.view.widget(id!(toolbar.info.title)).set_text(cx, &facts.title);
        // `showFileUploading`: a dropped file is on its way to the host.
        let status = if facts.status.is_empty() && self.upload_request.is_some() { t("workspace.terminal.uploadingFile") } else { facts.status.clone() };
        self.view.widget(id!(toolbar.info.status)).set_text(cx, &status);
        let copy = self.view.wa_button(id!(copy));
        copy.set_enabled(cx, facts.has_session);
        copy.set_tooltip(cx, &t("workspace.terminal.copy"));
        let find = self.view.wa_button(id!(toggle_find));
        find.set_enabled(cx, facts.has_session);
        find.set_selected(cx, self.find.open);
        find.set_tooltip(cx, &t("paneFind.title"));
        let body_width = self.view.widget(id!(body)).area().rect(cx).size.x;
        self.find.present(cx, &self.view.widget(id!(find_wrap)), body_width, self.compact, facts.matches, facts.active);
        let disconnected = self.view.widget(id!(body.disconnected));
        let show_disconnected = facts.has_session && !connected;
        disconnected.set_visible(cx, show_disconnected);
        if show_disconnected {
            disconnected.widget(id!(text)).set_text(cx, &t("workspace.terminal.hostDisconnected"));
        }
        self.view.widget(id!(attach)).set_visible(cx, facts.attaching && !show_disconnected);
        let error_row = self.view.widget(id!(error_row));
        error_row.set_visible(cx, facts.error.is_some());
        if let Some(message) = &facts.error {
            error_row.widget(id!(text)).set_text(cx, &ellipsize(message, 240));
            let retry = self.view.wa_button(id!(retry));
            retry.set_visible(cx, facts.has_session);
            retry.set_text(cx, &t("common.actions.retry"));
            // A failed terminal can always leave its tab.
            let close = self.view.wa_button(id!(close_tab));
            close.set_visible(cx, self.target.as_ref().is_some_and(|t| !t.tab_key.is_empty()));
            close.set_text(cx, &t("common.actions.close"));
        }
        // A normal exit is not a failure: a neutral row offers a new shell or closing the tab.
        let exit_row = self.view.widget(id!(exit_row));
        let show_exit = facts.exited && facts.error.is_none();
        exit_row.set_visible(cx, show_exit);
        if show_exit {
            let message = match facts.exit_code {
                Some(code) if code != 0 => ta("workspace.terminal.exitedWithCode", &[("code", &code.to_string())]),
                _ => t("workspace.terminal.exited"),
            };
            exit_row.widget(id!(text)).set_text(cx, &message);
            let new_terminal = self.view.wa_button(id!(new_terminal));
            new_terminal.set_visible(cx, self.new_terminal_cwd.is_some() && self.target.as_ref().is_some_and(|t| !t.tab_key.is_empty()));
            new_terminal.set_text(cx, &t("workspace.terminal.newTerminal"));
            new_terminal.set_busy(cx, !self.create_pending.is_empty());
            let close = self.view.wa_button(id!(exit_close));
            close.set_visible(cx, self.target.as_ref().is_some_and(|t| !t.tab_key.is_empty()));
            close.set_text(cx, &t("common.actions.close"));
        }
        // `shouldShowTerminalVirtualKeyBar`: native (mobile) or compact layouts.
        let show_keys = facts.has_session && !facts.exited && (cfg!(any(target_os = "android", target_os = "ios")) || self.compact);
        self.view.widget(id!(keys)).set_visible(cx, show_keys);
        if show_keys {
            self.view.wa_terminal_key_bar(id!(keybar)).set_state(cx, facts.sticky, facts.keyboard, self.compact);
        }
        let step = self.view.draw_walk(cx, scope, walk);
        self.find.take_focus(cx, &self.view.widget(id!(find_wrap)));
        // The drop backdrop covers the output (`styles.outputContainer`).
        let zone = self.view.widget(id!(body)).area().rect(cx);
        self.file_drop.draw(cx, zone);
        step
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
            // Files dropped on the previous terminal are not pasted into this one.
            inner.upload_queue.clear();
            inner.upload_request = None;
            inner.paste_request = None;
            inner.create_pending = Pending::default();
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
