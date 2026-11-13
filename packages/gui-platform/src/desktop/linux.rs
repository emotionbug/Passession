//! Linux desktop integration (original my_application.cc, the Linux
//! paseo_desktop_integrations plugin, flutter_local_notifications Linux and
//! tray_manager). D-Bus runs on a dedicated runtime that lives for the process.

use super::{deliver, notification_clicked, Activation, DesktopNotification, NativeSignal};
use crate::{
    action::*,
    platform::{Completion, Shared},
    Bootstrap,
};
use futures_util::StreamExt as _;
use std::{
    collections::HashMap,
    ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CString, OsString},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use zbus::zvariant::{OwnedValue, Value as ZValue};

const BUS_NAME: &str = crate::paths::DESKTOP_APP_ID;
const OBJECT_PATH: &str = "/sh/paseo/sessions/desktop";
/// Desktop-entry / launcher identity used by the original Linux runner.
const DESKTOP_ENTRY: &str = "sh.paseo.Paseo";

static BUS: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("paseo-dbus")
        .enable_all()
        .build()
        .expect("D-Bus runtime")
});

static SESSION: tokio::sync::OnceCell<zbus::Connection> = tokio::sync::OnceCell::const_new();
static SYSTEM: tokio::sync::OnceCell<zbus::Connection> = tokio::sync::OnceCell::const_new();

async fn session() -> Result<zbus::Connection, PlatformError> {
    SESSION
        .get_or_try_init(|| async { zbus::Connection::session().await })
        .await
        .cloned()
        .map_err(|e| PlatformError::new("session_bus_unavailable", e.to_string()))
}

async fn system() -> Result<zbus::Connection, PlatformError> {
    SYSTEM
        .get_or_try_init(|| async { zbus::Connection::system().await })
        .await
        .cloned()
        .map_err(|e| PlatformError::new("system_bus_unavailable", e.to_string()))
}

fn dbus_error(error: zbus::Error) -> PlatformError {
    PlatformError::native(error.to_string())
}

pub(crate) fn prepare_process() -> anyhow::Result<()> {
    Ok(())
}

// ----- single instance -----

struct SessionsInterface;

#[zbus::interface(name = "sh.paseo.Sessions1")]
impl SessionsInterface {
    /// GApplication-style command line forwarding from a later launch.
    async fn command_line(&self, argv: Vec<String>, cwd: String) -> bool {
        deliver(NativeSignal::Activation(parse_activation(&argv, Some(Path::new(&cwd)), false)));
        true
    }
}

struct ApplicationInterface;

#[zbus::interface(name = "org.freedesktop.Application")]
impl ApplicationInterface {
    async fn activate(&self, _platform_data: HashMap<String, OwnedValue>) {
        deliver(NativeSignal::Activation(Activation::default()));
    }
    async fn open(&self, uris: Vec<String>, _platform_data: HashMap<String, OwnedValue>) {
        for uri in uris {
            let activation = if uri.to_ascii_lowercase().starts_with("paseo:") {
                Activation { url: Some(uri), ..Default::default() }
            } else if let Some(path) = url::Url::parse(&uri).ok().and_then(|u| u.to_file_path().ok()).filter(|p| p.is_dir()) {
                Activation { project_path: Some(path.display().to_string()), ..Default::default() }
            } else {
                continue;
            };
            deliver(NativeSignal::Activation(activation));
        }
    }
    async fn activate_action(&self, action_name: String, _parameter: Vec<OwnedValue>, _platform_data: HashMap<String, OwnedValue>) {
        if action_name == "quit" {
            deliver(NativeSignal::QuitRequestedByOs);
        }
    }
}

/// my_application.cc `command_line`: a paseo: URL, else the directory after
/// --open-project/--paseo-project, else the first existing directory argument.
pub(crate) fn parse_activation(argv: &[String], cwd: Option<&Path>, initial: bool) -> Activation {
    let mut index = 1;
    while index < argv.len() {
        let arg = &argv[index];
        if arg.to_ascii_lowercase().starts_with("paseo:") {
            return Activation { url: Some(arg.clone()), project_path: None, initial };
        }
        let candidate = if arg == "--open-project" || arg == "--paseo-project" {
            index += 1;
            match argv.get(index) {
                Some(value) => value.clone(),
                None => break,
            }
        } else if arg.starts_with('-') {
            index += 1;
            continue;
        } else {
            arg.clone()
        };
        let path = Path::new(&candidate);
        let absolute = if path.is_absolute() { path.to_path_buf() } else { cwd.map(|c| c.join(path)).unwrap_or_else(|| path.to_path_buf()) };
        if absolute.is_dir() {
            let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
            return Activation { url: None, project_path: Some(canonical.display().to_string()), initial };
        }
        index += 1;
    }
    Activation { initial, ..Default::default() }
}

