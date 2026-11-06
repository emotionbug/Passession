//! iOS integration (original ios/Runner AppDelegate, PaseoPushEnvironmentPlugin,
//! flutter_local_notifications/firebase_messaging iOS paths, mobile_scanner's
//! permission handling, file_selector/share_plus/pasteboard iOS). Delegate
//! methods are added to Makepad's `NSAppDelegate`; UIKit work runs on the main
//! queue, which is Makepad's GUI thread.

pub(crate) mod native;

use crate::{
    action::PlatformError,
    apple::{self, imp, rust_string, Id},
    mobile::{deliver, MobileSignal},
};
use objc2::{
    class, msg_send,
    runtime::{AnyObject, Bool, Sel},
};
use serde_json::{Map, Value};
use std::sync::Mutex;

/// Waiters for `registerForRemoteNotifications` results.
static TOKEN_WAITERS: Mutex<Vec<tokio::sync::oneshot::Sender<Result<String, PlatformError>>>> = Mutex::new(Vec::new());

pub(crate) fn token_waiter() -> tokio::sync::oneshot::Receiver<Result<String, PlatformError>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    TOKEN_WAITERS.lock().unwrap().push(tx);
    rx
}

fn finish_token(result: Result<String, PlatformError>) {
    for waiter in TOKEN_WAITERS.lock().unwrap().drain(..) {
        let _ = waiter.send(result.clone());
    }
}

extern "C" fn open_url(_this: &AnyObject, _sel: Sel, _app: Id, url: Id, _options: Id) -> Bool {
    let text: Option<String> = unsafe { rust_string(msg_send![url, absoluteString]) };
    if let Some(text) = text {
        // Parsing and approval belong to the existing host dialog.
        deliver(MobileSignal::Link(text));
        return Bool::YES;
    }
    Bool::NO
}

extern "C" fn did_register(_this: &AnyObject, _sel: Sel, _app: Id, token: Id) {
    let hex: String = apple::data_bytes(token).iter().map(|b| format!("{b:02x}")).collect();
    finish_token(Ok(hex));
}

extern "C" fn did_fail_register(_this: &AnyObject, _sel: Sel, _app: Id, error: Id) {
    finish_token(Err(PlatformError::new("push_unavailable", apple::error_message(error))));
}

/// APNs `userInfo` → (application data). Expo sends `body` as a dictionary;
/// otherwise every key except `aps` is application data.
fn apns_data(info: &Value) -> Map<String, Value> {
    match info.get("body") {
        Some(Value::Object(body)) => body.clone(),
        _ => info
            .as_object()
            .map(|map| map.iter().filter(|(k, _)| k.as_str() != "aps").map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    }
}

extern "C" fn did_receive_response(_this: &AnyObject, _sel: Sel, _center: Id, response: Id, completion: Id) {
    let (identifier, info) = apple::user_notifications::response_info(response);
    match info.get("payload").and_then(Value::as_str).and_then(|p| serde_json::from_str::<Value>(p).ok()) {
        // A locally presented notification: `{id, data}`.
        Some(payload) => {
            let id = payload.get("id").and_then(Value::as_str).unwrap_or(&identifier).to_owned();
            let data = payload.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
            deliver(MobileSignal::NotificationTap(id, data));
        }
        None => deliver(MobileSignal::NotificationTap(identifier, apns_data(&info))),
    }
    call_block(completion, None);
}

/// Direct APNs in the foreground: list only (original `[.list]`); local
/// notifications: banner + list + sound.
extern "C" fn will_present(_this: &AnyObject, _sel: Sel, _center: Id, notification: Id, completion: Id) {
    let local = unsafe {
        let request: Id = msg_send![notification, request];
        let content: Id = msg_send![request, content];
        let info: Id = msg_send![content, userInfo];
        apple::to_json(info).get("payload").is_some()
    };
    // UNNotificationPresentationOptionSound 2 | List 8 | Banner 16
    call_block(completion, Some(if local { 2 | 8 | 16 } else { 8 }));
}

fn call_block(block: Id, value: Option<usize>) {
    if block.is_null() {
        return;
    }
    unsafe {
        match value {
            Some(value) => (*(block as *const block2::Block<dyn Fn(usize)>)).call((value,)),
            None => (*(block as *const block2::Block<dyn Fn()>)).call(()),
        }
    }
}

/// Adds the URL, remote-notification and notification-center delegate methods.
pub(crate) fn install_delegates() {
    let bool_enc = apple::BOOL_ENC;
    unsafe {
        apple::add_method(c"NSAppDelegate", c"application:openURL:options:", imp!(open_url), &format!("{bool_enc}@:@@@"));
        apple::add_method(
            c"NSAppDelegate",
            c"application:didRegisterForRemoteNotificationsWithDeviceToken:",
            imp!(did_register),
            "v@:@@",
        );
        apple::add_method(
            c"NSAppDelegate",
            c"application:didFailToRegisterForRemoteNotificationsWithError:",
            imp!(did_fail_register),
            "v@:@@",
        );
        let class = apple::define_class(
            c"PaseoNotificationDelegate",
            class!(NSObject),
            &[
                (c"userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:", imp!(did_receive_response), "v@:@@@?"),
                (c"userNotificationCenter:willPresentNotification:withCompletionHandler:", imp!(will_present), "v@:@@@?"),
            ],
        );
        let delegate: Id = msg_send![class, new];
        let _: () = msg_send![apple::user_notifications::center(), setDelegate: delegate];
    }
}
