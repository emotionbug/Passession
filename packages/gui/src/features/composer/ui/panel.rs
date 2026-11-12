//! `ComposerPanel`: the native composer (Flutter `agent_panel.dart` `_composer` and its
//! input, toolbar, attachment tray, queue track and delivery status). Rendering is
//! immediate-mode (`layout.rs`, `overlays.rs`); the text editor is Makepad's `TextInput`
//! (IME composition, selection, clipboard, undo), drawn inline or in the fullscreen editor.
//!
//! IME and streaming: the editor text is only replaced when the persisted draft changed
//! *externally* (restore, acknowledgement of this exact submission, queued edit, another
//! pane). Keystrokes and IME composition update the draft from the editor and never round
//! trip, so composition, caret and selection survive timeline streaming and redraws.

use super::draw::DrawComposerImage;
use super::look::Look;
use super::sheet::SheetState;
use crate::common::overlay::{self, MenuEntry, MenuItem};
use crate::common::paint::{HitMap, Painter};
use crate::common::{UiCommand, UiContext, UiEffect};
use crate::features::composer::controller::*;
use crate::features::composer::i18n::t;
use crate::features::composer::json::*;
use crate::features::composer::service;
use crate::features::settings::SettingsPanelWidgetRefExt;
use makepad_widgets::makepad_draw::text::selection::Cursor;
use makepad_widgets::*;
use serde_json::Value;
use std::collections::HashMap;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::paint::PaseoPainter;
    use crate::features::composer::ui::draw::DrawComposerImage;
    use crate::features::settings::panel::SettingsSheetHost;

    pub ComposerInput = <TextInput> {
        width: Fill, height: Fit,
        padding: 0, margin: 0,
        empty_text: "",
        draw_bg: {
            fn pixel(self) -> vec4 { return vec4(0.0, 0.0, 0.0, 0.0); }
        }
        draw_text: {
            text_style: <PASEO_FONT_REGULAR> { line_spacing: 1.4 }
            // The hint keeps its colour while focused (TextInput mixes the focus colour
            // over the empty one).
            fn get_color(self) -> vec4 {
                return mix(mix(self.color, self.color_empty, self.empty), self.color_disabled, self.disabled);
            }
        }
        // TextInput starts in `empty.off` and only re-evaluates after an edit, so an empty
        // editor showed its hint in the text colour until the first keystroke.
        animator: { empty = { default: on } }
    }

    pub ComposerPanelBase = {{ComposerPanel}} {}
    pub ComposerPanel = <ComposerPanelBase> {
        width: Fill, height: Fit,
        draw_bg: { color: #0000 }
        overlay_bg: { color: #0000 }
        painter: <PaseoPainter> {}
        draw_icon: {
            fn get_color(self) -> vec4 { return self.color; }
        }
        draw_image: <DrawComposerImage> {}
        caret_text: { text_style: <PASEO_FONT_REGULAR> { line_spacing: 1.4 } }
        input: <ComposerInput> {}
        profile_editor: <SettingsSheetHost> {}
        search: <ComposerInput> { draw_text: { text_style: { line_spacing: 1.2 } } }
    }
}

/// Interactive items of the inline composer.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Attach,
    Model,
    Thinking,
    Mode,
    Feature(usize),
    Features,
    Meter,
    Send,
    Stop,
    Expand,
    Image(usize),
    ImageRemove(usize),
    Attachment(usize),
    AttachmentRemove(usize),
    QueueEdit(String),
    QueueSend(String),
    QueueRemove(String),
    Check(String),
    Retry(String),
    StopWaiting(String),
    Restore,
    RetryFailed,
    Discard,
    Suggest(usize),
    DismissError,
}

/// Interactive items of the modal overlays.
#[derive(Clone, Debug, PartialEq)]
pub enum OItem {
    Scrim,
    Panel,
    Close,
    Back,
    Gear,
    Row(usize),
    RowAction(usize),
    Forge(usize),
    FullscreenSend,
    UsageRefresh(usize),
}

#[derive(Clone, Debug)]
pub enum Overlay {
    None,
    Sheet(SheetState),
    Forge,
    Preview(Value),
    Fullscreen,
    /// Compact context window details sheet.
    Context,
}

/// Why a shell menu was opened.
#[derive(Clone, Debug)]
pub enum Menu {
    Attach,
    Thinking,
    Mode,
    Feature(Value),
}

#[derive(Clone, Debug, DefaultNone)]
pub enum ComposerAction {
    PromptSubmitted { agent_id: String },
    ReloadAgent { agent_id: String },
    None,
}

#[derive(Default)]
pub struct PanelState {
    pub ctl: Option<Controller>,
    pub pending_target: Option<ComposerTarget>,
    pub pending_agent: Option<AgentSnapshot>,
    pub pending_commands: Vec<ComposerCommand>,
    pub hidden: bool,
    pub clear_focus_scope: bool,
    pub search_focused: bool,
    pub search_focus_pending: bool,
    pub selected_override: Option<bool>,
    pub hits: HitMap<Item>,
    pub tips: Vec<(Rect, String)>,
    pub hover: Option<Item>,
    pub ohits: HitMap<OItem>,
    pub ohover: Option<OItem>,
    pub tap: crate::common::overlay::TapTarget<Item>,
    pub overlay_tap: crate::common::overlay::TapTarget<OItem>,
    pub sheet_viewport: Rect,
    pub restore_input_focus: bool,
    pub overlay_rect: Option<Rect>,
    pub overlay: Option<Overlay>,
    pub menus: HashMap<u64, Menu>,
    pub confirms: HashMap<u64, Value>,
    pub file_timer: Timer,
    pub forge_timer: Timer,
    pub picker_timer: Timer,
    pub tooltip_timer: Timer,
    pub tooltip: Option<(String, Rect)>,
    pub tooltip_pending: Option<(String, Rect)>,
    pub next_frame: NextFrame,
    pub spin: f64,
    pub input_scroll: f64,
    pub input_manual_scroll: bool,
    pub input_natural: f64,
    pub input_clip: Rect,
    /// The host's stationary viewport (dock `ComposerViewport`), set by the host each draw.
    pub viewport_height: Option<f64>,
    /// This frame's input height bound from the slot capacity (layout.rs draw_composer).
    pub input_cap: Option<f64>,
    pub input_focused: bool,
    pub last_cursor: usize,
    pub density: crate::features::composer::controls_model::Density,
    pub wrapper: Rect,
    pub attach_rect: Rect,
    pub meter_rect: Rect,
    pub meter_hover: bool,
    pub over_meter: bool,
    pub card_hover: bool,
    pub meter_timer: Timer,
    pub seen_revision: u64,
    pub theme_epoch: u64,
    pub applied_placeholder: String,
    pub guard: Option<(String, bool)>,
    pub suggest_scroll: f64,
    pub sheet_scroll: f64,
    pub suggest_scroll_max: f64,
    pub sheet_scroll_max: f64,
    pub touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    pub window: DVec2,
    pub search_text: String,
    /// Shortcut combos shown in tooltips (composer_tooltip.dart resolveShortcutCombo).
    pub mode_shortcut: Option<String>,
    pub interrupt_shortcut: Option<String>,
}

