//! `SettingsPanel`: the native port of `settings_screen.dart`. Wide layouts
//! split a 320px sidebar from a centered 720px detail column; compact layouts
//! show the sidebar as a root list of islands and a back-header detail page.
//! Everything is drawn immediate-mode through `ui::kit`; editable fields are
//! Makepad `TextInput`s hosted by `ui::field`; menus and sheets draw into an
//! overlay draw list.

use std::cell::RefCell;
use std::collections::HashMap;

use makepad_widgets::*;
use makepad_widgets::makepad_draw::event::TouchState;
use makepad_widgets::scroll_bars::TouchScroll;
use serde_json::Value;

use super::host::{self, HostView, Status};
use super::i18n::t;
use super::pages::{self, View};
use super::platform::{Bridge, DESKTOP};
use super::rpc::{Env, Handler, ReqError, handler};
use super::state::{Settings, SheetKind, Tip};
use super::ui::draw::{Draws, Font, Icon};
use super::ui::field::{FieldHost, Layer};
use super::ui::kit::{Ui, Zone};
use super::ui::overlay::{self, Menu, item};
use super::ui::theme::{Palette, palette};
use crate::common::{Route, UiContext, UiEffect};
use crate::common::overlay::WindowModal;
use crate::common::widgets::RowMetrics;
use crate::features::workspace::{WorkspacePanelWidgetRefExt, WorkspaceTarget};

const SIDEBAR_W: f64 = 320.0;
const DETAIL_MAX: f64 = 720.0;

/// What another feature asked Settings to open (see [`open_profile_editor`]).
pub enum Intent {
    ProfileEditor { host: String, seed: Option<Value>, profile: Option<Value> },
    ProviderSheet { host: String, provider: String },
}

thread_local! {
    static INTENTS: RefCell<Vec<Intent>> = const { RefCell::new(Vec::new()) };
}

pub fn queue_intent(intent: Intent) {
    INTENTS.with(|queue| queue.borrow_mut().push(intent));
}

fn take_intents() -> Vec<Intent> {
    INTENTS.with(|queue| std::mem::take(&mut *queue.borrow_mut()))
}

const APP_SECTIONS: &[(&str, &str, Icon, bool)] = &[
    ("general", "settings.sections.general", Icon::Settings, false),
    ("appearance", "settings.sections.appearance", Icon::Palette, false),
    ("sidebar", "settings.sections.sidebar", Icon::PanelLeft, false),
    ("chat", "settings.sections.chat", Icon::MessageSquare, false),
    ("files", "settings.sections.files", Icon::Files, false),
    ("shortcuts", "settings.sections.shortcuts", Icon::Keyboard, true),
    ("notifications", "settings.sections.notifications", Icon::Bell, true),
    ("diagnostics", "settings.sections.diagnostics", Icon::Stethoscope, false),
    ("about", "settings.sections.about", Icon::Info, false),
];

const HOST_SECTIONS: &[(&str, &str, Icon)] = &[
    ("host", "settings.hostSections.host", Icon::Server),
    ("projects", "settings.hostSections.projects", Icon::FolderGit),
    ("connections", "settings.hostSections.connections", Icon::Network),
    ("agents", "settings.hostSections.agents", Icon::Bot),
    ("metadata", "settings.hostSections.metadata", Icon::Sparkles),
    ("providers", "settings.hostSections.providers", Icon::Boxes),
    ("usage", "settings.hostSections.usage", Icon::Gauge),
    ("terminals", "settings.hostSections.terminals", Icon::SquareTerminal),
    ("plugins", "settings.hostSections.plugins", Icon::Plug),
];

fn app_sections() -> impl Iterator<Item = &'static (&'static str, &'static str, Icon, bool)> {
    APP_SECTIONS.iter().filter(|(_, _, _, desktop_only)| !desktop_only || DESKTOP)
}

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::features::settings::ui::draw::SettingsDraws;
    use crate::features::workspace::WorkspacePanel;

    pub SettingsFieldInput = <TextInput> {
        width: Fill, height: Fill,
        padding: { left: 12, right: 12, top: 9, bottom: 9 }
        margin: 0,
        empty_text: "",
        // Colours and radius come from the palette in `FieldHost::draw`.
        draw_text: {
            text_style: <PASEO_FONT_REGULAR> { font_size: 10.5, line_spacing: 1.3 }
            // Flutter hints keep the hint colour while focused or hovered.
            fn get_color(self) -> vec4 {
                return mix(mix(self.color, self.color_empty, self.empty), self.color_disabled, self.disabled);
            }
        }
    }

    // Flutter `SelectableText`: no box, no padding, no caret (`showCursor: false`).
    SettingsSelectable = <TextInput> {
        width: Fill, height: Fill,
        padding: 0, margin: 0,
        empty_text: "",
        is_read_only: true,
        draw_bg: { fn pixel(self) -> vec4 { return vec4(0.0, 0.0, 0.0, 0.0); } }
        draw_cursor: { fn pixel(self) -> vec4 { return vec4(0.0, 0.0, 0.0, 0.0); } }
        draw_text: {
            fn get_color(self) -> vec4 { return self.color; }
        }
    }
    pub SettingsSelectableRegular = <SettingsSelectable> {
        draw_text: { text_style: <PASEO_FONT_REGULAR> { font_size: 10.5, line_spacing: 1.3 } }
    }
    pub SettingsSelectableMedium = <SettingsSelectable> {
        draw_text: { text_style: <PASEO_FONT_MEDIUM> { font_size: 10.5, line_spacing: 1.3 } }
    }
    pub SettingsSelectableCode = <SettingsSelectable> {
        draw_text: { text_style: <PASEO_FONT_MONO> { font_size: 9.0, line_spacing: 1.4 } }
    }

    pub SettingsPanel = {{SettingsPanel}} {
        width: Fill, height: Fill
        draws: <SettingsDraws> {}
        overlay_draws: <SettingsDraws> {}
        field_template: <SettingsFieldInput> {}
        selectable_regular: <SettingsSelectableRegular> {}
        selectable_medium: <SettingsSelectableMedium> {}
        selectable_code: <SettingsSelectableCode> {}
        project_template: <WorkspacePanel> {}
    }

    // Embedded profile editor (composer): no page, sheets over the window.
    pub SettingsSheetHost = <SettingsPanel> {
        width: 0, height: 0
        sheet_only: true
    }
}

/// One stacked overlay layer, drawn into its own draw list.
#[derive(Clone, Copy)]
enum OverlayLayer {
    Sheet(u64),
    Menu,
    Tip,
}

