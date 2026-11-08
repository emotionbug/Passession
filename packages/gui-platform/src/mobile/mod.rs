//! Remote-only mobile services shared by Android and iOS (original
//! platform/mobile/mobile_services.dart, mobile_notification_delivery.dart,
//! expo_push_client.dart, mobile_services_host.dart). Mobile never runs or
//! links a daemon.

pub(crate) mod expo;

use crate::{
    action::*,
    platform::{Completion, PlatformState, Shared},
    prefs::PlatformPrefs,
};
use makepad_widgets::{Cx, Event};
use paseo_gui_core::CoreEvent;
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

const TOKEN_PREFIX: &str = "mobile.expo.host.";
const BACKGROUND_ERROR: &str = "mobile.push.backgroundError";

/// Events native code raises from any thread; drained on the GUI thread.
#[derive(Debug, Clone)]
pub(crate) enum MobileSignal {
    /// An OS link (`paseo://…`, pairing link) opened the app.
    Link(String),
    /// A notification tap: (delivery identity, application data).
    NotificationTap(String, Map<String, Value>),
    /// The app returned to the foreground.
    Resumed,
    Insets(SafeInsets),
    AudioInterrupted,
    /// Push credentials changed (FCM token refresh).
    TokenRefreshed,
}

static SIGNALS: Mutex<Vec<MobileSignal>> = Mutex::new(Vec::new());

pub(crate) fn deliver(signal: MobileSignal) {
    SIGNALS.lock().unwrap().push(signal);
    makepad_widgets::SignalToUI::set_ui_signal();
}

#[derive(Default)]
struct PushState {
    /// Expo token issued for the current device token.
    token: Option<String>,
    device_token: Option<String>,
    /// Builds without push credentials: a packaging fact, not a runtime error.
    unavailable: bool,
    busy: bool,
    /// host id → whether `pushTokenRevocation` is advertised, for connected hosts.
    connected: HashMap<String, bool>,
    /// host id → token sent on the current connection.
    registered: HashMap<String, String>,
}

#[derive(Default)]
struct Routing {
    ready: bool,
    pending: VecDeque<PlatformEvent>,
    links: VecDeque<String>,
}

pub(crate) struct MobileShared {
    pub prefs: PlatformPrefs,
    push: Mutex<PushState>,
    routing: Mutex<Routing>,
    handled_notifications: Mutex<HashSet<String>>,
}

impl MobileShared {
    pub fn new() -> Self {
        Self {
            prefs: PlatformPrefs::new(),
            push: Mutex::new(PushState::default()),
            routing: Mutex::new(Routing::default()),
            handled_notifications: Mutex::new(HashSet::new()),
        }
    }

    pub fn window_ready(&self) {
        let pending: Vec<_> = {
            let mut routing = self.routing.lock().unwrap();
            routing.ready = true;
            routing.pending.drain(..).collect()
        };
        for event in pending {
            post_event(event);
        }
    }

    fn route(&self, event: PlatformEvent) {
        let mut routing = self.routing.lock().unwrap();
        if routing.ready {
            drop(routing);
            post_event(event);
        } else {
            routing.pending.push_back(event);
        }
    }

    /// Original `_receiveLink`: parsing and approval belong to the existing host
    /// dialog; duplicates of a queued link are dropped.
    fn receive_link(&self, url: String) {
        {
            let mut routing = self.routing.lock().unwrap();
            if routing.links.contains(&url) {
                return;
            }
            routing.links.push_back(url.clone());
            if routing.links.len() > 32 {
                routing.links.pop_front();
            }
        }
        self.route(PlatformEvent::ConnectionLink { url });
    }

    fn report(&self, message: impl Into<String>) {
        post_event(PlatformEvent::HostError { message: message.into() });
    }
}

pub(crate) fn start(cx: &mut Cx, shared: &Arc<Shared>, state: &mut PlatformState) {
    crate::native::start_services(cx, shared, state);
    // Original `_start`: links first (they do not depend on push provisioning),
    // then the permission request.
    request_notifications(shared, true, None);
}

