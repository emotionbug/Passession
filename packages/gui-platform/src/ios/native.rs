//! `crate::native` for iOS.

use super::token_waiter;
use crate::{
    action::*,
    apple::{self, imp, ns_string, rust_string, user_notifications, Id},
    mobile::{deliver, MobileSignal},
    paths::AppPaths,
    platform::{Capabilities, Completion, PickFilesOptions, PlatformState, Shared},
    NotificationRequest,
};
use block2::RcBlock;
use makepad_widgets::{Cx, Event, VirtualKeyboardEvent};
use objc2::{
    class, msg_send,
    runtime::{AnyObject, Bool, Sel},
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    fn UIImagePNGRepresentation(image: Id) -> Id;
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

unsafe impl objc2::encode::Encode for EdgeInsets {
    const ENCODING: objc2::encode::Encoding =
        objc2::encode::Encoding::Struct("UIEdgeInsets", &[objc2::encode::Encoding::Double; 4]);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

unsafe impl objc2::encode::Encode for Rect {
    const ENCODING: objc2::encode::Encoding = objc2::encode::Encoding::Struct(
        "CGRect",
        &[
            objc2::encode::Encoding::Struct("CGPoint", &[objc2::encode::Encoding::Double, objc2::encode::Encoding::Double]),
            objc2::encode::Encoding::Struct("CGSize", &[objc2::encode::Encoding::Double, objc2::encode::Encoding::Double]),
        ],
    );
}

pub(crate) fn os_app_paths() -> AppPaths {
    // path_provider iOS: Library/Application Support, Library/Caches, NSTemporaryDirectory.
    unsafe extern "C" {
        fn NSSearchPathForDirectoriesInDomains(directory: usize, domains: usize, expand: Bool) -> Id;
        fn NSTemporaryDirectory() -> Id;
    }
    let directory = |kind: usize| -> PathBuf {
        unsafe {
            let paths = NSSearchPathForDirectoriesInDomains(kind, 1, Bool::YES);
            let first: Id = msg_send![paths, firstObject];
            PathBuf::from(rust_string(first).unwrap_or_default())
        }
    };
    let temp = unsafe { rust_string(NSTemporaryDirectory()) }.map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    AppPaths { support_dir: directory(14), cache_dir: directory(13), temp_dir: temp }
}

pub(crate) fn prepare_process(_cx: &mut Cx) -> anyhow::Result<()> {
    super::install_delegates();
    Ok(())
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        desktop_host: false,
        tray: false,
        editors: false,
        updater: false,
        directory_picker: false,
        save_dialog: false,
        push: true,
        qr_scan: true,
        share: true,
        idle: false,
    }
}

static OBSERVERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

pub(crate) fn start_services(_cx: &mut Cx, _shared: &Arc<Shared>, _state: &mut PlatformState) {
    unsafe {
        let center: Id = msg_send![class!(NSNotificationCenter), defaultCenter];
        let queue: Id = msg_send![class!(NSOperationQueue), mainQueue];
        let active = RcBlock::new(|_note: Id| deliver(MobileSignal::Resumed));
        let token: Id = msg_send![center, addObserverForName: ns_string("UIApplicationDidBecomeActiveNotification"), object: std::ptr::null_mut::<AnyObject>(), queue: queue, usingBlock: &*active];
        // AVAudioSessionInterruptionTypeBegan (1): nothing queued may resume.
        let interrupted = RcBlock::new(|note: Id| {
            let info: Id = msg_send![note, userInfo];
            let kind = apple::to_json(info).get("AVAudioSessionInterruptionTypeKey").and_then(Value::as_u64);
            if kind == Some(1) {
                deliver(MobileSignal::AudioInterrupted);
            }
        });
        let token2: Id = msg_send![center, addObserverForName: ns_string("AVAudioSessionInterruptionNotification"), object: std::ptr::null_mut::<AnyObject>(), queue: queue, usingBlock: &*interrupted];
        let _: Id = msg_send![token, retain];
        let _: Id = msg_send![token2, retain];
        OBSERVERS.lock().unwrap().extend([token as usize, token2 as usize]);
    }
    publish_insets(None);
}

fn key_window() -> Id {
    unsafe {
        let app: Id = msg_send![class!(UIApplication), sharedApplication];
        let windows: Id = msg_send![app, windows];
        let count: usize = msg_send![windows, count];
        for i in 0..count {
            let window: Id = msg_send![windows, objectAtIndex: i];
            let key: bool = msg_send![window, isKeyWindow];
            if key {
                return window;
            }
        }
        msg_send![windows, firstObject]
    }
}

fn root_controller() -> Id {
    let window = key_window();
    if window.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let mut controller: Id = msg_send![window, rootViewController];
        loop {
            let presented: Id = msg_send![controller, presentedViewController];
            if presented.is_null() {
                break;
            }
            controller = presented;
        }
        controller
    }
}

