//! In-process desktop lifecycle smoke driver (`desktop-smoke-driver.md`, run by
//! `tools/native-desktop-smoke/run.py`). Compiled only with the `desktop-smoke` feature and
//! active only when `PASEO_DESKTOP_SMOKE_PROOF` is set. It replaces the Flutter
//! `integration_test/desktop_lifecycle.dart` target and drives this process's real windows,
//! close guards and quit flows; the harness screenshots the phases it records.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use makepad_widgets::*;
use paseo_gui_platform::{
    PlatformAction, PlatformEvent, PlatformRequestId, PlatformValue, QuitReason,
};
use serde_json::{Value, json};

use crate::common::MAIN_WINDOW_ID;
use crate::features::timeline;
use crate::shell::{self, PaseoShellRef, WindowState, hosts};

/// Every wait fails after this long (the harness's own budget per phase).
const WAIT: Duration = Duration::from_secs(35);
const WAIT_COMMAND: &str = "bash running: python3 \"/home/user/project with spaces/scripts/validate_audio_latency_and_bluetooth_lifecycle.py\" --target android --variant release --output /tmp/validation-results.json\nWaiting for the build to finish";

thread_local! {
    static DRIVER: RefCell<Option<Driver>> = const { RefCell::new(None) };
    /// The registered dirty-document guard (`registerCloseGuard(.., () => false)`).
    static CLOSE_GUARD: Cell<bool> = const { Cell::new(false) };
}

/// Shell close flows: a registered smoke guard refuses window close and quit.
pub(crate) fn close_guard_refuses() -> bool {
    CLOSE_GUARD.with(Cell::get)
}

/// What the driver reads from the app on each event.
pub struct Ctx<'a> {
    pub state: &'a paseo_gui_core::CoreState,
    pub shell: &'a dyn Fn(&str) -> Option<PaseoShellRef>,
}

/// App startup: arms the driver when the harness provided a proof directory.
pub fn start(cx: &mut Cx) {
    let Some(proof) = std::env::var_os("PASEO_DESKTOP_SMOKE_PROOF").filter(|v| !v.is_empty())
    else {
        return;
    };
    let driver = Driver {
        proof: PathBuf::from(proof),
        logout: std::env::var("PASEO_SMOKE_LOGOUT").is_ok_and(|v| v == "1"),
        step: Step::Startup,
        deadline: Instant::now() + WAIT,
        settle: None,
        poll: cx.start_interval(0.1),
        daemon_request: None,
        daemon: None,
        next_query: None,
        daemon_identity: None,
        errors: Vec::new(),
        present: None,
        sheet: None,
        main_size: DVec2::default(),
        restore_size: DVec2::default(),
        second: String::new(),
        quit_cancelled: false,
        native_close_received: false,
        quit_requested: false,
    };
    DRIVER.with(|d| *d.borrow_mut() = Some(driver));
}

