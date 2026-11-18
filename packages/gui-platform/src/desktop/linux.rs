//! Linux desktop integration. D-Bus runs on a dedicated runtime that lives for
//! the process.

use super::{Activation, DesktopNotification, NativeSignal, deliver, notification_clicked};
use crate::{
    Bootstrap,
    action::*,
    platform::{Completion, Shared},
};
use futures_util::StreamExt as _;
use std::{
    collections::HashMap,
    ffi::{CString, OsString, c_char, c_int, c_long, c_uint, c_ulong, c_void},
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
        deliver(NativeSignal::Activation(parse_activation(
            &argv,
            Some(Path::new(&cwd)),
            false,
        )));
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
                Activation {
                    url: Some(uri),
                    ..Default::default()
                }
            } else if let Some(path) = url::Url::parse(&uri)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .filter(|p| p.is_dir())
            {
                Activation {
                    project_path: Some(path.display().to_string()),
                    ..Default::default()
                }
            } else {
                continue;
            };
            deliver(NativeSignal::Activation(activation));
        }
    }
    async fn activate_action(
        &self,
        action_name: String,
        _parameter: Vec<OwnedValue>,
        _platform_data: HashMap<String, OwnedValue>,
    ) {
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
            return Activation {
                url: Some(arg.clone()),
                project_path: None,
                initial,
            };
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
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.map(|c| c.join(path))
                .unwrap_or_else(|| path.to_path_buf())
        };
        if absolute.is_dir() {
            let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
            return Activation {
                url: None,
                project_path: Some(canonical.display().to_string()),
                initial,
            };
        }
        index += 1;
    }
    Activation {
        initial,
        ..Default::default()
    }
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
        if server.at(OBJECT_PATH, SessionsInterface).await.is_err()
            || server.at(OBJECT_PATH, ApplicationInterface).await.is_err()
        {
            return Bootstrap::Primary;
        }
        let reply = connection
            .request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
            .await;
        match reply {
            Ok(
                zbus::fdo::RequestNameReply::PrimaryOwner
                | zbus::fdo::RequestNameReply::AlreadyOwner,
            )
            | Err(_) => Bootstrap::Primary,
            Ok(_) => {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let forwarded = tokio::time::timeout(
                    Duration::from_secs(30),
                    connection.call_method(
                        Some(BUS_NAME),
                        OBJECT_PATH,
                        Some("sh.paseo.Sessions1"),
                        "CommandLine",
                        &(argv, cwd),
                    ),
                )
                .await;
                let accepted = match &forwarded {
                    Ok(Ok(reply)) => reply.body().deserialize::<bool>().unwrap_or(false),
                    _ => false,
                };
                Bootstrap::Forwarded {
                    exit_code: if accepted { 0 } else { 1 },
                }
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
                    "No GNOME SessionManager or usable XSMP session manager is available. XSMP needs libSM/libICE runtime libraries and SESSION_MANAGER.",
                )
            })
        }
        .await;
        deliver(NativeSignal::LifecycleStarted(result));
    });
}