static KEYBOARD: Mutex<(f64, bool)> = Mutex::new((0.0, false));

/// Window safe-area insets plus the keyboard height (logical points).
fn publish_insets(keyboard: Option<(f64, bool)>) {
    if let Some(value) = keyboard {
        *KEYBOARD.lock().unwrap() = value;
    }
    let window = key_window();
    let insets: EdgeInsets = if window.is_null() { EdgeInsets::default() } else { unsafe { msg_send![window, safeAreaInsets] } };
    let (keyboard, visible) = *KEYBOARD.lock().unwrap();
    deliver(MobileSignal::Insets(SafeInsets {
        top: insets.top,
        bottom: insets.bottom,
        left: insets.left,
        right: insets.right,
        keyboard,
        keyboard_visible: visible,
    }));
}

pub(crate) fn handle_event(_cx: &mut Cx, _shared: &Arc<Shared>, _state: &mut PlatformState, event: &Event) {
    match event {
        Event::VirtualKeyboard(VirtualKeyboardEvent::DidShow { height, .. } | VirtualKeyboardEvent::WillShow { height, .. }) => {
            publish_insets(Some((*height, true)))
        }
        Event::VirtualKeyboard(VirtualKeyboardEvent::DidHide { .. } | VirtualKeyboardEvent::WillHide { .. }) => {
            publish_insets(Some((0.0, false)))
        }
        Event::WindowGeomChange(_) => publish_insets(None),
        _ => {}
    }
}

pub(crate) fn report_error(_shared: &Arc<Shared>, message: &str) {
    post_event(PlatformEvent::HostError { message: message.to_owned() });
}

/// iOS has no system Back; navigation stays inside the app.
pub(crate) fn background_app(_cx: &mut Cx) {}

/// Original AudioEngine.swift: playback/spokenAudio while playing.
pub(crate) fn acquire_audio_session() -> anyhow::Result<()> {
    unsafe {
        let session: Id = msg_send![class!(AVAudioSession), sharedInstance];
        let mut error: Id = std::ptr::null_mut();
        let ok: bool = msg_send![session, setCategory: ns_string("AVAudioSessionCategoryPlayback"), mode: ns_string("AVAudioSessionModeSpokenAudio"), options: 0usize, error: &mut error];
        if !ok {
            anyhow::bail!("{}", apple::error_message(error));
        }
        let ok: bool = msg_send![session, setActive: true, error: &mut error];
        if !ok {
            anyhow::bail!("{}", apple::error_message(error));
        }
    }
    Ok(())
}

/// Back to ambient, mixing with others (original release path).
pub(crate) fn release_audio_session() {
    unsafe {
        let session: Id = msg_send![class!(AVAudioSession), sharedInstance];
        let mut error: Id = std::ptr::null_mut();
        // AVAudioSessionSetActiveOptionNotifyOthersOnDeactivation 1; MixWithOthers 1
        let _: bool = msg_send![session, setActive: false, withOptions: 1usize, error: &mut error];
        let _: bool = msg_send![session, setCategory: ns_string("AVAudioSessionCategoryAmbient"), withOptions: 1usize, error: &mut error];
    }
}