impl PanelState {
    pub fn overlay(&self) -> &Overlay {
        self.overlay.as_ref().unwrap_or(&Overlay::None)
    }

    pub fn modal_open(&self) -> bool {
        !matches!(self.overlay(), Overlay::None)
    }

    pub(super) fn cancel_gestures(&mut self) {
        self.touch_scroll.reset();
        self.tap.cancel();
        self.overlay_tap.cancel();
    }
}

#[derive(Live, LiveHook, Widget)]
pub struct ComposerPanel {
    #[walk]
    pub walk: Walk,
    #[layout]
    pub layout: Layout,
    #[redraw]
    #[live]
    pub draw_bg: DrawColor,
    #[live]
    pub overlay_bg: DrawColor,
    #[live]
    pub painter: Painter,
    #[live]
    pub draw_icon: DrawIcon,
    #[live]
    pub draw_image: DrawComposerImage,
    /// Lays the draft out exactly like the editor to find the caret row (TextInput keeps its
    /// layout private).
    #[live]
    pub caret_text: DrawText,
    #[live]
    pub input: TextInput,
    #[live]
    pub search: TextInput,
    /// Settings' agent profile editor shown as a sheet over the composer (Flutter
    /// `ProfileEditor` from the model sheet's create/edit actions).
    #[live]
    pub profile_editor: WidgetRef,
    #[rust(DrawList2d::new(cx))]
    pub overlay_list: DrawList2d,
    #[rust]
    pub st: PanelState,
}

impl Drop for ComposerPanel {
    fn drop(&mut self) {
        if let Some(ctl) = &self.st.ctl {
            let panel = ctl.panel;
            service::with(|svc| svc.release_panel(panel));
        }
    }
}

fn ctrl_key(ke: &KeyEvent) -> bool {
    ke.modifiers.is_primary() || ke.modifiers.control || ke.modifiers.logo
}

impl Widget for ComposerPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let path = scope.path.clone();
        let uid = self.widget_uid();
        if let Some(ui) = scope.data.get_mut::<UiContext>() {
            let hidden = !ui.activity.visible || !ui.activity.app_active;
            if hidden && !self.st.hidden {
                self.hide(cx);
            }
            self.st.hidden = hidden;
            if hidden && self.profile_editor.as_settings_panel().is_open() {
                self.profile_editor.as_settings_panel().hide(cx, ui);
            }
            if self.st.clear_focus_scope {
                self.st.clear_focus_scope = false;
                ui.push(UiEffect::SetFocusScope { owner: self.input.widget_uid(), scope: None });
                ui.push(UiEffect::SetFocusScope { owner: self.search.widget_uid(), scope: None });
            }
            if self.st.input_focused && (!self.input.area().is_valid(cx) || !cx.has_key_focus(self.input.area())) {
                self.input.release_focus(cx);
                self.st.input_focused = false;
                ui.push(UiEffect::SetFocusScope { owner: self.input.widget_uid(), scope: None });
            }
            if self.st.search_focused && (!self.search.area().is_valid(cx) || !cx.has_key_focus(self.search.area())) {
                self.search.release_focus(cx);
                self.st.search_focused = false;
                ui.push(UiEffect::SetFocusScope { owner: self.search.widget_uid(), scope: None });
            }
        }
        // The profile editor sheet sees every event first (core replies, platform results);
        // while it is open it owns pointer and keys.
        let editor = self.profile_editor.as_settings_panel();
        let editor_was_open = editor.is_open();
        let editor_actions = cx.capture_actions(|cx| self.profile_editor.handle_event(cx, event, scope));
        let editor_closed = editor.editor_closed(&editor_actions);
        cx.extend_actions(editor_actions);
        let editor_open = editor_was_open || editor.is_open();
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return };
        self.sync(cx, ui);
        if self.st.ctl.is_none() {
            return;
        }

        // Core batch, service notices and platform results.
        if !ui.events.is_empty() {
            let out = self.ctl().handle_batch(ui);
            self.apply(cx, ui, &path, uid, out);
        }
        let panel = self.ctl().panel;
        let notices = service::with(|svc| svc.take_notices(panel));
        if !notices.is_empty() {
            let out = self.ctl().handle_notices(ui, notices);
            self.apply(cx, ui, &path, uid, out);
        }
        ui.effects.extend(service::with(|svc| svc.take_effects()));
        if let Event::Actions(actions) = event {
            for action in actions.iter() {
                if let Some(paseo_gui_platform::PlatformAction::Completed { request, result }) =
                    action.downcast_ref::<paseo_gui_platform::PlatformAction>()
                    && self.ctl().platform_requests.contains_key(request)
                {
                    let out = self.ctl().handle_platform(ui, *request, result.clone());
                    self.apply(cx, ui, &path, uid, out);
                }
            }
        }
        self.commands(cx, ui, &path, uid);

        // Timers and animation.
        if self.st.file_timer.is_event(event).is_some() {
            self.ctl().fetch_files(ui);
            self.draw_bg.redraw(cx);
        }
        if self.st.forge_timer.is_event(event).is_some() {
            let out = self.ctl().forge_timer_fired(ui);
            self.apply(cx, ui, &path, uid, out);
        }
        if self.st.picker_timer.is_event(event).is_some() {
            let query = self.st.search_text.clone();
            self.ctl().search_forge_picker(ui, &query);
            self.draw_bg.redraw(cx);
        }
        if self.st.meter_timer.is_event(event).is_some() && !self.st.over_meter && !self.st.card_hover {
            self.st.meter_hover = false;
            self.draw_bg.redraw(cx);
        }
        if self.st.tooltip_timer.is_event(event).is_some() {
            self.st.tooltip = self.st.tooltip_pending.take();
            self.draw_bg.redraw(cx);
        }
        if let Some(frame) = self.st.next_frame.is_event(event).filter(|_| !self.st.hidden) {
            self.st.spin = frame.time;
            self.draw_bg.redraw(cx);
        }

        if let Some(saved_on_host) = editor_closed {
            // Saved profiles show in the model sheet without waiting for the config push.
            if saved_on_host.is_some() {
                self.ctl().reload_daemon_config(ui);
            }
            if !self.st.hidden {
                self.draw_bg.redraw(cx);
            }
        }
        let focus_transition = matches!(event, Event::KeyFocus(_) | Event::KeyFocusLost(_));
        if matches!(event, Event::TextHandle(_) | Event::Pause | Event::AppLostFocus)
            || matches!(event, Event::TouchUpdate(update) if update.cancelled)
        {
            self.st.cancel_gestures();
        }
        if editor_open && !focus_transition {
            return;
        }
        if !self.st.hidden || focus_transition {
            // Dismissal belongs to the top overlay, but its editor must claim
            // pointer hits before the full-window overlay background.
            let modal = self.st.modal_open();
            let overlay_consumed = modal && self.dismiss_overlay_event(cx, event, ui, &path, uid);
            let input_allowed = !modal || matches!(self.st.overlay(), Overlay::Fullscreen)
                || !crate::common::is_user_input(event);

            // Keys on the focused editor (autocomplete, Enter policy, paste, Escape).
            let mut swallow = overlay_consumed;
            if let Event::KeyDown(ke) = event
                && !overlay_consumed
                && input_allowed
                && cx.has_key_focus(self.input.area())
                && self.input.area().is_valid(cx)
                && !self.st.hidden
            {
                swallow = self.key_down(cx, ke, ui, &path, uid);
            }
            if !swallow && input_allowed {
                let actions = cx.capture_actions(|cx| self.input.handle_event(cx, event, &mut Scope::empty()));
                self.input_actions(cx, ui, &path, uid, &actions);
                cx.extend_actions(actions);
            }
            if !overlay_consumed && (matches!(self.st.overlay(), Overlay::Sheet(_) | Overlay::Forge) || focus_transition) {
                let actions = cx.capture_actions(|cx| self.search.handle_event(cx, event, &mut Scope::empty()));
                self.search_actions(cx, ui, &actions);
                cx.extend_actions(actions);
            }
            if self.st.hidden {
                // A focus transition queued before hiding must not restart blinking/IME.
                self.input.release_focus(cx);
                self.search.release_focus(cx);
            }
            if modal && !overlay_consumed && self.st.modal_open() {
                self.overlay_event(cx, event, ui, &path, uid);
            }

            if !modal {
                self.pointer_event(cx, event, ui, &path, uid);
            }
        }

        // Close guard (sends, uploads, the fullscreen editor).
        let dirty = self.ctl().close_guard_dirty();
        let owner = self.ctl().target.tab_key.clone();
        if !owner.is_empty() && self.st.guard.as_ref() != Some(&(owner.clone(), dirty)) {
            self.st.guard = Some((owner.clone(), dirty));
            ui.push(UiEffect::SetCloseGuard { owner, dirty });
        }

        // Anything the service changed since the last draw (other panes, delivery, blobs).
        let revision = service::with(|svc| svc.revision());
        if !self.st.hidden && revision != self.st.seen_revision {
            self.st.seen_revision = revision;
            let mut out = Outputs::default();
            self.ctl().sync_editor(&mut out);
            let pending_inbox = {
                let ctl = self.ctl();
                let (host, ws) = (ctl.target.host_id.clone(), ctl.target.workspace_id.clone());
                service::with(|svc| svc.drafts.inbox_pending(&host, &ws))
            };
            if pending_inbox {
                self.ctl().take_attachments();
            }
            out.redraw = true;
            self.apply(cx, ui, &path, uid, out);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return DrawStep::done() };
        self.sync(cx, ui);
        if self.st.ctl.is_none() {
            return DrawStep::done();
        }
        let window = cx.current_pass_size();
        self.st.window = window;
        let look = Look::new(cx, ui, window);
        self.theme_input(cx, &look);
        self.draw_composer(cx, ui, &look, walk);
        self.draw_overlays(cx, ui, &look);
        let editor_walk = self.profile_editor.walk(cx);
        self.profile_editor.draw_walk(cx, scope, editor_walk)?;
        DrawStep::done()
    }
}

