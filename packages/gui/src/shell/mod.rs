//! Port of `lib/ui/shell.dart` (`PaseoShell`): one shell per window owns that window's layout
//! (tabs, panes, Explorer, sidebar), mounts feature roots per tab, routes commands and
//! shortcuts, and draws the chrome (header, tab strips, sidebar, overlays).

pub mod activity;
pub mod chrome;
pub mod commands;
pub mod hosts;
pub mod layout;
pub mod overlays;
pub mod panels;
pub mod platform;
pub mod sheets;
pub mod shortcuts;
pub mod sidebar;
pub mod ui;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use makepad_widgets::*;
use makepad_widgets::event::TouchState;
use paseo_gui_core::{CoreError, CoreEvent, RequestId};
use serde_json::Value;

use crate::common::overlay::{self as overlay_api, OVERLAY_ACTION, OverlayRequest, SHELL_OWNER};
use crate::common::paint::{HitMap, Painter};
use crate::common::{
    MAIN_WINDOW_ID, PanelActivity, Route, UiCommand, UiContext, json as j, set_compact_layout, tr,
};
use crate::theme;

use layout::{CompactDestination, Layout};
use panels::PanelTemplates;
use ui::Pointer;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::paint::PaseoPainter;
    use crate::common::widgets::*;
    use crate::shell::panels::*;
    use paseo_gui_platform::qr::QrScanView;

    pub PaseoShell = {{PaseoShell}} {
        width: Fill, height: Fill,
        painter: <PaseoPainter> {}
        overlay_painter: <PaseoPainter> {}
        draw_icon: { fn get_color(self) -> vec4 { return self.color; } }
        overlay_icon: { fn get_color(self) -> vec4 { return self.color; } }
        templates: <PanelTemplates> {}
        text_field: <PTextField> {}
        search_field: <PTextField> { bare: true }
        qr_scan: <QrScanView> {}
    }

    pub PaseoWindow = <Window> {
        pass: { clear_color: (PASEO_DARK_SURFACE1) }
        window: { title: "Passession", inner_size: vec2(1280, 820) }
        caption_bar = { caption_label = { label = { text: "Passession" } } }
        window_menu = {
            main = Main { items: [app, file] }
            app = Sub { name: "Passession", items: [paseo_quit] }
            paseo_quit = Item { name: "Quit Passession", shift: false, key: KeyQ, enabled: true }
            file = Sub { name: "File", items: [new_window] }
            new_window = Item { name: "New Window", shift: true, key: KeyN, enabled: true }
        }
        // Native surface resize and shell safe insets own keyboard avoidance.
        // Window's default KeyboardView would pan the already-adjusted UI again.
        body = <View> {
            width: Fill, height: Fill,
            shell = <PaseoShell> {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Process-wide shell signals read by `App`.

/// A new desktop window (`DesktopHost.openNew`), optionally opening a route in it.
#[derive(Clone, Debug, Default)]
pub struct WindowRequest {
    pub route: Option<paseo_gui_platform::PlatformRoute>,
}

/// Lifetime of one desktop window (Flutter `_logicalOpen` + visibility).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowState {
    Visible,
    /// Hidden to the tray; still logically open (activation shows it again).
    Tray,
    /// The first window after the user closed it while others stayed open: hidden, not
    /// destroyed (it carries the daemon and tray ownership), reused by the next New Window.
    Dormant,
}

thread_local! {
    static WINDOW_REQUESTS: RefCell<Vec<WindowRequest>> = const { RefCell::new(Vec::new()) };
    static QUIT: Cell<bool> = const { Cell::new(false) };
    static STARTUP_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    static WINDOW_STATES: RefCell<Vec<(String, WindowState)>> = const { RefCell::new(Vec::new()) };
}

pub fn request_window(request: WindowRequest) {
    WINDOW_REQUESTS.with(|r| r.borrow_mut().push(request));
    SignalToUI::set_ui_signal();
}

pub fn take_window_requests() -> Vec<WindowRequest> {
    WINDOW_REQUESTS.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

pub(crate) fn request_quit_now() {
    QUIT.with(|q| q.set(true));
}

pub fn take_quit() -> bool {
    QUIT.with(|q| q.replace(false))
}

pub fn set_startup_error(error: Option<String>) {
    STARTUP_ERROR.with(|e| *e.borrow_mut() = error);
}

pub(crate) fn startup_error() -> Option<String> {
    STARTUP_ERROR.with(|e| e.borrow().clone())
}

/// App: a window exists (visible) until `unregister_window`.
pub fn register_window(id: &str) {
    WINDOW_STATES.with(|states| {
        let mut states = states.borrow_mut();
        match states.iter_mut().find(|(w, _)| w == id) {
            Some(entry) => entry.1 = WindowState::Visible,
            None => states.push((id.to_string(), WindowState::Visible)),
        }
    });
}

pub fn unregister_window(id: &str) {
    WINDOW_STATES.with(|states| states.borrow_mut().retain(|(w, _)| w != id));
}

pub fn window_state(id: &str) -> Option<WindowState> {
    WINDOW_STATES.with(|states| states.borrow().iter().find(|(w, _)| w == id).map(|(_, s)| *s))
}

pub fn window_states() -> Vec<(String, WindowState)> {
    WINDOW_STATES.with(|states| states.borrow().clone())
}

pub(crate) fn set_window_state(id: &str, state: WindowState) {
    WINDOW_STATES.with(|states| {
        if let Some(entry) = states.borrow_mut().iter_mut().find(|(w, _)| w == id) {
            entry.1 = state;
        }
    });
}

/// Logically open windows (visible or hidden to the tray): close-to-tray / quit decisions and
/// quit votes.
pub(crate) fn open_windows() -> usize {
    WINDOW_STATES.with(|states| states.borrow().iter().filter(|(_, s)| *s != WindowState::Dormant).count())
}

/// The platform hid (`true`) or showed (`false`) every logically open window.
pub(crate) fn set_tray_hidden(hidden: bool) {
    let (from, to) = if hidden { (WindowState::Visible, WindowState::Tray) } else { (WindowState::Tray, WindowState::Visible) };
    WINDOW_STATES.with(|states| {
        for entry in states.borrow_mut().iter_mut().filter(|(_, s)| *s == from) {
            entry.1 = to;
        }
    });
}

// ---------------------------------------------------------------------------------------------

/// Desktop pages shown beside the sidebar (`ShellPages`) or pushed full screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Page {
    /// `SettingsScreen` (always full window, a pushed route in Flutter).
    Settings { host_id: Option<String>, page: String },
    Schedules { host_id: String },
    /// `/sessions` history (`showSessionHistory`).
    Sessions { host_id: String, all_hosts: bool, cwd: String, project_key: String },
}

/// What a core response we issued means to the shell.
#[derive(Clone, Debug)]
pub enum Pending {
    /// `openAgent`: `fetch_agent_request`, then the tab opens (with a submission error).
    OpenAgent { host_id: String, error: Option<String>, placement: crate::common::TabPlacement },
    /// `navigationRequest` whose success refreshes the host; failures toast.
    Refresh { host_id: String },
    /// `archive_workspace_request`: close that workspace's tabs, refresh.
    ArchiveWorkspace { host_id: String, workspace_id: String },
    /// `archive_agent_request`: mark History archived only after the daemon confirms.
    ArchiveSession { host_id: String, agent_id: String },
    /// `project.remove.request`: close tabs of the removed workspaces.
    RemoveProject { host_id: String },
    /// `refresh_agent_request`, then `conversation.sync` to the tab (`_reloadAgent`).
    ReloadAgent { tab_key: String },
    /// Only failures toast.
    Plain,
    /// A sheet's (or the sessions page's) request; routed by tag.
    Sheet { sheet: u64, tag: String },
    /// `workspace.recovery.inspect.request` before the restore confirmation.
    RecoverInspect { host_id: String, workspace_id: String },
    /// `workspace.label.list.request` for the sidebar display menu catalog.
    LabelCatalog,
    /// Silent (child tab labels).
    Silent,
    /// `AppPreferenceSettings.save`: a failed durable write of `key` restores `previous`
    /// (`None`: the key was absent) and toasts the error.
    SavePreference { key: String, previous: Option<Value> },
    /// `list_terminals_request` deciding whether closing `key` needs the terminal confirmation.
    TerminalCloseCheck { key: String, host_id: String, terminal_id: String },
    /// `workspace.terminal.new`: `create_terminal_request`, then the terminal tab opens.
    CreateTerminal { host_id: String, workspace_id: String, cwd: String, placement: crate::common::TabPlacement },
}

#[derive(Default)]
pub struct Data {
    pub window_id: String,
    pub layout: Layout,
    pub restored: bool,
    pub save_timer: Timer,
    pub compact: bool,
    pub size: DVec2,
    pub page: Option<Page>,
    pub page_serial: u64,
    /// Mounted roots by tab key (Flutter `_activated` + retained panels).
    pub panels: HashMap<String, panels::Panel>,
    pub page_root: Option<(Page, WidgetRef)>,
    pub command_center: Option<WidgetRef>,
    pub command_center_open: bool,
    pub header_git: Option<WidgetRef>,
    pub header_open_in: Option<WidgetRef>,
    /// Dirty close guards by owner (`SetCloseGuard`; modified tab dot).
    pub dirty: HashSet<String>,
    pub focus_scope: shortcuts::FocusScope,
    pub pending: HashMap<RequestId, Pending>,
    pub commands: Vec<UiCommand>,
    pub router: shortcuts::Router,
    pub hits: HitMap<String>,
    pub tooltips: HashMap<String, String>,
    pub pointer: Pointer,
    pub bg_area: Area,
    pub overlays: overlays::Overlays,
    pub sidebar: sidebar::SidebarState,
    pub chrome: chrome::ChromeState,
    pub activity: activity::Activity,
    pub os_window: Option<WindowId>,
    pub geometry_applied: bool,
    pub closing: Option<commands::Closing>,
    pub quit_token: Option<u64>,
    pub close_request: Option<paseo_gui_platform::PlatformRequestId>,
    /// Host removals waiting for their push revocation (request, host, 2.5 s bound).
    pub revoking: Vec<(paseo_gui_platform::PlatformRequestId, String, Timer)>,
    /// A route this window opens once its host is known (New Window with a route).
    pub pending_route: Option<paseo_gui_platform::PlatformRoute>,
    /// A close of this window was requested programmatically; runs the same guarded flow as
    /// the OS close button on the next event.
    pub close_requested: bool,
    pub search: Option<WidgetRef>,
    /// Shell-owned menus/dialogs awaiting an answer, by overlay id.
    pub menus: HashMap<u64, commands::ShellOverlay>,
    /// The sessions page (`/sessions`) while shown.
    pub history: Option<sheets::history::HistoryPage>,
    /// Merged label catalog for the display menu (`workspace.label.list`), by label key.
    pub label_catalog: Vec<(String, String, String)>,
    /// The open display menu, rebuilt when preferences change.
    pub display_menu: Option<u64>,
    /// Child agent tabs this window marked open (`paseo.open-agent-tab.<clientId>`).
    pub marked_child_tabs: HashSet<String>,
    /// Close allowed for the OS window after guards ran (next close request passes).
    pub close_approved: bool,
    /// Window lost focus or the app went to the background (Flutter not `resumed`).
    pub app_inactive: bool,
    /// A notification asked to open this agent (server, agent id) showing what it waits on.
    pub reveal_agent: Option<(String, String)>,
    /// The agent tab whose chat root reveals its pending permission/question on next draw.
    pub reveal_attention: Option<String>,
}

#[derive(Live, LiveHook, Widget)]
pub struct PaseoShell {
    #[redraw]
    #[live]
    painter: Painter,
    #[live]
    overlay_painter: Painter,
    #[live]
    draw_icon: DrawIcon,
    #[live]
    overlay_icon: DrawIcon,
    #[walk]
    walk: Walk,
    #[live]
    templates: PanelTemplates,
    #[live]
    text_field: Option<LivePtr>,
    #[live]
    search_field: Option<LivePtr>,
    #[live]
    qr_scan: Option<LivePtr>,
    #[rust(DrawList2d::new(cx))]
    overlay_list: DrawList2d,
    #[rust]
    data: Data,
}


impl PaseoShell {
    fn ui<'a>(scope: &'a mut Scope) -> Option<&'a mut UiContext> {
        scope.data.get_mut::<UiContext>()
    }

    fn is_main(&self) -> bool {
        self.data.window_id == MAIN_WINDOW_ID || self.data.window_id.is_empty()
    }

    /// First state: restore this window's layout (Flutter `_restoreLayout`).
    fn restore(&mut self, ui: &UiContext) {
        if self.data.restored || !j::boolean(&ui.state, "initialized", false) {
            return;
        }
        self.data.restored = true;
        let id = if self.data.window_id.is_empty() { MAIN_WINDOW_ID } else { self.data.window_id.as_str() };
        let windows = j::object(&ui.state, "windowLayouts");
        let saved = match windows.get(id) {
            Some(layout @ Value::Object(_)) => layout.clone(),
            _ if id == MAIN_WINDOW_ID => j::object(&ui.state, "layout").clone(),
            _ => Value::Null,
        };
        if saved.is_object() {
            self.data.layout = Layout::restore(&saved);
        }
        self.data.layout.dirty = false;
    }

    fn schedule_save(&mut self, cx: &mut Cx) {
        if !self.data.layout.dirty || !self.data.restored {
            return;
        }
        self.data.layout.dirty = false;
        cx.stop_timer(self.data.save_timer);
        self.data.save_timer = cx.start_timeout(0.35);
    }

    fn save_layout(&self, ui: &UiContext) {
        let key = if self.is_main() { "layout".to_string() } else { format!("layout:{}", self.data.window_id) };
        ui.core.save_state(&key, self.data.layout.to_json());
    }

    /// Shell-level reactions to the core batch (identity remaps, responses).
    fn handle_core_batch(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let events = std::mem::take(&mut ui.events);
        for event in &events {
            match event {
                CoreEvent::State { .. } => {
                    self.restore(ui);
                    self.redraw(cx);
                }
                CoreEvent::HostEvent { event, .. } => {
                    let kind = j::text(event, "type");
                    if kind == "host.identity_changed" || kind == "host.merged" {
                        let payload = j::object(event, "payload");
                        let previous = j::text(payload, "previousHostId");
                        let next = j::text(payload, "hostId");
                        if !previous.is_empty() && !next.is_empty() {
                            self.data.layout.remap_server(previous, next);
                            self.redraw(cx);
                        }
                    }
                    if kind == "agent_update" || kind == "workspace_update" {
                        self.redraw(cx);
                    }
                }
                CoreEvent::Response { request_id, result, .. } => {
                    if let Some(pending) = self.data.pending.remove(request_id) {
                        commands::on_response(self, cx, ui, request_id, pending, result);
                    }
                }
                CoreEvent::Connected { .. } | CoreEvent::Disconnected { .. } => self.redraw(cx),
                CoreEvent::Error { error, .. } => {
                    if error.code != "INVALID_STATE_KEY" {
                        overlays::toast(&mut self.data.overlays, cx, crate::common::error_message(error), true);
                    }
                }
            }
        }
        ui.events = events;
    }

    /// Applies effects pushed by feature roots, hooks and the shell itself, in order.
    fn apply_effects(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let mut guard = 0;
        while !ui.effects.is_empty() && guard < 16 {
            guard += 1;
            let effects = std::mem::take(&mut ui.effects);
            for effect in effects {
                commands::apply_effect(self, cx, ui, effect);
            }
        }
        for request in overlay_api::take_requests(&self.data.window_id) {
            self.open_overlay(cx, request);
        }
        if let Some(tip) = overlay_api::take_tooltip(&self.data.window_id) {
            let tip = if self.data.overlays.is_modal() { None } else { tip };
            overlays::show_tooltip(&mut self.data.overlays, cx, tip);
        }
    }

    fn open_overlay(&mut self, cx: &mut Cx, request: OverlayRequest) {
        let ptr = self.text_field;
        self.data.overlays.open(cx, request, ptr);
        self.redraw(cx);
    }

    /// Answers an overlay back to its owner (feature tab, page, or the shell itself).
    pub(crate) fn overlay_answer(&mut self, cx: &mut Cx, ui: &mut UiContext, owner: &str, id: u64, args: Value) {
        if owner == SHELL_OWNER {
            commands::shell_overlay_result(self, cx, ui, id, &args);
            return;
        }
        let mut args = args;
        if let Value::Object(map) = &mut args {
            map.insert("id".into(), Value::from(id));
        }
        if owner.starts_with(overlay_api::WIDGET_OWNER_PREFIX) {
            overlay_api::deliver_widget_answer(owner, id, args);
            return;
        }
        self.queue_command(owner.to_string(), OVERLAY_ACTION.to_string(), args);
    }

    /// Commands go to one tab root on the next signal (Flutter `PaseoCommands.invoke`).
    pub(crate) fn queue_command(&mut self, tab_key: String, action: String, args: Value) {
        self.data.commands.push(UiCommand { tab_key, action, args, handled: false });
        SignalToUI::set_ui_signal();
    }

    /// Dispatches `event` to every mounted root with its own route/target/activity.
    fn dispatch_panels(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope, window_modal: bool) {
        let requires_visibility = crate::common::is_user_input(event);
        let commands = if matches!(event, Event::Signal) { std::mem::take(&mut self.data.commands) } else { Vec::new() };
        let mut unhandled: Vec<UiCommand> = Vec::new();
        let mut keys: Vec<String> = self.data.panels.keys().cloned().collect();
        let command_center_input = requires_visibility && self.data.command_center_open;
        let explorer_input = requires_visibility && !command_center_input
            && self.data.compact && self.data.chrome.explorer_position > 0.0;
        if explorer_input {
            let selected = chrome::explorer_keys(self).1;
            keys.retain(|key| selected.as_ref() == Some(key));
        }
        let page_accepts_input = !command_center_input && !explorer_input
            && (!window_modal || overlay_api::window_modal_accepts_tab(&self.data.window_id, overlay_api::PAGE_OWNER));
        if window_modal {
            // Snapshot before dispatch: closing a sheet must not pass its
            // dismissal input through to another pane in the same event.
            keys.retain(|key| overlay_api::window_modal_accepts_tab(&self.data.window_id, key));
        }
        let visible = chrome::visible_tabs(self);
        let focused = chrome::focused_tab(self);
        let app_active = !self.data.app_inactive;
        let compact = self.data.compact;
        let window_id = self.data.window_id.clone();
        let snapshot = std::rc::Rc::new(self.data.layout.snapshot());
        let mut routed: HashSet<String> = HashSet::new();
        for key in keys {
            let Some(panel) = self.data.panels.get(&key) else { continue };
            let is_visible = visible.contains(&key);
            if command_center_input || (requires_visibility && !is_visible) {
                continue;
            }
            let widget = panel.widget.clone();
            let target = panel.target.clone();
            {
                let Some(ui) = Self::ui(scope) else { return };
                ui.route = Route::from_target(&target);
                ui.target = Some(target);
                ui.tab_key = Some(key.clone());
                ui.activity = PanelActivity { visible: is_visible, focused: is_visible && focused.as_deref() == Some(key.as_str()), app_active };
                ui.compact = compact;
                ui.window_id = window_id.clone();
                ui.layout = snapshot.clone();
                ui.commands = commands.iter().filter(|c| c.tab_key == key).cloned().collect();
                if !ui.commands.is_empty() {
                    routed.insert(key.clone());
                }
            }
            widget.handle_event(cx, event, scope);
            if let Some(ui) = Self::ui(scope) {
                unhandled.extend(ui.commands.drain(..).filter(|c| !c.handled));
                ui.target = None;
                ui.tab_key = None;
                ui.activity = PanelActivity::default();
            }
        }
        // Pages and the command center (no tab key; owner "page").
        if page_accepts_input && !(requires_visibility && self.data.command_center_open)
            && let Some((page, widget)) = self.data.page_root.clone()
        {
            if let Some(ui) = Self::ui(scope) {
                ui.route = commands::page_route(&page);
                ui.commands = commands.iter().filter(|c| c.tab_key == overlay_api::PAGE_OWNER).cloned().collect();
            }
            widget.handle_event(cx, event, scope);
            if let Some(ui) = Self::ui(scope) {
                ui.commands.clear();
            }
            routed.insert(overlay_api::PAGE_OWNER.to_string());
        }
        if (!requires_visibility || self.data.command_center_open)
            && (!window_modal || command_center_input)
            && let Some(widget) = self.data.command_center.clone()
        {
            if let Some(ui) = Self::ui(scope) {
                ui.route = Route::CommandCenter;
                ui.layout = snapshot.clone();
                ui.compact = compact;
            }
            widget.handle_event(cx, event, scope);
        }
        for command in commands {
            if !routed.contains(&command.tab_key) {
                unhandled.push(command);
            }
        }
        if let Some(ui) = Self::ui(scope) {
            for command in unhandled {
                if command.action == OVERLAY_ACTION {
                    continue;
                }
                commands::execute(self, cx, ui, &command.action, &command.args);
            }
        }
    }
}

