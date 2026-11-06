//! Objective-C helpers shared by macOS and iOS. Makepad 1.0.0 registers its own
//! `NSAppDelegate` class; Paseo adds the delegate methods it needs to that class
//! at `Platform::start`, before `finishLaunching` delivers launch URLs.

#![allow(clippy::missing_safety_doc)]

use objc2::{
    class, msg_send,
    runtime::{AnyClass, AnyObject, Imp, Sel},
};
use serde_json::{Map, Value};
use std::ffi::{c_char, c_void, CStr, CString};

pub(crate) type Id = *mut AnyObject;

/// BOOL encoding: `B` (C bool) on arm64, `c` (signed char) on x86_64.
#[cfg(target_arch = "aarch64")]
pub(crate) const BOOL_ENC: &str = "B";
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const BOOL_ENC: &str = "c";

/// Autoreleased NSString.
pub(crate) fn ns_string(text: &str) -> Id {
    let text = CString::new(text.replace('\0', "")).unwrap();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
}

pub(crate) fn rust_string(string: Id) -> Option<String> {
    if string.is_null() {
        return None;
    }
    let utf8: *const c_char = unsafe { msg_send![string, UTF8String] };
    (!utf8.is_null()).then(|| unsafe { CStr::from_ptr(utf8) }.to_string_lossy().into_owned())
}

/// Autoreleased NSURL for a file path.
pub(crate) fn file_url(path: &std::path::Path) -> Id {
    unsafe { msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.display().to_string())] }
}

pub(crate) fn url(text: &str) -> Id {
    unsafe { msg_send![class!(NSURL), URLWithString: ns_string(text)] }
}

pub(crate) fn is_kind_of(object: Id, class: &AnyClass) -> bool {
    !object.is_null() && unsafe { msg_send![object, isKindOfClass: class] }
}

/// Plist-compatible Foundation object → JSON.
pub(crate) fn to_json(object: Id) -> Value {
    if object.is_null() {
        return Value::Null;
    }
    unsafe {
        if is_kind_of(object, class!(NSString)) {
            return rust_string(object).map(Value::String).unwrap_or(Value::Null);
        }
        if is_kind_of(object, class!(NSNumber)) {
            let objc_type: *const c_char = msg_send![object, objCType];
            let kind = if objc_type.is_null() { "" } else { CStr::from_ptr(objc_type).to_str().unwrap_or("") };
            return match kind {
                "c" | "B" => {
                    let value: bool = msg_send![object, boolValue];
                    Value::Bool(value)
                }
                "f" | "d" => {
                    let value: f64 = msg_send![object, doubleValue];
                    serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
                }
                _ => {
                    let value: i64 = msg_send![object, longLongValue];
                    Value::from(value)
                }
            };
        }
        if is_kind_of(object, class!(NSArray)) {
            let count: usize = msg_send![object, count];
            return Value::Array((0..count).map(|i| to_json(msg_send![object, objectAtIndex: i])).collect());
        }
        if is_kind_of(object, class!(NSDictionary)) {
            let keys: Id = msg_send![object, allKeys];
            let count: usize = msg_send![keys, count];
            let mut map = Map::new();
            for i in 0..count {
                let key: Id = msg_send![keys, objectAtIndex: i];
                if let Some(name) = rust_string(key).filter(|_| is_kind_of(key, class!(NSString))) {
                    let value: Id = msg_send![object, objectForKey: key];
                    map.insert(name, to_json(value));
                }
            }
            return Value::Object(map);
        }
    }
    Value::Null
}

/// `NSUserDefaults.standardUserDefaults` values whose key starts with `prefix`
/// (Flutter shared_preferences stores `flutter.<key>` there on Apple platforms).
pub(crate) fn user_defaults_with_prefix(prefix: &str) -> Vec<(String, Value)> {
    unsafe {
        let defaults: Id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let dictionary: Id = msg_send![defaults, dictionaryRepresentation];
        match to_json(dictionary) {
            Value::Object(map) => map.into_iter().filter(|(key, _)| key.starts_with(prefix)).collect(),
            _ => Vec::new(),
        }
    }
}