/// Actions of an embedded (`sheet_only`) settings panel.
#[derive(Clone, Debug, DefaultNone)]
pub enum SettingsPanelAction {
    /// The editor opened by [`SettingsPanelRef::open_profile_editor`] closed;
    /// `saved_on_host` is the host whose `agentProfiles` were written, if any.
    EditorClosed { saved_on_host: Option<String> },
    None,
}

#[derive(Live, LiveHook, Widget)]
pub struct SettingsPanel {
    #[walk]
    walk: Walk,
    /// Embedded editor mode (`SettingsSheetHost`): no page, no route, only the
    /// sheets opened through [`SettingsPanelRef::open_profile_editor`].
    #[live]
    sheet_only: bool,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    draws: Draws,
    #[live]
    overlay_draws: Draws,
    #[live]
    field_template: Option<LivePtr>,
    #[live]
    selectable_regular: Option<LivePtr>,
    #[live]
    selectable_medium: Option<LivePtr>,
    #[live]
    selectable_code: Option<LivePtr>,
    #[live]
    project_template: Option<LivePtr>,
    /// One overlay draw list per stacked layer (sheets, menu, tooltip).
    #[rust]
    layer_lists: Vec<DrawList2d>,
    #[rust]
    overlay_area: Area,
    #[rust]
    window_modal: Option<WindowModal>,
    #[rust]
    touch_scroll: TouchScroll,
    /// The region the current touch gesture started on.
    #[rust]
    touch_region: Option<Region>,
    /// Requested only while a spinner was drawn last frame.
    #[rust]
    next_frame: NextFrame,
    /// Hides the info tooltip (100 ms after hover exit, 1.5 s after a tap).
    #[rust]
    tip_timer: Timer,
    /// Shows `tip_pending` after the 500 ms button tooltip wait.
    #[rust]
    tip_show_timer: Timer,
    #[rust]
    tip_pending: Option<Tip>,
    #[rust]
    st: Settings,
    #[rust]
    fields: FieldHost,
    #[rust]
    bridge: Bridge,
    #[rust]
    zones: Vec<Zone>,
    #[rust]
    hovered: Option<u64>,
    #[rust]
    pressed: Option<u64>,
    /// Scrollable regions drawn last frame: (region, clip rect).
    #[rust]
    regions: Vec<(Region, Rect)>,
    #[rust]
    sheet_heights: HashMap<u64, f64>,
    #[rust]
    project: Option<(WidgetRef, String, String)>,
    #[rust]
    started: bool,
    #[rust]
    compact: bool,
    /// Last menu list (content height, visible height).
    #[rust]
    menu_height: (f64, f64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Region {
    Sidebar,
    Content,
    Sheet(u64),
    Menu,
}

impl SettingsPanel {
    fn view(&self, ui: &UiContext) -> View {
        let hosts = host::hosts(&ui.state);
        let prefs = self.st.prefs(&host::preferences(&ui.state));
        View { hosts, prefs, compact: ui.compact, desktop: DESKTOP }
    }

    /// The layer and depth whose fields take pointer input: the menu over
    /// everything, else the top sheet, else the page (tooltips take none).
    fn field_layer(&self) -> (Layer, usize) {
        if !self.overlay_open() {
            return (Layer::Page, 0);
        }
        (Layer::Overlay, self.st.sheets.len() + usize::from(self.st.menu.is_some()))
    }

    fn redraw_overlays(&mut self, cx: &mut Cx) {
        for list in &mut self.layer_lists {
            list.redraw(cx);
        }
    }

    fn overlay_open(&self) -> bool {
        !self.st.sheets.is_empty() || self.st.menu.is_some()
    }

    /// Route → section/host (first mount and external navigation).
    fn sync_route(&mut self, ui: &UiContext) {
        if let Route::Settings { host_id, page } = &ui.route {
            let seen = (host_id.clone(), page.clone());
            if self.st.route_seen.as_ref() != Some(&seen) {
                self.st.route_seen = Some(seen);
                if host_id.is_some() {
                    self.st.host = host_id.clone();
                }
                self.st.section = (!page.is_empty()).then(|| page.clone());
                self.st.project_root = None;
                self.st.scroll = 0.0;
            }
        }
    }

    fn with_env<R>(&mut self, cx: &mut Cx, ui: &mut UiContext, f: impl FnOnce(&mut Settings, &mut Env) -> R) -> R {
        let mut env = Env { cx, ctx: ui, fields: &mut self.fields, bridge: &mut self.bridge };
        f(&mut self.st, &mut env)
    }

    fn run(&mut self, cx: &mut Cx, ui: &mut UiContext, h: Handler) {
        self.with_env(cx, ui, |s, env| h(s, env));
    }

    fn zone_at(&self, pos: DVec2) -> Option<&Zone> {
        let layer = if self.overlay_open() { Layer::Overlay } else { Layer::Page };
        self.zones.iter().rev().find(|z| z.layer == layer && z.rect.contains(pos))
    }

    /// Whether `region` takes scrolling now: the open menu over everything,
    /// else the top sheet, else the page's columns.
    fn scrolls(&self, region: Region) -> bool {
        if self.st.menu.is_some() {
            return region == Region::Menu;
        }
        if let Some(top) = self.st.sheets.last() {
            return region == Region::Sheet(top.id);
        }
        matches!(region, Region::Sidebar | Region::Content)
    }

    /// The scrollable region under `pos` that takes scrolling now.
    fn region_at(&self, pos: DVec2) -> Option<(Region, Rect)> {
        self.regions.iter().rev().find(|(region, rect)| self.scrolls(*region) && rect.contains(pos)).copied()
    }

    /// Scrolls the region under `pos`. A touch gesture stays with the region it
    /// started on: once a menu or sheet takes over, its pan and inertia stop.
    fn scroll_region(&mut self, pos: DVec2, dy: f64, touch: bool) {
        let found = self.region_at(pos);
        if touch && found.map(|(region, _)| region) != self.touch_region {
            self.touch_region = None;
            self.touch_scroll.reset();
            return;
        }
        let Some((region, rect)) = found else {
            self.touch_scroll.reset();
            return;
        };
        let (scroll, max) = match region {
            Region::Sidebar => (&mut self.st.sidebar_scroll, self.st.sidebar_h - rect.size.y),
            Region::Content => (&mut self.st.scroll, self.st.content_h - rect.size.y),
            Region::Menu => {
                let (content, visible) = self.menu_height;
                let Some(menu) = &mut self.st.menu else { return };
                (&mut menu.scroll, content - visible)
            }
            Region::Sheet(id) => {
                let Some(sheet) = self.st.sheets.iter_mut().find(|s| s.id == id) else { return };
                (&mut sheet.scroll, self.sheet_heights.get(&id).copied().unwrap_or(0.0) - rect.size.y)
            }
        };
        self.touch_scroll.apply_delta(scroll, dy, max);
    }

    /// Core batch, connection changes, intents and loads.
    fn handle_core(&mut self, cx: &mut Cx, ui: &mut UiContext) -> bool {
        let closed_editor = self.sheet_only && !self.overlay_open();
        let mut dirty = !self.sheet_only;
        let events = std::mem::take(&mut ui.events);
        for event in &events {
            match event {
                paseo_gui_core::CoreEvent::Response { request_id, result, .. } if self.st.owns_request(request_id) => {
                    let result = result.clone().map_err(|e| ReqError::from_core(&e));
                    let id = request_id.clone();
                    self.with_env(cx, ui, |s, env| s.on_response(env, &id, result));
                    dirty |= !closed_editor || self.overlay_open();
                }
                paseo_gui_core::CoreEvent::HostEvent { host_id, event }
                    if !closed_editor && matches!(event.get("type").and_then(Value::as_str),
                        Some("status" | "providers_snapshot_update" | "usage.list_reports.update" | "daemon.update.progress")) =>
                {
                    self.with_env(cx, ui, |s, env| pages::on_host_event(s, env, host_id, event));
                    dirty = true;
                }
                // Events during a reconnect are not replayed: every admitted
                // connection re-reads config, provider snapshot, usage, plugins.
                paseo_gui_core::CoreEvent::Connected { host_id, .. } if !closed_editor => {
                    let id = host_id.clone();
                    self.with_env(cx, ui, |s, env| {
                        pages::on_status_change(s, &id);
                        pages::profiles::on_reconnect(s, env, &id);
                    });
                    dirty = true;
                }
                _ => {}
            }
        }
        ui.events = events;
        // Connection status changes reload host-bound flows.
        let hosts = host::hosts(&ui.state);
        for h in &hosts {
            let previous = self.st.last_status.insert(h.server_id.clone(), h.status);
            if !closed_editor && previous.is_some() && previous != Some(h.status) {
                let id = h.server_id.clone();
                let connected = h.status == Status::Connected;
                self.with_env(cx, ui, |s, env| {
                    pages::on_status_change(s, &id);
                    if connected {
                        pages::profiles::on_reconnect(s, env, &id);
                    }
                });
                dirty = true;
            }
        }
        // Keep pending response ownership and status tracking alive without running
        // the closed embedded editor's page loads or cloning host stream payloads.
        if closed_editor {
            return dirty;
        }
        let prefs = host::preferences(&ui.state);
        self.st.settle_prefs(&prefs);
        // An embedded editor never takes the Settings page's queued intents.
        let intents = if self.sheet_only { Vec::new() } else { take_intents() };
        for intent in intents {
            match intent {
                Intent::ProfileEditor { host, seed, profile } => {
                    self.st.host = Some(host.clone());
                    self.with_env(cx, ui, |s, env| pages::profiles::open_editor(s, env, &host, profile, seed));
                }
                Intent::ProviderSheet { host, provider } => {
                    self.st.host = Some(host.clone());
                    pages::providers::open_provider_sheet(&mut self.st, &host, &provider);
                }
            }
        }
        pages::providers::tick(&mut self.st);
        if self.sheet_only {
            return dirty;
        }
        let view = self.view(ui);
        if self.st.section.as_deref() == Some("shortcuts") {
            pages::shortcuts::ensure(&mut self.st);
        }
        self.with_env(cx, ui, |s, env| pages::ensure(s, env, &view));
        dirty
    }

    /// Navigates within settings (Flutter `select`).
    fn select(s: &mut Settings, env: &mut Env, section: Option<String>) {
        s.section = section.clone();
        s.project_root = None;
        s.scroll = 0.0;
        let route = Route::Settings { host_id: s.host.clone(), page: section.clone().unwrap_or_default() };
        s.route_seen = Some((s.host.clone(), section.unwrap_or_default()));
        env.navigate(route);
    }

    fn back_to_parent(s: &mut Settings, env: &mut Env) {
        if s.project_root.is_some() {
            s.project_root = None;
        } else {
            Self::select(s, env, None);
        }
    }
}

fn nav(section: &'static str) -> Handler {
    handler(move |s, env| SettingsPanel::select(s, env, Some(section.to_string())))
}

/// Flutter `selectHostSection`: no host opens the host dialog.
fn nav_host(section: &'static str) -> Handler {
    handler(move |s, env| {
        if env.hosts().is_empty() {
            env.ctx.effects.push(UiEffect::OpenHostDialog);
            return;
        }
        SettingsPanel::select(s, env, Some(format!("host/{section}")));
    })
}

impl Widget for SettingsPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return };
        self.compact = ui.compact;
        if !self.sheet_only {
            self.sync_route(ui);
        }
        let was_open = self.overlay_open();
        let mut dirty = false;
        if !self.started || !ui.events.is_empty() {
            self.started = true;
            dirty |= self.handle_core(cx, ui);
        }
        // Platform completions.
        let replies = self.bridge.collect(event);
        if !replies.is_empty() {
            dirty = true;
        }
        for (token, result) in replies {
            self.with_env(cx, ui, |s, env| s.on_bridge(env, token, result));
        }
        if matches!(event, Event::Foreground | Event::Resume | Event::AppGotFocus) {
            self.with_env(cx, ui, pages::system::refresh_notification_permission);
            dirty |= self.st.notifications.checking;
        }
        if self.with_env(cx, ui, |s, env| s.on_timer(env, event)) {
            dirty = true;
        }
        if self.next_frame.is_event(event).is_some() {
            dirty = true;
        }
        if self.tip_timer.is_event(event).is_some() && self.st.tip.take().is_some() {
            dirty = true;
        }
        if self.tip_show_timer.is_event(event).is_some()
            && let Some(tip) = self.tip_pending.take()
        {
            cx.stop_timer(self.tip_timer);
            self.st.tip = Some(tip);
            dirty = true;
        }

