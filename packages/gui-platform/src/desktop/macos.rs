//! macOS desktop integration (original macos/Runner/AppDelegate.swift, the macOS
//! paseo_desktop_integrations plugin, flutter_local_notifications macOS and
//! tray_manager). Delegate methods are added to Makepad's `NSAppDelegate`.

use super::{deliver, notification_clicked, Activation, DesktopNotification, NativeSignal};
use crate::{
    action::*,
    apple::{self, imp, ns_string, rust_string, user_notifications, Id, BOOL_ENC},
    platform::{Completion, Shared},
    Bootstrap,
};
use objc2::{class, msg_send, runtime::{AnyObject, Bool, Sel}};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const BUNDLE_ID: &str = crate::paths::DESKTOP_APP_ID;

// ----- single instance -----

/// LaunchServices keeps one instance per bundle (`LSMultipleInstancesProhibited`).
/// A direct executable launch while the app runs forwards through `open -b`,
/// which reaches the primary as `application:openURLs:` (URL or directory).
pub(crate) fn bootstrap(argv: &[String]) -> Bootstrap {
    let others = unsafe {
        let apps: Id = msg_send![class!(NSRunningApplication), runningApplicationsWithBundleIdentifier: ns_string(BUNDLE_ID)];
        let count: usize = msg_send![apps, count];
        let own = std::process::id() as i32;
        (0..count)
            .filter(|i| {
                let app: Id = msg_send![apps, objectAtIndex: *i];
                let pid: i32 = msg_send![app, processIdentifier];
                pid != own
            })
            .count()
    };
    if others == 0 {
        return Bootstrap::Primary;
    }
    let args: Vec<String> = argv.iter().skip(1).cloned().collect();
    let mut open: Vec<OsString> = vec!["-b".into(), BUNDLE_ID.into()];
    if let Some(link) = args.iter().find(|a| a.to_ascii_lowercase().starts_with("paseo:")) {
        open.push(link.into());
    } else if let Some(path) = crate::deep_link::open_project_path_from_argv(&args) {
        open.push(path.into());
    }
    let ok = std::process::Command::new("/usr/bin/open").args(&open).status().is_ok_and(|s| s.success());
    Bootstrap::Forwarded { exit_code: if ok { 0 } else { 1 } }
}

// ----- app delegate additions -----

static TERMINATION_ALLOWED: AtomicBool = AtomicBool::new(false);
static TERMINATION_PENDING: AtomicBool = AtomicBool::new(false);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn open_urls(_this: &AnyObject, _sel: Sel, _app: Id, urls: Id) {
    let ready = crate::platform().is_some_and(|p| p.0.desktop.routes_ready());
    unsafe {
        let count: usize = msg_send![urls, count];
        for i in 0..count {
            let url: Id = msg_send![urls, objectAtIndex: i];
            let is_file: bool = msg_send![url, isFileURL];
            if is_file {
                let standardized: Id = msg_send![url, URLByStandardizingPath];
                if let Some(path) = rust_string(msg_send![standardized, path]).filter(|p| Path::new(p).is_dir()) {
                    deliver(NativeSignal::Activation(Activation { url: None, project_path: Some(path), initial: !ready }));
                }
            } else if let Some(text) = rust_string(msg_send![url, absoluteString]) {
                if text.to_ascii_lowercase().starts_with("paseo:") {
                    deliver(NativeSignal::Activation(Activation { url: Some(text), project_path: None, initial: !ready }));
                }
            }
        }
    }
}

extern "C" fn should_handle_reopen(_this: &AnyObject, _sel: Sel, _app: Id, _visible: Bool) -> Bool {
    deliver(NativeSignal::Activated);
    Bool::NO
}