pub(crate) fn open_external(_shared: &Arc<Shared>, url: String, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let app: Id = msg_send![class!(UIApplication), sharedApplication];
        let target = apple::url(&url);
        if target.is_null() {
            done.fail(PlatformError::invalid(format!("Not a valid URL: {url}")));
            return;
        }
        let done = Mutex::new(Some(done));
        let block = RcBlock::new(move |success: Bool| {
            if let Some(done) = done.lock().unwrap().take() {
                if success.as_bool() {
                    done.ok(PlatformValue::None);
                } else {
                    done.fail(PlatformError::native("No application can open this link."));
                }
            }
        });
        let options: Id = msg_send![class!(NSDictionary), dictionary];
        let _: () = msg_send![app, openURL: target, options: options, completionHandler: &*block];
    });
}

static DOCUMENT_CONTROLLERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// UIDocumentInteractionController "Open in" for a downloaded file.
pub(crate) fn open_path(_shared: &Arc<Shared>, path: PathBuf, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let controller: Id = msg_send![class!(UIDocumentInteractionController), interactionControllerWithURL: apple::file_url(&path)];
        let root = root_controller();
        if root.is_null() {
            done.fail(PlatformError::native("No window is available to present the document."));
            return;
        }
        let view: Id = msg_send![root, view];
        let bounds: Rect = msg_send![view, bounds];
        let presented: bool = msg_send![controller, presentOpenInMenuFromRect: bounds, inView: view, animated: true];
        if presented {
            // The controller must outlive its menu.
            let _: Id = msg_send![controller, retain];
            DOCUMENT_CONTROLLERS.lock().unwrap().push(controller as usize);
            done.ok(PlatformValue::None);
        } else {
            done.fail(PlatformError::native("No application can open this file."));
        }
    });
}

pub(crate) fn reveal_path(_shared: &Arc<Shared>, _path: PathBuf, done: Completion) {
    done.fail(PlatformError::unsupported("iOS has no file manager reveal; use share or open."));
}

// ----- document picker -----

static PICKER: Mutex<Option<(Completion, usize)>> = Mutex::new(None);

fn finish_picker(urls: Id) {
    let Some((done, delegate)) = PICKER.lock().unwrap().take() else { return };
    let mut files = Vec::new();
    let mut failure = None;
    if !urls.is_null() {
        unsafe {
            let count: usize = msg_send![urls, count];
            for i in 0..count {
                let url: Id = msg_send![urls, objectAtIndex: i];
                let Some(source) = rust_string(msg_send![url, path]).map(PathBuf::from) else { continue };
                let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                // Import mode copies into the app's inbox; keep a private staging copy.
                match crate::paths::transfer_staging_file(&name).and_then(|target| std::fs::copy(&source, &target).map(|size| (target, size))) {
                    Ok((target, size)) => files.push(PickedFile {
                        mime_type: crate::paths::mime_for_name(&name),
                        path: target,
                        name,
                        size,
                        temporary: true,
                    }),
                    Err(error) => failure = Some(PlatformError::io(&error)),
                }
            }
        }
    }
    unsafe {
        let _: () = msg_send![delegate as Id, release];
    }
    match failure {
        Some(error) if files.is_empty() => done.fail(error),
        _ => done.ok(PlatformValue::Files(files)),
    }
}

extern "C" fn picked(_this: &AnyObject, _sel: Sel, _picker: Id, urls: Id) {
    finish_picker(urls);
}

extern "C" fn picker_cancelled(_this: &AnyObject, _sel: Sel, _picker: Id) {
    finish_picker(std::ptr::null_mut());
}

