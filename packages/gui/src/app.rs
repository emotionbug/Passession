//! Port of `lib/main.dart` + `lib/app.dart`: process bootstrap, the core event pump, theme and
//! locale resolution, platform lifecycle and multi-window dispatch.
//!
//! One `GuiCore` serves every window (Flutter ran one engine and store per window; host
//! connections are now shared, identity and routes stay per window).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use makepad_widgets::*;
use paseo_gui_core::{CoreConfig, CoreEvent, GuiCore};
use serde_json::Value;

use crate::common::{self, UiContext, i18n};
use crate::shell::{self, PaseoShellWidgetRefExt, platform as shell_platform};
use crate::theme;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    // Manual editors construct handles lazily; wildcard imports prune unused Live types.
    use makepad_widgets::text_selection_handles::DrawTextSelectionHandle;
    use crate::shell::PaseoWindow;

    App = {{App}} {
        ui: <Root> {
            main_window = <PaseoWindow> {}
        }
        window_template: <PaseoWindow> {}
    }
}

app_main!(App);

static LAUNCH_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// `main.rs` records argv (relative paths already made absolute) before Makepad starts.
pub fn set_launch_args(argv: Vec<String>) {
    let _ = LAUNCH_ARGS.set(argv);
}

fn launch_args() -> Vec<String> {
    LAUNCH_ARGS.get().cloned().unwrap_or_else(|| std::env::args().collect())
}