impl ComposerPanel {
    /// A retained composer must release its editors even after their areas expired.
    fn hide(&mut self, cx: &mut Cx) {
        self.st.cancel_gestures();
        self.st.restore_input_focus = false;
        self.input.release_focus(cx);
        self.search.release_focus(cx);
        self.st.input_focused = false;
        self.st.search_focused = false;
        self.st.search_focus_pending = false;
        self.st.clear_focus_scope = true;
        self.st.overlay = None;
        self.st.ohover = None;
        self.st.menus.clear();
        self.st.pending_commands.clear();
        self.st.meter_hover = false;
        self.st.over_meter = false;
        self.st.card_hover = false;
        cx.stop_timer(self.st.picker_timer);
        cx.stop_timer(self.st.file_timer);
        cx.stop_timer(self.st.meter_timer);
        self.st.picker_timer = Timer::empty();
        self.st.file_timer = Timer::empty();
        self.st.meter_timer = Timer::empty();
        self.dismiss_tooltip(cx);
        if let Some(ctl) = self.st.ctl.as_mut() {
            ctl.fullscreen_open = false;
        }
    }

    pub(super) fn ctl(&mut self) -> &mut Controller {
        self.st.ctl.as_mut().expect("composer controller")
    }

    pub(super) fn ctl_ref(&self) -> &Controller {
        self.st.ctl.as_ref().expect("composer controller")
    }

    /// Apply target/agent/commands handed over by the Ref API (which has no UiContext).
    fn sync(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if self.st.ctl.is_none() {
            if self.st.pending_target.is_none() {
                return;
            }
            let panel = service::with(|svc| svc.allocate_panel());
            self.st.ctl = Some(Controller::new(panel));
        }
        let uid = self.widget_uid();
        let path = HeapLiveIdPath::default();
        if let Some(target) = self.st.pending_target.take() {
            let out = self.ctl().set_target(ui, target);
            self.st.guard = None;
            self.apply(cx, ui, &path, uid, out);
        }
        if !self.ctl().initialized {
            let out = self.ctl().ensure_initialized(ui);
            self.apply(cx, ui, &path, uid, out);
        }
        if let Some(agent) = self.st.pending_agent.take() {
            let was_blocked = self.ctl().execution_blocked();
            let was_running = self.ctl().agent.running;
            self.ctl().agent = agent;
            if was_blocked && !self.ctl().execution_blocked() {
                // A ready agent clears the not-ready notice.
                self.ctl().not_ready_notice = false;
            }
            if was_running != self.ctl().agent.running || self.ctl().commands.is_empty() {
                self.ctl().load_commands(ui);
            }
            self.draw_bg.redraw(cx);
        }
        let selected = self.st.selected_override.unwrap_or_else(|| {
            let key = &self.ctl_ref().target.tab_key;
            ui.layout.selected.as_deref() == Some(key.as_str())
                || ui.layout.panes.iter().any(|pane| pane.selected.as_deref() == Some(key.as_str()))
        });
        if selected != self.ctl().selected {
            self.ctl().selected = selected;
            if selected {
                self.ctl().take_attachments();
            }
        }
        for command in std::mem::take(&mut self.st.pending_commands) {
            self.run_command(cx, ui, &path, uid, command);
        }
    }