impl Widget for PaseoShell {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        // A dismissing key still belongs to the modal that was open when it arrived.
        let shell_modal = self.data.overlays.is_modal();
        let command_center = self.data.command_center_open;
        // Every window's events reach every shell: only this window's geometry is recorded.
        if let Event::WindowGeomChange(e) = event
            && Some(e.window_id) == self.data.os_window
        {
            commands::record_geometry(self, &e.new_geom);
        }
        commands::apply_initial_geometry(self, cx);
        sheets::register_field_ptr(self.text_field);
        sheets::register_qr_ptr(self.qr_scan);

        let Some(ui) = Self::ui(scope) else {
            // Startup failed: only redraw on resize.
            return;
        };
        set_compact_layout(self.data.compact);
        ui.window_id = self.data.window_id.clone();
        ui.compact = self.data.compact;
        self.restore(ui);
        if self.is_main() {
            self.data.activity.handle_event(cx, event, ui);
        }
        if !ui.events.is_empty() {
            self.handle_core_batch(cx, ui);
            if sidebar::sync_terminals(&mut self.data.sidebar, ui) {
                self.redraw(cx);
            }
        }
        self.data.router.update(ui.preferences());
        self.data.router.handle_timer(event);
        if self.data.save_timer.is_event(event).is_some() {
            self.save_layout(ui);
        }
        commands::handle_platform_actions(self, cx, event, ui);
        commands::handle_window_events(self, cx, event, ui);
        overlays::handle_timers(&mut self.data.overlays, cx, event);
        if sidebar::track_modifier(&mut self.data.sidebar, event) {
            self.redraw(cx);
        }