pub(crate) fn pick_files(_shared: &Arc<Shared>, options: PickFilesOptions, done: Completion) {
    apple::on_main_queue(move || unsafe {
        if PICKER.lock().unwrap().is_some() {
            done.fail(PlatformError::native("A file picker is already open."));
            return;
        }
        let root = root_controller();
        if root.is_null() {
            done.fail(PlatformError::native("No window is available to present the file picker."));
            return;
        }
        let class = apple::define_class(
            c"PaseoDocumentPickerDelegate",
            class!(NSObject),
            &[
                (c"documentPicker:didPickDocumentsAtURLs:", imp!(picked), "v@:@@"),
                (c"documentPickerWasCancelled:", imp!(picker_cancelled), "v@:@"),
            ],
        );
        let delegate: Id = msg_send![class, new];
        let types: Id = msg_send![class!(NSArray), arrayWithObject: ns_string(if options.images_only { "public.image" } else { "public.item" })];
        let picker: Id = msg_send![class!(UIDocumentPickerViewController), alloc];
        // UIDocumentPickerModeImport 0
        let picker: Id = msg_send![picker, initWithDocumentTypes: types, inMode: 0usize];
        let _: () = msg_send![picker, setAllowsMultipleSelection: options.multiple];
        let _: () = msg_send![picker, setDelegate: delegate];
        *PICKER.lock().unwrap() = Some((done, delegate as usize));
        let _: () = msg_send![root, presentViewController: picker, animated: true, completion: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![picker, release];
    });
}

pub(crate) fn pick_directory(_shared: &Arc<Shared>, _title: Option<String>, done: Completion) {
    done.fail(PlatformError::unsupported("Mobile clients browse folders on the selected host."));
}

pub(crate) fn save_location(_shared: &Arc<Shared>, _suggested_name: String, done: Completion) {
    done.fail(PlatformError::unsupported("Mobile clients save through the share sheet."));
}

/// share_plus iOS: UIActivityViewController (popover anchored on iPad).
pub(crate) fn share_file(_shared: &Arc<Shared>, path: PathBuf, _mime_type: String, _name: String, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let root = root_controller();
        if root.is_null() {
            done.fail(PlatformError::native("No window is available to present the share sheet."));
            return;
        }
        let items: Id = msg_send![class!(NSArray), arrayWithObject: apple::file_url(&path)];
        let sheet: Id = msg_send![class!(UIActivityViewController), alloc];
        let sheet: Id = msg_send![sheet, initWithActivityItems: items, applicationActivities: std::ptr::null_mut::<AnyObject>()];
        let popover: Id = msg_send![sheet, popoverPresentationController];
        if !popover.is_null() {
            let view: Id = msg_send![root, view];
            let bounds: Rect = msg_send![view, bounds];
            let _: () = msg_send![popover, setSourceView: view];
            let _: () = msg_send![popover, setSourceRect: Rect { x: bounds.width / 2.0, y: bounds.height, width: 1.0, height: 1.0 }];
        }
        let _: () = msg_send![root, presentViewController: sheet, animated: true, completion: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![sheet, release];
        done.ok(PlatformValue::None);
    });
}

// ----- clipboard -----

fn pasteboard() -> Id {
    unsafe { msg_send![class!(UIPasteboard), generalPasteboard] }
}

pub(crate) fn copy_text(_shared: &Arc<Shared>, text: String, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let _: () = msg_send![pasteboard(), setString: ns_string(&text)];
        done.ok(PlatformValue::None);
    });
}

pub(crate) fn read_clipboard(_shared: &Arc<Shared>, images_only: bool, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let board = pasteboard();
        let mut bytes = Vec::new();
        for kind in ["public.png", "public.jpeg", "com.compuserve.gif", "org.webmproject.webp"] {
            let data: Id = msg_send![board, dataForPasteboardType: ns_string(kind)];
            if !data.is_null() {
                bytes = apple::data_bytes(data);
                break;
            }
        }
        if bytes.is_empty() {
            let image: Id = msg_send![board, image];
            if !image.is_null() {
                // A platform-specific representation is transcoded to real PNG bytes.
                bytes = apple::data_bytes(UIImagePNGRepresentation(image));
            }
        }
        if let Some(mime) = crate::image::sniff_image_mime(&bytes) {
            done.ok(PlatformValue::Clipboard(ClipboardContent {
                images: vec![ClipboardImage { bytes: Arc::new(bytes), mime_type: mime.to_owned() }],
                text: None,
            }));
            return;
        }
        if images_only {
            done.fail(PlatformError::new("no_image", "No image in the current clipboard."));
            return;
        }
        let text: Id = msg_send![board, string];
        done.ok(PlatformValue::Clipboard(ClipboardContent { images: Vec::new(), text: rust_string(text) }));
    });
}