    pub(super) fn apply(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, out: Outputs) {
        if let Some((text, caret)) = out.set_editor {
            if self.input.text() != text {
                self.input.set_text(cx, &text);
            }
            let caret = caret.min(text.len());
            self.input.set_cursor(cx, Cursor { index: caret, prefer_next_row: false }, false);
            self.st.last_cursor = caret;
            self.st.input_scroll = f64::MAX;
        }
        if let Some(text) = out.insert {
            self.insert_text(cx, ui, &text);
        }
        if out.focus && !self.st.hidden {
            self.input.set_key_focus(cx);
        }
        if let Some(agent_id) = out.prompt_submitted {
            cx.widget_action(uid, path, ComposerAction::PromptSubmitted { agent_id });
        }
        if let Some(agent_id) = out.reload_agent {
            cx.widget_action(uid, path, ComposerAction::ReloadAgent { agent_id });
        }
        if out.start_file_debounce && !self.st.hidden {
            cx.stop_timer(self.st.file_timer);
            // The original debounces the workspace search by 180ms.
            self.st.file_timer = cx.start_timeout(0.18);
        }
        match out.start_forge_debounce {
            Some(true) => {
                cx.stop_timer(self.st.forge_timer);
                self.st.forge_timer = cx.start_timeout(0.3);
            }
            Some(false) => cx.stop_timer(self.st.forge_timer),
            None => {}
        }
        if out.redraw && !self.st.hidden {
            self.draw_bg.redraw(cx);
        }
        ui.effects.extend(service::with(|svc| svc.take_effects()));
    }

    /// Insert at the caret (replacing the selection), as one user edit.
    fn insert_text(&mut self, cx: &mut Cx, ui: &mut UiContext, insert: &str) {
        let text = self.input.text();
        let selection = self.input.selection();
        let start = selection.start().index.min(text.len());
        let end = selection.end().index.min(text.len()).max(start);
        let next = format!("{}{}{}", &text[..start], insert, &text[end..]);
        let caret = start + insert.len();
        self.input.set_text(cx, &next);
        self.input.set_cursor(cx, Cursor { index: caret, prefer_next_row: false }, false);
        if !self.st.hidden {
            self.input.set_key_focus(cx);
        }
        self.st.last_cursor = caret;
        let out = self.ctl().editor_changed(&next, caret);
        let path = HeapLiveIdPath::default();
        let uid = self.widget_uid();
        self.apply(cx, ui, &path, uid, out);
    }

    fn input_actions(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, actions: &ActionsBuf) {
        let input_uid = self.input.widget_uid();
        let mut changed = None;
        for action in actions.filter_widget_actions_cast::<TextInputAction>(input_uid) {
            match action {
                TextInputAction::Changed(text) => changed = Some(text),
                TextInputAction::KeyFocus | TextInputAction::KeyFocusLost => {
                    let focused = !self.st.hidden && self.input.area().is_valid(cx) && cx.has_key_focus(self.input.area());
                    self.st.input_focused = focused;
                    ui.push(UiEffect::SetFocusScope { owner: input_uid, scope: focused.then(|| "message-input".to_owned()) });
                    self.draw_bg.redraw(cx);
                }
                _ => {}
            }
        }
        let cursor = self.input.cursor().index;
        if let Some(text) = changed {
            self.st.input_manual_scroll = false;
            self.st.last_cursor = cursor;
            let out = self.ctl().editor_changed(&text, cursor);
            self.apply(cx, ui, path, uid, out);
        } else if cursor != self.st.last_cursor {
            self.st.input_manual_scroll = false;
            self.st.last_cursor = cursor;
            let out = self.ctl().cursor_moved(cursor);
            self.apply(cx, ui, path, uid, out);
        }
    }

    fn search_actions(&mut self, cx: &mut Cx, ui: &mut UiContext, actions: &ActionsBuf) {
        let uid = self.search.widget_uid();
        for action in actions.filter_widget_actions_cast::<TextInputAction>(uid) {
            match action {
                TextInputAction::Changed(text) => {
                    self.st.cancel_gestures();
                    self.st.search_text = text;
                    self.st.sheet_scroll = 0.0;
                    if matches!(self.st.overlay(), Overlay::Forge) {
                        cx.stop_timer(self.st.picker_timer);
                        self.st.picker_timer = cx.start_timeout(0.25);
                    }
                    self.draw_bg.redraw(cx);
                }
                TextInputAction::Escaped => self.close_overlay(cx, ui),
                TextInputAction::KeyFocus | TextInputAction::KeyFocusLost => {
                    self.st.search_focused = !self.st.hidden && self.search.area().is_valid(cx) && cx.has_key_focus(self.search.area());
                    ui.push(UiEffect::SetFocusScope { owner: uid, scope: self.st.search_focused.then(|| "editable".into()) });
                }
                _ => {}
            }
        }
    }

    fn autocomplete_owns_escape(&self, cx: &Cx, ke: &KeyEvent) -> bool {
        ke.key_code == KeyCode::Escape
            && !ke.modifiers.shift
            && !ke.is_repeat
            && cx.has_key_focus(self.input.area())
            && self.input.area().is_valid(cx)
            && !self.st.hidden
            && !matches!(self.st.overlay(), Overlay::Fullscreen)
            && self.st.ctl.as_ref().is_some_and(|ctl| ctl.autocomplete.visible())
    }

    /// Flutter `_composerKey`. Returns true when the key must not reach the editor.
    fn key_down(&mut self, cx: &mut Cx, ke: &KeyEvent, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid) -> bool {
        use crate::features::composer::autocomplete::{AutocompleteKey, NavKey};
        let fullscreen = matches!(self.st.overlay(), Overlay::Fullscreen);
        if fullscreen && ke.key_code == KeyCode::Escape {
            self.close_overlay(cx, ui);
            return true;
        }
        if self.autocomplete_owns_escape(cx, ke) {
            // Dismiss the suggestions, not the editor's draft. A later edit
            // or caret move reopens autocomplete with the actual editor text.
            self.ctl().autocomplete.update("", 0);
            cx.stop_timer(self.st.file_timer);
            self.draw_bg.redraw(cx);
            return true;
        }
        // Autocomplete navigation first (not over the fullscreen editor).
        if !fullscreen {
            let nav = match ke.key_code {
                KeyCode::ArrowUp => Some(NavKey::Up),
                KeyCode::ArrowDown => Some(NavKey::Down),
                KeyCode::Tab => Some(NavKey::Tab),
                KeyCode::ReturnKey | KeyCode::NumpadEnter => Some(NavKey::Enter),
                _ => None,
            };
            if let Some(nav) = nav.filter(|_| !ke.modifiers.shift) {
                match self.ctl().autocomplete.handle_key(nav, ke.is_repeat) {
                    AutocompleteKey::Ignored => {}
                    AutocompleteKey::Moved | AutocompleteKey::Escape => {
                        self.draw_bg.redraw(cx);
                        return true;
                    }
                    AutocompleteKey::Select(option) => {
                        let out = self.ctl().apply_autocomplete(ui, &option);
                        self.apply(cx, ui, path, uid, out);
                        return true;
                    }
                }
            }
        }
        if ke.key_code == KeyCode::KeyV && ctrl_key(ke) && !ke.modifiers.shift {
            // Clipboard images become image pills; the platform pastes text natively.
            self.ctl().paste(false, true);
            return false;
        }
        if !matches!(ke.key_code, KeyCode::ReturnKey | KeyCode::NumpadEnter) || ke.modifiers.shift {
            return false;
        }
        let look_native = matches!(cx.os_type(), OsType::Ios | OsType::Android(_));
        let ios = matches!(cx.os_type(), OsType::Ios);
        let submit_on_enter = (!look_native && !ui.compact) || ios;
        let busy = self.ctl().sending() || self.ctl().uploading() || !self.ctl().initialized;
        if !submit_on_enter || busy {
            self.newline(cx, ke);
            return true;
        }
        if fullscreen {
            // Send exits the fullscreen editor, then sends.
            if self.ctl().editor_text.trim().is_empty() {
                return true;
            }
            self.close_overlay(cx, ui);
        }
        let running = self.ctl().running();
        let out = if ctrl_key(ke) && running && !ios {
            self.ctl().alternate_send(ui)
        } else {
            self.ctl().submit(ui, None, None)
        };
        self.apply(cx, ui, path, uid, out);
        true
    }