        // Standalone tool-call sheets (timeline, drawn above the shell overlays) take input first;
        // while one is open the shell overlays, shortcuts, chrome and panels get no input events.
        let tool_sheet = crate::features::timeline::handle_tool_call_sheets(cx, event, &self.data.window_id);
        let input = crate::common::is_user_input(event);
        let locked = input && (tool_sheet || shell_modal);
        // A panel's full-window sheet (`overlay::set_window_modal`) owns the window's input:
        // panels still dispatch (the sheet lives in one), the shell's chrome does not. Shell
        // overlays opened above it (its confirm dialogs) still take input first.
        let window_modal = input && overlay_api::window_modal_open(&self.data.window_id)
            && !shell_modal && !command_center;
        // The compact explorer's controls own input before its full-window
        // backdrop. Dispatch only that root, not the conversation underneath.
        let explorer_input = input && self.data.compact && self.data.chrome.explorer_position > 0.0
            && !tool_sheet && !shell_modal && !window_modal && !command_center;
        if explorer_input {
            cx.sweep_unlock(self.data.overlays.area);
            self.dispatch_panels(cx, event, scope, false);
        }

        // Modal overlays get the event first and lock the sweep for the content below.
        let modal = if tool_sheet || window_modal || (command_center && !shell_modal) { true } else { overlays::handle_event(self, cx, event, scope) };
        let modal = shell_modal || modal;
        let sidebar_blocked = !chrome::sidebar_visible(self) || self.data.app_inactive
            || modal || overlay_api::window_modal_open(&self.data.window_id)
            || (self.data.compact && self.data.chrome.explorer_position > 0.0);
        let fresh_pointer = matches!(event, Event::MouseDown(_) | Event::TextHandle(_))
            || matches!(event, Event::TouchUpdate(e) if e.touches.iter().any(|t| matches!(t.state, TouchState::Start)));
        if sidebar_blocked || fresh_pointer {
            self.data.sidebar.stop_motion();
        }
        if !sidebar_blocked {
            sidebar::animate(self, cx, event);
        }