        // Embedded project settings (WorkspaceAutomation) gets page events.
        if let Some((widget, _, _)) = &self.project
            && !self.overlay_open()
            && self.st.project_root.is_some()
        {
            let widget = widget.clone();
            widget.handle_event(cx, event, scope);
        }
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return };

        // Fields first so they capture their own clicks.
        let outcome = self.fields.handle_event(cx, event, self.field_layer());
        if outcome.changed {
            self.with_env(cx, ui, pages::profiles::sync_fields);
            dirty = true;
        }
        for h in outcome.run {
            self.run(cx, ui, h);
            dirty = true;
        }

        // Shortcut capture keys.
        if self.st.shortcuts.capturing.is_some()
            && (self.st.section.as_deref() != Some("shortcuts")
                || matches!(event, Event::KeyFocusLost(e) if e.prev == self.area)
                || (!self.area.is_valid(cx) && !self.st.shortcuts.want_focus))
        {
            pages::shortcuts::cancel_capture(&mut self.st);
            cx.clear_key_focus_if(self.area);
            dirty = true;
        }
        if self.st.shortcuts.capturing.is_none() && std::mem::take(&mut self.st.shortcuts.focus_scope_active) {
            ui.push(UiEffect::SetFocusScope { owner: self.widget_uid(), scope: None });
        }
        if let Event::KeyDown(ke) = event
            && self.st.shortcuts.capturing.is_some() && !self.overlay_open()
            && cx.has_key_focus(self.area) && self.area.is_valid(cx)
            && pages::shortcuts::on_key(&mut self.st, ke.key_code, ke.modifiers)
        {
            dirty = true;
        }
        if self.overlay_open()
            && (event.back_pressed() || matches!(event, Event::KeyDown(KeyEvent { key_code: KeyCode::Escape, .. })))
        {
            if self.st.menu.is_some() {
                self.st.menu = None;
            } else if let Some(sheet) = self.st.sheets.last()
                && !pages::sheet_busy(sheet)
            {
                self.with_env(cx, ui, |s, env| s.close_top_sheet(env));
            }
            if let Event::BackPressed { handled } = event {
                handled.set(true);
            }
            dirty = true;
        } else if self.compact && !self.sheet_only
            && (self.st.section.is_some() || self.st.project_root.is_some())
            && event.back_pressed()
        {
            self.with_env(cx, ui, Self::back_to_parent);
            if let Event::BackPressed { handled } = event {
                handled.set(true);
            }
            dirty = true;
        }
        if std::mem::take(&mut self.st.shortcuts.want_focus) {
            cx.set_key_focus(self.area);
            self.st.shortcuts.focus_scope_active = true;
            ui.push(UiEffect::SetFocusScope { owner: self.widget_uid(), scope: Some("shortcut-capture".into()) });
        }

        let area = if self.overlay_open() { self.overlay_area } else { self.area };
        let hit = event.hits_with_capture_overload(cx, area, matches!(event, Event::TouchUpdate(_)));
        if let Event::TouchUpdate(e) = event
            && e.touches.iter().any(|t| matches!(t.state, TouchState::Move) && !t.handled.get().is_empty())
        {
            // An embedded project's ScrollYView owns this gesture.
            self.touch_scroll.reset();
        }
        if !area.is_valid(cx) {
            self.touch_scroll.reset();
        }
        if let Some(dy) = self.touch_scroll.handle_event(cx, event, &hit) {
            self.scroll_region(self.touch_scroll.origin(), dy, true);
            self.pressed = None;
            dirty = true;
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let zone = self.zone_at(e.abs).map(|z| (z.key, z.on.is_some(), z.tip.clone(), z.rect));
                let hovered = zone.as_ref().map(|(k, ..)| *k);
                if zone.as_ref().map(|(_, active, ..)| *active).unwrap_or(false) {
                    cx.set_cursor(MouseCursor::Hand);
                } else {
                    cx.set_cursor(MouseCursor::Default);
                }
                if hovered != self.hovered {
                    self.hovered = hovered;
                    // Flutter `Tooltip(waitDuration: zero)`: shows on hover at once,
                    // hides 100 ms after the pointer leaves (default exit duration).
                    cx.stop_timer(self.tip_show_timer);
                    self.tip_pending = None;
                    match zone {
                        Some((_, _, Some((text, false)), rect)) => {
                            cx.stop_timer(self.tip_timer);
                            self.st.tip = Some(Tip { text, anchor: rect, tapped: false });
                        }
                        Some((_, _, Some((text, true)), rect)) => {
                            // `TooltipThemeData(waitDuration: 500ms)`.
                            self.tip_pending = Some(Tip { text, anchor: rect, tapped: false });
                            self.tip_show_timer = cx.start_timeout(0.5);
                            if self.st.tip.is_some() {
                                self.tip_timer = cx.start_timeout(0.1);
                            }
                        }
                        _ if self.st.tip.is_some() => self.tip_timer = cx.start_timeout(0.1),
                        _ => {}
                    }
                    dirty = true;
                }
            }
            Hit::FingerHoverOut(_) => {
                cx.stop_timer(self.tip_show_timer);
                self.tip_pending = None;
                if self.hovered.take().is_some() {
                    if self.st.tip.is_some() {
                        self.tip_timer = cx.start_timeout(0.1);
                    }
                    dirty = true;
                }
            }
            Hit::FingerLongPress(e) => {
                // Touch: Flutter tooltips show on long press for 1.5 s.
                if let Some((text, rect)) = self.zone_at(e.abs).and_then(|z| z.tip.clone().map(|(text, _)| (text, z.rect))) {
                    self.st.tip = Some(Tip { text, anchor: rect, tapped: true });
                    self.tip_timer = cx.start_timeout(1.5);
                    dirty = true;
                }
            }
            Hit::FingerDown(e) => {
                cx.stop_timer(self.tip_show_timer);
                self.tip_pending = None;
                self.pressed = self.zone_at(e.abs).map(|z| z.key);
                self.touch_region = self.region_at(e.abs).map(|(region, _)| region);
                // Any press dismisses a tooltip (a tap on its own glyph shows it again).
                if self.st.tip.take().is_some() {
                    cx.stop_timer(self.tip_timer);
                }
                dirty = true;
            }
            Hit::FingerUp(e) => {
                let pressed = self.pressed.take();
                if e.is_over
                    && e.was_tap()
                    && let Some((k, on, rect)) = self.zone_at(e.abs).map(|z| (z.key, z.on.clone(), z.rect))
                    && Some(k) == pressed
                    && let Some(on) = on
                {
                    self.st.anchor = rect;
                    self.run(cx, ui, on);
                    // `TooltipTriggerMode.tap`: a tapped tooltip stays 1.5 s (default show duration).
                    if self.st.tip.as_ref().is_some_and(|tip| tip.tapped) {
                        self.tip_timer = cx.start_timeout(1.5);
                    }
                }
                dirty = true;
            }
            Hit::FingerScroll(e) => {
                self.scroll_region(e.abs, e.scroll.y, false);
                dirty = true;
            }
            Hit::KeyDown(_) => {}
            _ => {}
        }
        if dirty {
            // A tap may have opened another section or host: start its loads.
            if !self.sheet_only {
                let view = self.view(ui);
                if self.st.section.as_deref() == Some("shortcuts") {
                    pages::shortcuts::ensure(&mut self.st);
                }
                self.with_env(cx, ui, |s, env| pages::ensure(s, env, &view));
            }
            self.area.redraw(cx);
            self.redraw_overlays(cx);
        }
        let owner = self.widget_uid().0;
        let open = self.overlay_open();
        crate::common::overlay::WindowModal::sync(&mut self.window_modal, ui, owner, open);
        if self.sheet_only && was_open && !self.overlay_open() {
            let saved = std::mem::take(&mut self.st.saved_profile_host);
            cx.widget_action(self.widget_uid(), &scope.path, SettingsPanelAction::EditorClosed { saved_on_host: saved });
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            cx.walk_turtle(walk);
            return DrawStep::done();
        };
        if !self.sheet_only {
            self.sync_route(ui);
        }
        let owner = self.widget_uid().0;
        let open = self.overlay_open();
        crate::common::overlay::WindowModal::sync(&mut self.window_modal, ui, owner, open);
        self.compact = ui.compact;
        let view = self.view(ui);
        let pal = palette();
        self.fields.set_templates(self.field_template, [self.selectable_regular, self.selectable_medium, self.selectable_code]);
        self.fields.begin_frame();
        self.zones.clear();
        self.regions.clear();
        let rect = cx.walk_turtle(walk);
        // Panel area for hit testing and redraw.
        cx.add_aligned_rect_area(&mut self.area, rect);

        let compact = view.compact;
        let ty = crate::theme::current_typography();
        let compact_header_h = 64.0_f64.max(ty.ui_size(16.0) * 1.4 + 40.0);
        let wide_header_h = 36.0_f64.max(ty.ui_size(14.0) * 1.4 + 16.0);
        let mut content_h = 0.0;
        let mut sidebar_h = 0.0;
        let mut animating = false;
        // An embedded editor draws no page, only its sheets.
        if !self.sheet_only {
            {
                let mut kit = Ui::new(cx, &mut self.draws, &mut self.zones, &mut self.fields, Layer::Page, pal, self.hovered, self.pressed, compact);
                kit.rect(rect, pal.surface0, None, 0.0);
                if compact && self.st.section.is_none() && self.st.project_root.is_none() {
                    // Compact root: back header over the sidebar islands.
                    draw_compact_header(&mut kit, rect, &t("settings.title"), handler(|_, env| env.ctx.effects.push(UiEffect::CloseSettings)));
                    let body = Rect { pos: dvec2(rect.pos.x, rect.pos.y + compact_header_h), size: dvec2(rect.size.x, rect.size.y - compact_header_h) };
                    sidebar_h = scrolled(&mut kit, body, self.st.sidebar_scroll, 8.0, |kit| draw_sidebar_body(kit, &self.st, &view, true));
                    self.regions.push((Region::Sidebar, body));
                } else if compact {
                    let title = page_title(&self.st, &view);
                    let back = handler(SettingsPanel::back_to_parent);
                    draw_compact_header(&mut kit, rect, &title.unwrap_or_default(), back);
                    let body = Rect { pos: dvec2(rect.pos.x, rect.pos.y + compact_header_h), size: dvec2(rect.size.x, rect.size.y - compact_header_h) };
                    if self.st.project_root.is_none() {
                        content_h = scrolled(&mut kit, body, self.st.scroll, 16.0, |kit| draw_detail(kit, &self.st, &view, None));
                        self.regions.push((Region::Content, body));
                    }
                } else {
                    // Wide: sidebar + detail column.
                    let sidebar = Rect { pos: rect.pos, size: dvec2(SIDEBAR_W.min(rect.size.x * 0.5), rect.size.y) };
                    kit.rect(sidebar, pal.sidebar, None, 0.0);
                    kit.rect(Rect { pos: dvec2(sidebar.pos.x + sidebar.size.x - 1.0, rect.pos.y), size: dvec2(1.0, rect.size.y) }, pal.border, None, 0.0);
                    // Back row in a 36px header strip.
                    let header = Rect { pos: sidebar.pos, size: dvec2(sidebar.size.x - 1.0, wide_header_h) };
                    kit.hline(header.pos.x, header.pos.y + wide_header_h - 1.0, header.size.x);
                    sidebar_item(&mut kit, Rect { pos: dvec2(header.pos.x + 8.0, header.pos.y + 4.0), size: dvec2(header.size.x - 16.0, wide_header_h - 8.0) }, "settings.back", Icon::ArrowLeft, &t("settings.backToWorkspace"), false, false, handler(|_, env| env.ctx.effects.push(UiEffect::CloseSettings)));
                    let list = Rect { pos: dvec2(sidebar.pos.x, sidebar.pos.y + wide_header_h), size: dvec2(sidebar.size.x - 1.0, sidebar.size.y - wide_header_h) };
                    sidebar_h = scrolled(&mut kit, list, self.st.sidebar_scroll, 0.0, |kit| draw_sidebar_body(kit, &self.st, &view, false));
                    self.regions.push((Region::Sidebar, list));
                    let detail = Rect { pos: dvec2(sidebar.pos.x + sidebar.size.x, rect.pos.y + wide_header_h), size: dvec2(rect.size.x - sidebar.size.x, rect.size.y - wide_header_h) };
                    if self.st.project_root.is_none() {
                        let title = page_title(&self.st, &view);
                        content_h = scrolled(&mut kit, detail, self.st.scroll, 0.0, |kit| draw_detail(kit, &self.st, &view, title));
                        self.regions.push((Region::Content, detail));
                    }
                }
                animating = kit.animating;
            }
            self.st.content_h = content_h;
            self.st.sidebar_h = sidebar_h;
            clamp_scroll(&mut self.st.scroll, content_h, self.regions.iter().find(|(r, _)| *r == Region::Content).map(|(_, r)| r.size.y));
            clamp_scroll(&mut self.st.sidebar_scroll, sidebar_h, self.regions.iter().find(|(r, _)| *r == Region::Sidebar).map(|(_, r)| r.size.y));

            // Embedded project settings.
            if let (Some(root), Some(host)) = (self.st.project_root.clone(), view.active_host(&self.st).map(|h| h.server_id.clone())) {
                let detail = if compact {
                    Rect { pos: dvec2(rect.pos.x, rect.pos.y + compact_header_h), size: dvec2(rect.size.x, rect.size.y - compact_header_h) }
                } else {
                    let sx = SIDEBAR_W.min(rect.size.x * 0.5);
                    Rect { pos: dvec2(rect.pos.x + sx, rect.pos.y + wide_header_h), size: dvec2(rect.size.x - sx, rect.size.y - wide_header_h) }
                };
                let stale = self.project.as_ref().map(|(_, h, r)| *h != host || *r != root).unwrap_or(true);
                if stale {
                    let widget = WidgetRef::new_from_ptr(cx, self.project_template);
                    widget.as_workspace_panel().set_target(cx, WorkspaceTarget::ProjectSettings { host_id: host.clone(), project_key: root.clone() });
                    self.project = Some((widget, host, root));
                }
                if let Some((widget, _, _)) = &self.project {
                    widget.draw_walk_all(cx, scope, Walk::fixed(detail.size.x, detail.size.y).with_abs_pos(detail.pos));
                }
            } else {
                self.project = None;
            }
        }

        // Overlay: menus and sheets.
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return DrawStep::done() };
        let view = self.view(ui);
        if self.overlay_open() || self.st.tip.is_some() {
            // One draw list per stacked layer: a list keeps every primitive of
            // its layer (boxes, text, glyphs and native text fields) above the
            // layers before it, which batching inside one list cannot.
            let mut layers: Vec<OverlayLayer> = self.st.sheets.iter().map(|s| OverlayLayer::Sheet(s.id)).collect();
            if self.st.menu.is_some() {
                layers.push(OverlayLayer::Menu);
            }
            if self.st.tip.is_some() {
                layers.push(OverlayLayer::Tip);
            }
            while self.layer_lists.len() < layers.len() {
                self.layer_lists.push(DrawList2d::new(cx));
            }
            let size = cx.current_pass_size();
            let screen = Rect { pos: dvec2(0.0, 0.0), size };
            // Sheets, menus and tips lie inside the safe area above the keyboard;
            // barriers still cover the whole window.
            let insets = crate::shell::platform::safe_insets();
            let viewport = Rect {
                pos: dvec2(insets.left, insets.top),
                size: dvec2((size.x - insets.left - insets.right).max(0.0), (size.y - insets.top - insets.bottom_occlusion()).max(0.0)),
            };
            let mut remeasure = false;
            for (index, layer) in layers.into_iter().enumerate() {
                let list = &mut self.layer_lists[index];
                list.begin_overlay_last(cx);
                cx.begin_sized_turtle(size, Layout::flow_overlay());
                // Menus and sheets take the pointer; a tooltip alone leaves it to the page.
                if index == 0 && self.overlay_open() {
                    cx.add_aligned_rect_area(&mut self.overlay_area, screen);
                }
                let mut kit = Ui::new(cx, &mut self.overlay_draws, &mut self.zones, &mut self.fields, Layer::Overlay, pal, self.hovered, self.pressed, compact);
                kit.depth = index + 1;
                match layer {
                    OverlayLayer::Sheet(id) => {
                        if let Some(sheet) = self.st.sheets.iter().find(|s| s.id == id) {
                            // Flutter `confirmAction` is an adaptive alert dialog, not a sheet.
                            if let SheetKind::Confirm(confirm) = &sheet.kind {
                                overlay::draw_alert(&mut kit, screen, viewport, id, confirm);
                            } else {
                                let head = pages::sheet_head(&self.st, &view, sheet);
                                let measured = self.sheet_heights.get(&id).copied().unwrap_or(200.0);
                                let busy = pages::sheet_busy(sheet);
                                let (frame, body) = overlay::sheet_frame(&mut kit, screen, viewport, sheet, &head, measured, busy);
                                let scroll = sheet.scroll;
                                let h = scrolled(&mut kit, body, scroll, 24.0, |kit| pages::sheet_body(kit, &self.st, &view, sheet));
                                // The frame is sized from last frame's content height: draw
                                // again once it changes so the sheet fits its content.
                                if self.sheet_heights.insert(id, h).is_none_or(|old| (old - h).abs() > 0.5) {
                                    remeasure = true;
                                }
                                self.regions.push((Region::Sheet(id), body));
                                overlay::sheet_controls(&mut kit, frame, head);
                            }
                        }
                    }
                    OverlayLayer::Menu => {
                        if let Some(menu) = &self.st.menu {
                            let (list, h) = overlay::draw_menu(&mut kit, menu, screen, viewport);
                            self.regions.push((Region::Menu, list));
                            self.menu_height = (h, list.size.y);
                        }
                    }
                    OverlayLayer::Tip => {
                        if let Some(tip) = &self.st.tip {
                            overlay::draw_tip(&mut kit, tip, viewport);
                        }
                    }
                }
                animating |= kit.animating;
                cx.end_pass_sized_turtle();
                self.layer_lists[index].end(cx);
            }
            // A redraw requested mid-draw is dropped: re-lay the sheet next frame.
            animating |= remeasure;
        }
        for sheet in self.st.sheets.iter_mut() {
            let visible = self.regions.iter().find(|(r, _)| *r == Region::Sheet(sheet.id)).map(|(_, r)| r.size.y);
            let h = self.sheet_heights.get(&sheet.id).copied().unwrap_or(0.0);
            clamp_scroll(&mut sheet.scroll, h, visible);
        }
        self.sheet_heights.retain(|id, _| self.st.sheets.iter().any(|s| s.id == *id));
        self.fields.end_frame(cx);
        // Spinners animate only while one is on screen; a resized sheet re-lays once.
        if animating {
            self.next_frame = cx.new_next_frame();
        }
        DrawStep::done()
    }
}