    /// A plain newline through the editor's own edit history (Shift+Enter semantics).
    fn newline(&mut self, cx: &mut Cx, ke: &KeyEvent) {
        let mut modifiers = ke.modifiers;
        modifiers.shift = true;
        let synthetic = Event::KeyDown(KeyEvent { key_code: KeyCode::ReturnKey, is_repeat: false, modifiers, time: ke.time });
        let actions = cx.capture_actions(|cx| self.input.handle_event(cx, &synthetic, &mut Scope::empty()));
        cx.extend_actions(actions);
    }

    /// Shell commands and overlay answers addressed to this tab.
    fn commands(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid) {
        let tab_key = self.ctl_ref().target.tab_key.clone();
        let mut commands: Vec<UiCommand> = Vec::new();
        for command in ui.commands.iter_mut() {
            if command.tab_key != tab_key || command.handled {
                continue;
            }
            if let Some((id, _)) = overlay::result_of(command) {
                if self.st.menus.contains_key(&id) || self.st.confirms.contains_key(&id) {
                    command.handled = true;
                    commands.push(command.clone());
                }
                continue;
            }
            if self.ctl_ref().is_subagent() {
                continue;
            }
            let ours = matches!(command.action.as_str(), "message-input.focus" | "message-input.send" | "message-input.mode-cycle")
                || (command.action == "agent.interrupt" && !self.ctl_ref().is_draft());
            if ours {
                command.handled = true;
                commands.push(command.clone());
            }
        }
        for command in commands {
            if let Some((id, args)) = overlay::result_of(&command) {
                let args = args.clone();
                self.overlay_answer(cx, ui, path, uid, id, &args);
                continue;
            }
            let command = match command.action.as_str() {
                "message-input.focus" => ComposerCommand::Focus,
                "message-input.send" => ComposerCommand::Send,
                "message-input.mode-cycle" => ComposerCommand::CycleMode,
                _ => ComposerCommand::Interrupt,
            };
            self.run_command(cx, ui, path, uid, command);
        }
    }

    pub(super) fn run_command(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, command: ComposerCommand) {
        if self.st.hidden || self.ctl_ref().is_subagent() {
            return;
        }
        let out = match command {
            ComposerCommand::Focus => Outputs { focus: true, ..Default::default() },
            ComposerCommand::Send => self.ctl().submit(ui, None, None),
            ComposerCommand::CycleMode => self.ctl().cycle_mode(ui),
            ComposerCommand::Interrupt => self.ctl().cancel_agent(ui),
        };
        self.apply(cx, ui, path, uid, out);
    }

    fn overlay_answer(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, id: u64, args: &Value) {
        if let Some(message) = self.st.confirms.remove(&id) {
            if get(args, "confirmed") == &Value::Bool(true) {
                self.ctl().check_delivery(&message, true);
            }
            self.draw_bg.redraw(cx);
            return;
        }
        let Some(menu) = self.st.menus.remove(&id) else { return };
        let Some(value) = args.get("value").and_then(Value::as_str).map(str::to_owned) else { return };
        let config = self.ctl().config();
        let model = {
            let ctl = self.ctl_ref();
            (ctl.providers.clone(), ctl.modes(), ctl.features())
        };
        let controls = crate::features::composer::controls_model::ControlsModel {
            providers: &model.0,
            config: &config,
            modes: &model.1,
            features: &model.2,
        };
        let out = match menu {
            Menu::Attach => {
                match value.as_str() {
                    "image" => self.ctl().pick_files(true),
                    "paste" => self.ctl().paste(true, false),
                    "forge" => self.open_forge(cx, ui),
                    "file" => self.ctl().pick_files(false),
                    _ => {}
                }
                Outputs::default()
            }
            Menu::Thinking => {
                let next = controls.with_thinking(&value);
                self.ctl().save_config(ui, next)
            }
            Menu::Mode => {
                let next = controls.with_mode(&value);
                self.ctl().save_config(ui, next)
            }
            Menu::Feature(feature) => {
                let next = controls.with_feature(s(get(&feature, "id")), Value::String(value));
                self.ctl().save_config(ui, next)
            }
        };
        self.apply(cx, ui, path, uid, out);
    }

    pub(super) fn open_menu(&mut self, ui: &mut UiContext, anchor: Rect, title: Option<String>, entries: Vec<MenuEntry>, width: f64, menu: Menu) {
        let id = overlay::open_menu_with(ui, anchor, title, entries, overlay::MenuOptions { width, align_end: false, sheet: true });
        self.st.menus.insert(id, menu);
    }

    /// index.tsx attachment menu: Add image, Paste image (native), Add issue or PR, Upload file.
    pub(super) fn open_attach_menu(&mut self, ui: &mut UiContext, native: bool) {
        self.ctl().ensure_forge(ui);
        let forge_label = if self.ctl_ref().forge == "gitlab" {
            t("composer.attachments.addIssueOrPr_mr")
        } else {
            t("composer.attachments.addIssueOrPr")
        };
        let mut entries = vec![MenuItem::new("image", t("composer.attachments.addImage")).icon("image").entry()];
        if native {
            entries.push(MenuItem::new("paste", t("composer.attachments.pasteImage")).icon("clipboard-paste").entry());
        }
        let forge = MenuItem::new("forge", forge_label);
        let forge = match crate::common::icons::forge_brand_svg(&self.ctl_ref().forge) {
            Some(svg) => forge.svg(svg),
            None => forge.icon("git-pull-request"),
        };
        entries.push(forge.entry());
        entries.push(MenuItem::new("file", t("composer.attachments.addFile")).icon("paperclip").entry());
        let anchor = self.st.attach_rect;
        self.open_menu(ui, anchor, Some(t("composer.input.addAttachment")), entries, 220.0, Menu::Attach);
    }