pub(crate) fn bootstrap(argv: &[String]) -> Bootstrap {
    let argv = argv.to_vec();
    BUS.block_on(async move {
        let connection = match session().await {
            Ok(connection) => connection,
            // Without a session bus there is no single-instance channel; this
            // process stays the only instance it can know about.
            Err(_) => return Bootstrap::Primary,
        };
        let server = connection.object_server();
        if server.at(OBJECT_PATH, SessionsInterface).await.is_err() || server.at(OBJECT_PATH, ApplicationInterface).await.is_err() {
            return Bootstrap::Primary;
        }
        let reply = connection
            .request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
            .await;
        match reply {
            Ok(zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner) | Err(_) => Bootstrap::Primary,
            Ok(_) => {
                let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                let forwarded = tokio::time::timeout(
                    Duration::from_secs(30),
                    connection.call_method(Some(BUS_NAME), OBJECT_PATH, Some("sh.paseo.Sessions1"), "CommandLine", &(argv, cwd)),
                )
                .await;
                let accepted = match &forwarded {
                    Ok(Ok(reply)) => reply.body().deserialize::<bool>().unwrap_or(false),
                    _ => false,
                };
                Bootstrap::Forwarded { exit_code: if accepted { 0 } else { 1 } }
            }
        }
    })
}

// ----- lifecycle -----

