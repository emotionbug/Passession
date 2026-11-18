//! Port of `lib/main.dart` + `lib/app.dart`: process bootstrap, the core event pump, theme and
//! locale resolution, platform lifecycle and multi-window dispatch.
//!
//! One `GuiCore` serves every window (Flutter ran one engine and store per window; host
//! connections are now shared, identity and routes stay per window).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use makepad_widgets::*;
use paseo_gui_core::{CoreConfig, CoreEvent, GuiCore};
use paseo_gui_platform::{TailnetPresence, TailnetPresenceState};
use serde_json::Value;

use crate::common::state_delta::StateDelta;
use crate::common::{self, UiContext, i18n};
use crate::fonts::{self, FontRoles, SystemFontState};
use crate::shell::{self, PaseoShellWidgetRefExt, platform as shell_platform};
use crate::theme;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    // Manual editors construct handles lazily; wildcard imports prune unused Live types.
    use makepad_widgets::text_selection_handles::DrawTextSelectionHandle;
    use crate::shell::PaseoWindow;
    use crate::fonts::PaseoFontRoles;

    App = {{App}} {
        ui: <Root> {
            main_window = <PaseoWindow> {}
        }
        window_template: <PaseoWindow> {}
        font_roles: <PaseoFontRoles> {}
    }
}

app_main!(App);

static LAUNCH_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// `main.rs` records argv (relative paths already made absolute) before Makepad starts.
pub fn set_launch_args(argv: Vec<String>) {
    let _ = LAUNCH_ARGS.set(argv);
}

fn launch_args() -> Vec<String> {
    LAUNCH_ARGS
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::args().collect())
}

/// `PASEO_VERSION` from the build (xtask always sets it), else the crate version for plain
/// `cargo run`.
pub fn app_version() -> &'static str {
    option_env!("PASEO_VERSION")
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn core_config() -> CoreConfig {
    // `cargo xtask dev app` isolates development state from the installed app.
    let state_dir = std::env::var_os("PASEO_GUI_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paseo_gui_platform::paths::app_paths().support_dir.clone());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let daemon_home = std::env::var_os("PASEO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let daemon_home = None;
    CoreConfig {
        state_dir,
        daemon_home,
        default_endpoint: None,
        tailnet_key_store: Some(paseo_gui_platform::tailnet_key_store()),
    }
}

// Leave event-loop turns between transcript batches for input and painting.
const CORE_BATCH_LIMIT: usize = 64;
const CORE_BATCH_INTERVAL: f64 = 1.0 / 60.0;
const LAYOUT_RETIREMENT_INTERVAL: f64 = 1.0 / 60.0;

/// Everything that exists once startup succeeded.
struct Runtime {
    core: GuiCore,
    ui: UiContext,
    platform: Option<paseo_gui_platform::Platform>,
    /// Secondary windows (`desktop_multi_window` engines in Flutter), by window id.
    windows: Vec<(String, WidgetRef)>,
    next_window: u32,
    system_dark: bool,
    theme_key: String,
    window_ready: bool,
    /// The language the native application and tray menus were last built in.
    menu_locale: &'static str,
    /// The embedded Tailscale background presence last handed to the platform.
    tailnet_presence: Option<TailnetPresence>,
    core_batch_timer: Timer,
    /// The core's last `State` publication: batch deltas compare publications, so a local
    /// optimistic write (`write_preference`) still reaches every window with its echo.
    published: Arc<paseo_gui_core::CoreState>,
}

#[derive(Live, LiveHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    #[live]
    window_template: Option<LivePtr>,
    #[rust]
    runtime: Option<Runtime>,
    #[rust]
    startup_error: Option<String>,
    #[rust]
    layout_retirement_timer: Timer,
    #[live]
    font_roles: FontRoles,
    #[rust]
    system_fonts: SystemFontState,
}