    pub(super) fn open_forge(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        self.st.cancel_gestures();
        self.st.restore_input_focus = self.st.input_focused;
        self.input.release_focus(cx);
        self.st.input_focused = false;
        ui.push(UiEffect::SetFocusScope { owner: self.input.widget_uid(), scope: None });
        self.st.overlay = Some(Overlay::Forge);
        self.st.search_text.clear();
        self.search.set_text(cx, "");
        self.st.search_focus_pending = true;
        self.st.sheet_scroll = 0.0;
        self.ctl().search_forge_picker(ui, "");
        self.draw_bg.redraw(cx);
    }

    pub(super) fn open_sheet(&mut self, cx: &mut Cx, ui: &mut UiContext, state: SheetState) {
        self.st.cancel_gestures();
        self.st.restore_input_focus = self.st.input_focused;
        self.ctl().refetch_if_loading(ui);
        self.ctl().reload_daemon_config(ui);
        if self.ctl_ref().is_draft() {
            self.ctl().refresh_draft_features(ui, None);
        }
        self.input.release_focus(cx);
        self.st.input_focused = false;
        ui.push(UiEffect::SetFocusScope { owner: self.input.widget_uid(), scope: None });
        self.st.overlay = Some(Overlay::Sheet(state));
        self.st.search_text.clear();
        self.search.set_text(cx, "");
        self.st.search_focus_pending = !ui.compact;
        self.st.sheet_scroll = 0.0;
        self.draw_bg.redraw(cx);
    }

    pub(super) fn open_fullscreen(&mut self, cx: &mut Cx) {
        self.st.cancel_gestures();
        self.st.overlay = Some(Overlay::Fullscreen);
        self.ctl().fullscreen_open = true;
        self.input.set_key_focus(cx);
        self.draw_bg.redraw(cx);
    }

    pub(super) fn close_overlay(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        self.st.cancel_gestures();
        let was_fullscreen = matches!(self.st.overlay(), Overlay::Fullscreen);
        let restore_input = std::mem::take(&mut self.st.restore_input_focus);
        self.search.release_focus(cx);
        self.st.search_focused = false;
        self.st.search_focus_pending = false;
        ui.push(UiEffect::SetFocusScope { owner: self.search.widget_uid(), scope: None });
        self.st.overlay = None;
        self.st.ohover = None;
        cx.stop_timer(self.st.picker_timer);
        self.dismiss_tooltip(cx);
        if let Some(ctl) = self.st.ctl.as_mut() {
            ctl.fullscreen_open = false;
        }
        if (was_fullscreen || restore_input) && !self.st.hidden {
            self.input.set_key_focus(cx);
        }
        ui.push(UiEffect::Redraw);
        self.draw_bg.redraw(cx);
    }