        // Keyboard shortcuts (not while a modal overlay is open).
        let mut key_consumed = false;
        if !modal {
            if let Event::KeyDown(key) = event
                && let Some(ui) = Self::ui(scope)
            {
                key_consumed = commands::handle_shortcut(self, cx, ui, key);
            }
        }
        if !modal && !locked {
            chrome::observe_pane_focus(self, event);
        }
        if !key_consumed && !locked && !explorer_input {
            self.dispatch_panels(cx, event, scope, window_modal);
        }
        // Feature-owned modal/editor layers must see Back before shell navigation.
        // Keep the arrival-time modal lock even when that event closes its owner.
        if !modal
            && let Event::BackPressed { handled } = event
            && !handled.get()
            && let Some(ui) = Self::ui(scope)
            && commands::back(self, cx, ui)
        {
            handled.set(true);
        }
        if !locked && !window_modal && !explorer_input && !(input && command_center) && !key_consumed {
            if let Some(search) = self.data.search.clone() {
                search.handle_event(cx, event, scope);
            }
            for widget in [self.data.header_git.clone(), self.data.header_open_in.clone()].into_iter().flatten() {
                widget.handle_event(cx, event, scope);
            }
        }
        if !modal {
            chrome::handle_event(self, cx, event, scope);
        }
        if let Some(ui) = Self::ui(scope) {
            sidebar::handle_actions(self, cx, event, ui);
            if let Some(host_id) = self.data.sidebar.expand_host.take() {
                sidebar::expand(ui, &format!("host:{host_id}"));
            }
            self.apply_effects(cx, ui);
            self.schedule_save(cx);
            let target = chrome::focused_target(self).cloned();
            if self.is_main() {
                let visible = !self.data.app_inactive;
                self.data.activity.update_view(ui, target.as_ref(), visible);
            }
        }
        // Commands above can change the destination. Arm motion in the same event,
        // and consume animation frames even when a modal temporarily owns input.
        self.data.overlays.sync_focus(cx);
        chrome::animate_explorer(self, cx, event);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let rect = cx.walk_turtle(walk);
        // The OS window this shell draws into (close, hide, geometry).
        if self.data.os_window.is_none() {
            self.data.os_window = cx.get_current_window_id();
        }
        self.data.size = rect.size;
        let compact = crate::common::is_compact_width(rect.size.x) || theme::MOBILE && rect.size.x < crate::common::COMPACT_WIDTH;
        if compact != self.data.compact {
            self.data.compact = compact;
        }
        set_compact_layout(compact);
        if let Some(ui) = Self::ui(scope) {
            ui.compact = compact;
        }
        chrome::draw(self, cx, scope, rect);
        // The Command Center (its own overlay draw list) above the content; the shell overlay
        // layer below begins after it, so menus, dialogs and toasts stay on top.
        if self.data.command_center_open
            && let Some(widget) = self.data.command_center.clone()
        {
            if let Some(ui) = Self::ui(scope) {
                ui.route = Route::CommandCenter;
                ui.compact = compact;
            }
            cx.begin_turtle(Walk::abs_rect(rect), makepad_widgets::Layout::default());
            widget.draw_all(cx, scope);
            cx.end_turtle();
        }
        overlays::draw(self, cx, scope, rect);
        DrawStep::done()
    }
}