impl LiveRegister for App {
    fn live_register(cx: &mut Cx) {
        makepad_widgets::live_design(cx);
        // Re-link `link::theme` to the Paseo copy (Hangul font fallback in every stock widget).
        crate::theme_makepad::live_design(cx);
        cx.link(live_id!(theme), live_id!(paseo_makepad_theme));
        paseo_gui_platform::live_design(cx);
        theme::live_design(cx);
        fonts::live_design(cx);
        common::live_design(cx);
        crate::features::live_design(cx);
        shell::panels::live_design(cx);
        shell::sheets::live_design(cx);
        shell::live_design(cx);
    }
}

impl App {
    fn start(&mut self, cx: &mut Cx) {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(SignalToUI::set_ui_signal);
        let core = match GuiCore::start(core_config(), wake) {
            Ok(core) => core,
            Err(error) => {
                self.startup_error = Some(format!("{error:#}"));
                shell::set_startup_error(self.startup_error.clone());
                return;
            }
        };
        let handle = core.handle();
        let platform = match paseo_gui_platform::Platform::start(
            cx,
            paseo_gui_platform::PlatformConfig {
                app_version: app_version().to_string(),
                argv: launch_args(),
                core: handle.clone(),
            },
        ) {
            Ok(platform) => Some(platform),
            Err(error) => {
                shell::set_startup_error(Some(format!("{error:#}")));
                None
            }
        };
        let system_dark = shell_platform::system_prefers_dark();
        theme::update(&Value::Null, system_dark);
        self.runtime = Some(Runtime {
            core,
            ui: UiContext::new(handle),
            platform,
            windows: Vec::new(),
            next_window: 1,
            system_dark,
            theme_key: String::new(),
            window_ready: false,
            menu_locale: "",
            tailnet_presence: None,
            core_batch_timer: Timer::default(),
            published: Arc::new(paseo_gui_core::CoreState::default()),
        });
        self.ui
            .widget(id!(main_window.shell))
            .as_paseo_shell()
            .set_window(cx, common::MAIN_WINDOW_ID);
        shell::register_window(common::MAIN_WINDOW_ID);
        #[cfg(feature = "desktop-smoke")]
        crate::smoke::start(cx);
    }

    /// Recomputes theme and locale when their preference inputs change (Flutter
    /// `_appearance`): theme fields and `language`.
    fn appearance(runtime: &mut Runtime, cx: &mut Cx) {
        let preferences = runtime
            .ui
            .state
            .get("preferences")
            .cloned()
            .unwrap_or(Value::Null);
        let fields = [
            "theme",
            "uiFontFamily",
            "monoFontFamily",
            "uiBaseFontSize",
            "contentFontSize",
            "codeFontSize",
            "contentMaxWidth",
            "language",
        ];
        let key = serde_json::to_string(
            &fields
                .iter()
                .map(|f| preferences.get(f))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
            + if runtime.system_dark { "d" } else { "l" };
        if key == runtime.theme_key {
            return;
        }
        runtime.theme_key = key;
        let themed = theme::update(&preferences, runtime.system_dark);
        #[cfg(target_os = "android")]
        {
            let palette = theme::current_palette();
            let color = palette.background;
            let background = 0xff000000
                | (((color.x * 255.0).round() as u32) << 16)
                | (((color.y * 255.0).round() as u32) << 8)
                | (color.z * 255.0).round() as u32;
            paseo_gui_platform::appearance::set_system_bars(palette.is_dark, background);
        }
        // Desktop title bars (Windows caption, macOS title bar, X11 frame) match the palette.
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        cx.set_window_dark_appearance(theme::current_palette().is_dark);
        let localized = i18n::set_language(preferences.get("language"));
        if runtime.menu_locale != i18n::locale_tag() {
            runtime.menu_locale = i18n::locale_tag();
            update_native_menu(cx);
            update_tray_labels();
        }
        if themed || localized {
            // Palette, typography and strings change every layer of every window.
            #[allow(clippy::disallowed_methods)]
            cx.redraw_all();
        }
    }

    /// Admits one bounded FIFO batch; a live producer cannot extend this dispatch.
    fn drain_core(runtime: &mut Runtime) -> bool {
        let mut state_changed = false;
        runtime
            .core
            .drain_events(&mut runtime.ui.events, CORE_BATCH_LIMIT);
        for event in &runtime.ui.events {
            if let Some(platform) = runtime.platform.as_mut() {
                platform.observe_core_event(event);
            }
            if let CoreEvent::Response {
                request_id, result, ..
            } = event
            {
                runtime.ui.terminal_terminations.settle(request_id, result);
            }
            if let CoreEvent::State { state } = &event {
                let delta = StateDelta::between(&runtime.published, state);
                runtime.ui.state_delta.merge(delta);
                runtime.published = state.clone();
                runtime.ui.state = state.clone();
                state_changed = true;
            }
            if let CoreEvent::Connected {
                host_id,
                server_info,
            } = &event
            {
                let mut infos = (*runtime.ui.server_infos).clone();
                infos.insert(host_id.clone(), server_info.clone());
                runtime.ui.server_infos = std::rc::Rc::new(infos);
            }
        }
        state_changed
    }

    fn open_window(&mut self, cx: &mut Cx, request: shell::WindowRequest) {
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };
        // Original `openNew`: a dormant first window is shown again instead of a new one.
        if shell::window_state(common::MAIN_WINDOW_ID) == Some(shell::WindowState::Dormant) {
            let main = self.ui.widget(id!(main_window.shell)).as_paseo_shell();
            if let Some(window) = main.os_window() {
                shell_platform::show_window(cx, window);
            }
            shell::register_window(common::MAIN_WINDOW_ID);
            main.route(cx, request.route);
            return;
        }
        let id = runtime.next_window.to_string();
        runtime.next_window += 1;
        let window = WidgetRef::new_from_ptr(cx, self.window_template);
        let shell = window.widget(id!(shell)).as_paseo_shell();
        shell.set_window(cx, &id);
        shell.route(cx, request.route);
        shell::register_window(&id);
        runtime.windows.push((id, window));
        // Each new window's `window_menu` rebuilds the native menu from its English DSL names.
        update_native_menu(cx);
    }