fn clamp_scroll(scroll: &mut f64, content: f64, visible: Option<f64>) {
    let max = (content - visible.unwrap_or(content)).max(0.0);
    *scroll = scroll.clamp(0.0, max);
}

/// Draws `body` inside a clipped region scrolled by `scroll`; returns the content height.
fn scrolled(kit: &mut Ui, region: Rect, scroll: f64, pad: f64, body: impl FnOnce(&mut Ui)) -> f64 {
    kit.cx.begin_turtle(Walk::fixed(region.size.x, region.size.y).with_abs_pos(region.pos), Layout::flow_overlay().with_clip(true, true));
    kit.set_clip(Some(region));
    kit.x = region.pos.x + pad;
    kit.w = region.size.x - pad * 2.0;
    kit.y = region.pos.y + pad - scroll;
    let top = kit.y;
    body(kit);
    let h = kit.y - top + pad * 2.0;
    kit.set_clip(None);
    kit.cx.end_turtle();
    h
}

/// Original `CompactHeader` with back navigation.
fn draw_compact_header(kit: &mut Ui, rect: Rect, title: &str, back: Handler) {
    let pal = kit.pal;
    let y = rect.pos.y + 8.0;
    let button = Rect { pos: dvec2(rect.pos.x + 12.0, y + 12.0), size: dvec2(32.0, 32.0) };
    let (hovered, pressed) = kit.state("settings.compactBack");
    if hovered || pressed {
        kit.rect(button, pal.interaction, None, 16.0);
    }
    kit.icon(Icon::ArrowLeft, dvec2(button.pos.x + 8.0, button.pos.y + 8.0), 16.0, pal.muted);
    kit.zone("settings.compactBack", Rect { pos: dvec2(button.pos.x, button.pos.y - 8.0), size: dvec2(32.0, 48.0) }, Some(back));
    kit.text_line(Font::Medium, 16.0, pal.foreground, dvec2(button.pos.x + 40.0, y + 18.0), rect.size.x - 100.0, title);
}