pub(crate) fn copy_image(_shared: &Arc<Shared>, bytes: Vec<u8>, mime_type: String, done: Completion) {
    let kind = match crate::image::sniff_image_mime(&bytes) {
        Some("image/png") => "public.png",
        Some("image/jpeg") => "public.jpeg",
        Some("image/gif") => "com.compuserve.gif",
        Some("image/webp") => "org.webmproject.webp",
        _ => {
            done.fail(PlatformError::invalid(format!("Unsupported clipboard image type {mime_type}.")));
            return;
        }
    };
    apple::on_main_queue(move || unsafe {
        let _: () = msg_send![pasteboard(), setData: apple::ns_data(&bytes), forPasteboardType: ns_string(kind)];
        done.ok(PlatformValue::None);
    });
}

// ----- notifications / badge / permissions -----

pub(crate) fn notify(_shared: &Arc<Shared>, request: NotificationRequest, done: Completion) {
    let identity = request.event_key.clone().unwrap_or_else(|| request.id.to_string());
    let payload = serde_json::json!({"id": identity, "data": request.data}).to_string();
    user_notifications::add(
        &request.id.to_string(),
        &request.title,
        request.body.as_deref(),
        request.sound.unwrap_or(true),
        &payload,
        move |result| done.finish(result.map(|()| PlatformValue::None)),
    );
}

pub(crate) fn request_notification_permission(_shared: &Arc<Shared>, done: Completion) {
    user_notifications::request_authorization(move |result| done.finish(result.map(PlatformValue::Permission)));
}

pub(crate) fn notification_permission_status(_shared: &Arc<Shared>, done: Completion) {
    user_notifications::authorization_status(move |state| done.ok(PlatformValue::Permission(state)));
}

pub(crate) fn set_badge_count(_shared: &Arc<Shared>, count: u32, done: Completion) {
    apple::on_main_queue(move || unsafe {
        let center = user_notifications::center();
        let modern: bool = msg_send![center, respondsToSelector: objc2::sel!(setBadgeCount:withCompletionHandler:)];
        if modern {
            let done = Mutex::new(Some(done));
            let block = RcBlock::new(move |error: Id| {
                if let Some(done) = done.lock().unwrap().take() {
                    if error.is_null() {
                        done.ok(PlatformValue::None);
                    } else {
                        done.fail(PlatformError::native(apple::error_message(error)));
                    }
                }
            });
            let _: () = msg_send![center, setBadgeCount: count as isize, withCompletionHandler: &*block];
        } else {
            let app: Id = msg_send![class!(UIApplication), sharedApplication];
            let _: () = msg_send![app, setApplicationIconBadgeNumber: count as isize];
            done.ok(PlatformValue::None);
        }
    });
}

pub(crate) fn idle_milliseconds(_shared: &Arc<Shared>, done: Completion) {
    done.fail(PlatformError::new("idle_unsupported", "Mobile clients report app activity only."));
}

/// AVCaptureDevice video authorization (mobile_scanner/permission_handler).
pub(crate) fn camera_permission(_shared: &Arc<Shared>, request: bool, done: Completion) {
    unsafe {
        let media = ns_string("vide"); // AVMediaTypeVideo
        let status: isize = msg_send![class!(AVCaptureDevice), authorizationStatusForMediaType: media];
        let state = match status {
            3 => PermissionState::Granted,
            1 | 2 => PermissionState::Denied,
            _ => PermissionState::NotDetermined,
        };
        if state != PermissionState::NotDetermined || !request {
            done.ok(PlatformValue::Permission(state));
            return;
        }
        let done = Mutex::new(Some(done));
        let block = RcBlock::new(move |granted: Bool| {
            if let Some(done) = done.lock().unwrap().take() {
                done.ok(PlatformValue::Permission(if granted.as_bool() { PermissionState::Granted } else { PermissionState::Denied }));
            }
        });
        let _: () = msg_send![class!(AVCaptureDevice), requestAccessForMediaType: media, completionHandler: &*block];
    }
}