    /// The shell of window `id` (`main` or a secondary window).
    #[cfg_attr(not(feature = "desktop-smoke"), allow(dead_code))]
    fn shell_of(&self, id: &str) -> Option<shell::PaseoShellRef> {
        if id == common::MAIN_WINDOW_ID {
            return Some(self.ui.widget(id!(main_window.shell)).as_paseo_shell());
        }
        let runtime = self.runtime.as_ref()?;
        runtime
            .windows
            .iter()
            .find(|(w, _)| w == id)
            .map(|(_, window)| window.widget(id!(shell)).as_paseo_shell())
    }

    /// Hands the event to the lifecycle smoke driver after it was dispatched and drawn.
    #[cfg(feature = "desktop-smoke")]
    fn smoke_event(&self, cx: &mut Cx, event: &Event) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let shell_of = |id: &str| self.shell_of(id);
        crate::smoke::handle_event(
            cx,
            event,
            &crate::smoke::Ctx {
                state: &runtime.ui.state,
                shell: &shell_of,
            },
        );
    }
}

/// The macOS application menu (`PaseoWindow`'s `window_menu`: app ▸ Quit, File ▸ New Window)
/// in the active language, with the same commands and keys.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn native_menu() -> MacosMenu {
    let item = |name: String, command: LiveId, shift: bool, key: KeyCode| MacosMenu::Item {
        name,
        command,
        shift,
        key,
        enabled: true,
    };
    MacosMenu::Main {
        items: vec![
            MacosMenu::Sub {
                name: "Passession".into(),
                items: vec![item(
                    common::tr("shell.menu.quitApp"),
                    live_id!(paseo_quit),
                    false,
                    KeyCode::KeyQ,
                )],
            },
            MacosMenu::Sub {
                name: common::tr("shell.menu.file"),
                items: vec![item(
                    common::tr("shell.menu.newWindow"),
                    live_id!(new_window),
                    true,
                    KeyCode::KeyN,
                )],
            },
        ],
    }
}