/// Adds `imp` for `selector` to an existing class (no-op if it already has one).
pub(crate) unsafe fn add_method(class_name: &CStr, selector: &CStr, imp: Imp, types: &str) {
    let Some(class) = AnyClass::get(class_name) else { return };
    let types = CString::new(types).unwrap();
    unsafe {
        objc2::ffi::class_addMethod(class as *const AnyClass as *mut AnyClass, Sel::register(selector), imp, types.as_ptr());
    }
}

/// Defines a new NSObject subclass with the given methods (once per process).
pub(crate) unsafe fn define_class(name: &CStr, superclass: &AnyClass, methods: &[(&CStr, Imp, &str)]) -> &'static AnyClass {
    if let Some(existing) = AnyClass::get(name) {
        return existing;
    }
    let mut builder = objc2::runtime::ClassBuilder::new(name, superclass).expect("class name collision");
    let class = builder.register();
    for (selector, imp, types) in methods {
        let types = CString::new(*types).unwrap();
        unsafe {
            objc2::ffi::class_addMethod(class as *const AnyClass as *mut AnyClass, Sel::register(selector), *imp, types.as_ptr());
        }
    }
    class
}

/// Casts an `extern "C"` function to the runtime IMP type.
macro_rules! imp {
    ($function:expr) => {
        unsafe { std::mem::transmute::<*const (), objc2::runtime::Imp>($function as *const ()) }
    };
}
pub(crate) use imp;

// Frameworks whose classes are looked up by name at runtime.
#[link(name = "UserNotifications", kind = "framework")]
#[link(name = "AVFoundation", kind = "framework")]
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

extern "C" fn run_boxed(context: *mut c_void) {
    let work = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce() + Send>) };
    work();
}

/// Runs `work` on the main thread's dispatch queue (serviced in modal run-loop
/// modes too, e.g. while a deferred `terminate:` waits).
pub(crate) fn on_main_queue(work: impl FnOnce() + Send + 'static) {
    let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(work));
    unsafe {
        dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(boxed) as *mut c_void, run_boxed);
    }
}

/// `[[NSLocale preferredLanguages] firstObject]`.
pub(crate) fn preferred_language() -> String {
    unsafe {
        let languages: Id = msg_send![class!(NSLocale), preferredLanguages];
        let first: Id = msg_send![languages, firstObject];
        rust_string(first).unwrap_or_else(|| "en".into())
    }
}

/// `NSData` → bytes.
pub(crate) fn data_bytes(data: Id) -> Vec<u8> {
    if data.is_null() {
        return Vec::new();
    }
    unsafe {
        let length: usize = msg_send![data, length];
        let bytes: *const u8 = msg_send![data, bytes];
        if bytes.is_null() || length == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(bytes, length).to_vec()
    }
}

/// Autoreleased `NSData` copy of `bytes`.
pub(crate) fn ns_data(bytes: &[u8]) -> Id {
    unsafe { msg_send![class!(NSData), dataWithBytes: bytes.as_ptr() as *const c_void, length: bytes.len()] }
}

/// `NSError.localizedDescription`.
pub(crate) fn error_message(error: Id) -> String {
    if error.is_null() {
        return "Unknown error".into();
    }
    unsafe { rust_string(msg_send![error, localizedDescription]).unwrap_or_else(|| "Unknown error".into()) }
}

/// UNUserNotificationCenter authorization, shared by macOS and iOS.
pub(crate) mod user_notifications {
    use super::*;
    use crate::action::{PermissionState, PlatformError};
    use block2::RcBlock;

    pub(crate) fn center() -> Id {
        unsafe { msg_send![class!(UNUserNotificationCenter), currentNotificationCenter] }
    }

