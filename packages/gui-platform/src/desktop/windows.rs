//! Windows desktop integration (original windows/runner/main.cpp + flutter_window,
//! the Windows paseo_desktop_integrations plugin, flutter_local_notifications
//! Windows and tray_manager).

use super::{
    deliver, notification_clicked,
    windows_util::{hstring, Wide},
    Activation, DesktopNotification, NativeSignal,
};
use crate::{
    action::*,
    platform::{Completion, Shared},
    Bootstrap,
};
use std::{
    collections::HashMap,
    ffi::OsString,
    os::windows::process::CommandExt as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};
use windows::{
    core::{w, Interface, HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM},
        Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, FillRect,
            SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
            DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, ANTIALIASED_QUALITY,
            OUT_DEFAULT_PRECIS, TRANSPARENT, CreateBitmap, HGDIOBJ,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
            DataExchange::COPYDATASTRUCT,
            LibraryLoader::GetModuleHandleW,
            Threading::{CreateEventW, CreateMutexW, CreateProcessW, SetEvent, WaitForSingleObject, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW},
        },
        UI::{
            Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
            Shell::{IDataTransferManagerInterop, ITaskbarList3, SetCurrentProcessExplicitAppUserModelID, ShellExecuteW, TaskbarList},
            WindowsAndMessaging::{
                CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyIcon, DispatchMessageW, EnumWindows, FindWindowW, GetClassNameW,
                GetMessageW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, RegisterClassExW, SendMessageTimeoutW, SetForegroundWindow,
                ShowWindow, TranslateMessage, AllowSetForegroundWindow, GetAncestor, GetForegroundWindow, GA_ROOT, ICONINFO, MSG, SMTO_ABORTIFHUNG, SW_HIDE,
                SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, WINDOW_EX_STYLE, WM_ACTIVATEAPP, WM_COPYDATA, WM_ENDSESSION, WM_QUERYENDSESSION,
                WNDCLASSEXW, WS_OVERLAPPED,
            },
        },
    },
};

const AUMID: &str = crate::paths::DESKTOP_APP_ID;
const ACTIVATION_CLASS: PCWSTR = w!("PaseoSessionsActivation");
const COPYDATA_ACTIVATION: usize = 0x5053_4143; // 'PSAC'

fn native_error(context: &str, error: windows::core::Error) -> PlatformError {
    PlatformError::native(format!("{context}: {error}"))
}

pub(crate) fn prepare_process() -> anyhow::Result<()> {
    // Toasts attribute to this AUMID, which the installer registers.
    unsafe { SetCurrentProcessExplicitAppUserModelID(&hstring(AUMID))? };
    Ok(())
}

// ----- single instance + lifecycle window -----

struct Handles {
    _mutex: HANDLE,
    _ready: HANDLE,
}
unsafe impl Send for Handles {}
static HANDLES: Mutex<Option<Handles>> = Mutex::new(None);
static LIFECYCLE: AtomicBool = AtomicBool::new(false);
static ACTIVATION_WINDOW: AtomicIsize = AtomicIsize::new(0);

/// main.cpp `ReadActivation`: a single `paseo:` argument is a link; otherwise the
/// first existing directory (or the one after --open-project), made absolute.
fn read_activation(argv: &[String]) -> Activation {
    let args = &argv[1.min(argv.len())..];
    if args.len() == 1 && args[0].to_ascii_lowercase().starts_with("paseo:") {
        return Activation { url: Some(args[0].clone()), project_path: None, initial: false };
    }
    let mut index = 0;
    while index < args.len() {
        let candidate = if args[index] == "--open-project" || args[index] == "--paseo-project" {
            index += 1;
            match args.get(index) {
                Some(value) => value.clone(),
                None => break,
            }
        } else if args[index].starts_with('-') {
            index += 1;
            continue;
        } else {
            args[index].clone()
        };
        if Path::new(&candidate).is_dir() {
            if let Ok(absolute) = std::path::absolute(&candidate) {
                return Activation { url: None, project_path: Some(absolute.display().to_string()), initial: false };
            }
        }
        index += 1;
    }
    Activation::default()
}