/// Rebuilds the native application menu in the active language (macOS; other platforms have
/// no native menu).
fn update_native_menu(cx: &mut Cx) {
    #[cfg(target_os = "macos")]
    cx.update_macos_menu(native_menu());
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

/// Relabels the desktop tray's Open and Quit items in the active language.
fn update_tray_labels() {
    if let Some(platform) = paseo_gui_platform::platform() {
        platform.set_tray_labels(
            common::tr("shell.menu.openApp"),
            common::tr("shell.menu.quitApp"),
        );
    }
}

/// The OS background presence of embedded Tailscale, labelled in the active language. None
/// while it is off, or when no platform secret store lets a node run.
fn tailnet_presence(tailnet: &Value) -> Option<TailnetPresence> {
    if tailnet["enabled"] != true || tailnet["available"] != true {
        return None;
    }
    let (state, status) = match tailnet["backendState"].as_str().unwrap_or("") {
        "Running" => (TailnetPresenceState::Connected, "tailnet.states.running"),
        "Paused" => (TailnetPresenceState::Paused, "tailnet.states.paused"),
        "Starting" | "NoState" | "" => {
            (TailnetPresenceState::Connecting, "tailnet.states.starting")
        }
        // Sign-in, device approval, a stopped node or an error.
        _ => (
            TailnetPresenceState::NeedsAttention,
            "tailnet.presence.attention",
        ),
    };
    Some(TailnetPresence {
        state,
        title: common::tr("tailnet.embedded"),
        status: common::tr(status),
        pause: common::tr("tailnet.pause"),
        resume: common::tr("tailnet.resume"),
        stop: common::tr("tailnet.stop"),
    })
}

/// Hands the platform a presence only when it changed.
fn sync_tailnet_presence(runtime: &mut Runtime) {
    let presence = tailnet_presence(runtime.ui.state.get("tailnet").unwrap_or(&Value::Null));
    if presence == runtime.tailnet_presence {
        return;
    }
    if let Some(platform) = paseo_gui_platform::platform() {
        platform.set_tailnet_presence(presence.clone());
    }
    runtime.tailnet_presence = presence;
}

fn window_accepts_event(
    cx: &Cx,
    event: &Event,
    group: u64,
    timer: Option<TimerOwner>,
    native_window: impl FnOnce() -> Option<WindowId>,
) -> bool {
    if matches!(event, Event::Timer(_)) {
        return timer.is_some_and(|owner| owner.group == group);
    }
    let source = match event {
        Event::WindowGeomChange(e) => Some(e.window_id),
        Event::WindowCloseRequested(e) => Some(e.window_id),
        Event::WindowClosed(e) => Some(e.window_id),
        Event::WindowDragQuery(e) => Some(e.window_id),
        Event::MouseDown(e) => Some(e.window_id),
        Event::MouseMove(e) => Some(e.window_id),
        Event::MouseUp(e) => Some(e.window_id),
        Event::MouseLeave(e) => Some(e.window_id),
        Event::TouchUpdate(e) => Some(e.window_id),
        Event::LongPress(e) => Some(e.window_id),
        Event::Scroll(e) => Some(e.window_id),
        _ => None,
    };
    if source.is_none()
        && !event.is_keyboard_input()
        && !matches!(event, Event::Drag(_) | Event::Drop(_))
    {
        return true;
    }
    native_window().is_none_or(|id| {
        source.is_none_or(|source| source == id)
            && cx.keyboard_event_is_for_window(event, id)
            && cx.drag_event_is_for_window(event, id)
    })
}

fn dispatch_window(
    cx: &mut Cx,
    event: &Event,
    widget: &WidgetRef,
    timer: Option<TimerOwner>,
    ui: &mut UiContext,
    native_window: impl FnOnce() -> Option<WindowId>,
) {
    let group = widget.widget_uid().0;
    if window_accepts_event(cx, event, group, timer, native_window) {
        ui.timer_target = timer.map(|owner| owner.target);
        cx.with_timer_owner(TimerOwner { group, target: 0 }, |cx| {
            widget.handle_event(cx, event, &mut Scope::with_data(ui));
        });
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        cx.with_timer_owner(common::APP_TIMERS, |cx| self.dispatch_event(cx, event));
    }
}

impl App {
    fn dispatch_event(&mut self, cx: &mut Cx, event: &Event) {
        let timer = match event {
            Event::Timer(e) => cx.take_timer_owner(e.timer_id),
            _ => None,
        };
        if let Event::Startup = event {
            self.start(cx);
        }
        // Retired layouts outlive their row/window. Drain independently of drawing
        // and core startup, with one paced continuation rather than a signal loop.
        let retirement_tick = self.layout_retirement_timer.is_event(event).is_some();
        if retirement_tick {
            self.layout_retirement_timer = Timer::default();
        }
        if (retirement_tick
            || (matches!(event, Event::Signal) && self.layout_retirement_timer.is_empty()))
            && makepad_widgets::makepad_draw::text::collect_retired_layouts()
        {
            self.layout_retirement_timer = cx.start_timeout(LAYOUT_RETIREMENT_INTERVAL);
        }
        let Some(runtime) = self.runtime.as_mut() else {
            // Startup failed: the shell still renders the error (no core, no dispatch scope).
            let owner = TimerOwner {
                group: self.ui.widget_uid().0,
                target: 0,
            };
            if !matches!(event, Event::Timer(_)) || timer.is_some_and(|t| t.group == owner.group) {
                cx.with_timer_owner(owner, |cx| {
                    self.ui.handle_event(cx, event, &mut Scope::empty())
                });
            }
            return;
        };
        if !matches!(event, Event::Timer(_))
            && let Some(platform) = runtime.platform.as_mut()
        {
            platform.handle_event(cx, event);
        }
        if !matches!(event, Event::Timer(_)) || timer == Some(common::APP_TIMERS) {
            common::clock::handle_event(cx, event);
        }
        let appearance_changed = matches!(event, Event::Actions(actions) if actions.iter().any(|action| {
            matches!(
                action.downcast_ref::<paseo_gui_platform::PlatformAction>(),
                Some(paseo_gui_platform::PlatformAction::Event(paseo_gui_platform::PlatformEvent::AppearanceChanged))
            )
        }));
        let refresh_appearance = appearance_changed
            || matches!(
                event,
                Event::Foreground | Event::Resume | Event::AppGotFocus
            );
        if refresh_appearance {
            runtime.system_dark = shell_platform::system_prefers_dark();
            runtime.theme_key.clear();
        }
        let mut appearance = refresh_appearance || matches!(event, Event::Startup);
        let core_tick = runtime.core_batch_timer.is_event(event).is_some();
        if core_tick {
            runtime.core_batch_timer = Timer::default();
        }
        if core_tick || (matches!(event, Event::Signal) && runtime.core_batch_timer.is_empty()) {
            appearance |= Self::drain_core(runtime);
            if !runtime.ui.events.is_empty() {
                // Poll once more even if the queue was empty after admission:
                // arrivals during processing must not bypass the cadence. A timer
                // also services hidden windows which have no animation frames.
                runtime.core_batch_timer = cx.start_timeout(CORE_BATCH_INTERVAL);
            }
        }
        if appearance {
            Self::appearance(runtime, cx);
            // After `appearance`: a language change relabels the presence.
            sync_tailnet_presence(runtime);
            // `uiFontFamily`/`monoFontFamily` or the OS fonts may have changed;
            // an unchanged request is a no-op.
            let preferences = runtime.ui.state.get("preferences").unwrap_or(&Value::Null);
            self.system_fonts.request(preferences, refresh_appearance);
        }
        // Paint with bundled fonts immediately; discovery signals apply system
        // faces and invalidate retained layouts through the normal late-font path.
        if matches!(event, Event::Signal) {
            self.system_fonts.poll(cx, &self.font_roles);
        }
        let batch = !runtime.ui.events.is_empty();
        if batch {
            crate::features::composer::handle_core_events(cx, &mut runtime.ui);
        }
        if let Event::Signal = event {
            crate::features::composer::poll_background(cx);
        }
        // The continuation timer belongs to the app, not every panel receiving
        // the batch. Deliver the data as a worker wake, without the raw timer.
        let widget_event = if core_tick && batch {
            &Event::Signal
        } else {
            event
        };
        let widget_timer = if core_tick && batch { None } else { timer };
        let signal = matches!(widget_event, Event::Signal);
        let platform_completion = matches!(widget_event, Event::Actions(actions) if actions.iter().any(|action| {
            matches!(action.downcast_ref::<paseo_gui_platform::PlatformAction>(),
                Some(paseo_gui_platform::PlatformAction::Completed { .. }))
        }));
        let group = timer.map(|owner| owner.group);
        if batch
            || signal
            || platform_completion
            || group == Some(common::TIMELINE_TIMERS.group)
            || matches!(
                event,
                Event::Foreground | Event::Resume | Event::Background | Event::Pause
            )
        {
            crate::features::timeline::handle_event(cx, widget_event, &mut runtime.ui);
        }
        if batch || signal || platform_completion || group == Some(common::FILE_TIMERS.group) {
            let timer_session = timer
                .filter(|owner| owner.group == common::FILE_TIMERS.group)
                .map(|owner| owner.target);
            crate::features::files::handle_event(cx, widget_event, &mut runtime.ui, timer_session);
        }
        if batch || signal || group == Some(common::GIT_TIMERS.group) {
            crate::features::git::store::with_store(|store| {
                store.handle_event(cx, widget_event, &mut runtime.ui)
            });
        }

        // Main window first: the first dispatched shell consumes global effects.
        dispatch_window(
            cx,
            widget_event,
            &self.ui,
            widget_timer,
            &mut runtime.ui,
            || {
                self.ui
                    .widget(id!(main_window.shell))
                    .as_paseo_shell()
                    .os_window()
            },
        );
        for (_, window) in &runtime.windows {
            dispatch_window(
                cx,
                widget_event,
                window,
                widget_timer,
                &mut runtime.ui,
                || window.widget(id!(shell)).as_paseo_shell().os_window(),
            );
        }
        runtime.ui.timer_target = None;
        if matches!(event, Event::Draw(_)) {
            common::clock::finish_draw(cx);
        }
        runtime.ui.events.clear();
        runtime.ui.terminal_terminations.finish_batch();
        runtime.ui.state_delta = StateDelta::default();

        if !runtime.window_ready {
            if let Some(platform) = runtime.platform.as_mut() {
                platform.window_ready();
            }
            runtime.window_ready = true;
        }
        if let Some(platform) = runtime.platform.as_mut() {
            platform.finish_event(cx, event);
        }

        // Windows closed this event (their shells already ran their guards).
        if let Event::Actions(actions) = event {
            runtime.windows.retain(|(id, window)| {
                let closed = matches!(
                    actions
                        .find_widget_action(window.widget_uid())
                        .cast_ref::<WindowAction>(),
                    WindowAction::WindowClosed
                );
                if closed {
                    shell::unregister_window(id);
                }
                !closed
            });
        }
        for request in shell::take_window_requests() {
            self.open_window(cx, request);
        }
        if shell::take_quit() {
            cx.quit();
        }
        #[cfg(feature = "desktop-smoke")]
        self.smoke_event(cx, event);
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    #[test]
    fn dispatch_timer_reaches_only_the_registered_owner() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let owner = TimerOwner {
            group: 20,
            target: 42,
        };
        let other = TimerOwner {
            group: 21,
            target: 42,
        };
        let timer = cx.with_timer_owner(owner, |cx| {
            cx.with_timer_owner(other, |cx| cx.start_timeout(1.0));
            cx.start_timeout(1.0)
        });
        let event = Event::Timer(makepad_widgets::event::TimerEvent {
            timer_id: timer.0,
            time: None,
        });
        let recipient = cx.take_timer_owner(timer.0);
        let mut received = Vec::new();
        for group in [19, 20, 21] {
            if window_accepts_event(&cx, &event, group, recipient, || None) {
                received.push(recipient.unwrap());
            }
        }
        assert_eq!(received, [owner]);
        assert!(cx.take_timer_owner(timer.0).is_none());
    }
}