pub(crate) fn handle_event(cx: &mut Cx, shared: &Arc<Shared>, state: &mut PlatformState, event: &Event) {
    let signals = std::mem::take(&mut *SIGNALS.lock().unwrap());
    for signal in signals {
        match signal {
            MobileSignal::Link(url) => shared.mobile.receive_link(url),
            MobileSignal::NotificationTap(id, data) => receive_tap(shared, id, data),
            MobileSignal::Resumed => {
                // Original didChangeAppLifecycleState(resumed).
                request_notifications(shared, false, None);
                crate::native::set_badge_count(shared, 0, shared.completion());
            }
            MobileSignal::Insets(insets) => {
                *shared.insets.lock().unwrap() = insets;
                post_event(PlatformEvent::Insets(insets));
            }
            MobileSignal::AudioInterrupted => state.audio.interrupted(cx),
            MobileSignal::TokenRefreshed => request_notifications(shared, false, None),
        }
    }
    crate::native::handle_event(cx, shared, state, event);
}

/// Original `_receiveTap` + `_navigateNotification`.
fn receive_tap(shared: &Arc<Shared>, id: String, data: Map<String, Value>) {
    if id.is_empty() || !shared.mobile.handled_notifications.lock().unwrap().insert(id) {
        return;
    }
    if let Some(route) = PlatformRoute::from_notification_data(&data) {
        shared.mobile.route(PlatformEvent::Route(route));
        crate::native::set_badge_count(shared, 0, shared.completion());
    }
}

/// Tracks connected hosts and their revocation capability, then registers.
pub(crate) fn observe_core_event(shared: &Arc<Shared>, event: &CoreEvent) {
    match event {
        CoreEvent::Connected { host_id, server_info } => {
            let revocation = server_info
                .get("features")
                .and_then(|f| f.get("pushTokenRevocation"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut push = shared.mobile.push.lock().unwrap();
            push.connected.insert(host_id.clone(), revocation);
            push.registered.remove(host_id);
            drop(push);
            sync_hosts(shared);
        }
        CoreEvent::Disconnected { host_id, .. } => {
            let mut push = shared.mobile.push.lock().unwrap();
            push.connected.remove(host_id);
            push.registered.remove(host_id);
        }
        _ => {}
    }
}

/// Original `_register`: one `register_push_token` per connection and token,
/// persisted first so the token can be revoked when the host is removed. The
/// protocol message has no acknowledgement; acceptance is never claimed.
fn sync_hosts(shared: &Arc<Shared>) {
    let (token, targets) = {
        let mut push = shared.mobile.push.lock().unwrap();
        let Some(token) = push.token.clone() else { return };
        let targets: Vec<String> = push
            .connected
            .keys()
            .filter(|host| push.registered.get(*host) != Some(&token))
            .cloned()
            .collect();
        for host in &targets {
            // Marked synchronously so repeated events do not duplicate writes.
            push.registered.insert(host.clone(), token.clone());
        }
        (token, targets)
    };
    for host in targets {
        let task_shared = shared.clone();
        let token = token.clone();
        shared.spawn_blocking(move || {
            let saved = task_shared.mobile.prefs.set_string(&format!("{TOKEN_PREFIX}{host}"), &token);
            if let Err(error) = saved {
                task_shared.mobile.push.lock().unwrap().registered.remove(&host);
                task_shared.mobile.report(format!("Could not save the Expo token for later host revocation: {error}"));
                return;
            }
            let still_current = {
                let push = task_shared.mobile.push.lock().unwrap();
                push.connected.contains_key(&host) && push.token.as_deref() == Some(token.as_str())
            };
            if still_current {
                task_shared.core.notify(&host, json!({"type": "register_push_token", "token": token}));
            }
        });
    }
}

/// Original `revokeHost`: best effort (2 s, only when the host is connected and
/// advertises `pushTokenRevocation`); the cached token is always forgotten.
pub(crate) fn revoke_host(shared: &Arc<Shared>, host_id: String, done: Completion) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        revoke(&task_shared, &host_id).await;
        done.ok(PlatformValue::None);
    });
}

async fn revoke(shared: &Arc<Shared>, host_id: &str) {
    let key = format!("{TOKEN_PREFIX}{host_id}");
    let Some(token) = shared.mobile.prefs.get_string(&key) else { return };
    let revocable = shared.mobile.push.lock().unwrap().connected.get(host_id).copied().unwrap_or(false);
    if revocable {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let message = json!({"token": token});
        shared.core.operation(host_id, move |client| async move {
            let result = tokio::time::timeout(Duration::from_secs(2), client.unregister_push_token(message)).await;
            let _ = tx.send(());
            match result {
                Ok(Ok(_)) => Ok(Value::Null),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(anyhow::anyhow!("Push token revocation timed out.")),
            }
        });
        let _ = tokio::time::timeout(Duration::from_millis(2500), rx).await;
    }
    let prefs_shared = shared.clone();
    let _ = tokio::task::spawn_blocking(move || prefs_shared.mobile.prefs.remove(&key)).await;
    shared.mobile.push.lock().unwrap().registered.remove(host_id);
}