pub(crate) fn system_prefers_dark() -> bool {
    unsafe {
        let traits: Id = msg_send![class!(UITraitCollection), currentTraitCollection];
        let style: isize = msg_send![traits, userInterfaceStyle];
        style == 2
    }
}

pub(crate) fn system_language() -> String {
    apple::preferred_language()
}

/// Dynamic Type: `[[UIFontMetrics defaultMetrics] scaledValueForValue:]` at
/// each SP knot (identity at the default "Large" content size).
pub(crate) fn system_text_scale() -> crate::appearance::TextScale {
    unsafe {
        let metrics: Id = msg_send![class!(UIFontMetrics), defaultMetrics];
        if metrics.is_null() {
            return crate::appearance::TextScale::default();
        }
        crate::appearance::TextScale::from_curve(|sp| msg_send![metrics, scaledValueForValue: sp])
    }
}

// ----- push -----

/// Push works when the signing profile carries the APNs entitlement.
pub(crate) async fn push_provisioned() -> bool {
    push_environment().is_ok()
}

pub(crate) async fn notification_permission(request: bool) -> Result<PermissionState, PlatformError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if request {
        user_notifications::request_authorization(move |result| {
            let _ = tx.send(result);
        });
    } else {
        user_notifications::authorization_status(move |state| {
            let _ = tx.send(Ok(state));
        });
    }
    rx.await.unwrap_or_else(|_| Err(PlatformError::native("Notification authorization was dropped.")))
}

/// The APNs device token (hex), from `registerForRemoteNotifications`.
pub(crate) async fn device_push_token() -> Result<String, PlatformError> {
    let waiter = token_waiter();
    apple::on_main_queue(|| unsafe {
        let app: Id = msg_send![class!(UIApplication), sharedApplication];
        let _: () = msg_send![app, registerForRemoteNotifications];
    });
    match tokio::time::timeout(Duration::from_secs(30), waiter).await {
        Ok(Ok(result)) => result,
        _ => Err(PlatformError::native("The operating system has not issued a push token.")),
    }
}

/// Original PaseoPushEnvironmentPlugin: the signing profile's `aps-environment`;
/// App Store distribution has no embedded profile and uses production.
pub(crate) fn push_environment() -> Result<String, PlatformError> {
    let path = unsafe {
        let bundle: Id = msg_send![class!(NSBundle), mainBundle];
        rust_string(msg_send![bundle, pathForResource: ns_string("embedded"), ofType: ns_string("mobileprovision")])
    };
    let Some(path) = path else { return Ok("production".into()) };
    let profile = std::fs::read(&path)?;
    let start = find(&profile, b"<?xml").ok_or_else(|| PlatformError::native("Cannot decode the app signing profile."))?;
    let end = find(&profile[start..], b"</plist>").map(|e| start + e + b"</plist>".len()).ok_or_else(|| PlatformError::native("Cannot decode the app signing profile."))?;
    let plist = unsafe {
        let data = apple::ns_data(&profile[start..end]);
        let mut error: Id = std::ptr::null_mut();
        let object: Id = msg_send![class!(NSPropertyListSerialization), propertyListWithData: data, options: 0usize, format: std::ptr::null_mut::<usize>(), error: &mut error];
        apple::to_json(object)
    };
    match plist.get("Entitlements").and_then(|e| e.get("aps-environment")).and_then(Value::as_str) {
        Some(environment @ ("development" | "production")) => Ok(environment.to_owned()),
        _ => Err(PlatformError::new("push_unavailable", "Push Notifications capability is missing from the signing profile.")),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// iOS background notifications are rendered by the OS; nothing is recorded.
pub(crate) fn take_background_push_error() -> Option<String> {
    None
}