    /// Pointer events on the inline composer.
    fn pointer_event(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid) {
        if let Event::Scroll(scroll) = event {
            self.st.touch_scroll.reset();
            if self.st.input_clip.contains(scroll.abs) && self.st.input_natural > self.st.input_clip.size.y + 0.5 {
                self.st.input_manual_scroll = true;
                self.st.input_scroll = (self.st.input_scroll + scroll.scroll.y)
                    .clamp(0.0, (self.st.input_natural - self.st.input_clip.size.y).max(0.0));
                self.draw_bg.redraw(cx);
            } else if self.suggestions_rect().is_some_and(|r| r.contains(scroll.abs)) {
                self.st.suggest_scroll = (self.st.suggest_scroll - scroll.scroll.y).max(0.0);
                self.draw_bg.redraw(cx);
            }
        }
        if self.popup_event(cx, event, ui, path, uid) {
            return;
        }
        let hit = event.hits_with_capture_overload(cx, self.draw_bg.area(), true);
        if let Some(dy) = self.st.touch_scroll.handle_event(cx, event, &hit) {
            self.st.tap.cancel();
            let origin = self.st.touch_scroll.origin();
            if self.st.input_clip.contains(origin) {
                self.st.input_manual_scroll = true;
                let max = (self.st.input_natural - self.st.input_clip.size.y).max(0.0);
                self.st.touch_scroll.apply_delta(&mut self.st.input_scroll, dy, max);
                self.draw_bg.redraw(cx);
            } else if self.suggestions_rect().is_some_and(|r| r.contains(origin)) {
                self.st.touch_scroll.apply_delta(&mut self.st.suggest_scroll, -dy, self.st.suggest_scroll_max);
                self.draw_bg.redraw(cx);
            } else {
                self.st.touch_scroll.reset();
            }
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let item = self.st.hits.at(e.abs);
                let meter = self.st.meter_rect.contains(e.abs) && !ui.compact;
                if meter && !self.st.meter_hover {
                    // The hover card fetches the agent's usage only while it is open.
                    self.ctl().load_usage(ui);
                }
                self.st.over_meter = meter;
                if meter {
                    self.st.meter_hover = true;
                } else if self.st.meter_hover {
                    // Long enough to move onto the hover card and press Refresh.
                    cx.stop_timer(self.st.meter_timer);
                    self.st.meter_timer = cx.start_timeout(0.25);
                }
                self.set_hover(cx, item, e.abs);
            }
            Hit::FingerHoverOut(_) => {
                self.st.over_meter = false;
                if self.st.meter_hover {
                    cx.stop_timer(self.st.meter_timer);
                    self.st.meter_timer = cx.start_timeout(0.25);
                }
                self.set_hover(cx, None, DVec2::default());
            }
            Hit::FingerDown(e) => {
                self.st.tap.press(self.st.hits.at(e.abs));
            }
            Hit::FingerLongPress(_) => self.st.tap.cancel(),
            Hit::FingerUp(e) => {
                if let Some(item) = self.st.tap.release(self.st.hits.at(e.abs), e.is_over && e.was_tap() && !e.has_long_press_occurred) {
                    self.click(cx, ui, path, uid, item);
                }
            }
            _ => {}
        }
    }

    fn set_hover(&mut self, cx: &mut Cx, item: Option<Item>, abs: DVec2) {
        if item != self.st.hover {
            self.st.hover = item;
            cx.stop_timer(self.st.tooltip_timer);
            self.st.tooltip = None;
            self.st.tooltip_pending = self
                .st
                .tips
                .iter()
                .find(|(rect, _)| rect.contains(abs))
                .map(|(rect, text)| (text.clone(), *rect));
            if self.st.tooltip_pending.is_some() {
                self.st.tooltip_timer = cx.start_timeout(0.5);
            }
            self.draw_bg.redraw(cx);
        } else if self.st.meter_hover {
            self.draw_bg.redraw(cx);
        }
    }

    /// Pressing a control or opening a surface hides its tooltip until the pointer enters
    /// another control (composer_tooltip.dart dismisses on tap).
    fn dismiss_tooltip(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.st.tooltip_timer);
        self.st.tooltip_timer = Timer::empty();
        self.st.tooltip = None;
        self.st.tooltip_pending = None;
        self.draw_bg.redraw(cx);
    }

    fn click(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, item: Item) {
        self.dismiss_tooltip(cx);
        let look_native = matches!(cx.os_type(), OsType::Ios | OsType::Android(_));
        let sending = self.ctl_ref().sending();
        let out = match item {
            Item::Attach => {
                if self.ctl_ref().connected() {
                    self.open_attach_menu(ui, look_native);
                }
                Outputs::default()
            }
            Item::Model | Item::Features => {
                if self.controls_disabled() {
                    return;
                }
                let state = SheetState::new(self.ctl_ref().is_draft());
                self.open_sheet(cx, ui, state);
                Outputs::default()
            }
            Item::Thinking | Item::Mode | Item::Feature(_) => {
                if self.controls_disabled() {
                    return;
                }
                self.open_control_menu(ui, &item);
                Outputs::default()
            }
            Item::Meter => {
                if ui.compact {
                    self.ctl().load_usage(ui);
                    self.st.overlay = Some(Overlay::Context);
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::Send => {
                if self.ctl_ref().running() && (sending || self.ctl_ref().uploading()) {
                    self.ctl().cancel_agent(ui)
                } else {
                    self.ctl().submit(ui, None, None)
                }
            }
            Item::Stop => self.ctl().cancel_agent(ui),
            Item::Expand => {
                self.open_fullscreen(cx);
                Outputs::default()
            }
            Item::Image(index) => {
                if !sending {
                    let images = maps(get(&self.ctl_ref().draft(), "images"));
                    if let Some(image) = images.get(index) {
                        self.st.overlay = Some(Overlay::Preview(image.clone()));
                    }
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::ImageRemove(index) => {
                if !sending {
                    self.ctl().remove_image(index);
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::Attachment(index) => {
                if !sending {
                    let attachments = maps(get(&self.ctl_ref().draft(), "attachments"));
                    if let Some(attachment) = attachments.get(index)
                        && crate::features::composer::attachments::forge_labels(attachment).is_some()
                    {
                        let url = owned(get(attachment, "url"));
                        self.ctl().open_url(&url);
                    }
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::AttachmentRemove(index) => {
                if !sending {
                    self.ctl().remove_attachment(index);
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::QueueEdit(id) => match self.queue_item(&id) {
                Some(message) => self.ctl().edit_queued(&message),
                None => Outputs::default(),
            },
            Item::QueueSend(id) => match self.queue_item(&id) {
                Some(message) => self.ctl().queue_now(ui, &message),
                None => Outputs::default(),
            },
            Item::QueueRemove(id) => {
                if let Some(message) = self.queue_item(&id)
                    && self.ctl().remove_queued(&message)
                {
                    self.confirm_stop_waiting(ui, message);
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::Check(id) => {
                if let Some(message) = self.delivery_message(&id) {
                    self.ctl().check_delivery(&message, false);
                }
                Outputs { redraw: true, ..Default::default() }
            }
            Item::Retry(id) => match self.delivery_message(&id) {
                Some(message) => {
                    let queued = self.queue_item(&id).is_some();
                    if queued {
                        self.ctl().queue_now(ui, &message)
                    } else {
                        self.ctl().submit(ui, Some(message), None)
                    }
                }
                None => Outputs::default(),
            },
            Item::StopWaiting(id) => {
                if let Some(message) = self.delivery_message(&id) {
                    self.confirm_stop_waiting(ui, message);
                }
                Outputs::default()
            }
            Item::Restore => self.ctl().restore_failed(),
            Item::RetryFailed => {
                let failed = get(&self.ctl_ref().draft(), "failedMessage").clone();
                self.ctl().submit(ui, Some(failed), None)
            }
            Item::Discard => {
                self.ctl().discard_failed();
                Outputs { redraw: true, ..Default::default() }
            }
            Item::Suggest(index) => match self.ctl_ref().autocomplete.options.get(index).cloned() {
                Some(option) => self.ctl().apply_autocomplete(ui, &option),
                None => Outputs::default(),
            },
            Item::DismissError => {
                self.ctl().error = None;
                Outputs { redraw: true, ..Default::default() }
            }
        };
        self.apply(cx, ui, path, uid, out);
    }

    /// Flutter `_checkDelivery(dismiss: true)` confirmation.
    fn confirm_stop_waiting(&mut self, ui: &mut UiContext, message: Value) {
        let remote = !self.ctl_ref().is_draft() && self.ctl_ref().supports("agentMessageReceipts");
        let id = overlay::confirm(
            ui,
            &t("composer.delivery.stopWaitingTitle"),
            &t(if remote { "composer.delivery.stopWaitingMessage" } else { "composer.delivery.stopWaitingLegacyMessage" }),
            &t("composer.delivery.stopWaiting"),
            false,
        );
        self.st.confirms.insert(id, message);
    }

    pub(super) fn controls_disabled(&self) -> bool {
        let ctl = self.ctl_ref();
        !ctl.initialized || (ctl.is_draft() && ctl.sending()) || ctl.execution_blocked()
    }

    fn queue_item(&self, id: &str) -> Option<Value> {
        maps(get(&self.ctl_ref().draft(), "queue")).into_iter().find(|q| s(get(q, "messageId")) == id)
    }

    fn delivery_message(&self, id: &str) -> Option<Value> {
        let draft = self.ctl_ref().draft();
        let pending = get(&draft, "pending");
        if s(get(pending, "messageId")) == id {
            return Some(pending.clone());
        }
        self.queue_item(id)
    }

    /// The thinking / mode / select-feature popovers (desktop toolbar).
    fn open_control_menu(&mut self, ui: &mut UiContext, item: &Item) {
        use crate::features::composer::controls_model::*;
        let config = self.ctl().config();
        let (providers, modes, features) = {
            let ctl = self.ctl_ref();
            (ctl.providers.clone(), ctl.modes(), ctl.features())
        };
        let model = ControlsModel { providers: &providers, config: &config, modes: &modes, features: &features };
        let anchor = self.st.hits.rect_of(item).unwrap_or_default();
        match item {
            Item::Thinking => {
                let options = model.thinking_options();
                let selected = s_or(get(&config, "thinkingOptionId"), options.first().map(|o| s(get(o, "id"))).unwrap_or("")).to_owned();
                let extra = t("agentControls.thinking.extraHigh");
                let entries = options
                    .iter()
                    .map(|o| {
                        MenuItem::new(owned(get(o, "id")), format_thinking_option_label(o, &extra))
                            .icon("brain")
                            .checked(s(get(o, "id")) == selected)
                            .entry()
                    })
                    .collect();
                self.open_menu(ui, anchor, Some(t("agentControls.thinking.title")), entries, 200.0, Menu::Thinking);
            }
            Item::Mode => {
                let selected = model.selected_mode().map(|m| owned(get(m, "id"))).unwrap_or_default();
                let entries = modes
                    .iter()
                    .map(|m| {
                        let mut item = MenuItem::new(owned(get(m, "id")), format_agent_mode_label(m)).checked(s(get(m, "id")) == selected);
                        if let Some(icon) = mode_option_icon(Some(m)) {
                            item = item.icon(icon);
                        }
                        item.entry()
                    })
                    .collect();
                self.open_menu(ui, anchor, Some(t("agentControls.mode.title")), entries, 260.0, Menu::Mode);
            }
            Item::Feature(index) => {
                let Some(feature) = features.get(*index).cloned() else { return };
                let value = model.feature_value(&feature);
                if s(get(&feature, "type")) == "toggle" {
                    let next = model.with_feature(s(get(&feature, "id")), Value::Bool(value != Value::Bool(true)));
                    let out = self.ctl().save_config(ui, next);
                    let _ = out;
                    return;
                }
                let entries = maps(get(&feature, "options"))
                    .iter()
                    .map(|o| {
                        MenuItem::new(owned(get(o, "id")), s_or(get(o, "label"), s(get(o, "id"))).to_owned())
                            .checked(get(o, "id") == &value)
                            .entry()
                    })
                    .collect();
                let label = s_or(get(&feature, "label"), s(get(&feature, "id"))).to_owned();
                self.open_menu(ui, anchor, Some(label), entries, 220.0, Menu::Feature(feature));
            }
            _ => {}
        }
    }

    /// Apply the runtime palette/typography and placeholder to the editors once per change.
    fn theme_input(&mut self, cx: &mut Cx, look: &Look) {
        let placeholder = if !look.native && !look.compact {
            t("composer.placeholders.desktop")
        } else {
            t("composer.placeholders.mobile")
        };
        if placeholder != self.st.applied_placeholder {
            self.st.applied_placeholder = placeholder.clone();
            self.input.set_empty_text(cx, placeholder);
        }
        let read_only = self.ctl_ref().is_draft() && self.ctl_ref().sending();
        if self.input.is_read_only() != read_only {
            self.input.set_is_read_only(cx, read_only);
        }
        let epoch = crate::theme::epoch();
        if self.st.theme_epoch == epoch {
            return;
        }
        self.st.theme_epoch = epoch;
        let p = look.p;
        let content = crate::theme::Typography::pt(look.content()) as f64;
        let body = crate::theme::Typography::pt(look.body()) as f64;
        let selection = crate::theme::with_alpha(p.accent, 0.35);
        for (input, size) in [(&mut self.input, content), (&mut self.search, body)] {
            input.apply_over(
                cx,
                live! {
                    draw_text: {
                        color: (p.foreground), color_hover: (p.foreground), color_focus: (p.foreground),
                        color_down: (p.foreground), color_disabled: (crate::theme::fade(p.foreground, 0.5)),
                        color_empty: (p.surface4), color_empty_hover: (p.surface4), color_empty_focus: (p.surface4),
                        text_style: { font_size: (size) }
                    }
                    draw_cursor: { color: (p.foreground) }
                },
            );
            input.apply_over(cx, &crate::theme::text_selection(selection));
        }
    }
}

impl ComposerPanelRef {
    pub fn set_visible(&self, cx: &mut Cx, visible: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            if !visible && !inner.st.hidden {
                inner.hide(cx);
            }
            inner.st.hidden = !visible;
        }
    }

    /// The stationary viewport the host shares between its content and this composer
    /// (dock `ComposerViewport`), with keyboard coverage already removed. Set before drawing.
    pub fn set_viewport_height(&self, height: f64) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.st.viewport_height = Some(height);
        }
    }

    /// Embedded sheets own input above the timeline, including their dismissal event.
    pub fn captures_input(&self) -> bool {
        self.borrow().is_some_and(|inner| !inner.st.hidden && (inner.st.modal_open() || inner.profile_editor.as_settings_panel().is_open()))
    }

    /// Whether files dropped on the host's `FileDropZone` can be attached now
    /// (composer/index.tsx `useFileDrop`: a mounted composer that is not submitting or
    /// uploading). A sheet over the composer owns the pointer instead.
    pub fn accepts_file_drop(&self) -> bool {
        self.borrow().is_some_and(|inner| {
            !inner.st.hidden && !inner.st.modal_open() && inner.st.ctl.as_ref().is_some_and(Controller::accepts_file_drop)
        })
    }

    /// Files dropped on the host's `FileDropZone` (useFileDrop `onFiles`/`onGenericFiles`).
    pub fn drop_files(&self, cx: &mut Cx, paths: Vec<std::path::PathBuf>) {
        if let Some(mut inner) = self.borrow_mut()
            && inner.st.ctl.is_some()
        {
            inner.ctl().drop_paths(paths);
            inner.draw_bg.redraw(cx);
        }
    }

    /// Keyboard-only ownership; autocomplete must not become pointer-modal.
    /// Parents snapshot this before dispatch, since Escape hides suggestions.
    pub fn consumes_escape(&self, cx: &Cx, event: &Event) -> bool {
        let Event::KeyDown(ke) = event else { return false };
        self.borrow().is_some_and(|inner| inner.autocomplete_owns_escape(cx, ke))
    }

    pub fn set_target(&self, cx: &mut Cx, target: ComposerTarget) {
        if let Some(mut inner) = self.borrow_mut()
            && inner.st.ctl.as_ref().map(|c| &c.target) != Some(&target)
        {
            inner.st.pending_target = Some(target);
            inner.draw_bg.redraw(cx);
        }
    }

    pub fn set_agent(&self, cx: &mut Cx, agent: AgentSnapshot) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.st.pending_agent = Some(agent);
            inner.draw_bg.redraw(cx);
        }
    }

    /// Shell commands forwarded by ChatPanel. Commands that also arrive through
    /// `UiContext.commands` are taken from there and marked handled, so they never run twice.
    pub fn command(&self, cx: &mut Cx, command: ComposerCommand) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.st.pending_commands.push(command);
            inner.draw_bg.redraw(cx);
        }
    }

    pub fn set_selected(&self, cx: &mut Cx, selected: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.st.selected_override = Some(selected);
            inner.draw_bg.redraw(cx);
        }
    }

    pub fn prompt_submitted(&self, actions: &Actions) -> Option<String> {
        let inner = self.borrow()?;
        match actions.find_widget_action(inner.widget_uid()).cast_ref::<ComposerAction>() {
            ComposerAction::PromptSubmitted { agent_id } => Some(agent_id.clone()),
            _ => None,
        }
    }

    pub fn reload_agent(&self, actions: &Actions) -> Option<String> {
        let inner = self.borrow()?;
        match actions.find_widget_action(inner.widget_uid()).cast_ref::<ComposerAction>() {
            ComposerAction::ReloadAgent { agent_id } => Some(agent_id.clone()),
            _ => None,
        }
    }
}