/// NSTerminateCancel 0, NSTerminateNow 1, NSTerminateLater 2.
extern "C" fn should_terminate(_this: &AnyObject, _sel: Sel, _app: Id) -> usize {
    if TERMINATION_ALLOWED.load(Ordering::SeqCst) {
        return 1;
    }
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        // Logout/shutdown: defer (cancelling would abort the OS request) while
        // the owned daemon is stopped off the main thread; the reply arrives
        // through the main dispatch queue, which the modal wait services.
        if let Some(platform) = crate::platform() {
            TERMINATION_PENDING.store(true, Ordering::SeqCst);
            super::request_quit(&platform.0, QuitReason::Shutdown, true);
            return 2;
        }
        return 1;
    }
    // A user Quit runs the guarded flow (close guards may refuse); the final
    // `terminate:` after QuitReady is then allowed.
    deliver(NativeSignal::QuitRequestedByOs);
    0
}

pub(crate) fn prepare_process() -> anyhow::Result<()> {
    unsafe {
        apple::add_method(c"NSAppDelegate", c"application:openURLs:", imp!(open_urls), "v@:@@");
        apple::add_method(
            c"NSAppDelegate",
            c"applicationShouldHandleReopen:hasVisibleWindows:",
            imp!(should_handle_reopen),
            &format!("{BOOL_ENC}@:@{BOOL_ENC}"),
        );
        apple::add_method(c"NSAppDelegate", c"applicationShouldTerminate:", imp!(should_terminate), "Q@:@");
    }
    Ok(())
}

pub(crate) fn termination_decided(allowed: bool) {
    TERMINATION_ALLOWED.store(allowed, Ordering::SeqCst);
    if TERMINATION_PENDING.swap(false, Ordering::SeqCst) {
        apple::on_main_queue(move || unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, replyToApplicationShouldTerminate: allowed];
        });
    }
}

// ----- lifecycle -----

/// Observer tokens kept for the process lifetime.
struct Observers(Vec<usize>);
static OBSERVERS: Mutex<Observers> = Mutex::new(Observers(Vec::new()));

pub(crate) fn start_lifecycle(_shared: &Arc<Shared>) {
    unsafe {
        let workspace: Id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let workspace_center: Id = msg_send![workspace, notificationCenter];
        let power_off = block2::RcBlock::new(|_note: Id| {
            SHUTTING_DOWN.store(true, Ordering::SeqCst);
            deliver(NativeSignal::SessionEnded);
        });
        let main_queue: Id = msg_send![class!(NSOperationQueue), mainQueue];
        let token: Id = msg_send![
            workspace_center,
            addObserverForName: ns_string("NSWorkspaceWillPowerOffNotification"),
            object: std::ptr::null_mut::<AnyObject>(),
            queue: main_queue,
            usingBlock: &*power_off
        ];
        let _: Id = msg_send![token, retain];
        let center: Id = msg_send![class!(NSNotificationCenter), defaultCenter];
        let active = block2::RcBlock::new(|_note: Id| deliver(NativeSignal::Activated));
        let token2: Id = msg_send![
            center,
            addObserverForName: ns_string("NSApplicationDidBecomeActiveNotification"),
            object: std::ptr::null_mut::<AnyObject>(),
            queue: main_queue,
            usingBlock: &*active
        ];
        let _: Id = msg_send![token2, retain];
        OBSERVERS.lock().unwrap().0.extend([token as usize, token2 as usize]);
    }
    deliver(NativeSignal::LifecycleStarted(Ok(())));
}

// ----- notifications -----

extern "C" fn did_receive_response(_this: &AnyObject, _sel: Sel, _center: Id, response: Id, completion: Id) {
    let (identifier, info) = user_notifications::response_info(response);
    if let Some(payload) = info.get("payload").and_then(|p| p.as_str()) {
        notification_clicked(payload, identifier.parse::<i64>().ok(), user_notifications::action_identifier(response));
    }
    call_void_block(completion);
}

/// Foreground presentation: banner + list + sound (original presentAlert/Banner/List/Sound).
extern "C" fn will_present(_this: &AnyObject, _sel: Sel, _center: Id, _notification: Id, completion: Id) {
    // UNNotificationPresentationOptionSound 2 | List 8 | Banner 16
    let options: usize = 2 | 8 | 16;
    call_usize_block(completion, options);
}