/// The detail title (Flutter `title()`).
fn page_title(s: &Settings, _v: &View) -> Option<String> {
    if s.project_root.is_some() {
        return Some(t("settings.projects"));
    }
    let current = s.section.clone().unwrap_or_else(|| "general".into());
    if current == "local-daemon" {
        return Some(t("desktop.daemon.title"));
    }
    if let Some(section) = current.strip_prefix("host/") {
        return HOST_SECTIONS.iter().find(|(id, _, _)| *id == section).map(|(_, key, _)| t(key));
    }
    app_sections().find(|(id, _, _, _)| *id == current).map(|(_, key, _, _)| t(key))
}

/// Original `PageLayout`: centered 720px column with a 26px title on wide.
fn draw_detail(kit: &mut Ui, s: &Settings, v: &View, title: Option<String>) {
    let full_x = kit.x;
    let full_w = kit.w;
    let w = (full_w - 32.0).clamp(0.0, DETAIL_MAX - 32.0);
    kit.x = full_x + (full_w - w) / 2.0;
    kit.w = w;
    kit.y += 24.0;
    if let Some(title) = title {
        let pal = kit.pal;
        let size = kit.text(Font::Medium, 26.0, pal.foreground, dvec2(kit.x + 4.0, kit.y), Some(w - 4.0), &title);
        kit.y += size.y + 24.0;
    }
    let section = s.section.clone().unwrap_or_else(|| "general".into());
    pages::draw_section(kit, s, v, &section);
    kit.y += 16.0;
    kit.x = full_x;
    kit.w = full_w;
}