async fn start_gnome_session() -> Result<(), PlatformError> {
    let connection = session().await?;
    let manager = zbus::Proxy::new(
        &connection,
        "org.gnome.SessionManager",
        "/org/gnome/SessionManager",
        "org.gnome.SessionManager",
    )
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
    let private = zbus::Proxy::new(
        &connection,
        "org.gnome.SessionManager",
        client.clone(),
        "org.gnome.SessionManager.ClientPrivate",
    )
    .await
    .map_err(dbus_error)?;
    let mut signals = private.receive_all_signals().await.map_err(dbus_error)?;
    tokio::spawn(async move {
        while let Some(message) = signals.next().await {
            let header = message.header();
            let Some(member) = header.member().map(|m| m.as_str().to_owned()) else {
                continue;
            };
            match member.as_str() {
                "CancelEndSession" => deliver(NativeSignal::SessionEndingCancelled),
                "Stop" => deliver(NativeSignal::SessionEnded),
                "QueryEndSession" | "EndSession" => {
                    deliver(if member == "QueryEndSession" {
                        NativeSignal::SessionEnding
                    } else {
                        NativeSignal::SessionEnded
                    });
                    // Never veto the OS; teardown is best effort within its deadline.
                    let response = tokio::time::timeout(
                        Duration::from_secs(2),
                        private.call_method("EndSessionResponse", &(true, "")),
                    )
                    .await;
                    if !matches!(response, Ok(Ok(_))) {
                        deliver(NativeSignal::LifecycleError(
                            "GNOME SessionManager rejected the end-session response.".into(),
                        ));
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
    use super::{NativeSignal, deliver};
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

    static SAVE_DONE: std::sync::OnceLock<unsafe extern "C" fn(SmcConn, c_int)> =
        std::sync::OnceLock::new();

    unsafe extern "C" fn save_yourself(
        connection: SmcConn,
        _: *mut c_void,
        _: c_int,
        shutdown: c_int,
        _: c_int,
        _: c_int,
    ) {
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
                unsafe extern "C" fn(
                    *const c_char,
                    *mut c_void,
                    c_int,
                    c_int,
                    c_ulong,
                    *mut SmcCallbacks,
                    *const c_char,
                    *mut *mut c_char,
                    c_int,
                    *mut c_char,
                ) -> SmcConn,
            > = sm.get(b"SmcOpenConnection\0").map_err(|e| e.to_string())?;
            let get_ice: libloading::Symbol<unsafe extern "C" fn(SmcConn) -> IceConn> = sm
                .get(b"SmcGetIceConnection\0")
                .map_err(|e| e.to_string())?;
            let done: libloading::Symbol<unsafe extern "C" fn(SmcConn, c_int)> = sm
                .get(b"SmcSaveYourselfDone\0")
                .map_err(|e| e.to_string())?;
            let fd: libloading::Symbol<unsafe extern "C" fn(IceConn) -> c_int> = ice
                .get(b"IceConnectionNumber\0")
                .map_err(|e| e.to_string())?;
            let process: libloading::Symbol<
                unsafe extern "C" fn(IceConn, *mut c_void, *mut c_int) -> c_int,
            > = ice
                .get(b"IceProcessMessages\0")
                .map_err(|e| e.to_string())?;
            let _ = SAVE_DONE.set(*done);
            let mut callbacks = SmcCallbacks {
                save_yourself: Callback {
                    callback: save_yourself,
                    client_data: std::ptr::null_mut(),
                },
                die: Callback {
                    callback: die,
                    client_data: std::ptr::null_mut(),
                },
                save_complete: Callback {
                    callback: save_complete,
                    client_data: std::ptr::null_mut(),
                },
                shutdown_cancelled: Callback {
                    callback: cancelled,
                    client_data: std::ptr::null_mut(),
                },
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
                return Err(std::ffi::CStr::from_ptr(error.as_ptr())
                    .to_string_lossy()
                    .into_owned());
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
                        let mut poll = libc::pollfd {
                            fd: descriptor,
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        if libc::poll(&mut poll, 1, -1) < 0 {
                            continue;
                        }
                        if poll.revents & (libc::POLLERR | libc::POLLHUP) != 0
                            || process(ice_connection, std::ptr::null_mut(), std::ptr::null_mut())
                                != 0
                        {
                            deliver(NativeSignal::LifecycleError(
                                "The XSMP session manager disconnected".into(),
                            ));
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
/// The clicked notification's activation token, consumed by the next raise
/// (`NativeWindows::show_all`) that its click signal triggers.
static ACTIVATION_TOKEN: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn init_notifications(_shared: &Arc<Shared>) -> Result<(), PlatformError> {
    // flutter_local_notifications delivers Linux actions through its live D-Bus
    // callback; there is no cold-launch notification query.
    BUS.spawn(async {
        let Ok(connection) = session().await else {
            return;
        };
        let Ok(proxy) = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        else {
            return;
        };
        // One ordered stream: servers emit `ActivationToken` right before the
        // `ActionInvoked` of the same notification.
        let Ok(mut signals) = proxy.receive_all_signals().await else {
            return;
        };
        let mut token: Option<(u32, String)> = None;
        while let Some(message) = signals.next().await {
            let header = message.header();
            let Some(member) = header.member() else {
                continue;
            };
            match member.as_str() {
                "ActivationToken" => {
                    token = message.body().deserialize::<(u32, String)>().ok();
                }
                "ActionInvoked" => {
                    let Ok((server_id, action)) = message.body().deserialize::<(u32, String)>()
                    else {
                        continue;
                    };
                    let activation_token = token
                        .take()
                        .filter(|(id, _)| *id == server_id)
                        .map(|(_, token)| token);
                    let found = SHOWN
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(id, _, _)| *id == server_id)
                        .cloned();
                    if let Some((_, id, payload)) = found {
                        *ACTIVATION_TOKEN.lock().unwrap() = activation_token;
                        notification_clicked(&payload, Some(id.into()), Some(action));
                    }
                }
                "NotificationClosed" => {
                    if let Ok((server_id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                        SHOWN.lock().unwrap().retain(|(id, _, _)| *id != server_id);
                    }
                }
                _ => {}
            }
        }
    });
    Ok(())
}

pub(crate) async fn show_notification(
    notification: &DesktopNotification,
) -> Result<(), PlatformError> {
    let title = notification.title.clone();
    let body = notification.body.clone().unwrap_or_default();
    let sound = notification.sound;
    let id = notification.id;
    let payload = notification.payload.clone();
    let replaces = SHOWN
        .lock()
        .unwrap()
        .iter()
        .find(|(_, own, _)| *own == id)
        .map(|(server, _, _)| *server)
        .unwrap_or(0);
    BUS.spawn(async move {
        let connection = session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
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
                &(
                    "Passession",
                    replaces,
                    DESKTOP_ENTRY,
                    title.as_str(),
                    body.as_str(),
                    vec!["default", "Open Passession"],
                    hints,
                    -1i32,
                ),
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

/// Linux has no notification prompt; notifications need a running
/// notification server, which only the desktop session can provide.
pub(crate) fn request_notification_permission(shared: &Arc<Shared>, done: Completion) {
    notification_permission_status(shared, done);
}

pub(crate) fn notification_permission_status(_shared: &Arc<Shared>, done: Completion) {
    BUS.spawn(async move {
        let answered = tokio::time::timeout(Duration::from_secs(2), notification_server())
            .await
            .is_ok_and(|server| server.is_ok());
        done.ok(PlatformValue::Permission(if answered {
            PermissionState::Granted
        } else {
            PermissionState::Blocked
        }));
    });
}

/// `GetServerInformation` answers only when a server owns (or can be
/// activated for) `org.freedesktop.Notifications`.
async fn notification_server() -> Result<(), PlatformError> {
    let connection = session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
    .map_err(dbus_error)?;
    let _: (String, String, String, String) = proxy
        .call("GetServerInformation", &())
        .await
        .map_err(dbus_error)?;
    Ok(())
}

pub(crate) fn open_permission_settings(_target: &PermissionSettings) -> Result<(), PlatformError> {
    Err(PlatformError::unsupported(
        "Linux desktops manage notifications in their own settings.",
    ))
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
    intern_atom: unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_ulong,
    get_window_property: unsafe extern "C" fn(
        *mut c_void,
        c_ulong,
        c_ulong,
        c_long,
        c_long,
        c_int,
        c_ulong,
        *mut c_ulong,
        *mut c_int,
        *mut c_ulong,
        *mut c_ulong,
        *mut *mut u8,
    ) -> c_int,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
    change_property: unsafe extern "C" fn(
        *mut c_void,
        c_ulong,
        c_ulong,
        c_ulong,
        c_int,
        c_int,
        *const u8,
        c_int,
    ) -> c_int,
    query_tree: unsafe extern "C" fn(
        *mut c_void,
        c_ulong,
        *mut c_ulong,
        *mut c_ulong,
        *mut *mut c_ulong,
        *mut c_uint,
    ) -> c_int,
    map_raised: unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int,
    send_event: unsafe extern "C" fn(
        *mut c_void,
        c_ulong,
        c_int,
        c_long,
        *mut XClientMessageEvent,
    ) -> c_int,
    flush: unsafe extern "C" fn(*mut c_void) -> c_int,
    query_extension: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_int,
        *mut c_int,
        *mut c_int,
    ) -> c_int,
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
                intern_atom: symbol!(b"XInternAtom\0"),
                get_window_property: symbol!(b"XGetWindowProperty\0"),
                free: symbol!(b"XFree\0"),
                change_property: symbol!(b"XChangeProperty\0"),
                query_tree: symbol!(b"XQueryTree\0"),
                map_raised: symbol!(b"XMapRaised\0"),
                send_event: symbol!(b"XSendEvent\0"),
                flush: symbol!(b"XFlush\0"),
                query_extension: symbol!(b"XQueryExtension\0"),
                _x11: library,
            })
        }
    }
}

static X11_LIB: LazyLock<Result<X11, String>> = LazyLock::new(X11::load);
/// libXss `XScreenSaverQueryInfo(display, drawable, info)`.
type QueryScreenSaverInfo =
    unsafe extern "C" fn(*mut c_void, c_ulong, *mut ScreenSaverInfo) -> c_int;
static XSS: LazyLock<Result<(libloading::Library, QueryScreenSaverInfo), String>> =
    LazyLock::new(|| unsafe {
        let library = libloading::Library::new("libXss.so.1").map_err(|_| {
            "System idle time requires the optional libXss.so.1 runtime library".to_owned()
        })?;
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
        let x = X11_LIB
            .as_ref()
            .map_err(|e| PlatformError::new("idle_unavailable", e.clone()))?;
        let raw = unsafe { (x.open_display)(std::ptr::null()) };
        if raw.is_null() {
            return Err(PlatformError::new(
                "display_unavailable",
                "Cannot open the X11 display.",
            ));
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
    if unsafe {
        (display.x.query_extension)(
            display.raw,
            name.as_ptr(),
            &mut opcode,
            &mut event,
            &mut error,
        )
    } == 0
    {
        return Err(PlatformError::new(
            "idle_unavailable",
            "The XScreenSaver extension is unavailable",
        ));
    }
    let (_, query) = XSS
        .as_ref()
        .map_err(|e| PlatformError::new("idle_unavailable", e.clone()))?;
    let mut info = ScreenSaverInfo {
        window: 0,
        state: 0,
        kind: 0,
        til_or_since: 0,
        idle: 0,
        event_mask: 0,
    };
    let root = unsafe { (display.x.default_root)(display.raw) };
    if unsafe { query(display.raw, root, &mut info) } == 0 {
        return Err(PlatformError::new(
            "idle_failed",
            "XScreenSaverQueryInfo failed",
        ));
    }
    Ok(info.idle)
}

// ----- native window activation -----

pub(crate) fn show_windows() {
    WindowActivation::show_all();
}

struct WindowActivation;

fn wm_class() -> String {
    std::env::args_os()
        .next()
        .as_ref()
        .and_then(|arg| {
            Path::new(arg)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Makepad".into())
}

impl WindowActivation {
    fn property(
        display: &Display,
        window: c_ulong,
        name: &str,
        kind: c_ulong,
    ) -> Option<(Vec<u8>, c_int, c_ulong)> {
        let atom_name = CString::new(name).ok()?;
        let atom = unsafe { (display.x.intern_atom)(display.raw, atom_name.as_ptr(), 1) };
        if atom == 0 {
            return None;
        }
        let (mut actual_type, mut format, mut items, mut after) =
            (0 as c_ulong, 0 as c_int, 0 as c_ulong, 0 as c_ulong);
        let mut data: *mut u8 = std::ptr::null_mut();
        let status = unsafe {
            (display.x.get_window_property)(
                display.raw,
                window,
                atom,
                0,
                65536,
                0,
                kind,
                &mut actual_type,
                &mut format,
                &mut items,
                &mut after,
                &mut data,
            )
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
        let Some((bytes, 32, items)) = Self::property(display, root, "_NET_CLIENT_LIST", XA_WINDOW)
        else {
            return Vec::new();
        };
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
            .filter(|window| Self::has_class(display, *window, &class))
            .collect()
    }

    fn has_class(display: &Display, window: c_ulong, class: &str) -> bool {
        const XA_STRING: c_ulong = 31;
        Self::property(display, window, "WM_CLASS", XA_STRING).is_some_and(|(bytes, _, _)| {
            bytes
                .split(|b| *b == 0)
                .nth(1)
                .is_some_and(|c| c == class.as_bytes())
        })
    }

    /// Withdrawn top-level windows are direct root children again, so this also
    /// finds the window the shell hid itself (dormant), absent from `_NET_CLIENT_LIST`.
    fn root_children(display: &Display) -> Vec<c_ulong> {
        let root = unsafe { (display.x.default_root)(display.raw) };
        let (mut root_return, mut parent) = (0, 0);
        let mut children: *mut c_ulong = std::ptr::null_mut();
        let mut count: c_uint = 0;
        let status = unsafe {
            (display.x.query_tree)(
                display.raw,
                root,
                &mut root_return,
                &mut parent,
                &mut children,
                &mut count,
            )
        };
        if status == 0 || children.is_null() {
            return Vec::new();
        }
        let list = unsafe { std::slice::from_raw_parts(children, count as usize) }.to_vec();
        unsafe { (display.x.free)(children.cast()) };
        list
    }

    /// Raises and activates the visible windows. A notification's
    /// activation token is handed to the WM as `_NET_STARTUP_ID` (mutter and KWin
    /// grant focus to a mapped window when it is set, and to a withdrawn one when
    /// it maps later, e.g. the dormant first window) and its `_TIME` stamps the
    /// activation request, so focus-stealing prevention does not just flash it.
    fn show_all() {
        let token = ACTIVATION_TOKEN.lock().unwrap().take();
        let Ok(display) = Display::open() else { return };
        let root = unsafe { (display.x.default_root)(display.raw) };
        let active =
            unsafe { (display.x.intern_atom)(display.raw, c"_NET_ACTIVE_WINDOW".as_ptr(), 0) };
        let targets = Self::own_windows(&display);
        if let Some(token) = token.as_deref() {
            let class = wm_class();
            let withdrawn: Vec<c_ulong> = Self::root_children(&display)
                .into_iter()
                .filter(|w| !targets.contains(w) && Self::has_class(&display, *w, &class))
                .collect();
            for window in targets.iter().chain(&withdrawn) {
                Self::set_startup_id(&display, *window, token);
            }
        }
        let time = token.as_deref().map_or(0, startup_time);
        for window in targets {
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
                data: [2, time, 0, 0, 0],
            };
            // SubstructureRedirectMask | SubstructureNotifyMask
            let mask: c_long = (1 << 20) | (1 << 19);
            unsafe { (display.x.send_event)(display.raw, root, 0, mask, &mut event) };
        }
        unsafe { (display.x.flush)(display.raw) };
    }

    fn set_startup_id(display: &Display, window: c_ulong, token: &str) {
        const PROP_MODE_REPLACE: c_int = 0;
        unsafe {
            let property = (display.x.intern_atom)(display.raw, c"_NET_STARTUP_ID".as_ptr(), 0);
            let utf8 = (display.x.intern_atom)(display.raw, c"UTF8_STRING".as_ptr(), 0);
            (display.x.change_property)(
                display.raw,
                window,
                property,
                utf8,
                8,
                PROP_MODE_REPLACE,
                token.as_ptr(),
                token.len() as c_int,
            );
        }
    }
}

/// X11 startup ids end in `_TIME<server time>`; xdg-activation tokens carry
/// none, leaving the request's timestamp at 0 as before.
fn startup_time(token: &str) -> c_long {
    token
        .rfind("_TIME")
        .and_then(|at| token[at + 5..].parse::<c_ulong>().ok())
        .map_or(0, |time| time as c_long)
}

#[cfg(test)]
mod activation_tests {
    use super::startup_time;

    #[test]
    fn startup_ids_carry_their_x_server_time() {
        assert_eq!(
            startup_time("gnome-shell/Passession/2-31-host_TIME5141"),
            5141
        );
        assert_eq!(startup_time("5a1e9e0c-6f0b-4bde-a2c6-9f0a3b1d7e21"), 0);
        assert_eq!(startup_time("x_TIMEnot-a-number"), 0);
    }
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
    let uri = url::Url::from_file_path(file)
        .map_err(|_| PlatformError::invalid("Not an absolute path."))?
        .to_string();
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
fn bus_wait<T: Send + 'static>(
    future: impl Future<Output = Result<T, PlatformError>> + Send + 'static,
) -> Result<T, PlatformError> {
    let (tx, rx) = std::sync::mpsc::channel();
    BUS.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv()
        .map_err(|_| PlatformError::native("The D-Bus worker stopped."))?
}

/// A mail composer with the downloaded attachment (original xdg-email path).
pub(crate) fn share_file(
    shared: &Arc<Shared>,
    path: PathBuf,
    _mime_type: String,
    _name: String,
    done: Completion,
) {
    shared.complete_blocking(done, move || {
        external_command("xdg-email", vec!["--attach".into(), path.into_os_string()])
            .map(|()| PlatformValue::None)
            .map_err(|e| {
                PlatformError::native(format!(
                    "Could not open a mail composer with the downloaded attachment: {}",
                    e.message
                ))
            })
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
        .and_then(|text| {
            text.lines().find_map(|l| {
                l.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim_matches('"').to_owned())
            })
        });
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
        Err(_) => std::env::var("GTK_THEME")
            .is_ok_and(|theme| theme.to_ascii_lowercase().contains("dark")),
    }
}

pub(crate) fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "en_US".into())
}

// ----- fonts and text scale -----

/// One XDG portal `Settings.Read` value, unwrapped from its variant nesting
/// (`v(v(x))` on older portals, `v(x)` on newer ones) and converted by `extract`.
fn portal_setting<T: Send + 'static>(
    namespace: &'static str,
    key: &'static str,
    extract: fn(ZValue<'_>) -> Option<T>,
) -> Option<T> {
    bus_wait(async move {
        let connection = session().await?;
        let reply = tokio::time::timeout(
            Duration::from_millis(500),
            connection.call_method(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.portal.Settings"),
                "Read",
                &(namespace, key),
            ),
        )
        .await
        .map_err(|_| PlatformError::native("portal timeout"))?
        .map_err(dbus_error)?;
        let outer: OwnedValue = reply.body().deserialize().map_err(dbus_error)?;
        let mut value: ZValue<'_> = outer.into();
        while let ZValue::Value(inner) = value {
            value = *inner;
        }
        Ok(extract(value))
    })
    .ok()
    .flatten()
}

/// GNOME `org.gnome.desktop.interface text-scaling-factor` through the portal.
/// KDE has no separate factor (users change the font size), so it stays 1.
pub(crate) fn system_text_scale() -> crate::appearance::TextScale {
    portal_setting(
        "org.gnome.desktop.interface",
        "text-scaling-factor",
        |value| match value {
            ZValue::F64(factor) => Some(factor),
            _ => None,
        },
    )
    .map(crate::appearance::TextScale::from_factor)
    .unwrap_or_default()
}

/// The desktop's UI font family: KDE `kdeglobals [General] font` (portal, then
/// the file), otherwise GNOME `font-name` through the portal.
fn os_ui_family() -> Option<String> {
    fn string(value: ZValue<'_>) -> Option<String> {
        match value {
            ZValue::Str(text) => Some(text.as_str().to_owned()),
            _ => None,
        }
    }
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| {
        desktop
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("KDE"))
    });
    if kde {
        portal_setting("org.kde.kdeglobals.General", "font", string)
            .as_deref()
            .and_then(crate::fonts::qt_family)
            .or_else(kdeglobals_font)
    } else {
        portal_setting("org.gnome.desktop.interface", "font-name", string)
            .as_deref()
            .and_then(crate::fonts::pango_family)
    }
}

fn kdeglobals_font() -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    let text = std::fs::read_to_string(config.join("kdeglobals")).ok()?;
    let mut in_general = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_general = line == "[General]";
        } else if in_general && let Some(value) = line.strip_prefix("font=") {
            return crate::fonts::qt_family(value);
        }
    }
    None
}

/// fontconfig (dlopened `libfontconfig.so.1`) font discovery.
pub(crate) mod fonts {
    use crate::fonts::{
        Backend, Family, FileCache, FontFace, FontRequest, LATIN_PROBE, Role, SystemFonts, bcp47,
        finish_face, resolve, resolve_fallbacks,
    };
    use std::{
        ffi::{CStr, CString, c_char, c_int, c_void},
        sync::{Arc, LazyLock},
    };

    type FcBool = c_int;
    const FC_RESULT_MATCH: c_int = 0;
    const FC_MATCH_PATTERN: c_int = 0;
    /// `FC_WEIGHT_REGULAR` / `FC_WEIGHT_DEMIBOLD`, `FC_SLANT_ROMAN` / `FC_SLANT_ITALIC`.
    const FC_WEIGHT_REGULAR: c_int = 80;
    const FC_WEIGHT_DEMIBOLD: c_int = 180;
    const FC_SLANT_ROMAN: c_int = 0;
    const FC_SLANT_ITALIC: c_int = 100;

    #[repr(C)]
    struct FcFontSet {
        nfont: c_int,
        sfont: c_int,
        fonts: *mut *mut c_void,
    }

    struct Fc {
        init: unsafe extern "C" fn() -> *mut c_void,
        config_destroy: unsafe extern "C" fn(*mut c_void),
        name_parse: unsafe extern "C" fn(*const c_char) -> *mut c_void,
        pattern_destroy: unsafe extern "C" fn(*mut c_void),
        config_substitute: unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> FcBool,
        default_substitute: unsafe extern "C" fn(*mut c_void),
        font_match: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_int) -> *mut c_void,
        font_sort: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            FcBool,
            *mut *mut c_void,
            *mut c_int,
        ) -> *mut FcFontSet,
        font_set_destroy: unsafe extern "C" fn(*mut FcFontSet),
        get_string: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut u8) -> c_int,
        get_integer: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_int) -> c_int,
        get_double: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut f64) -> c_int,
        get_charset:
            unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void) -> c_int,
        charset_has_char: unsafe extern "C" fn(*mut c_void, u32) -> FcBool,
        /// fontconfig ≥ 2.12.92; older libraries report no axis values.
        weight_to_open_type: Option<unsafe extern "C" fn(f64) -> f64>,
        _library: libloading::Library,
    }

    impl Fc {
        fn load() -> Result<Self, String> {
            unsafe {
                let library =
                    libloading::Library::new("libfontconfig.so.1").map_err(|e| e.to_string())?;
                macro_rules! symbol {
                    ($name:literal) => {
                        *library.get($name).map_err(|e| e.to_string())?
                    };
                }
                Ok(Self {
                    init: symbol!(b"FcInitLoadConfigAndFonts\0"),
                    config_destroy: symbol!(b"FcConfigDestroy\0"),
                    name_parse: symbol!(b"FcNameParse\0"),
                    pattern_destroy: symbol!(b"FcPatternDestroy\0"),
                    config_substitute: symbol!(b"FcConfigSubstitute\0"),
                    default_substitute: symbol!(b"FcDefaultSubstitute\0"),
                    font_match: symbol!(b"FcFontMatch\0"),
                    font_sort: symbol!(b"FcFontSort\0"),
                    font_set_destroy: symbol!(b"FcFontSetDestroy\0"),
                    get_string: symbol!(b"FcPatternGetString\0"),
                    get_integer: symbol!(b"FcPatternGetInteger\0"),
                    get_double: symbol!(b"FcPatternGetDouble\0"),
                    get_charset: symbol!(b"FcPatternGetCharSet\0"),
                    charset_has_char: symbol!(b"FcCharSetHasChar\0"),
                    weight_to_open_type: library
                        .get(b"FcWeightToOpenTypeDouble\0")
                        .ok()
                        .map(|symbol| *symbol),
                    _library: library,
                })
            }
        }
    }

    static FC: LazyLock<Result<Fc, String>> = LazyLock::new(Fc::load);

    /// Escapes a family for `FcNameParse` (`\`, `-`, `:` and `,` are syntax).
    pub(crate) fn escape(family: &str) -> String {
        let mut out = String::with_capacity(family.len());
        for ch in family.chars() {
            if matches!(ch, '\\' | '-' | ':' | ',') {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }

    /// CSS generics fontconfig resolves by alias; any matched family is accepted.
    fn is_generic(name: &str) -> bool {
        [
            "serif",
            "sans-serif",
            "monospace",
            "cursive",
            "fantasy",
            "system-ui",
            "emoji",
            "math",
        ]
        .iter()
        .any(|g| g.eq_ignore_ascii_case(name))
    }

    /// A family name marking a Korean regional face (`Noto Sans CJK KR`, `Source Han Sans K`).
    pub(crate) fn is_korean_family(family: &str) -> bool {
        let family = family.trim();
        family.ends_with(" KR") || family.ends_with(" K") || family.contains("Korean")
    }

    /// An owned `FcPattern`.
    struct Pattern<'a>(&'a Fc, *mut c_void);

    impl Drop for Pattern<'_> {
        fn drop(&mut self) {
            unsafe { (self.0.pattern_destroy)(self.1) }
        }
    }

    impl Pattern<'_> {
        fn string(&self, object: &CStr, n: c_int) -> Option<String> {
            string_at(self.0, self.1, object, n)
        }
    }

    fn string_at(fc: &Fc, pattern: *mut c_void, object: &CStr, n: c_int) -> Option<String> {
        let mut value: *mut u8 = std::ptr::null_mut();
        let found =
            unsafe { (fc.get_string)(pattern, object.as_ptr(), n, &mut value) } == FC_RESULT_MATCH;
        (found && !value.is_null()).then(|| {
            unsafe { CStr::from_ptr(value.cast()) }
                .to_string_lossy()
                .into_owned()
        })
    }

    struct Fontconfig<'a> {
        fc: &'a Fc,
        config: *mut c_void,
        files: FileCache,
        lang: String,
        os_family: Option<Option<String>>,
    }

    impl Drop for Fontconfig<'_> {
        fn drop(&mut self) {
            unsafe { (self.fc.config_destroy)(self.config) }
        }
    }

    impl<'a> Fontconfig<'a> {
        fn pattern(&self, family: Option<&str>, role: Role, lang: &str) -> Option<Pattern<'a>> {
            let mut name = family.map(escape).unwrap_or_default();
            let weight = if role.weight() >= 600 {
                FC_WEIGHT_DEMIBOLD
            } else {
                FC_WEIGHT_REGULAR
            };
            let slant = if role.italic() {
                FC_SLANT_ITALIC
            } else {
                FC_SLANT_ROMAN
            };
            name.push_str(&format!(":weight={weight}:slant={slant}"));
            if !lang.is_empty() {
                name.push_str(&format!(":lang={}", escape(lang)));
            }
            let name = CString::new(name).ok()?;
            let pattern = unsafe { (self.fc.name_parse)(name.as_ptr()) };
            if pattern.is_null() {
                return None;
            }
            let pattern = Pattern(self.fc, pattern);
            unsafe {
                (self.fc.config_substitute)(self.config, pattern.1, FC_MATCH_PATTERN);
                (self.fc.default_substitute)(pattern.1);
            }
            Some(pattern)
        }

        /// A matched font pattern → validated face.
        fn load(&mut self, pattern: *mut c_void, role: Role, probe: char) -> Option<FontFace> {
            let fc = self.fc;
            let file = string_at(fc, pattern, c"file", 0)?;
            let mut index: c_int = 0;
            unsafe { (fc.get_integer)(pattern, c"index".as_ptr(), 0, &mut index) };
            let mut variations = Vec::new();
            if let Some(to_open_type) = fc.weight_to_open_type {
                let mut weight = 0.0f64;
                // Variable-font default patterns carry a weight range, which GetDouble rejects.
                if unsafe { (fc.get_double)(pattern, c"weight".as_ptr(), 0, &mut weight) }
                    == FC_RESULT_MATCH
                {
                    variations.push((*b"wght", unsafe { to_open_type(weight) } as f32));
                }
            }
            let mut width = 0.0f64;
            if unsafe { (fc.get_double)(pattern, c"width".as_ptr(), 0, &mut width) }
                == FC_RESULT_MATCH
            {
                variations.push((*b"wdth", width as f32));
            }
            let data: Arc<Vec<u8>> = self.files.read(&file)?;
            // The upper 16 bits select a named instance; its coordinates are the weight/width above.
            let face_index = (index as u32) & 0xffff;
            finish_face(
                data,
                face_index,
                &variations,
                role,
                probe,
                format!("{file}#{face_index}"),
            )
        }

        fn matched(&mut self, family: &str, role: Role, exact: bool) -> Option<(FontFace, String)> {
            let lang = self.lang.clone();
            let pattern = self.pattern(Some(family), role, &lang)?;
            let mut result: c_int = 0;
            let found = unsafe { (self.fc.font_match)(self.config, pattern.1, &mut result) };
            if found.is_null() {
                return None;
            }
            let found = Pattern(self.fc, found);
            let names: Vec<String> = (0..).map_while(|n| found.string(c"family", n)).collect();
            if exact && !names.iter().any(|name| name.eq_ignore_ascii_case(family)) {
                return None;
            }
            let face = self.load(found.1, role, LATIN_PROBE)?;
            Some((
                face,
                names
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| family.to_owned()),
            ))
        }

        fn os_family(&mut self) -> Option<String> {
            self.os_family
                .get_or_insert_with(super::os_ui_family)
                .clone()
        }
    }

    impl Backend for Fontconfig<'_> {
        fn face(&mut self, family: Family<'_>, role: Role) -> Option<(FontFace, String)> {
            match family {
                Family::SystemUi => {
                    if let Some(os_family) = self.os_family()
                        && let Some(found) = self.matched(&os_family, role, true)
                    {
                        return Some(found);
                    }
                    self.matched("system-ui", role, false)
                }
                Family::SystemMono => self.matched("monospace", role, false),
                Family::Named(name) => self.matched(name, role, !is_generic(name)),
            }
        }

        fn fallback(
            &mut self,
            ui_family: Option<&str>,
            role: Role,
            probe: char,
        ) -> Option<FontFace> {
            let language = if probe == '한' { "ko" } else { &self.lang };
            let pattern = self.pattern(Some(ui_family.unwrap_or("sans-serif")), role, language)?;
            let mut result: c_int = 0;
            // Untrimmed, so every face of a CJK collection stays in the list.
            let set = unsafe {
                (self.fc.font_sort)(self.config, pattern.1, 0, std::ptr::null_mut(), &mut result)
            };
            if set.is_null() {
                return None;
            }
            let fonts: Vec<*mut c_void> = unsafe {
                let set = &*set;
                if set.fonts.is_null() || set.nfont <= 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(set.fonts, set.nfont as usize).to_vec()
                }
            };
            let fc = self.fc;
            let covering: Vec<*mut c_void> = fonts
                .into_iter()
                .filter(|font| {
                    let mut charset: *mut c_void = std::ptr::null_mut();
                    let has_charset =
                        unsafe { (fc.get_charset)(*font, c"charset".as_ptr(), 0, &mut charset) }
                            == FC_RESULT_MATCH;
                    has_charset
                        && !charset.is_null()
                        && unsafe { (fc.charset_has_char)(charset, probe as u32) } != 0
                })
                .collect();
            // Every face of a CJK collection (Noto Sans CJK JP/KR/SC/…) claims `ko`,
            // and distro preferences often rank JP first; within the chosen file,
            // take its Korean regional face so Han characters use Korean forms.
            let korean_sibling = covering
                .first()
                .filter(|_| probe == '한')
                .and_then(|first| {
                    let file = string_at(fc, *first, c"file", 0)?;
                    covering.iter().copied().find(|font| {
                        string_at(fc, *font, c"file", 0).as_deref() == Some(file.as_str())
                            && (0..)
                                .map_while(|n| string_at(fc, *font, c"family", n))
                                .any(|family| is_korean_family(&family))
                    })
                });
            let face = korean_sibling
                .into_iter()
                .chain(covering.iter().copied())
                .find_map(|font| self.load(font, role, probe));
            unsafe { (self.fc.font_set_destroy)(set) };
            face
        }
    }

    /// fontconfig resolution: the desktop's font family (portal / kdeglobals),
    /// else `system-ui`; missing glyphs filtered by fontconfig charset coverage.
    /// Without libfontconfig nothing resolves and the bundled chain stays.
    fn backend(request: &FontRequest) -> Option<Fontconfig<'static>> {
        let Ok(fc) = &*FC else { return None };
        let config = unsafe { (fc.init)() };
        if config.is_null() {
            return None;
        }
        Some(Fontconfig {
            fc,
            config,
            files: FileCache::default(),
            lang: bcp47(&request.locale),
            os_family: None,
        })
    }

    pub(crate) fn system_fonts(request: &FontRequest) -> SystemFonts {
        backend(request)
            .map(|mut backend| resolve(&mut backend, request))
            .unwrap_or_default()
    }

    pub(crate) fn fallback_fonts(
        request: &FontRequest,
        probes: &[(Role, char)],
    ) -> Vec<Option<FontFace>> {
        backend(request)
            .map(|mut backend| resolve_fallbacks(&mut backend, request, probes))
            .unwrap_or_else(|| vec![None; probes.len()])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn family_names_are_escaped_for_fc_name_parse() {
            assert_eq!(escape("Noto Sans CJK KR"), "Noto Sans CJK KR");
            assert_eq!(escape("system-ui"), "system\\-ui");
            assert_eq!(escape("A:B,C\\D"), "A\\:B\\,C\\\\D");
        }

        #[test]
        fn korean_regional_faces_are_recognized() {
            assert!(is_korean_family("Noto Sans CJK KR"));
            assert!(is_korean_family("Source Han Sans K"));
            assert!(!is_korean_family("Noto Sans CJK JP"));
            assert!(!is_korean_family("Noto Sans KR Kurdish"));
        }

        #[test]
        fn resolves_ui_and_hangul_faces_on_this_machine() {
            if FC.is_err() {
                eprintln!("skipped: libfontconfig.so.1 is not installed");
                return;
            }
            let fonts = crate::appearance::system_fonts(&FontRequest::from_css(
                "",
                "monospace",
                "ko_KR.UTF-8",
            ));
            let regular = fonts.ui.regular.as_ref().expect("a UI regular face");
            eprintln!(
                "ui {:?}: {} {:?}",
                fonts.ui_family_name, regular.source, regular.variations
            );
            assert!(fonts.ui.semibold.is_some());
            // An explicit refresh must read the OS/files again, not reuse a
            // process-lifetime request cache after the desktop font changes.
            let again = crate::appearance::system_fonts(&FontRequest::from_css(
                "",
                "monospace",
                "ko_KR.UTF-8",
            ));
            assert!(!Arc::ptr_eq(&again.ui.regular.unwrap().data, &regular.data));
            let Some(hangul) = fonts.hangul.regular.as_ref() else {
                eprintln!("skipped Hangul: no font covering U+D55C is installed");
                return;
            };
            eprintln!("hangul: {} {:?}", hangul.source, hangul.variations);
            let parsed =
                makepad_widgets::makepad_draw::text::font_face::FontFace::from_data_and_index(
                    hangul.data.clone(),
                    hangul.index,
                )
                .unwrap();
            assert!(parsed.as_ttf_parser_face().glyph_index('한').is_some());
            if let Some(semibold) = &fonts.ui.semibold
                && let Some((_, wght)) = semibold.variations.iter().find(|(tag, _)| tag == b"wght")
            {
                assert!(
                    *wght > 400.0,
                    "semibold variable face uses a heavier wght, got {wght}"
                );
            }
        }

        #[test]
        fn missing_named_family_does_not_substitute() {
            if FC.is_err() {
                eprintln!("skipped: libfontconfig.so.1 is not installed");
                return;
            }
            let fonts = crate::appearance::system_fonts(&FontRequest::from_css(
                "'No Such Family 7f3a'",
                "'No Such Mono 7f3a'",
                "en_US",
            ));
            assert!(fonts.ui.regular.is_none());
            assert!(fonts.mono.is_none());
        }
    }
}