impl PaseoShellRef {
    /// App: names the window (layout file and geometry key).
    pub fn set_window(&self, cx: &mut Cx, id: &str) {
        if let Some(mut shell) = self.borrow_mut() {
            shell.data.window_id = id.to_string();
            shell.data.restored = false;
            shell.redraw(cx);
        }
    }

    /// App: a routed new window opens this route once its host is known.
    pub fn route(&self, cx: &mut Cx, route: Option<paseo_gui_platform::PlatformRoute>) {
        if let Some(mut shell) = self.borrow_mut() {
            shell.data.pending_route = route;
            shell.redraw(cx);
            SignalToUI::set_ui_signal();
        }
    }

    /// The OS window this shell draws into, once known.
    pub fn os_window(&self) -> Option<WindowId> {
        self.borrow().and_then(|shell| shell.data.os_window)
    }

    /// Closes this window like its close button (guards, close-to-tray, dormant first window).
    pub fn request_close(&self, cx: &mut Cx) {
        if let Some(mut shell) = self.borrow_mut() {
            shell.data.close_requested = true;
            shell.redraw(cx);
            SignalToUI::set_ui_signal();
        }
    }

    /// Last drawn size of this window's shell and whether it laid out compact.
    pub fn drawn_layout(&self) -> Option<(DVec2, bool)> {
        self.borrow().map(|shell| (shell.data.size, shell.data.compact))
    }
}

/// `navigationRequest` failure: transport errors, and payloads with `error`,
/// `accepted: false`, `success: false` or `status: agent_create_failed`.
pub fn response_message(result: &Result<Value, CoreError>) -> Option<String> {
    match result {
        Err(error) => Some(crate::common::error_message(error)),
        Ok(payload) => {
            let rejected = !j::is_null_or_missing(payload, "error")
                || payload.get("accepted") == Some(&Value::Bool(false))
                || payload.get("success") == Some(&Value::Bool(false))
                || j::text(payload, "status") == "agent_create_failed";
            rejected.then(|| match payload.get("error") {
                Some(Value::String(error)) if !error.is_empty() => error.clone(),
                Some(error @ Value::Object(_)) => hosts::error_text(error),
                _ => tr("shell.commands.hostRejected"),
            })
        }
    }
}

impl Data {
    pub fn compact_destination(&self) -> CompactDestination {
        self.layout.compact_destination
    }
}