/// App: after every event has been dispatched and drawn.
pub fn handle_event(cx: &mut Cx, event: &Event, ctx: &Ctx) {
    DRIVER.with(|d| {
        let mut d = d.borrow_mut();
        let Some(driver) = d.as_mut() else { return };
        if driver.step == Step::Done {
            return;
        }
        if let Err(error) = driver.handle_event(cx, event, ctx) {
            driver.fail(&error);
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Startup,
    StartupDaemon,
    ColdStart,
    SheetCompact,
    SheetOpen,
    SheetPresented,
    SheetCaptured,
    SheetRestored,
    SecondOpen,
    GuardClose,
    GuardQuit,
    GuardDaemon,
    GuardCaptured,
    PrimaryDormant,
    PrimaryReused,
    SecondTornDown,
    ReusePresented,
    ReuseCaptured,
    ReplacementOpen,
    ReplacementDormant,
    ReplacementDaemon,
    RetainPresented,
    RetainCaptured,
    GuardedLastClose,
    GuardedLastDaemon,
    GuardedLastCaptured,
    LastQuit,
    Done,
}

/// A screenshot phase waiting for the frame that shows its state.
struct Present {
    phase: &'static str,
    value: Value,
    /// A draw happened since the state changed; the next frame proves it was presented.
    drawn: bool,
}

struct Driver {
    proof: PathBuf,
    logout: bool,
    step: Step,
    deadline: Instant,
    /// Steps that prove something did not happen wait this long first.
    settle: Option<Instant>,
    /// 100 ms tick that keeps waiting steps advancing; stopped once the driver is done.
    poll: Timer,
    daemon_request: Option<PlatformRequestId>,
    daemon: Option<Value>,
    next_query: Option<Instant>,
    daemon_identity: Option<(u64, String)>,
    errors: Vec<String>,
    present: Option<Present>,
    sheet: Option<u64>,
    main_size: DVec2,
    restore_size: DVec2,
    second: String,
    quit_cancelled: bool,
    native_close_received: bool,
    quit_requested: bool,
}

impl Driver {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, ctx: &Ctx) -> Result<(), String> {
        match event {
            Event::Actions(actions) => {
                for action in shell::platform::actions(actions) {
                    match action {
                        PlatformAction::Completed { request, result }
                            if Some(request) == self.daemon_request =>
                        {
                            self.daemon_request = None;
                            match result {
                                Ok(PlatformValue::Json(status)) => self.daemon = Some(status),
                                Ok(_) => {
                                    return Err("The daemon status had an unexpected shape".into());
                                }
                                Err(error) => {
                                    return Err(format!("Daemon status failed: {}", error.message));
                                }
                            }
                        }
                        PlatformAction::Event(PlatformEvent::QuitCancelled) => {
                            self.quit_cancelled = true
                        }
                        PlatformAction::Event(PlatformEvent::QuitRequested {
                            reason: QuitReason::Quit,
                            ..
                        }) if self.step == Step::LastQuit => self.quit_requested = true,
                        PlatformAction::Event(PlatformEvent::HostError { message }) => {
                            self.record("desktop-error", Value::String(message.clone()));
                            self.errors.push(message);
                        }
                        _ => {}
                    }
                }
            }
            Event::WindowGeomChange(e) => {
                if (ctx.shell)(MAIN_WINDOW_ID).and_then(|s| s.os_window()) == Some(e.window_id) {
                    self.main_size = e.new_geom.inner_size;
                }
            }
            Event::WindowCloseRequested(e)
                if matches!(self.step, Step::GuardedLastClose | Step::LastQuit)
                    && (ctx.shell)(&self.second).and_then(|s| s.os_window())
                        == Some(e.window_id) =>
            {
                require(
                    !e.accept_close.get(),
                    "Native close bypassed the shell's close flow",
                )?;
                self.native_close_received = true;
            }
            Event::Draw(_) => {
                if let Some(present) = self.present.as_mut()
                    && !present.drawn
                {
                    present.drawn = true;
                    cx.new_next_frame();
                }
            }
            Event::NextFrame(_) => {
                if self.present.as_ref().is_some_and(|p| p.drawn) {
                    let present = self.present.take().unwrap();
                    self.record(present.phase, present.value);
                }
            }
            _ => {}
        }
        // Several steps can complete on one event.
        for _ in 0..8 {
            if !self.advance(cx, ctx)? {
                break;
            }
        }
        if self.step == Step::Done {
            cx.stop_timer(self.poll);
        }
        Ok(())
    }

    /// Runs the current step once; `true` when it moved on.
    fn advance(&mut self, cx: &mut Cx, ctx: &Ctx) -> Result<bool, String> {
        let main = (ctx.shell)(MAIN_WINDOW_ID).ok_or("The first window is missing")?;
        match self.step {
            Step::Startup => {
                if !self.visible(ctx, MAIN_WINDOW_ID) {
                    return self.waiting("first visible session shell in the primary window");
                }
                require(
                    shell::window_states().len() == 1,
                    "The primary process did not start with exactly one window",
                )?;
                require(
                    self.errors.is_empty(),
                    &format!("Desktop startup failed: {:?}", self.errors),
                )?;
                self.query_daemon();
                self.go(Step::StartupDaemon)
            }
            Step::StartupDaemon => {
                let Some(status) = self.daemon.take() else {
                    return self.waiting("desktop daemon status");
                };
                if status.get("status").and_then(Value::as_str) != Some("running") {
                    // The independent daemon must be ready for the app to discover it.
                    if Instant::now() > self.deadline {
                        return Err(format!(
                            "Desktop did not observe the independent daemon: {status}"
                        ));
                    }
                    self.next_query = Some(Instant::now() + Duration::from_millis(500));
                    return Ok(false);
                }
                let pid = status["pid"].as_u64().filter(|pid| *pid != 0);
                let listen = status["listen"]
                    .as_str()
                    .filter(|listen| !listen.is_empty());
                let (Some(pid), Some(listen)) = (pid, listen) else {
                    return Err(format!(
                        "Independent daemon has no PID or listener: {status}"
                    ));
                };
                self.daemon_identity = Some((pid, listen.to_owned()));
                write_atomic(
                    &self.proof,
                    "app-daemon-status.json",
                    status.to_string().as_bytes(),
                )
                .map_err(|e| format!("Writing app-daemon-status.json failed: {e}"))?;
                let windows = shell::window_states().len();
                self.record(
                    "native-single-primary-user-engine",
                    json!({ "primary": MAIN_WINDOW_ID, "windowCount": windows, "daemon": status }),
                );
                self.go(Step::ColdStart)
            }
            Step::ColdStart => {
                let path = self.proof.display().to_string();
                let routed = hosts::hosts(ctx.state).iter().any(|h| {
                    h.is_local()
                        && h.projects().iter().any(|p| {
                            p.get("projectRootPath").and_then(Value::as_str) == Some(path.as_str())
                        })
                });
                if !routed {
                    return self
                        .waiting("cold-start project routed after shell and connection readiness");
                }
                self.record("primary-cold-start-project-route", Value::String(path));
                let window = main
                    .os_window()
                    .ok_or("The first window has no OS window")?;
                self.restore_size = match main.drawn_layout() {
                    _ if self.main_size.x > 0.0 => self.main_size,
                    Some((size, _)) => size,
                    None => return Err("The first window has no size".into()),
                };
                cx.push_unique_platform_op(CxOsOp::ResizeWindow(window, dvec2(412.0, 915.0)));
                self.go(Step::SheetCompact)
            }
            Step::SheetCompact => {
                let compact = main
                    .drawn_layout()
                    .is_some_and(|(size, compact)| compact && size.x <= 412.5);
                if !compact {
                    return self.waiting("compact session shell");
                }
                let detail = json!({ "type": "plain_text", "label": WAIT_COMMAND, "text": "" });
                self.sheet = Some(timeline::show_tool_call_sheet(
                    cx,
                    MAIN_WINDOW_ID,
                    "Wait",
                    &detail,
                ));
                self.go(Step::SheetOpen)
            }
            Step::SheetOpen => {
                if !timeline::tool_call_sheet_settled(self.sheet.unwrap()) {
                    return self.waiting("full wait command in the opened sheet");
                }
                self.present(cx, "label-only-wait-command-sheet", Value::Null);
                self.go(Step::SheetPresented)
            }
            Step::SheetPresented => {
                if self.present.is_some() {
                    return self.waiting("presenting the wait command sheet");
                }
                self.go(Step::SheetCaptured)
            }
            Step::SheetCaptured => {
                if !self.captured("label-only-wait-command-sheet") {
                    return self.waiting("capturing wait command sheet");
                }
                timeline::close_tool_call_sheet(cx, self.sheet.take().unwrap());
                let window = main
                    .os_window()
                    .ok_or("The first window has no OS window")?;
                cx.push_unique_platform_op(CxOsOp::ResizeWindow(window, self.restore_size));
                self.go(Step::SheetRestored)
            }
            Step::SheetRestored => {
                let restored = main.drawn_layout().is_some_and(|(size, _)| size.x > 412.5);
                if !restored {
                    return self.waiting("restoring the window size");
                }
                self.open_second()
            }
            Step::SecondOpen => {
                let Some(second) = self.new_visible_window(ctx) else {
                    return self.waiting("second actual visible user window");
                };
                self.second = second;
                CLOSE_GUARD.with(|g| g.set(true));
                main.request_close();
                self.settle = Some(Instant::now() + Duration::from_millis(500));
                self.go(Step::GuardClose)
            }
            Step::GuardClose => {
                if self.settle.is_some_and(|t| Instant::now() < t) {
                    return Ok(false);
                }
                self.settle = None;
                require(
                    self.visible(ctx, MAIN_WINDOW_ID),
                    "Closing the primary ignored a dirty-document cancellation",
                )?;
                self.quit_cancelled = false;
                let platform =
                    paseo_gui_platform::platform().ok_or("The platform is not running")?;
                platform.request_quit(QuitReason::Quit, false);
                self.go(Step::GuardQuit)
            }
            Step::GuardQuit => {
                if !self.quit_cancelled {
                    return self.waiting("dirty-document guard cancelling Quit");
                }
                require(
                    self.visible(ctx, MAIN_WINDOW_ID) && self.visible(ctx, &self.second),
                    "Cancelled Quit did not keep both windows visible",
                )?;
                self.query_daemon();
                self.go(Step::GuardDaemon)
            }
            Step::GuardDaemon => {
                if !self.same_daemon("Cancelled Quit hid the primary or stopped the daemon")? {
                    return self.waiting("desktop daemon status");
                }
                CLOSE_GUARD.with(|g| g.set(false));
                self.present(cx, "primary-close-and-quit-cancellation", Value::Null);
                self.go(Step::GuardCaptured)
            }
            Step::GuardCaptured => {
                if !self.captured("primary-close-and-quit-cancellation") {
                    return self.waiting("capturing the guarded windows");
                }
                main.request_close();
                self.go(Step::PrimaryDormant)
            }
            Step::PrimaryDormant => {
                if shell::window_state(MAIN_WINDOW_ID) != Some(WindowState::Dormant) {
                    return self
                        .waiting("primary retained but hidden while second window stays open");
                }
                require(
                    self.visible(ctx, &self.second) && shell::window_states().len() == 2,
                    "Closing the primary destroyed another window or the owning window",
                )?;
                // New Window from the second window.
                shell::request_window(shell::WindowRequest::default());
                self.go(Step::PrimaryReused)
            }
            Step::PrimaryReused => {
                if !self.visible(ctx, MAIN_WINDOW_ID) {
                    return self.waiting("secondary New Window restores the retained primary");
                }
                require(
                    shell::window_states().len() == 2,
                    "New Window failed to reuse the retained primary",
                )?;
                let second = (ctx.shell)(&self.second).ok_or("The second window is missing")?;
                second.request_close();
                self.go(Step::SecondTornDown)
            }
            Step::SecondTornDown => {
                if shell::window_states().len() != 1 {
                    return self
                        .waiting("secondary window destroyed while primary continues rendering");
                }
                require(
                    self.visible(ctx, MAIN_WINDOW_ID),
                    "Secondary close hid the primary",
                )?;
                self.present(cx, "primary-reuse-and-secondary-teardown", Value::Null);
                self.go(Step::ReusePresented)
            }
            Step::ReusePresented => {
                if self.present.is_some() {
                    return self.waiting("presenting the primary after secondary teardown");
                }
                self.go(Step::ReuseCaptured)
            }
            Step::ReuseCaptured => {
                if !self.captured("primary-reuse-and-secondary-teardown") {
                    return self.waiting("capturing the primary after secondary teardown");
                }
                shell::request_window(shell::WindowRequest::default());
                self.go(Step::ReplacementOpen)
            }
            Step::ReplacementOpen => {
                let Some(second) = self.new_visible_window(ctx) else {
                    return self.waiting("replacement secondary visible");
                };
                self.second = second;
                main.request_close();
                self.go(Step::ReplacementDormant)
            }
            Step::ReplacementDormant => {
                if shell::window_state(MAIN_WINDOW_ID) != Some(WindowState::Dormant) {
                    return self.waiting("primary hidden again");
                }
                self.query_daemon();
                self.go(Step::ReplacementDaemon)
            }
            Step::ReplacementDaemon => {
                if !self.same_daemon("Closing one window stopped or replaced the daemon")? {
                    return self.waiting("desktop daemon status");
                }
                self.present(
                    cx,
                    "multiwindow-close-retains-daemon",
                    json!({ "retained": self.second }),
                );
                self.go(Step::RetainPresented)
            }
            Step::RetainPresented => {
                if self.present.is_some() {
                    return self.waiting("presenting the retained window");
                }
                self.go(Step::RetainCaptured)
            }
            Step::RetainCaptured => {
                if !self.captured("multiwindow-close-retains-daemon") {
                    return self.waiting("capturing the retained window");
                }
                require(
                    self.visible(ctx, &self.second),
                    "The last user window is not visible",
                )?;
                require(
                    self.errors.is_empty(),
                    &format!("Desktop lifecycle failed: {:?}", self.errors),
                )?;
                if self.logout {
                    self.record("expect-session-logout-quit", Value::Null);
                    self.session_command("logout")?;
                    return self.go(Step::Done);
                }
                CLOSE_GUARD.with(|g| g.set(true));
                self.session_command("close-last-window-with-guard")?;
                self.go(Step::GuardedLastClose)
            }
            Step::GuardedLastClose => {
                if !self.native_close_received {
                    return self.waiting("window manager close event for the guarded last window");
                }
                require(
                    self.visible(ctx, &self.second),
                    "Native close ignored the last window's dirty-document guard",
                )?;
                self.query_daemon();
                self.go(Step::GuardedLastDaemon)
            }
            Step::GuardedLastDaemon => {
                if !self.same_daemon("Guarded last-window close stopped or replaced the daemon")? {
                    return self.waiting("desktop daemon status");
                }
                self.present(cx, "guarded-last-window-close", Value::Null);
                self.go(Step::GuardedLastCaptured)
            }
            Step::GuardedLastCaptured => {
                if !self.captured("guarded-last-window-close") {
                    return self.waiting("capturing the guarded last window");
                }
                CLOSE_GUARD.with(|g| g.set(false));
                self.native_close_received = false;
                self.session_command("close-last-window")?;
                self.go(Step::LastQuit)
            }
            Step::LastQuit => {
                if !self.native_close_received || !self.quit_requested {
                    return self.waiting("native last-window close to request guarded Quit");
                }
                self.record("expect-last-window-close-quit", Value::Null);
                self.go(Step::Done)
            }
            Step::Done => Ok(false),
        }
    }

    fn go(&mut self, step: Step) -> Result<bool, String> {
        self.step = step;
        self.deadline = Instant::now() + WAIT;
        Ok(step != Step::Done)
    }

    /// Opens a second window and waits for it.
    fn open_second(&mut self) -> Result<bool, String> {
        shell::request_window(shell::WindowRequest::default());
        self.go(Step::SecondOpen)
    }

    fn waiting(&mut self, label: &str) -> Result<bool, String> {
        if Instant::now() > self.deadline {
            return Err(format!("Timed out: {label}"));
        }
        if self.daemon_request.is_none() && self.next_query.is_some_and(|t| Instant::now() >= t) {
            self.next_query = None;
            self.query_daemon();
        }
        Ok(false)
    }

    /// Logically open, shown, and drawn at least once.
    fn visible(&self, ctx: &Ctx, id: &str) -> bool {
        shell::window_state(id) == Some(WindowState::Visible)
            && (ctx.shell)(id).is_some_and(|s| s.os_window().is_some())
    }

    /// A visible secondary window other than the current `second`.
    fn new_visible_window(&self, ctx: &Ctx) -> Option<String> {
        shell::window_states()
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| id != MAIN_WINDOW_ID && *id != self.second && self.visible(ctx, id))
    }

    fn query_daemon(&mut self) {
        self.daemon = None;
        if let Some(platform) = paseo_gui_platform::platform() {
            self.daemon_request = Some(platform.desktop_invoke("desktop_daemon_status", json!({})));
        }
    }

    /// Checks the observed running daemon still has its original PID and listen address.
    fn same_daemon(&mut self, message: &str) -> Result<bool, String> {
        let Some(status) = self.daemon.take() else {
            return Ok(false);
        };
        require(
            status["status"] == "running"
                && self.daemon_identity.as_ref().is_some_and(|(pid, listen)| {
                    status["pid"].as_u64() == Some(*pid)
                        && status["listen"].as_str() == Some(listen.as_str())
                }),
            message,
        )?;
        Ok(true)
    }

    fn session_command(&self, action: &str) -> Result<(), String> {
        let command = json!({ "action": action }).to_string();
        write_atomic(&self.proof, "session-command.json", command.as_bytes())
            .map_err(|e| format!("Writing session-command.json failed: {e}"))
    }

    fn present(&mut self, cx: &mut Cx, phase: &'static str, value: Value) {
        self.present = Some(Present {
            phase,
            value,
            drawn: false,
        });
        // The capture phase repaints every window it records.
        #[allow(clippy::disallowed_methods)]
        cx.redraw_all();
    }

    fn captured(&self, phase: &str) -> bool {
        self.proof.join(format!("captured-{phase}")).exists()
    }

    /// Appends `{"phase", "value"}` to `events.jsonl` and echoes it to stdout.
    fn record(&self, phase: &str, value: Value) {
        let row = json!({ "phase": phase, "value": value }).to_string();
        println!("{row}");
        let mut contents = std::fs::read(self.proof.join("events.jsonl")).unwrap_or_default();
        contents.extend_from_slice(row.as_bytes());
        contents.push(b'\n');
        if let Err(error) = write_atomic(&self.proof, "events.jsonl", &contents) {
            eprintln!("Writing events.jsonl failed: {error}");
        }
    }

    fn fail(&mut self, error: &str) {
        self.record("FAIL", json!({ "error": error }));
        std::process::exit(1);
    }
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

/// Writes `<name>.<pid>.tmp` in `dir`, then renames it over `name`.
fn write_atomic(dir: &std::path::Path, name: &str, contents: &[u8]) -> std::io::Result<()> {
    let target = dir.join(name);
    let temporary = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, &target)
}