pub(crate) fn bootstrap(argv: &[String]) -> Bootstrap {
    let activation = read_activation(argv);
    unsafe {
        let ready = match CreateEventW(None, true, false, w!("Local\\sh.paseo.sessions.desktop.ready")) {
            Ok(handle) => handle,
            Err(_) => return Bootstrap::Forwarded { exit_code: 1 },
        };
        let mutex = CreateMutexW(None, true, w!("Local\\sh.paseo.sessions.desktop"));
        let existing = GetLastError() == ERROR_ALREADY_EXISTS;
        let Ok(mutex) = mutex else {
            return Bootstrap::Forwarded { exit_code: 1 };
        };
        if existing {
            // The first process may still be starting; never create a second
            // engine merely because its window is not ready yet.
            if WaitForSingleObject(ready, 30_000) != WAIT_OBJECT_0 {
                return Bootstrap::Forwarded { exit_code: 1 };
            }
            let Ok(primary) = FindWindowW(ACTIVATION_CLASS, PCWSTR::null()) else {
                return Bootstrap::Forwarded { exit_code: 1 };
            };
            let mut process_id = 0u32;
            GetWindowThreadProcessId(primary, Some(&mut process_id));
            let _ = AllowSetForegroundWindow(process_id);
            let payload = serde_json::to_vec(&activation).unwrap_or_default();
            let data = COPYDATASTRUCT { dwData: COPYDATA_ACTIVATION, cbData: payload.len() as u32, lpData: payload.as_ptr() as *mut _ };
            let mut accepted = 0usize;
            let sent = SendMessageTimeoutW(
                primary,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM(&data as *const _ as isize),
                SMTO_ABORTIFHUNG,
                5000,
                Some(&mut accepted),
            );
            let ok = sent.0 != 0 && accepted == 1;
            return Bootstrap::Forwarded { exit_code: if ok { 0 } else { 1 } };
        }
        *HANDLES.lock().unwrap() = Some(Handles { _mutex: mutex, _ready: ready });
        let initial = Activation { initial: true, ..activation };
        if initial.url.is_some() || initial.project_path.is_some() {
            deliver(NativeSignal::Activation(initial));
        }
        start_activation_window(ready);
    }
    Bootstrap::Primary
}

/// A hidden top-level window on its own thread: it receives WM_COPYDATA from
/// later launches and the session-end broadcasts sent to top-level windows.
fn start_activation_window(ready: HANDLE) {
    let ready = ready.0 as isize;
    std::thread::Builder::new()
        .name("paseo-activation".into())
        .spawn(move || unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: ACTIVATION_CLASS,
                ..Default::default()
            };
            RegisterClassExW(&class);
            let Ok(window) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                ACTIVATION_CLASS,
                w!("Passession"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            ) else {
                return;
            };
            ACTIVATION_WINDOW.store(window.0 as isize, Ordering::SeqCst);
            let _ = SetEvent(HANDLE(ready as *mut _));
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        })
        .expect("activation thread");
}