pub(crate) fn call_void_block(block: Id) {
    if block.is_null() {
        return;
    }
    let block = block as *const block2::Block<dyn Fn()>;
    unsafe { (*block).call(()) };
}

pub(crate) fn call_usize_block(block: Id, value: usize) {
    if block.is_null() {
        return;
    }
    let block = block as *const block2::Block<dyn Fn(usize)>;
    unsafe { (*block).call((value,)) };
}

pub(crate) fn init_notifications(_shared: &Arc<Shared>) -> Result<(), PlatformError> {
    unsafe {
        let class = apple::define_class(
            c"PaseoNotificationDelegate",
            class!(NSObject),
            &[
                (c"userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:", imp!(did_receive_response), "v@:@@@?"),
                (c"userNotificationCenter:willPresentNotification:withCompletionHandler:", imp!(will_present), "v@:@@@?"),
            ],
        );
        let delegate: Id = msg_send![class, new];
        let _: () = msg_send![user_notifications::center(), setDelegate: delegate];
    }
    Ok(())
}

pub(crate) async fn show_notification(notification: &DesktopNotification) -> Result<(), PlatformError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    user_notifications::add(
        &notification.id.to_string(),
        &notification.title,
        notification.body.as_deref(),
        notification.sound,
        &notification.payload,
        move |result| {
            let _ = tx.send(result);
        },
    );
    rx.await.unwrap_or_else(|_| Err(PlatformError::native("Notification delivery was dropped.")))
}

pub(crate) fn request_notification_permission(_shared: &Arc<Shared>, done: Completion) {
    user_notifications::request_authorization(move |result| done.finish(result.map(PlatformValue::Permission)));
}

pub(crate) fn notification_permission_status(_shared: &Arc<Shared>, done: Completion) {
    user_notifications::authorization_status(move |state| done.ok(PlatformValue::Permission(state)));
}

pub(crate) fn set_badge_count(_shared: &Arc<Shared>, count: u32, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let tile: Id = msg_send![app, dockTile];
        let label: Id = if count == 0 { std::ptr::null_mut() } else { ns_string(&count.to_string()) };
        let _: () = msg_send![tile, setBadgeLabel: label];
        let _: () = msg_send![tile, display];
        done.ok(PlatformValue::None);
    });
}

// ----- idle -----

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

pub(crate) fn idle_supported() -> bool {
    true
}

pub(crate) fn idle_milliseconds() -> Result<u64, PlatformError> {
    // kCGEventSourceStateCombinedSessionState 0; kCGAnyInputEventType ~0.
    let seconds = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(PlatformError::new("idle_failed", "CoreGraphics could not read system idle time"));
    }
    Ok((seconds * 1000.0) as u64)
}

// ----- native windows (close-to-tray) -----

#[derive(Default)]
pub(crate) struct NativeWindows {
    hidden: Vec<usize>,
}

impl NativeWindows {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hide_all(&mut self) {
        unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let windows: Id = msg_send![app, windows];
            let count: usize = msg_send![windows, count];
            for i in 0..count {
                let window: Id = msg_send![windows, objectAtIndex: i];
                let visible: bool = msg_send![window, isVisible];
                if visible {
                    let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
                    self.hidden.push(window as usize);
                }
            }
        }
    }

    pub fn show_all(&mut self) {
        unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let windows: Id = msg_send![app, windows];
            let count: usize = msg_send![windows, count];
            let hidden = std::mem::take(&mut self.hidden);
            for i in 0..count {
                let window: Id = msg_send![windows, objectAtIndex: i];
                let visible: bool = msg_send![window, isVisible];
                let minimized: bool = msg_send![window, isMiniaturized];
                if minimized {
                    let _: () = msg_send![window, deminiaturize: std::ptr::null_mut::<AnyObject>()];
                }
                if visible || minimized || hidden.contains(&(window as usize)) {
                    let _: () = msg_send![window, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
                }
            }
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
    }
}