pub(crate) fn start_lifecycle(shared: &Arc<Shared>) {
    let _ = shared;
    BUS.spawn(async {
        let result = async {
            let system = system().await?;
            let login = zbus::Proxy::new(&system, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager")
                .await
                .map_err(dbus_error)?;
            let mut shutdown = login.receive_signal("PrepareForShutdown").await.map_err(dbus_error)?;
            tokio::spawn(async move {
                while let Some(message) = shutdown.next().await {
                    let shutting_down = message.body().deserialize::<bool>().unwrap_or(false);
                    deliver(if shutting_down { NativeSignal::SessionEnded } else { NativeSignal::SessionEndingCancelled });
                }
            });
            if start_gnome_session().await.is_ok() {
                return Ok(());
            }
            xsmp::start().map_err(|_| {
                PlatformError::new(
                    "logout_unavailable",
                    "No GNOME SessionManager or usable XSMP session manager is available. Close-to-tray must stay disabled to avoid intercepting logout. XSMP needs libSM/libICE runtime libraries and SESSION_MANAGER.",
                )
            })
        }
        .await;
        deliver(NativeSignal::LifecycleStarted(result));
    });
}

async fn start_gnome_session() -> Result<(), PlatformError> {
    let connection = session().await?;
    let manager = zbus::Proxy::new(&connection, "org.gnome.SessionManager", "/org/gnome/SessionManager", "org.gnome.SessionManager")
        .await
        .map_err(dbus_error)?;
    let startup_id = std::env::var("DESKTOP_AUTOSTART_ID").unwrap_or_default();
    let client: zbus::zvariant::OwnedObjectPath = tokio::time::timeout(
        Duration::from_secs(2),
        manager.call("RegisterClient", &(DESKTOP_ENTRY, startup_id.as_str())),
    )
    .await
    .map_err(|_| PlatformError::native("GNOME SessionManager did not answer."))?
    .map_err(dbus_error)?;
    let private = zbus::Proxy::new(&connection, "org.gnome.SessionManager", client.clone(), "org.gnome.SessionManager.ClientPrivate")
        .await
        .map_err(dbus_error)?;
    let mut signals = private.receive_all_signals().await.map_err(dbus_error)?;
    tokio::spawn(async move {
        while let Some(message) = signals.next().await {
            let header = message.header();
            let Some(member) = header.member().map(|m| m.as_str().to_owned()) else { continue };
            match member.as_str() {
                "CancelEndSession" => deliver(NativeSignal::SessionEndingCancelled),
                "Stop" => deliver(NativeSignal::SessionEnded),
                "QueryEndSession" | "EndSession" => {
                    deliver(if member == "QueryEndSession" { NativeSignal::SessionEnding } else { NativeSignal::SessionEnded });
                    // Never veto the OS; teardown is best effort within its deadline.
                    let response = tokio::time::timeout(Duration::from_secs(2), private.call_method("EndSessionResponse", &(true, ""))).await;
                    if !matches!(response, Ok(Ok(_))) {
                        deliver(NativeSignal::LifecycleError("GNOME SessionManager rejected the end-session response.".into()));
                    }
                }
                _ => {}
            }
        }
    });
    Ok(())
}

/// XSMP fallback through dlopen'd libSM/libICE (no build-time headers).
mod xsmp {
    use super::{deliver, NativeSignal};
    use std::ffi::{c_char, c_int, c_ulong, c_void};

    type SmcConn = *mut c_void;
    type IceConn = *mut c_void;

    #[repr(C)]
    struct Callback<F> {
        callback: F,
        client_data: *mut c_void,
    }
    type SaveYourself = unsafe extern "C" fn(SmcConn, *mut c_void, c_int, c_int, c_int, c_int);
    type Simple = unsafe extern "C" fn(SmcConn, *mut c_void);
    #[repr(C)]
    struct SmcCallbacks {
        save_yourself: Callback<SaveYourself>,
        die: Callback<Simple>,
        save_complete: Callback<Simple>,
        shutdown_cancelled: Callback<Simple>,
    }

    static SAVE_DONE: std::sync::OnceLock<unsafe extern "C" fn(SmcConn, c_int)> = std::sync::OnceLock::new();

    unsafe extern "C" fn save_yourself(connection: SmcConn, _: *mut c_void, _: c_int, shutdown: c_int, _: c_int, _: c_int) {
        if shutdown != 0 {
            deliver(NativeSignal::SessionEnding);
        }
        if let Some(done) = SAVE_DONE.get() {
            unsafe { done(connection, 1) };
        }
    }
    unsafe extern "C" fn die(_: SmcConn, _: *mut c_void) {
        deliver(NativeSignal::SessionEnded);
    }
    unsafe extern "C" fn save_complete(_: SmcConn, _: *mut c_void) {}
    unsafe extern "C" fn cancelled(_: SmcConn, _: *mut c_void) {
        deliver(NativeSignal::SessionEndingCancelled);
    }

    pub(super) fn start() -> Result<(), String> {
        if std::env::var_os("SESSION_MANAGER").is_none() {
            return Err("SESSION_MANAGER is not set".into());
        }
        let sm = unsafe { libloading::Library::new("libSM.so.6") }.map_err(|e| e.to_string())?;
        let ice = unsafe { libloading::Library::new("libICE.so.6") }.map_err(|e| e.to_string())?;
        unsafe {
            let open: libloading::Symbol<
                unsafe extern "C" fn(*const c_char, *mut c_void, c_int, c_int, c_ulong, *mut SmcCallbacks, *const c_char, *mut *mut c_char, c_int, *mut c_char) -> SmcConn,
            > = sm.get(b"SmcOpenConnection\0").map_err(|e| e.to_string())?;
            let get_ice: libloading::Symbol<unsafe extern "C" fn(SmcConn) -> IceConn> =
                sm.get(b"SmcGetIceConnection\0").map_err(|e| e.to_string())?;
            let done: libloading::Symbol<unsafe extern "C" fn(SmcConn, c_int)> =
                sm.get(b"SmcSaveYourselfDone\0").map_err(|e| e.to_string())?;
            let fd: libloading::Symbol<unsafe extern "C" fn(IceConn) -> c_int> =
                ice.get(b"IceConnectionNumber\0").map_err(|e| e.to_string())?;
            let process: libloading::Symbol<unsafe extern "C" fn(IceConn, *mut c_void, *mut c_int) -> c_int> =
                ice.get(b"IceProcessMessages\0").map_err(|e| e.to_string())?;
            let _ = SAVE_DONE.set(*done);
            let mut callbacks = SmcCallbacks {
                save_yourself: Callback { callback: save_yourself, client_data: std::ptr::null_mut() },
                die: Callback { callback: die, client_data: std::ptr::null_mut() },
                save_complete: Callback { callback: save_complete, client_data: std::ptr::null_mut() },
                shutdown_cancelled: Callback { callback: cancelled, client_data: std::ptr::null_mut() },
            };
            let mut client_id: *mut c_char = std::ptr::null_mut();
            let mut error = [0 as c_char; 256];
            // SmcSaveYourselfProcMask | SmcDieProcMask | SmcShutdownCancelledProcMask
            let mask: c_ulong = (1 << 0) | (1 << 1) | (1 << 3);
            let connection = open(
                std::ptr::null(),
                std::ptr::null_mut(),
                1,
                0,
                mask,
                &mut callbacks,
                std::ptr::null(),
                &mut client_id,
                error.len() as c_int,
                error.as_mut_ptr(),
            );
            if !client_id.is_null() {
                libc::free(client_id.cast());
            }
            if connection.is_null() {
                return Err(std::ffi::CStr::from_ptr(error.as_ptr()).to_string_lossy().into_owned());
            }
            let ice_connection = get_ice(connection);
            let descriptor = fd(ice_connection);
            let process = *process;
            let ice_addr = ice_connection as usize;
            // The loop thread owns the libraries for as long as the session
            // manager connection is serviced.
            std::thread::Builder::new()
                .name("paseo-xsmp".into())
                .spawn(move || {
                    let _libraries = (sm, ice);
                    let ice_connection = ice_addr as IceConn;
                    loop {
                        let mut poll = libc::pollfd { fd: descriptor, events: libc::POLLIN, revents: 0 };
                        if libc::poll(&mut poll, 1, -1) < 0 {
                            continue;
                        }
                        if poll.revents & (libc::POLLERR | libc::POLLHUP) != 0
                            || process(ice_connection, std::ptr::null_mut(), std::ptr::null_mut()) != 0
                        {
                            deliver(NativeSignal::LifecycleError("The XSMP session manager disconnected".into()));
                            break;
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

pub(crate) fn termination_decided(_allowed: bool) {}

// ----- notifications -----

/// Server notification ids → (our id, click payload).
static SHOWN: Mutex<Vec<(u32, i32, String)>> = Mutex::new(Vec::new());

pub(crate) fn init_notifications(_shared: &Arc<Shared>) -> Result<(), PlatformError> {
    // flutter_local_notifications delivers Linux actions through its live D-Bus
    // callback; there is no cold-launch notification query.
    BUS.spawn(async {
        let Ok(connection) = session().await else { return };
        let Ok(proxy) = zbus::Proxy::new(&connection, "org.freedesktop.Notifications", "/org/freedesktop/Notifications", "org.freedesktop.Notifications").await else {
            return;
        };
        let Ok(mut actions) = proxy.receive_signal("ActionInvoked").await else { return };
        let Ok(mut closed) = proxy.receive_signal("NotificationClosed").await else { return };
        tokio::spawn(async move {
            while let Some(message) = closed.next().await {
                if let Ok((server_id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                    SHOWN.lock().unwrap().retain(|(id, _, _)| *id != server_id);
                }
            }
        });
        while let Some(message) = actions.next().await {
            let Ok((server_id, action)) = message.body().deserialize::<(u32, String)>() else { continue };
            let found = SHOWN.lock().unwrap().iter().find(|(id, _, _)| *id == server_id).cloned();
            if let Some((_, id, payload)) = found {
                notification_clicked(&payload, Some(id.into()), Some(action));
            }
        }
    });
    Ok(())
}

pub(crate) async fn show_notification(notification: &DesktopNotification) -> Result<(), PlatformError> {
    let title = notification.title.clone();
    let body = notification.body.clone().unwrap_or_default();
    let sound = notification.sound;
    let id = notification.id;
    let payload = notification.payload.clone();
    let replaces = SHOWN.lock().unwrap().iter().find(|(_, own, _)| *own == id).map(|(server, _, _)| *server).unwrap_or(0);
    BUS.spawn(async move {
        let connection = session().await?;
        let proxy = zbus::Proxy::new(&connection, "org.freedesktop.Notifications", "/org/freedesktop/Notifications", "org.freedesktop.Notifications")
            .await
            .map_err(dbus_error)?;
        let mut hints: HashMap<&str, ZValue<'_>> = HashMap::new();
        hints.insert("desktop-entry", ZValue::from(DESKTOP_ENTRY));
        if sound {
            hints.insert("sound-name", ZValue::from("message-new-instant"));
        } else {
            hints.insert("suppress-sound", ZValue::from(true));
        }
        let server_id: u32 = proxy
            .call(
                "Notify",
                &("Passession", replaces, DESKTOP_ENTRY, title.as_str(), body.as_str(), vec!["default", "Open Passession"], hints, -1i32),
            )
            .await
            .map_err(dbus_error)?;
        let mut shown = SHOWN.lock().unwrap();
        shown.retain(|(server, own, _)| *server != server_id && *own != id);
        shown.push((server_id, id, payload));
        if shown.len() > 512 {
            shown.remove(0);
        }
        Ok(())
    })
    .await
    .map_err(|e| PlatformError::native(e.to_string()))?
}

/// Linux manages notification permission in system settings.
pub(crate) fn request_notification_permission(_shared: &Arc<Shared>, done: Completion) {
    done.ok(PlatformValue::Permission(PermissionState::Granted));
}

/// com.canonical.Unity.LauncherEntry (Unity, KDE and several GNOME docks).
/// Delivery is not a guarantee that the shell shows badges.
pub(crate) fn set_badge_count(_shared: &Arc<Shared>, count: u32, done: Completion) {
    BUS.spawn(async move {
        let result = async {
            let connection = session().await?;
            let mut properties: HashMap<&str, ZValue<'_>> = HashMap::new();
            properties.insert("count", ZValue::from(i64::from(count)));
            properties.insert("count-visible", ZValue::from(count > 0));
            connection
                .emit_signal(
                    None::<&str>,
                    "/sh/paseo/Paseo",
                    "com.canonical.Unity.LauncherEntry",
                    "Update",
                    &(format!("application://{DESKTOP_ENTRY}.desktop"), properties),
                )
                .await
                .map_err(|e| PlatformError::new("badge_failed", e.to_string()))
        }
        .await;
        done.finish(result.map(|()| PlatformValue::None));
    });
}

// ----- idle -----

pub(crate) fn idle_supported() -> bool {
    std::env::var("XDG_SESSION_TYPE").ok().as_deref() != Some("wayland")
}

#[repr(C)]
struct ScreenSaverInfo {
    window: c_ulong,
    state: c_int,
    kind: c_int,
    til_or_since: c_ulong,
    idle: c_ulong,
    event_mask: c_ulong,
}

struct X11 {
    _x11: libloading::Library,
    open_display: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    close_display: unsafe extern "C" fn(*mut c_void) -> c_int,
    default_root: unsafe extern "C" fn(*mut c_void) -> c_ulong,
    default_screen: unsafe extern "C" fn(*mut c_void) -> c_int,
    intern_atom: unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_ulong,
    get_window_property: unsafe extern "C" fn(*mut c_void, c_ulong, c_ulong, c_long, c_long, c_int, c_ulong, *mut c_ulong, *mut c_int, *mut c_ulong, *mut c_ulong, *mut *mut u8) -> c_int,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
    withdraw: unsafe extern "C" fn(*mut c_void, c_ulong, c_int) -> c_int,
    map_raised: unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int,
    move_window: unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_int) -> c_int,
    send_event: unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_long, *mut XClientMessageEvent) -> c_int,
    get_normal_hints: unsafe extern "C" fn(*mut c_void, c_ulong, *mut XSizeHints, *mut c_long) -> c_int,
    set_normal_hints: unsafe extern "C" fn(*mut c_void, c_ulong, *mut XSizeHints),
    translate_coordinates: unsafe extern "C" fn(*mut c_void, c_ulong, c_ulong, c_int, c_int, *mut c_int, *mut c_int, *mut c_ulong) -> c_int,
    flush: unsafe extern "C" fn(*mut c_void) -> c_int,
    query_extension: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_int, *mut c_int, *mut c_int) -> c_int,
}

#[repr(C)]
struct XClientMessageEvent {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    message_type: c_ulong,
    format: c_int,
    data: [c_long; 5],
}

/// Xutil.h `XSizeHints`; preserve existing size constraints when pinning position.
#[repr(C)]
#[derive(Default)]
struct XSizeHints {
    flags: c_long,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    min_width: c_int,
    min_height: c_int,
    max_width: c_int,
    max_height: c_int,
    width_inc: c_int,
    height_inc: c_int,
    min_aspect: [c_int; 2],
    max_aspect: [c_int; 2],
    base_width: c_int,
    base_height: c_int,
    win_gravity: c_int,
}

impl X11 {
    fn load() -> Result<Self, String> {
        unsafe {
            let library = libloading::Library::new("libX11.so.6").map_err(|e| e.to_string())?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library.get($name).map_err(|e| e.to_string())?
                };
            }
            Ok(Self {
                open_display: symbol!(b"XOpenDisplay\0"),
                close_display: symbol!(b"XCloseDisplay\0"),
                default_root: symbol!(b"XDefaultRootWindow\0"),
                default_screen: symbol!(b"XDefaultScreen\0"),
                intern_atom: symbol!(b"XInternAtom\0"),
                get_window_property: symbol!(b"XGetWindowProperty\0"),
                free: symbol!(b"XFree\0"),
                withdraw: symbol!(b"XWithdrawWindow\0"),
                map_raised: symbol!(b"XMapRaised\0"),
                move_window: symbol!(b"XMoveWindow\0"),
                send_event: symbol!(b"XSendEvent\0"),
                get_normal_hints: symbol!(b"XGetWMNormalHints\0"),
                set_normal_hints: symbol!(b"XSetWMNormalHints\0"),
                translate_coordinates: symbol!(b"XTranslateCoordinates\0"),
                flush: symbol!(b"XFlush\0"),
                query_extension: symbol!(b"XQueryExtension\0"),
                _x11: library,
            })
        }
    }
}

static X11_LIB: LazyLock<Result<X11, String>> = LazyLock::new(X11::load);
/// libXss `XScreenSaverQueryInfo(display, drawable, info)`.
type QueryScreenSaverInfo = unsafe extern "C" fn(*mut c_void, c_ulong, *mut ScreenSaverInfo) -> c_int;
static XSS: LazyLock<Result<(libloading::Library, QueryScreenSaverInfo), String>> =
    LazyLock::new(|| unsafe {
        let library = libloading::Library::new("libXss.so.1")
            .map_err(|_| "System idle time requires the optional libXss.so.1 runtime library".to_owned())?;
        let query = *library
            .get::<QueryScreenSaverInfo>(b"XScreenSaverQueryInfo\0")
            .map_err(|_| "libXss does not export XScreenSaverQueryInfo".to_owned())?;
        Ok((library, query))
    });

/// A private display connection (Makepad keeps its own).
struct Display<'a> {
    x: &'a X11,
    raw: *mut c_void,
}

impl<'a> Display<'a> {
    fn open() -> Result<Self, PlatformError> {
        let x = X11_LIB.as_ref().map_err(|e| PlatformError::new("idle_unavailable", e.clone()))?;
        let raw = unsafe { (x.open_display)(std::ptr::null()) };
        if raw.is_null() {
            return Err(PlatformError::new("display_unavailable", "Cannot open the X11 display."));
        }
        Ok(Self { x, raw })
    }
}

impl Drop for Display<'_> {
    fn drop(&mut self) {
        unsafe { (self.x.close_display)(self.raw) };
    }
}

pub(crate) fn idle_milliseconds() -> Result<u64, PlatformError> {
    if !idle_supported() {
        return Err(PlatformError::new(
            "idle_unsupported",
            "Global idle time requires an X11 session; Wayland provides no portable global idle API",
        ));
    }
    let display = Display::open()?;
    let name = CString::new("MIT-SCREEN-SAVER").unwrap();
    let (mut opcode, mut event, mut error) = (0, 0, 0);
    if unsafe { (display.x.query_extension)(display.raw, name.as_ptr(), &mut opcode, &mut event, &mut error) } == 0 {
        return Err(PlatformError::new("idle_unavailable", "The XScreenSaver extension is unavailable"));
    }
    let (_, query) = XSS.as_ref().map_err(|e| PlatformError::new("idle_unavailable", e.clone()))?;
    let mut info = ScreenSaverInfo { window: 0, state: 0, kind: 0, til_or_since: 0, idle: 0, event_mask: 0 };
    let root = unsafe { (display.x.default_root)(display.raw) };
    if unsafe { query(display.raw, root, &mut info) } == 0 {
        return Err(PlatformError::new("idle_failed", "XScreenSaverQueryInfo failed"));
    }
    Ok(info.idle)
}

// ----- native windows (close-to-tray) -----

/// Makepad 1.0.0 leaves `CxOsOp::HideWindow` unimplemented on X11, so the app's
/// top-level windows are withdrawn/mapped directly, identified by WM_CLASS.
#[derive(Default)]
pub(crate) struct NativeWindows {
    hidden: Vec<c_ulong>,
}

fn wm_class() -> String {
    std::env::args_os()
        .next()
        .as_ref()
        .and_then(|arg| Path::new(arg).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Makepad".into())
}

impl NativeWindows {
    pub fn new() -> Self {
        Self::default()
    }

    fn property(display: &Display, window: c_ulong, name: &str, kind: c_ulong) -> Option<(Vec<u8>, c_int, c_ulong)> {
        let atom_name = CString::new(name).ok()?;
        let atom = unsafe { (display.x.intern_atom)(display.raw, atom_name.as_ptr(), 1) };
        if atom == 0 {
            return None;
        }
        let (mut actual_type, mut format, mut items, mut after) = (0 as c_ulong, 0 as c_int, 0 as c_ulong, 0 as c_ulong);
        let mut data: *mut u8 = std::ptr::null_mut();
        let status = unsafe {
            (display.x.get_window_property)(display.raw, window, atom, 0, 65536, 0, kind, &mut actual_type, &mut format, &mut items, &mut after, &mut data)
        };
        if status != 0 || data.is_null() {
            return None;
        }
        let unit = match format {
            8 => 1,
            16 => std::mem::size_of::<std::ffi::c_short>(),
            32 => std::mem::size_of::<c_long>(),
            _ => 0,
        };
        let bytes = unsafe { std::slice::from_raw_parts(data, items as usize * unit) }.to_vec();
        unsafe { (display.x.free)(data.cast()) };
        Some((bytes, format, items))
    }

    fn own_windows(display: &Display) -> Vec<c_ulong> {
        let root = unsafe { (display.x.default_root)(display.raw) };
        const XA_WINDOW: c_ulong = 33;
        const XA_STRING: c_ulong = 31;
        let Some((bytes, 32, items)) = Self::property(display, root, "_NET_CLIENT_LIST", XA_WINDOW) else { return Vec::new() };
        const WORD: usize = std::mem::size_of::<c_long>();
        let windows: Vec<c_ulong> = (0..items as usize)
            .map(|i| {
                let mut raw = [0u8; WORD];
                raw.copy_from_slice(&bytes[i * WORD..(i + 1) * WORD]);
                c_long::from_ne_bytes(raw) as c_ulong
            })
            .collect();
        let class = wm_class();
        windows
            .into_iter()
            .filter(|window| {
                Self::property(display, *window, "WM_CLASS", XA_STRING).is_some_and(|(bytes, _, _)| {
                    bytes.split(|b| *b == 0).nth(1).is_some_and(|c| c == class.as_bytes())
                })
            })
            .collect()
    }

    fn preserve_position(display: &Display, window: c_ulong) {
        let root = unsafe { (display.x.default_root)(display.raw) };
        let mut hints = XSizeHints::default();
        let (mut supplied, mut child) = (0, 0);
        unsafe {
            (display.x.get_normal_hints)(display.raw, window, &mut hints, &mut supplied);
            if (display.x.translate_coordinates)(display.raw, window, root, 0, 0, &mut hints.x, &mut hints.y, &mut child) != 0 {
                // Withdrawing removes the WM's placement record. On remap, keep
                // the client origin instead of letting it place a "new" window.
                hints.flags |= (1 << 0) | (1 << 9); // USPosition | PWinGravity
                hints.win_gravity = 10; // StaticGravity: client origin, not frame.
                (display.x.set_normal_hints)(display.raw, window, &mut hints);
            }
        }
    }

    fn restore_position(display: &Display, window: c_ulong) {
        let mut hints = XSizeHints::default();
        let mut supplied = 0;
        unsafe {
            if (display.x.get_normal_hints)(display.raw, window, &mut hints, &mut supplied) != 0
                && hints.flags & (1 << 0) != 0 && hints.flags & (1 << 9) != 0
                && hints.win_gravity == 10
            {
                // Withdrawal reparents the client at the old frame origin.
                // Modern WMs use that actual position, not the obsolete x/y
                // hint fields. Restore the client origin before remapping.
                (display.x.move_window)(display.raw, window, hints.x, hints.y);
            }
        }
    }

    pub fn hide_all(&mut self) {
        let Ok(display) = Display::open() else { return };
        let screen = unsafe { (display.x.default_screen)(display.raw) };
        for window in Self::own_windows(&display) {
            Self::preserve_position(&display, window);
            unsafe { (display.x.withdraw)(display.raw, window, screen) };
            if !self.hidden.contains(&window) {
                self.hidden.push(window);
            }
        }
        unsafe { (display.x.flush)(display.raw) };
    }

    pub fn show_all(&mut self) {
        let Ok(display) = Display::open() else { return };
        let root = unsafe { (display.x.default_root)(display.raw) };
        let active_name = CString::new("_NET_ACTIVE_WINDOW").unwrap();
        let active = unsafe { (display.x.intern_atom)(display.raw, active_name.as_ptr(), 0) };
        let mut targets = std::mem::take(&mut self.hidden);
        let hidden_count = targets.len();
        for window in Self::own_windows(&display) {
            if !targets.contains(&window) {
                targets.push(window);
            }
        }
        for (index, window) in targets.into_iter().enumerate() {
            if index < hidden_count {
                Self::restore_position(&display, window);
            }
            unsafe { (display.x.map_raised)(display.raw, window) };
            // EWMH activation request (source indication 2 = pager/user action).
            let mut event = XClientMessageEvent {
                kind: 33, // ClientMessage
                serial: 0,
                send_event: 1,
                display: display.raw,
                window,
                message_type: active,
                format: 32,
                data: [2, 0, 0, 0, 0],
            };
            // SubstructureRedirectMask | SubstructureNotifyMask
            let mask: c_long = (1 << 20) | (1 << 19);
            unsafe { (display.x.send_event)(display.raw, root, 0, mask, &mut event) };
        }
        unsafe { (display.x.flush)(display.raw) };
    }
}

// ----- tray -----

struct TrayModel;

impl ksni::Tray for TrayModel {
    fn id(&self) -> String {
        crate::paths::DESKTOP_APP_ID.into()
    }
    fn title(&self) -> String {
        "Passession".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        match crate::image::decode_png_rgba(include_bytes!("../../assets/tray.png")) {
            Ok((width, height, rgba)) => vec![ksni::Icon {
                width: width as i32,
                height: height as i32,
                // ARGB32, network byte order.
                data: rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect(),
            }],
            Err(_) => Vec::new(),
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Passession".into(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        deliver(NativeSignal::TrayOpen);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem {
                label: "Open Passession".into(),
                activate: Box::new(|_: &mut Self| deliver(NativeSignal::TrayOpen)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            ksni::menu::StandardItem {
                label: "Quit Passession".into(),
                activate: Box::new(|_: &mut Self| deliver(NativeSignal::TrayQuit)),
                ..Default::default()
            }
            .into(),
        ]
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        // Keep the service: a restarted panel re-registers the item.
        true
    }
}

pub(crate) struct Tray {
    handle: Option<ksni::Handle<TrayModel>>,
}

/// Registers the StatusNotifierItem on the D-Bus runtime; the handle is stored
/// on the GUI thread once the panel accepted it.
pub(crate) fn create_tray(shared: &Arc<Shared>) {
    use ksni::TrayMethods as _;
    let shared = shared.clone();
    BUS.spawn(async move {
        let spawned = tokio::time::timeout(Duration::from_secs(3), TrayModel.spawn()).await;
        match spawned {
            Ok(Ok(handle)) => {
                let owner = shared.clone();
                shared.on_main(move |_, state| {
                    if state.desktop.tray.is_none() && !owner.desktop.quitting() {
                        state.desktop.tray = Some(Tray { handle: Some(handle) });
                        owner.desktop.tray_created.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            }
            Ok(Err(error)) => deliver(NativeSignal::TrayFailed(format!("Unable to create the system tray icon: {error}"))),
            Err(_) => deliver(NativeSignal::TrayFailed("The system tray service did not respond.".into())),
        }
    });
}

impl Tray {
    pub fn poll(&mut self) {}
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let awaiter = handle.shutdown();
            BUS.spawn(awaiter);
        }
    }
}

/// A watcher without a registered panel host is not a usable tray.
pub(crate) async fn tray_host_available() -> bool {
    BUS.spawn(async {
        let Ok(connection) = session().await else { return false };
        let Ok(proxy) = zbus::Proxy::new(&connection, "org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher", "org.kde.StatusNotifierWatcher").await
        else {
            return false;
        };
        matches!(
            tokio::time::timeout(Duration::from_millis(1500), proxy.get_property::<bool>("IsStatusNotifierHostRegistered")).await,
            Ok(Ok(true))
        )
    })
    .await
    .unwrap_or(false)
}

// ----- open / reveal / share -----

fn external_command(program: &str, args: Vec<OsString>) -> Result<(), PlatformError> {
    super::editors::run(program, &args)
}

pub(crate) fn open_uri(url: &str) -> Result<(), PlatformError> {
    external_command("xdg-open", vec![url.into()])
}

pub(crate) fn open_path(path: &Path) -> Result<(), PlatformError> {
    external_command("xdg-open", vec![path.as_os_str().to_owned()])
}

/// The standardized FileManager1 API reveals the selected file itself.
pub(crate) fn reveal(workspace: &Path, file: Option<&Path>) -> Result<(), PlatformError> {
    let Some(file) = file else {
        return external_command("xdg-open", vec![workspace.as_os_str().to_owned()]);
    };
    let uri = url::Url::from_file_path(file).map_err(|_| PlatformError::invalid("Not an absolute path."))?.to_string();
    bus_wait(async move {
        let connection = session().await?;
        connection
            .call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![uri], ""),
            )
            .await
            .map(|_| ())
            .map_err(dbus_error)
    })
}

/// Runs `future` on the D-Bus runtime and blocks the calling (worker) thread
/// for its result; safe from inside another runtime's blocking pool.
fn bus_wait<T: Send + 'static>(future: impl Future<Output = Result<T, PlatformError>> + Send + 'static) -> Result<T, PlatformError> {
    let (tx, rx) = std::sync::mpsc::channel();
    BUS.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv().map_err(|_| PlatformError::native("The D-Bus worker stopped."))?
}

/// A mail composer with the downloaded attachment (original xdg-email path).
pub(crate) fn share_file(shared: &Arc<Shared>, path: PathBuf, _mime_type: String, _name: String, done: Completion) {
    shared.complete_blocking(done, move || {
        external_command("xdg-email", vec!["--attach".into(), path.into_os_string()])
            .map(|()| PlatformValue::None)
            .map_err(|e| PlatformError::native(format!("Could not open a mail composer with the downloaded attachment: {}", e.message)))
    });
}

// ----- runtime info -----

pub(crate) fn memory_usage() -> (Option<u64>, Option<u64>) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|kb| kb.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

pub(crate) fn os_version() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_owned())));
    match pretty {
        Some(pretty) => format!("{pretty} {}", release.trim()),
        None => release.trim().to_owned(),
    }
}

/// XDG desktop portal `org.freedesktop.appearance color-scheme` (1 = prefer dark).
pub(crate) fn system_prefers_dark() -> bool {
    let portal = bus_wait(async {
        let connection = session().await?;
        let reply = tokio::time::timeout(
            Duration::from_millis(500),
            connection.call_method(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.portal.Settings"),
                "Read",
                &("org.freedesktop.appearance", "color-scheme"),
            ),
        )
        .await
        .map_err(|_| PlatformError::native("portal timeout"))?
        .map_err(dbus_error)?;
        // Read returns v(v(u)) on older portals and v(u) on newer ones.
        let outer: OwnedValue = reply.body().deserialize().map_err(dbus_error)?;
        let mut value: ZValue<'_> = outer.into();
        while let ZValue::Value(inner) = value {
            value = *inner;
        }
        Ok(matches!(value, ZValue::U32(1)))
    });
    match portal {
        Ok(dark) => dark,
        // No portal: the GTK theme name is the remaining desktop signal.
        Err(_) => std::env::var("GTK_THEME").is_ok_and(|theme| theme.to_ascii_lowercase().contains("dark")),
    }
}

pub(crate) fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "en_US".into())
}

#[allow(dead_code)]
fn _assert_types(_: c_uint) {}