static TERMINATION: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_COPYDATA => {
            let data = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
            if data.dwData != COPYDATA_ACTIVATION {
                return LRESULT(0);
            }
            let bytes = unsafe { std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize) };
            match serde_json::from_slice::<Activation>(bytes) {
                Ok(activation) => {
                    deliver(NativeSignal::Activation(activation));
                    LRESULT(1)
                }
                Err(_) => LRESULT(0),
            }
        }
        WM_QUERYENDSESSION if LIFECYCLE.load(Ordering::SeqCst) => {
            deliver(NativeSignal::SessionEnding);
            // Never veto OS shutdown.
            LRESULT(1)
        }
        WM_ENDSESSION if LIFECYCLE.load(Ordering::SeqCst) => {
            if wparam.0 != 0 {
                // Windows may end the process when this returns: run the forced
                // quit (stop the owned daemon unless keep-running) and wait for it
                // within the OS deadline.
                if let Some(platform) = crate::platform() {
                    super::request_quit(&platform.0, QuitReason::Shutdown, true);
                    let (lock, ready) = &TERMINATION;
                    let guard = lock.lock().unwrap();
                    let _ = ready.wait_timeout_while(guard, Duration::from_secs(4), |done| !*done);
                }
            } else {
                deliver(NativeSignal::SessionEndingCancelled);
            }
            LRESULT(0)
        }
        WM_ACTIVATEAPP if LIFECYCLE.load(Ordering::SeqCst) && wparam.0 != 0 => {
            deliver(NativeSignal::Activated);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

pub(crate) fn start_lifecycle(_shared: &Arc<Shared>) {
    LIFECYCLE.store(true, Ordering::SeqCst);
    deliver(NativeSignal::LifecycleStarted(Ok(())));
}

pub(crate) fn termination_decided(allowed: bool) {
    if allowed {
        let (lock, ready) = &TERMINATION;
        *lock.lock().unwrap() = true;
        ready.notify_all();
    }
}

// ----- notifications (WinRT toast with this process's AUMID) -----

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The toast activator class id the Flutter client registered
/// (`WindowsInitializationSettings.guid`), kept so existing registrations match.
const ACTIVATOR_CLSID: &str = "{72054DD0-FC41-4C29-8F0E-D407AFCC7265}";
const ACTIVATOR_GUID: windows::core::GUID = windows::core::GUID::from_u128(0x72054dd0_fc41_4c29_8f0e_d407afcc7265);

/// flutter_local_notifications_windows `NotificationActivationCallback`: a
/// toast click (or one of its actions) calls `Activate` with the toast's
/// `launch` payload, whether or not this process was running at the time.
#[windows::core::implement(windows::Win32::UI::Notifications::INotificationActivationCallback)]
struct ToastActivation;

impl windows::Win32::UI::Notifications::INotificationActivationCallback_Impl for ToastActivation_Impl {
    fn Activate(
        &self,
        _app: &PCWSTR,
        invoked_args: &PCWSTR,
        _data: *const windows::Win32::UI::Notifications::NOTIFICATION_USER_INPUT_DATA,
        _count: u32,
    ) -> windows::core::Result<()> {
        let payload = if invoked_args.is_null() { String::new() } else { unsafe { invoked_args.to_string() }.unwrap_or_default() };
        notification_clicked(&payload, None, None);
        Ok(())
    }
}

#[windows::core::implement(windows::Win32::System::Com::IClassFactory)]
struct ToastActivationFactory;

impl windows::Win32::System::Com::IClassFactory_Impl for ToastActivationFactory_Impl {
    fn CreateInstance(
        &self,
        outer: windows::core::Ref<'_, windows::core::IUnknown>,
        iid: *const windows::core::GUID,
        object: *mut *mut std::ffi::c_void,
    ) -> windows::core::Result<()> {
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(windows::Win32::Foundation::CLASS_E_NOAGGREGATION.into());
        }
        let callback: windows::Win32::UI::Notifications::INotificationActivationCallback = ToastActivation.into();
        unsafe { callback.query(iid, object) }.ok()
    }

    fn LockServer(&self, _lock: windows::core::BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

fn set_registry_string(root: windows::Win32::System::Registry::HKEY, path: &str, name: Option<&str>, value: &str) -> Result<(), PlatformError> {
    use windows::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ};
    let path = Wide::new(path);
    let mut key = HKEY::default();
    let created = unsafe { RegCreateKeyExW(root, path.pcwstr(), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None) };
    created.ok().map_err(|e| native_error("Could not register notifications", e))?;
    let name = name.map(Wide::new);
    let data: Vec<u8> = value.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
    let written = unsafe { RegSetValueExW(key, name.as_ref().map_or(PCWSTR::null(), Wide::pcwstr), None, REG_SZ, Some(&data)) };
    unsafe {
        let _ = RegCloseKey(key);
    }
    written.ok().map_err(|e| native_error("Could not register notifications", e))
}

/// flutter_local_notifications_windows `registerApp`: notification settings
/// backup, the AUMID display name and `CustomActivator`, then the class
/// object. `LocalServer32` lets COM start this executable for a click that
/// arrives after the app has exited (it starts with `-ToastActivated`, takes
/// the primary instance, registers the class object here and receives
/// `Activate`; the route waits for the first window).
pub(crate) fn init_notifications(_shared: &Arc<Shared>) -> Result<(), PlatformError> {
    use windows::Win32::System::Registry::HKEY_CURRENT_USER;
    let backup = format!("Software\\Microsoft\\Windows\\CurrentVersion\\PushNotifications\\Backup\\{AUMID}");
    set_registry_string(HKEY_CURRENT_USER, &backup, Some("appType"), "app:desktop")?;
    set_registry_string(HKEY_CURRENT_USER, &backup, Some("Setting"), "s:banner,s:toast,s:audio,c:toast,c:ringing")?;
    set_registry_string(HKEY_CURRENT_USER, &backup, Some("wnsId"), "NonImmersivePackage")?;
    let app = format!("Software\\Classes\\AppUserModelId\\{AUMID}");
    set_registry_string(HKEY_CURRENT_USER, &app, Some("DisplayName"), "Passession")?;
    set_registry_string(HKEY_CURRENT_USER, &app, Some("CustomActivator"), ACTIVATOR_CLSID)?;
    let executable = std::env::current_exe().map_err(|e| PlatformError::io(&e))?;
    let server = format!("Software\\Classes\\CLSID\\{ACTIVATOR_CLSID}\\LocalServer32");
    set_registry_string(HKEY_CURRENT_USER, &server, None, &format!("\"{}\" -ToastActivated", executable.display()))?;
    // The class object lives in the multithreaded apartment, kept by a
    // dedicated thread, so activations do not depend on the GUI message loop.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("paseo-toast-activator".into())
        .spawn(move || unsafe {
            use windows::Win32::System::Com::{CoRegisterClassObject, IClassFactory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE};
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let factory: IClassFactory = ToastActivationFactory.into();
            let registered = CoRegisterClassObject(&ACTIVATOR_GUID, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE);
            let failed = registered.is_err();
            let _ = tx.send(registered.map(|_| ()).map_err(|e| native_error("Could not register notification activation", e)));
            if !failed {
                // Keeps the apartment (and the registration) for the process lifetime.
                loop {
                    std::thread::park();
                }
            }
        })
        .map_err(|e| PlatformError::native(e.to_string()))?;
    rx.recv().map_err(|_| PlatformError::native("Notification activation registration stopped."))?
}

/// The toast's `launch` attribute carries the click payload to `Activate`.
pub(crate) async fn show_notification(notification: &DesktopNotification) -> Result<(), PlatformError> {
    use windows::{Data::Xml::Dom::XmlDocument, UI::Notifications::*};
    let audio = if notification.sound {
        r#"<audio src="ms-winsoundevent:Notification.Default"/>"#
    } else {
        r#"<audio silent="true"/>"#
    };
    let xml = format!(
        r#"<toast launch="{}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>{audio}</toast>"#,
        xml_escape(&notification.payload),
        xml_escape(&notification.title),
        xml_escape(notification.body.as_deref().unwrap_or("")),
    );
    let id = notification.id;
    tokio::task::spawn_blocking(move || -> Result<(), PlatformError> {
        let document = XmlDocument::new().map_err(|e| native_error("toast", e))?;
        document.LoadXml(&HSTRING::from(xml)).map_err(|e| native_error("toast", e))?;
        let toast = ToastNotification::CreateToastNotification(&document).map_err(|e| native_error("toast", e))?;
        let _ = toast.SetTag(&HSTRING::from(id.to_string()));
        ToastNotificationManager::CreateToastNotifierWithId(&hstring(AUMID))
            .and_then(|notifier| notifier.Show(&toast))
            .map_err(|e| native_error("Could not show the notification", e))
    })
    .await
    .map_err(|e| PlatformError::native(e.to_string()))?
}

/// Windows manages notification permission in system settings.
pub(crate) fn request_notification_permission(_shared: &Arc<Shared>, done: Completion) {
    done.ok(PlatformValue::Permission(PermissionState::Granted));
}

pub(crate) fn notification_permission_status(shared: &Arc<Shared>, done: Completion) {
    request_notification_permission(shared, done);
}

/// Small opaque red badge icon (the taskbar copies it before it is freed).
unsafe fn badge_icon(count: u32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    const SIZE: i32 = 32;
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        let mut pixels: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0).ok()?;
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
        let old = SelectObject(dc, HGDIOBJ(bitmap.0));
        let mut rect = RECT { left: 0, top: 0, right: SIZE, bottom: SIZE };
        let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(50 << 16 | 35 << 8 | 190));
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let font = CreateFontW(-20, 0, 0, 0, 700, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, DEFAULT_PITCH.0 as u32, w!("Segoe UI"));
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00ff_ffff));
        let mut text: Vec<u16> = if count > 99 { "99+".encode_utf16().collect() } else { count.to_string().encode_utf16().collect() };
        DrawTextW(dc, &mut text, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        let rgba = std::slice::from_raw_parts_mut(pixels as *mut u32, (SIZE * SIZE) as usize);
        for pixel in rgba.iter_mut() {
            *pixel |= 0xff00_0000;
        }
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let icon_info = ICONINFO { fIcon: true.into(), hbmColor: bitmap, hbmMask: mask, ..Default::default() };
        let icon = CreateIconIndirect(&icon_info).ok();
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

pub(crate) fn set_badge_count(shared: &Arc<Shared>, count: u32, done: Completion) {
    shared.complete_blocking(done, move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let taskbar: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| PlatformError::new("taskbar_unavailable", e.to_string()))?;
        taskbar.HrInit().map_err(|e| PlatformError::new("taskbar_unavailable", e.to_string()))?;
        let icon = if count == 0 { None } else { Some(badge_icon(count).ok_or_else(|| PlatformError::new("badge_icon_failed", "Could not draw the badge icon."))?) };
        let description = Wide::new(format!("{count} unread notifications"));
        let mut result = Ok(PlatformValue::None);
        for window in own_windows() {
            if let Err(error) = taskbar.SetOverlayIcon(window, icon.unwrap_or_default(), description.pcwstr()) {
                result = Err(PlatformError::new("badge_failed", error.to_string()));
            }
        }
        if let Some(icon) = icon {
            let _ = DestroyIcon(icon);
        }
        result
    });
}