fn status_color(pal: &Palette, h: &HostView) -> Vec4 {
    pages::host::status_color(h, pal)
}

/// Original settings sidebar item: 52/28px rows, md icon, lg/base label.
#[allow(clippy::too_many_arguments)]
fn sidebar_item(kit: &mut Ui, r: Rect, key_text: &str, icon: Icon, label: &str, selected: bool, mobile: bool, on: Handler) {
    let pal = kit.pal;
    let (hovered, pressed) = kit.state(key_text);
    let fill = if pressed {
        pal.surface2
    } else if hovered || selected {
        pal.sidebar_hover
    } else if mobile {
        pal.surface1
    } else {
        pal.transparent
    };
    kit.rect(r, fill, None, if mobile { 0.0 } else { 8.0 });
    let color = if selected { pal.foreground } else { pal.muted };
    let pad = if mobile { 16.0 } else { 8.0 };
    let icon_size = if mobile { 18.0 } else { 16.0 };
    let font = if mobile { 16.0 } else { 14.0 };
    let metrics = RowMetrics::new(kit.measure(Font::Regular, font, "Ag", None).y, 0.0, 0.0);
    let gap = if mobile { 12.0 } else { 8.0 };
    let slots = metrics.layout(r, pad, icon_size, 0.0, gap);
    kit.icon(icon, dvec2(slots.leading.pos.x, slots.leading.pos.y + (slots.leading.size.y - icon_size) * 0.5), icon_size, color);
    if slots.title.size.x > 0.0 {
        kit.text_line(Font::Regular, font, color, slots.title.pos, slots.title.size.x, label);
    }
    kit.zone(key_text, slots.bounds, Some(on));
}