fn saved_token_hosts(shared: &Shared) -> Vec<(String, String)> {
    shared
        .mobile
        .prefs
        .keys_with_prefix(TOKEN_PREFIX)
        .into_iter()
        .filter_map(|(key, value)| Some((key.strip_prefix(TOKEN_PREFIX)?.to_owned(), value)))
        .collect()
}

/// Original `requestNotifications`: permission, device token, Expo exchange,
/// retirement of previous subscriptions, registration on connected hosts.
pub(crate) fn request_notifications(shared: &Arc<Shared>, request_permission: bool, done: Option<Completion>) {
    {
        let mut push = shared.mobile.push.lock().unwrap();
        if push.busy {
            if let Some(done) = done {
                done.ok(PlatformValue::None);
            }
            return;
        }
        push.busy = true;
    }
    let task_shared = shared.clone();
    shared.spawn(async move {
        let result = request_inner(&task_shared, request_permission).await;
        task_shared.mobile.push.lock().unwrap().busy = false;
        match result {
            Ok(value) => {
                if let Some(done) = done {
                    done.ok(value);
                }
            }
            Err(error) => {
                task_shared.mobile.report(error.message.clone());
                if let Some(done) = done {
                    done.fail(error);
                }
            }
        }
    });
}

async fn request_inner(shared: &Arc<Shared>, request_permission: bool) -> Result<PlatformValue, PlatformError> {
    if shared.mobile.push.lock().unwrap().unavailable {
        return Ok(PlatformValue::None);
    }
    if !crate::native::push_provisioned().await {
        // Local notifications, links and hosts keep working without push.
        shared.mobile.push.lock().unwrap().unavailable = true;
        return Ok(PlatformValue::None);
    }
    let permission = crate::native::notification_permission(request_permission).await?;
    if !permission.allowed() {
        {
            let mut push = shared.mobile.push.lock().unwrap();
            push.token = None;
            push.device_token = None;
        }
        for (host, _) in saved_token_hosts(shared) {
            revoke(shared, &host).await;
        }
        return Ok(PlatformValue::Permission(permission));
    }
    let device_token = crate::native::device_push_token().await?;
    if device_token.is_empty() {
        return Err(PlatformError::native(
            "The operating system has not issued a push token. Check notification signing and Firebase configuration, then retry.",
        ));
    }
    let (changed, has_token) = {
        let push = shared.mobile.push.lock().unwrap();
        (push.device_token.as_deref() != Some(device_token.as_str()), push.token.is_some())
    };
    if changed || !has_token {
        let next = expo::issue(shared, &device_token).await?;
        // Retire each previous backend subscription before replacing its identity.
        for (host, old) in saved_token_hosts(shared) {
            if old != next {
                revoke(shared, &host).await;
            }
        }
        let mut push = shared.mobile.push.lock().unwrap();
        push.token = Some(next);
        push.device_token = Some(device_token);
    }
    sync_hosts(shared);
    // Background-delivery failures surface when the UI resumes.
    let background = crate::native::take_background_push_error();
    let stored = shared.mobile.prefs.get_string(BACKGROUND_ERROR);
    if stored.is_some() {
        let prefs_shared = shared.clone();
        let _ = tokio::task::spawn_blocking(move || prefs_shared.mobile.prefs.remove(BACKGROUND_ERROR)).await;
    }
    if let Some(error) = background.or(stored) {
        shared.mobile.report(error);
    }
    Ok(PlatformValue::Permission(permission))
}

/// Expo's FCM transport stores application data as JSON in `body`; native
/// APNs messages expose it directly (original `mobileNotificationData`).
pub(crate) fn notification_data(raw: &Map<String, Value>) -> Map<String, Value> {
    match raw.get("body") {
        Some(Value::String(body)) => match serde_json::from_str::<Value>(body) {
            Ok(Value::Object(map)) => map,
            _ => raw.clone(),
        },
        Some(Value::Object(map)) => map.clone(),
        _ => raw.clone(),
    }
}