/// `PASEO_VERSION` from the build (xtask always sets it), else the crate version for plain
/// `cargo run`.
pub fn app_version() -> &'static str {
    option_env!("PASEO_VERSION").filter(|v| !v.is_empty()).unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn core_config() -> CoreConfig {
    // `cargo xtask dev app` isolates development state from the installed app.
    let state_dir = std::env::var_os("PASEO_GUI_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paseo_gui_platform::paths::app_paths().support_dir.clone());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let daemon_home = std::env::var_os("PASEO_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let daemon_home = None;
    CoreConfig { state_dir, daemon_home, default_endpoint: None }
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
    core_batch_timer: Timer,
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
}

impl LiveRegister for App {
    fn live_register(cx: &mut Cx) {
        makepad_widgets::live_design(cx);
        // Re-link `link::theme` to the Paseo copy (Hangul font fallback in every stock widget).
        crate::theme_makepad::live_design(cx);
        cx.link(live_id!(theme), live_id!(paseo_makepad_theme));
        paseo_gui_platform::live_design(cx);
        theme::live_design(cx);
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
            core_batch_timer: Timer::default(),
        });
        self.ui.widget(id!(main_window.shell)).as_paseo_shell().set_window(cx, common::MAIN_WINDOW_ID);
        shell::register_window(common::MAIN_WINDOW_ID);
        #[cfg(feature = "desktop-smoke")]
        crate::smoke::start(cx);
    }

    /// Recomputes theme and locale when their preference inputs change (Flutter
    /// `_appearance`): theme fields and `language`.
    fn appearance(runtime: &mut Runtime, cx: &mut Cx) {
        let preferences = runtime.ui.state.get("preferences").cloned().unwrap_or(Value::Null);
        let fields = ["theme", "uiFontFamily", "monoFontFamily", "uiBaseFontSize", "contentFontSize", "codeFontSize", "contentMaxWidth", "language"];
        let key = serde_json::to_string(&fields.iter().map(|f| preferences.get(f)).collect::<Vec<_>>()).unwrap_or_default()
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
        let localized = i18n::set_language(preferences.get("language"));
        if themed || localized {
            cx.redraw_all();
        }
    }

    /// Admits one bounded FIFO batch; a live producer cannot extend this dispatch.
    fn drain_core(runtime: &mut Runtime) -> bool {
        let mut state_changed = false;
        runtime.core.drain_events(&mut runtime.ui.events, CORE_BATCH_LIMIT);
        for event in &runtime.ui.events {
            if let Some(platform) = runtime.platform.as_mut() {
                platform.observe_core_event(event);
            }
            if let CoreEvent::State { state } = &event {
                runtime.ui.state = state.clone();
                state_changed = true;
            }
            if let CoreEvent::Connected { host_id, server_info } = &event {
                let mut infos = (*runtime.ui.server_infos).clone();
                infos.insert(host_id.clone(), server_info.clone());
                runtime.ui.server_infos = std::rc::Rc::new(infos);
            }
        }
        state_changed
    }

    fn open_window(&mut self, cx: &mut Cx, request: shell::WindowRequest) {
        let Some(runtime) = self.runtime.as_mut() else { return };
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
    }

    /// The shell of window `id` (`main` or a secondary window).
    #[cfg_attr(not(feature = "desktop-smoke"), allow(dead_code))]
    fn shell_of(&self, id: &str) -> Option<shell::PaseoShellRef> {
        if id == common::MAIN_WINDOW_ID {
            return Some(self.ui.widget(id!(main_window.shell)).as_paseo_shell());
        }
        let runtime = self.runtime.as_ref()?;
        runtime.windows.iter().find(|(w, _)| w == id).map(|(_, window)| window.widget(id!(shell)).as_paseo_shell())
    }

    /// Hands the event to the lifecycle smoke driver after it was dispatched and drawn.
    #[cfg(feature = "desktop-smoke")]
    fn smoke_event(&self, cx: &mut Cx, event: &Event) {
        let Some(runtime) = self.runtime.as_ref() else { return };
        let shell_of = |id: &str| self.shell_of(id);
        crate::smoke::handle_event(cx, event, &crate::smoke::Ctx { state: &runtime.ui.state, shell: &shell_of });
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if let Event::Startup = event {
            self.start(cx);
        }
        // Retired layouts outlive their row/window. Drain independently of drawing
        // and core startup, with one paced continuation rather than a signal loop.
        let retirement_tick = self.layout_retirement_timer.is_event(event).is_some();
        if retirement_tick {
            self.layout_retirement_timer = Timer::default();
        }
        if (retirement_tick || (matches!(event, Event::Signal) && self.layout_retirement_timer.is_empty()))
            && makepad_widgets::makepad_draw::text::collect_retired_layouts()
        {
            self.layout_retirement_timer = cx.start_timeout(LAYOUT_RETIREMENT_INTERVAL);
        }
        let Some(runtime) = self.runtime.as_mut() else {
            // Startup failed: the shell still renders the error (no core, no dispatch scope).
            self.ui.handle_event(cx, event, &mut Scope::empty());
            return;
        };
        if let Some(platform) = runtime.platform.as_mut() {
            platform.handle_event(cx, event);
        }
        common::clock::handle_event(cx, event);
        let appearance_changed = matches!(event, Event::Actions(actions) if actions.iter().any(|action| {
            matches!(
                action.downcast_ref::<paseo_gui_platform::PlatformAction>(),
                Some(paseo_gui_platform::PlatformAction::Event(paseo_gui_platform::PlatformEvent::AppearanceChanged))
            )
        }));
        let refresh_appearance = appearance_changed || matches!(event, Event::Foreground | Event::Resume | Event::AppGotFocus);
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
        }
        let batch = !runtime.ui.events.is_empty();
        if batch {
            crate::features::composer::handle_core_events(cx, &mut runtime.ui);
        }
        if let Event::Signal = event {
            crate::features::composer::poll_background(cx);
        }
        crate::features::timeline::handle_event(cx, event, &mut runtime.ui);
        crate::features::files::handle_event(cx, event, &mut runtime.ui);

        // Main window first, then the others; the first shell consumes global effects.
        let windows: Vec<WidgetRef> = runtime.windows.iter().map(|(_, w)| w.clone()).collect();
        self.ui.handle_event(cx, event, &mut Scope::with_data(&mut runtime.ui));
        for window in &windows {
            window.handle_event(cx, event, &mut Scope::with_data(&mut runtime.ui));
        }
        runtime.ui.events.clear();

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
                let closed = matches!(actions.find_widget_action(window.widget_uid()).cast_ref::<WindowAction>(), WindowAction::WindowClosed);
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