/// Original `_sidebarBody`: App group, then the Host group (picker + host
/// sections) or Add host / Enable built-in daemon.
fn draw_sidebar_body(kit: &mut Ui, s: &Settings, v: &View, mobile: bool) {
    let pal = kit.pal;
    let current = s.section.clone().or_else(|| (!mobile).then(|| "general".to_string()));
    let host_section = if s.project_root.is_some() {
        Some("projects".to_string())
    } else {
        current.as_deref().and_then(|c| c.strip_prefix("host/")).map(String::from)
    };
    let metrics = RowMetrics::new(kit.measure(Font::Regular, if mobile { 16.0 } else { 14.0 }, "Ag", None).y, 0.0, 0.0);
    let row_h = metrics.height(if mobile { 52.0 } else { 28.0 }, 4.0, if mobile { 18.0 } else { 16.0 });
    let gap = 2.0;
    let no_local = DESKTOP && !v.hosts.iter().any(|h| h.local);
    let x = kit.x + 8.0;
    let w = kit.w - 16.0;
    let group = |kit: &mut Ui, label: Option<&str>| {
        kit.y += 8.0;
        if let Some(label) = label {
            let size = kit.text(Font::Regular, 14.0, pal.extra_muted, dvec2(x + if mobile { 16.0 } else { 8.0 }, kit.y + 4.0), None, label);
            kit.y += size.y + 8.0 + if mobile { 4.0 } else { 2.0 };
        }
    };
    group(kit, Some(&t("settings.groups.app")));
    for (id, label, icon, _) in app_sections() {
        let r = Rect { pos: dvec2(x, kit.y), size: dvec2(w, row_h) };
        let selected = !mobile && host_section.is_none() && current.as_deref() == Some(*id);
        sidebar_item(kit, r, &format!("side.app.{id}"), *icon, &t(label), selected, mobile, nav(id));
        kit.y += row_h + gap;
    }
    kit.y += 8.0;
    if !v.hosts.is_empty() {
        group(kit, Some(&t("settings.groups.host")));
        // Host picker row (status dot, label, chevron).
        let active = v.active_host(s);
        let r = Rect { pos: dvec2(x, kit.y), size: dvec2(w, row_h) };
        let label = active.map(|h| h.label.clone()).unwrap_or_else(|| t("settings.groups.host"));
        let (hovered, pressed) = kit.state("side.hostPicker");
        let fill = if pressed { pal.surface2 } else if hovered { pal.sidebar_hover } else if mobile { pal.surface1 } else { pal.transparent };
        kit.rect(r, fill, None, if mobile { 0.0 } else { 8.0 });
        let pad = if mobile { 16.0 } else { 8.0 };
        let slots = metrics.layout(r, pad, 16.0, 14.0, 8.0);
        if let Some(h) = active {
            let color = status_color(&pal, h);
            kit.rect(Rect { pos: dvec2(slots.leading.pos.x + 4.0, slots.leading.pos.y + (row_h - 8.0) * 0.5), size: dvec2(8.0, 8.0) }, color, None, 4.0);
        }
        let font = if mobile { 16.0 } else { 14.0 };
        if slots.title.size.x > 0.0 {
            kit.text_line(Font::Regular, font, pal.foreground, slots.title.pos, slots.title.size.x, &label);
        }
        kit.icon(Icon::ChevronDown, dvec2(slots.trailing.pos.x, slots.trailing.pos.y + (row_h - 14.0) * 0.5 + 1.0), 14.0, pal.muted);
        let hosts = v.hosts.clone();
        let active_id = active.map(|h| h.server_id.clone());
        let picker = handler(move |s, _| {
            let mut items = Vec::new();
            let dark = palette();
            for h in &hosts {
                let id = h.server_id.clone();
                items.push(
                    item(h.label.clone(), handler(move |s, env| {
                        s.host = Some(id.clone());
                        if s.project_root.take().is_some() {
                            s.section = Some("host/projects".into());
                        }
                        let page = s.section.clone().unwrap_or_default();
                        s.route_seen = Some((s.host.clone(), page.clone()));
                        env.navigate(Route::Settings { host_id: s.host.clone(), page });
                    }))
                    .hint(pages::host::picker_hint(h))
                    .lead(super::ui::kit::Lead::Dot(status_color(&dark, h)))
                    .selected(active_id.as_deref() == Some(h.server_id.as_str())),
                );
            }
            let mut add = item(t("settings.addHost"), handler(|_, env| env.ctx.effects.push(UiEffect::OpenHostDialog))).lead(super::ui::kit::Lead::Icon(Icon::Plus, None));
            add.separator_before = true;
            items.push(add);
            if no_local {
                items.push(item(t("settings.enableBuiltInDaemon"), nav("local-daemon")).lead(super::ui::kit::Lead::Icon(Icon::Server, None)));
            }
            let menu = Menu::new(s, t("settings.hostPicker.switchHost"), items).width(240.0).align_start();
            s.open_menu(menu);
        });
        kit.zone("side.hostPicker", r, Some(picker));
        kit.y += row_h + gap;
        for (id, label, icon) in HOST_SECTIONS {
            let r = Rect { pos: dvec2(x, kit.y), size: dvec2(w, row_h) };
            let selected = !mobile && host_section.as_deref() == Some(*id);
            sidebar_item(kit, r, &format!("side.host.{id}"), *icon, &t(label), selected, mobile, nav_host(id));
            kit.y += row_h + gap;
        }
    } else {
        group(kit, None);
        let r = Rect { pos: dvec2(x, kit.y), size: dvec2(w, row_h) };
        sidebar_item(kit, r, "side.addHost", Icon::Plus, &t("settings.addHost"), false, mobile, handler(|_, env| env.ctx.effects.push(UiEffect::OpenHostDialog)));
        kit.y += row_h + gap;
        if no_local {
            let r = Rect { pos: dvec2(x, kit.y), size: dvec2(w, row_h) };
            let selected = !mobile && current.as_deref() == Some("local-daemon");
            sidebar_item(kit, r, "side.builtIn", Icon::Server, &t("settings.enableBuiltInDaemon"), selected, mobile, nav("local-daemon"));
            kit.y += row_h + gap;
        }
    }
    kit.y += 8.0;
}