// ----- tray -----

pub(crate) use super::native_tray::{create_tray, tray_host_available, Tray};

// ----- open / reveal / share -----

fn open(args: Vec<OsString>) -> Result<(), PlatformError> {
    super::editors::run("/usr/bin/open", &args)
}

pub(crate) fn open_uri(url: &str) -> Result<(), PlatformError> {
    open(vec![url.into()])
}

pub(crate) fn open_path(path: &Path) -> Result<(), PlatformError> {
    open(vec![path.as_os_str().to_owned()])
}

pub(crate) fn reveal(workspace: &Path, file: Option<&Path>) -> Result<(), PlatformError> {
    match file {
        Some(file) => open(vec!["-R".into(), file.as_os_str().to_owned()]),
        None => open(vec![workspace.as_os_str().to_owned()]),
    }
}

/// share_plus macOS: NSSharingServicePicker anchored to the key window.
pub(crate) fn share_file(_shared: &Arc<Shared>, path: PathBuf, _mime_type: String, _name: String, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let mut window: Id = msg_send![app, keyWindow];
        if window.is_null() {
            window = msg_send![app, mainWindow];
        }
        if window.is_null() {
            done.fail(PlatformError::native("No window is available to anchor the share sheet."));
            return;
        }
        let view: Id = msg_send![window, contentView];
        let items: Id = msg_send![class!(NSArray), arrayWithObject: apple::file_url(&path)];
        let picker: Id = msg_send![class!(NSSharingServicePicker), alloc];
        let picker: Id = msg_send![picker, initWithItems: items];
        let bounds: objc2_rect::Rect = msg_send![view, bounds];
        // NSMinYEdge 1
        let _: () = msg_send![picker, showRelativeToRect: bounds, ofView: view, preferredEdge: 1usize];
        done.ok(PlatformValue::None);
    });
}

/// NSRect with an Objective-C encoding, for `bounds` / `showRelativeToRect:`.
mod objc2_rect {
    use objc2::encode::{Encode, Encoding};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct Rect {
        pub x: f64,
        pub y: f64,
        pub width: f64,
        pub height: f64,
    }

    unsafe impl Encode for Rect {
        const ENCODING: Encoding = Encoding::Struct(
            "CGRect",
            &[
                Encoding::Struct("CGPoint", &[Encoding::Double, Encoding::Double]),
                Encoding::Struct("CGSize", &[Encoding::Double, Encoding::Double]),
            ],
        );
    }
}

// ----- runtime info / appearance -----

pub(crate) fn memory_usage() -> (Option<u64>, Option<u64>) {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    let read = unsafe { libc::proc_pidinfo(std::process::id() as i32, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    let resident = (read == size).then_some(info.pti_resident_size);
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // ru_maxrss is in bytes on macOS.
    let peak = (unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0).then_some(usage.ru_maxrss as u64);
    (resident, peak)
}

pub(crate) fn os_version() -> String {
    unsafe {
        let info: Id = msg_send![class!(NSProcessInfo), processInfo];
        rust_string(msg_send![info, operatingSystemVersionString]).unwrap_or_default()
    }
}

pub(crate) fn locale() -> String {
    apple::preferred_language()
}

/// macOS has no system-wide text-size setting for AppKit apps (only per-app
/// and Accessibility zoom), so text keeps the identity scale.
pub(crate) fn system_text_scale() -> crate::appearance::TextScale {
    crate::appearance::TextScale::default()
}

pub(crate) fn system_prefers_dark() -> bool {
    unsafe {
        let defaults: Id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let style: Id = msg_send![defaults, stringForKey: ns_string("AppleInterfaceStyle")];
        rust_string(style).is_some_and(|s| s.eq_ignore_ascii_case("dark"))
    }
}