    /// `requestAuthorizationWithOptions:` (alert | badge | sound).
    pub(crate) fn request_authorization(reply: impl FnOnce(Result<PermissionState, PlatformError>) + Send + 'static) {
        let reply = std::sync::Mutex::new(Some(reply));
        let block = RcBlock::new(move |granted: objc2::runtime::Bool, error: Id| {
            if let Some(reply) = reply.lock().unwrap().take() {
                if !error.is_null() && !granted.as_bool() {
                    reply(Err(PlatformError::native(error_message(error))));
                } else {
                    reply(Ok(if granted.as_bool() { PermissionState::Granted } else { PermissionState::Denied }));
                }
            }
        });
        // UNAuthorizationOptionBadge 1 | Sound 2 | Alert 4
        let options: usize = 1 | 2 | 4;
        unsafe {
            let _: () = msg_send![center(), requestAuthorizationWithOptions: options, completionHandler: &*block];
        }
    }

    /// Current authorization without prompting.
    pub(crate) fn authorization_status(reply: impl FnOnce(PermissionState) + Send + 'static) {
        let reply = std::sync::Mutex::new(Some(reply));
        let block = RcBlock::new(move |settings: Id| {
            let status: isize = unsafe { msg_send![settings, authorizationStatus] };
            let state = match status {
                2 | 4 => PermissionState::Granted, // authorized, ephemeral
                3 => PermissionState::Provisional,
                1 => PermissionState::Denied,
                _ => PermissionState::NotDetermined,
            };
            if let Some(reply) = reply.lock().unwrap().take() {
                reply(state);
            }
        });
        unsafe {
            let _: () = msg_send![center(), getNotificationSettingsWithCompletionHandler: &*block];
        }
    }

    /// Adds a notification request. `payload` is stored in `userInfo["payload"]`.
    pub(crate) fn add(identifier: &str, title: &str, body: Option<&str>, sound: bool, payload: &str, reply: impl FnOnce(Result<(), PlatformError>) + Send + 'static) {
        unsafe {
            let content: Id = msg_send![class!(UNMutableNotificationContent), new];
            let _: () = msg_send![content, setTitle: ns_string(title)];
            if let Some(body) = body {
                let _: () = msg_send![content, setBody: ns_string(body)];
            }
            if sound {
                let sound: Id = msg_send![class!(UNNotificationSound), defaultSound];
                let _: () = msg_send![content, setSound: sound];
            }
            let info: Id = msg_send![class!(NSDictionary), dictionaryWithObject: ns_string(payload), forKey: ns_string("payload")];
            let _: () = msg_send![content, setUserInfo: info];
            let request: Id = msg_send![
                class!(UNNotificationRequest),
                requestWithIdentifier: ns_string(identifier),
                content: content,
                trigger: std::ptr::null_mut::<AnyObject>()
            ];
            let _: () = msg_send![content, release];
            let reply = std::sync::Mutex::new(Some(reply));
            let block = RcBlock::new(move |error: Id| {
                if let Some(reply) = reply.lock().unwrap().take() {
                    reply(if error.is_null() { Ok(()) } else { Err(PlatformError::native(error_message(error))) });
                }
            });
            let _: () = msg_send![center(), addNotificationRequest: request, withCompletionHandler: &*block];
        }
    }

    /// `UNNotificationResponse` → (notification identifier, userInfo JSON).
    pub(crate) fn response_info(response: Id) -> (String, Value) {
        unsafe {
            let notification: Id = msg_send![response, notification];
            let request: Id = msg_send![notification, request];
            let identifier = rust_string(msg_send![request, identifier]).unwrap_or_default();
            let content: Id = msg_send![request, content];
            let info: Id = msg_send![content, userInfo];
            (identifier, to_json(info))
        }
    }

    pub(crate) fn action_identifier(response: Id) -> Option<String> {
        unsafe { rust_string(msg_send![response, actionIdentifier]) }
    }
}