// ----- idle -----

pub(crate) fn idle_supported() -> bool {
    true
}

pub(crate) fn idle_milliseconds() -> Result<u64, PlatformError> {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return Err(PlatformError::new("idle_failed", "GetLastInputInfo failed"));
    }
    let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
    Ok(u64::from(now.wrapping_sub(info.dwTime)))
}

// ----- native windows (close-to-tray) -----

/// This process's Makepad frame windows, resolved at every call (never
/// cached from early initialization) and normalized to the top-level frame
/// with `GetAncestor(GA_ROOT)`, so show/hide/restore/focus, the taskbar
/// overlay and the share UI act on the frame that owns the taskbar button,
/// not on a child/content HWND. (Makepad 1.0.0 leaves `CxOsOp::HideWindow`
/// unimplemented on Windows.)
fn own_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(window: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid == std::process::id() {
            let mut name = [0u16; 64];
            let length = unsafe { GetClassNameW(window, &mut name) } as usize;
            if String::from_utf16_lossy(&name[..length]) == "MakepadWindow" {
                let root = unsafe { GetAncestor(window, GA_ROOT) };
                let frame = if root.is_invalid() { window } else { root };
                if !list.contains(&frame) {
                    list.push(frame);
                }
            }
        }
        true.into()
    }
    let mut list: Vec<HWND> = Vec::new();
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize)) };
    list
}