impl SettingsPanelRef {
    /// Opens the agent profile editor sheet in this panel (Flutter
    /// `ProfileEditor` shown over the composer). Meant for a `sheet_only`
    /// instance: the sheet draws over the window and the panel emits
    /// [`SettingsPanelAction::EditorClosed`] when it closes. `seed` pre-fills
    /// a new profile (`{provider, model|modelId, name}`), `profile` edits a
    /// stored one.
    pub fn open_profile_editor(&self, cx: &mut Cx, ui: &mut UiContext, host_id: &str, seed: Option<Value>, profile: Option<Value>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.st.host = Some(host_id.to_string());
        inner.with_env(cx, ui, |s, env| pages::profiles::open_editor(s, env, host_id, profile, seed));
        inner.area.redraw(cx);
        inner.redraw_overlays(cx);
    }

    /// Dismiss a hidden retained owner's layers; pending replies remain routable.
    pub fn hide(&self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.st.menu = None;
        inner.with_env(cx, ui, |s, env| {
            while !s.sheets.is_empty() {
                s.close_top_sheet(env);
            }
        });
        inner.fields.forget_prefix(cx, "");
        pages::shortcuts::cancel_capture(&mut inner.st);
        if std::mem::take(&mut inner.st.shortcuts.focus_scope_active) {
            ui.push(UiEffect::SetFocusScope { owner: inner.widget_uid(), scope: None });
        }
        cx.clear_key_focus_if(inner.area);
        inner.redraw_overlays(cx);
    }

    /// Whether a sheet or menu of this panel is open.
    pub fn is_open(&self) -> bool {
        self.borrow().is_some_and(|inner| inner.overlay_open())
    }

    /// The editor-closed action of this panel in `actions`.
    pub fn editor_closed(&self, actions: &Actions) -> Option<Option<String>> {
        match actions.find_widget_action(self.widget_uid()).cast() {
            SettingsPanelAction::EditorClosed { saved_on_host } => Some(saved_on_host),
            SettingsPanelAction::None => None,
        }
    }
}