#[derive(Default)]
pub(crate) struct NativeWindows {
    hidden: Vec<isize>,
}

impl NativeWindows {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hide_all(&mut self) {
        for window in own_windows() {
            if unsafe { IsWindowVisible(window) }.as_bool() {
                unsafe {
                    let _ = ShowWindow(window, SW_HIDE);
                }
                self.hidden.push(window.0 as isize);
            }
        }
    }

    /// Restores the windows the tray hid plus any visible or minimized ones.
    /// An invisible window the shell hid itself (the dormant first window,
    /// `PlatformHandle::hide_window`) stays hidden, as on macOS and X11.
    pub fn show_all(&mut self) {
        let hidden = std::mem::take(&mut self.hidden);
        let mut shown = Vec::new();
        for window in own_windows() {
            unsafe {
                let iconic = IsIconic(window).as_bool();
                let visible = IsWindowVisible(window).as_bool();
                if iconic {
                    let _ = ShowWindow(window, SW_RESTORE);
                } else if hidden.contains(&(window.0 as isize)) {
                    let _ = ShowWindow(window, SW_SHOW);
                } else if !visible {
                    continue;
                }
            }
            shown.push(window);
        }
        if let Some(first) = shown.first() {
            unsafe {
                if GetForegroundWindow() != *first {
                    let _ = SetForegroundWindow(*first);
                }
            }
        }
    }
}

// ----- tray -----

pub(crate) use super::native_tray::{create_tray, tray_host_available, Tray};

// ----- open / reveal / share / editors -----

fn shell_open(target: &std::ffi::OsStr) -> Result<(), PlatformError> {
    let target = Wide::new(target);
    let result = unsafe { ShellExecuteW(None, w!("open"), target.pcwstr(), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecute reports success with a value greater than 32.
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(PlatformError::native(format!("No application can open {} ({})", target_display(&target), result.0 as isize)))
    }
}

fn target_display(wide: &Wide) -> String {
    String::from_utf16_lossy(&wide.0[..wide.0.len().saturating_sub(1)])
}

pub(crate) fn open_uri(url: &str) -> Result<(), PlatformError> {
    shell_open(std::ffi::OsStr::new(url))
}

pub(crate) fn open_path(path: &Path) -> Result<(), PlatformError> {
    shell_open(path.as_os_str())
}

pub(crate) fn reveal(workspace: &Path, file: Option<&Path>) -> Result<(), PlatformError> {
    let mut command = std::process::Command::new("explorer.exe");
    match file {
        // explorer parses its own command line; Windows paths cannot contain quotes.
        Some(file) => command.raw_arg(format!("/select,\"{}\"", file.display())),
        None => command.arg(workspace),
    };
    command.spawn().map(|_| ()).map_err(|e| PlatformError::native(format!("explorer.exe: {e}")))
}

/// cmd.exe receives its exact grammar; each argument is expanded once from the
/// private environment block (`PASEO_EDITOR_ARG_<n>`), delayed expansion off.
pub(crate) fn launch_command_script(environment: &HashMap<OsString, OsString>, count: usize) -> Result<(), PlatformError> {
    let system = std::env::var_os("SystemRoot").map(PathBuf::from).ok_or_else(|| PlatformError::native("Could not locate the Windows system directory"))?;
    let shell = system.join("System32").join("cmd.exe");
    let mut command = format!("\"{}\" /D /V:OFF /S /C \"", shell.display());
    for index in 0..count {
        if index > 0 {
            command.push(' ');
        }
        command.push_str(&format!("\"%PASEO_EDITOR_ARG_{index}%\""));
    }
    command.push('"');
    let mut entries: Vec<String> = environment
        .iter()
        .filter_map(|(k, v)| Some(format!("{}={}", k.to_str()?, v.to_str()?)))
        .collect();
    entries.sort_by_key(|entry| entry.to_ascii_lowercase());
    let mut block: Vec<u16> = Vec::new();
    for entry in entries {
        block.extend(entry.encode_utf16());
        block.push(0);
    }
    block.push(0);
    let shell_wide = Wide::new(&shell);
    let mut command_wide = Wide::new(command);
    let startup = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            shell_wide.pcwstr(),
            Some(PWSTR(command_wide.0.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
            Some(block.as_ptr() as *const std::ffi::c_void),
            PCWSTR::null(),
            &startup,
            &mut process,
        )
        .map_err(|e| PlatformError::native(format!("Could not launch the editor: {e}")))?;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

fn storage_item_for_share(path: &Path) -> windows::core::Result<windows::core::AgileReference<windows::Storage::IStorageItem>> {
    use windows::Storage::{IStorageItem, StorageFile};
    // Marshal the file across the worker, UI and share-event apartments.
    StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
        .and_then(|op| op.get())
        .and_then(|file| file.cast::<IStorageItem>())
        .and_then(|item| windows::core::AgileReference::new(&item))
}

/// share_plus Windows: the DataTransferManager share UI for this window.
pub(crate) fn share_file(shared: &Arc<Shared>, path: PathBuf, _mime_type: String, name: String, done: Completion) {
    use windows::{ApplicationModel::DataTransfer::*, Storage::*};
    let owner = shared.clone();
    shared.spawn_blocking(move || {
        let item = match storage_item_for_share(&path) {
            Ok(item) => item,
            Err(error) => {
                done.fail(native_error("Could not share the file", error));
                return;
            }
        };
        owner.on_main(move |_, _| {
            let result = (|| -> windows::core::Result<()> {
                let window = *own_windows().first().ok_or_else(|| windows::core::Error::from_win32())?;
                let interop: IDataTransferManagerInterop = windows::core::factory::<DataTransferManager, IDataTransferManagerInterop>()?;
                let manager: DataTransferManager = unsafe { interop.GetForWindow(window)? };
                let title = HSTRING::from(name.as_str());
                manager.DataRequested(&windows::Foundation::TypedEventHandler::new(
                    move |_sender, args: windows::core::Ref<'_, DataRequestedEventArgs>| {
                        if let Some(args) = args.as_ref() {
                            let data = args.Request()?.Data()?;
                            data.Properties()?.SetTitle(&title)?;
                            let items: windows_collections::IIterable<IStorageItem> = vec![Some(item.resolve()?)].into();
                            data.SetStorageItemsReadOnly(&items)?;
                        }
                        Ok(())
                    },
                ))?;
                unsafe { interop.ShowShareUIForWindow(window) }
            })();
            done.finish(result.map(|()| PlatformValue::None).map_err(|e| native_error("Could not open the share sheet", e)));
        });
    });
}

// ----- runtime info / appearance -----

pub(crate) fn memory_usage() -> (Option<u64>, Option<u64>) {
    use windows::Win32::System::{ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS}, Threading::GetCurrentProcess};
    let mut counters = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    match unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) } {
        Ok(()) => (Some(counters.WorkingSetSize as u64), Some(counters.PeakWorkingSetSize as u64)),
        Err(_) => (None, None),
    }
}

fn registry_string(path: PCWSTR, name: PCWSTR) -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buffer = [0u16; 256];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, path, name, RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) };
    status.is_ok().then(|| String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)]))
}

pub(crate) fn os_version() -> String {
    let key = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let product = registry_string(key, w!("ProductName")).unwrap_or_else(|| "Windows".into());
    let build = registry_string(key, w!("CurrentBuild")).unwrap_or_default();
    format!("{product} (build {build})")
}

pub(crate) fn locale() -> String {
    let mut buffer = [0u16; 85];
    let length = unsafe { windows::Win32::Globalization::GetUserDefaultLocaleName(&mut buffer) };
    if length > 1 {
        String::from_utf16_lossy(&buffer[..length as usize - 1])
    } else {
        "en-US".into()
    }
}

pub(crate) fn system_prefers_dark() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 0
}

// ----- fonts and text scale -----

/// Settings ▸ Accessibility ▸ Text size: `HKCU\Software\Microsoft\Accessibility
/// TextScaleFactor` (percent, 100–225; absent = 100).
pub(crate) fn system_text_scale() -> crate::appearance::TextScale {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value = 100u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Accessibility"),
            w!("TextScaleFactor"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    if status.is_ok() && (100..=225).contains(&value) {
        crate::appearance::TextScale::from_factor(f64::from(value) / 100.0)
    } else {
        crate::appearance::TextScale::default()
    }
}

/// DirectWrite font discovery.
pub(crate) mod fonts {
    use crate::fonts::{finish_face, resolve, Backend, Family, FileCache, FontFace, FontRequest, Role, SystemFonts, HANGUL_PROBE, LATIN_PROBE};
    use windows::{
        core::{Interface, BOOL, PCWSTR},
        Win32::{
            Graphics::DirectWrite::{
                DWriteCreateFactory, IDWriteFactory, IDWriteFont, IDWriteFontCollection, IDWriteFontFace5, IDWriteFontFamily,
                IDWriteLocalFontFileLoader, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_AXIS_VALUE, DWRITE_FONT_STRETCH_NORMAL,
                DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
            },
            UI::WindowsAndMessaging::{SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS},
        },
    };

    /// Windows' default monospace family (`monospace` / `ui-monospace`).
    const SYSTEM_MONO: &str = "Consolas";
    /// The UI family when the message font cannot be read.
    const SYSTEM_UI: &str = "Segoe UI";
    /// The Korean UI font shipped with Windows 7–11.
    const HANGUL_FAMILIES: [&str; 1] = ["Malgun Gothic"];

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// `NONCLIENTMETRICS.lfMessageFont`: the font Windows uses for dialog and message text.
    fn message_font_family() -> Option<String> {
        let mut metrics = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        unsafe {
            SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, metrics.cbSize, Some((&raw mut metrics).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
        }
        .ok()?;
        let name = &metrics.lfMessageFont.lfFaceName;
        let length = name.iter().position(|unit| *unit == 0).unwrap_or(name.len());
        let family = String::from_utf16_lossy(&name[..length]);
        (!family.is_empty()).then_some(family)
    }

    struct DirectWrite {
        collection: IDWriteFontCollection,
        files: FileCache,
        ui_family: Option<String>,
    }

    impl DirectWrite {
        fn family(&self, name: &str) -> Option<IDWriteFontFamily> {
            let name = wide(name);
            let mut index = 0u32;
            let mut exists = BOOL(0);
            unsafe { self.collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists) }.ok()?;
            if !exists.as_bool() {
                return None;
            }
            unsafe { self.collection.GetFontFamily(index) }.ok()
        }

        fn font(family: &IDWriteFontFamily, role: Role) -> Option<IDWriteFont> {
            let style = if role.italic() { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL };
            unsafe { family.GetFirstMatchingFont(DWRITE_FONT_WEIGHT(i32::from(role.weight())), DWRITE_FONT_STRETCH_NORMAL, style) }.ok()
        }

        fn family_name(family: &IDWriteFontFamily) -> Option<String> {
            let names = unsafe { family.GetFamilyNames() }.ok()?;
            let english = wide("en-us");
            let mut index = 0u32;
            let mut exists = BOOL(0);
            let found = unsafe { names.FindLocaleName(PCWSTR(english.as_ptr()), &mut index, &mut exists) }.is_ok() && exists.as_bool();
            if !found {
                index = 0;
            }
            let length = unsafe { names.GetStringLength(index) }.ok()? as usize;
            let mut buffer = vec![0u16; length + 1];
            unsafe { names.GetString(index, &mut buffer) }.ok()?;
            Some(String::from_utf16_lossy(&buffer[..length]))
        }

        /// The font's local file, collection index and (Windows 10 1809+) axis values.
        fn load(&mut self, font: &IDWriteFont, role: Role, probe: char) -> Option<FontFace> {
            let face = unsafe { font.CreateFontFace() }.ok()?;
            let mut count = 0u32;
            unsafe { face.GetFiles(&mut count, None) }.ok()?;
            // Multi-file faces are Type 1 (`.pfm` + `.pfb`), which is not sfnt.
            if count != 1 {
                return None;
            }
            let mut files = [None];
            unsafe { face.GetFiles(&mut count, Some(files.as_mut_ptr())) }.ok()?;
            let file = files[0].take()?;
            let mut key = std::ptr::null_mut();
            let mut key_size = 0u32;
            unsafe { file.GetReferenceKey(&mut key, &mut key_size) }.ok()?;
            // Only local files have a path; remote/downloadable fonts are skipped.
            let loader: IDWriteLocalFontFileLoader = unsafe { file.GetLoader() }.ok()?.cast().ok()?;
            let length = unsafe { loader.GetFilePathLengthFromKey(key, key_size) }.ok()? as usize;
            let mut path = vec![0u16; length + 1];
            unsafe { loader.GetFilePathFromKey(key, key_size, &mut path) }.ok()?;
            let path = String::from_utf16_lossy(&path[..length]);
            let index = unsafe { face.GetIndex() };
            let mut variations = Vec::new();
            if let Ok(face) = face.cast::<IDWriteFontFace5>() {
                let mut values = vec![DWRITE_FONT_AXIS_VALUE::default(); unsafe { face.GetFontAxisValueCount() } as usize];
                if !values.is_empty() && unsafe { face.GetFontAxisValues(&mut values) }.is_ok() {
                    // DWRITE_MAKE_FONT_AXIS_TAG packs the first character in the low byte.
                    variations = values.iter().map(|value| (value.axisTag.0.to_le_bytes(), value.value)).collect();
                }
            }
            let data = self.files.read(&path)?;
            finish_face(data, index, &variations, role, probe, format!("{path}#{index}"))
        }

        fn named(&mut self, name: &str, role: Role) -> Option<(FontFace, String)> {
            let family = self.family(name)?;
            let font = Self::font(&family, role)?;
            let face = self.load(&font, role, LATIN_PROBE)?;
            Some((face, Self::family_name(&family).unwrap_or_else(|| name.to_owned())))
        }
    }

    impl Backend for DirectWrite {
        fn face(&mut self, family: Family<'_>, role: Role) -> Option<(FontFace, String)> {
            match family {
                Family::SystemUi => {
                    let ui_family = self.ui_family.get_or_insert_with(|| message_font_family().unwrap_or_else(|| SYSTEM_UI.to_owned())).clone();
                    self.named(&ui_family, role).or_else(|| self.named(SYSTEM_UI, role))
                }
                Family::SystemMono => self.named(SYSTEM_MONO, role),
                // DirectWrite has no CSS generic aliases; other generics resolve only if installed by that name.
                Family::Named(name) => self.named(name, role),
            }
        }

        fn hangul(&mut self, ui_family: Option<&str>, role: Role) -> Option<FontFace> {
            for name in HANGUL_FAMILIES.into_iter().chain(ui_family) {
                let Some(family) = self.family(name) else { continue };
                let Some(font) = Self::font(&family, role) else { continue };
                if !unsafe { font.HasCharacter(HANGUL_PROBE as u32) }.is_ok_and(|has| has.as_bool()) {
                    continue;
                }
                if let Some(face) = self.load(&font, role, HANGUL_PROBE) {
                    return Some(face);
                }
            }
            None
        }
    }

    /// DirectWrite resolution: the message font (`SPI_GETNONCLIENTMETRICS`, Segoe UI)
    /// through the system collection to its local file, index and axis values;
    /// Hangul from Malgun Gothic (then the UI family) when it maps U+D55C.
    pub(crate) fn system_fonts(request: &FontRequest) -> SystemFonts {
        let Ok(factory) = (unsafe { DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) }) else {
            return SystemFonts::default();
        };
        let mut collection = None;
        if unsafe { factory.GetSystemFontCollection(&mut collection, true) }.is_err() {
            return SystemFonts::default();
        }
        let Some(collection) = collection else { return SystemFonts::default() };
        resolve(&mut DirectWrite { collection, files: FileCache::default(), ui_family: None }, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{CoUninitialize, COINIT, COINIT_APARTMENTTHREADED};

    struct Apartment;
    impl Apartment {
        fn initialize(kind: COINIT) -> Self {
            unsafe { CoInitializeEx(None, kind).ok().unwrap() };
            Self
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    #[test]
    fn share_item_resolves_across_apartments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("공유.txt");
        std::fs::write(&path, b"apartment-safe shared file").unwrap();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let (finish, wait) = std::sync::mpsc::channel::<()>();
        let source = std::thread::spawn(move || {
            let _apartment = Apartment::initialize(COINIT_MULTITHREADED);
            send.send(storage_item_for_share(&path)).unwrap();
            let _ = wait.recv();
        });
        let reference = receive.recv().unwrap().unwrap();
        let destination = std::thread::spawn(move || {
            let _apartment = Apartment::initialize(COINIT_APARTMENTTHREADED);
            let item = reference.resolve().unwrap();
            assert_eq!(item.Name().unwrap().to_string(), "공유.txt");
            let resolved = PathBuf::from(item.Path().unwrap().to_string());
            assert_eq!(std::fs::read(resolved).unwrap(), b"apartment-safe shared file");
        });
        let result = destination.join();
        drop(finish);
        source.join().unwrap();
        result.unwrap();
    }
}
