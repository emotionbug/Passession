//! Remote-only mobile services shared by Android and iOS (original
//! platform/mobile/mobile_services.dart, mobile_notification_delivery.dart,
//! expo_push_client.dart, mobile_services_host.dart). Mobile never runs or
//! links a daemon.

pub(crate) mod expo;
#[cfg(target_os = "ios")]
mod network_ios;

use crate::{
    action::*,
    platform::{Completion, PlatformState, Shared},
    prefs::PlatformPrefs,
};
use makepad_widgets::{Cx, Event};
use paseo_gui_core::CoreEvent;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{OnceCell, watch};

const TOKEN_PREFIX: &str = "mobile.expo.host.";
const BACKGROUND_ERROR: &str = "mobile.push.backgroundError";
const PUSH_RETRY_INITIAL: Duration = Duration::from_secs(5);
const PUSH_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// Events native code raises from any thread; drained on the GUI thread.
#[derive(Debug, Clone)]
pub(crate) enum MobileSignal {
    /// An OS link (`paseo://…`, pairing link) opened the app.
    Link(String),
    /// A notification tap: (delivery identity, application data).
    NotificationTap(String, Map<String, Value>),
    /// The Android Sharesheet handed us private copies, not provider URI grants.
    #[cfg(target_os = "android")]
    SharedImages(Result<Arc<crate::SharedImages>, crate::ImageShareError>),
    /// The app returned to the foreground.
    Resumed,
    Insets(SafeInsets),
    AudioInterrupted,
    /// Push credentials changed (FCM token refresh).
    #[cfg(target_os = "android")]
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
    /// host id → whether `pushTokenRevocation` is advertised, for connected hosts.
    connected: HashMap<String, bool>,
    /// host id → token sent on the current connection.
    registered: HashMap<String, String>,
}

#[derive(Default)]
struct Routing {
    ready: bool,
    pending: VecDeque<PlatformEvent>,
    /// Links waiting for window readiness or sitting in Makepad's action queue.
    queued_links: HashSet<String>,
}

impl Routing {
    fn window_ready(&mut self) -> std::collections::vec_deque::Drain<'_, PlatformEvent> {
        self.ready = true;
        self.pending.drain(..)
    }

    fn route(&mut self, event: PlatformEvent) -> Option<PlatformEvent> {
        if let PlatformEvent::ConnectionLink { url } = &event
            && !self.queued_links.insert(url.clone())
        {
            return None;
        }
        if self.ready {
            Some(event)
        } else {
            self.pending.push_back(event);
            None
        }
    }

    fn link_delivered(&mut self, url: &str) {
        self.queued_links.remove(url);
    }
}

pub(crate) struct MobileShared {
    pub prefs: PlatformPrefs,
    push: Mutex<PushState>,
    /// Only successful provisioning checks are cached, including unprovisioned builds.
    push_available: OnceCell<bool>,
    permission_check: tokio::sync::Mutex<()>,
    push_requests: watch::Sender<Option<PermissionState>>,
    push_task: Mutex<Option<tokio::task::AbortHandle>>,
    routing: Mutex<Routing>,
    handled_notifications: Mutex<HashSet<String>>,
}

impl MobileShared {
    pub fn new() -> Self {
        Self {
            prefs: PlatformPrefs::new(),
            push: Mutex::new(PushState::default()),
            push_available: OnceCell::new(),
            permission_check: tokio::sync::Mutex::new(()),
            push_requests: watch::channel(None).0,
            push_task: Mutex::new(None),
            routing: Mutex::new(Routing::default()),
            handled_notifications: Mutex::new(HashSet::new()),
        }
    }

    pub fn window_ready(&self) {
        for event in self.routing.lock().unwrap().window_ready() {
            post_event(event);
        }
    }

    fn route(&self, event: PlatformEvent) {
        let next = self.routing.lock().unwrap().route(event);
        if let Some(next) = next {
            post_event(next);
        }
    }

    pub async fn push_available(&self) -> Result<bool, PlatformError> {
        self.push_available
            .get_or_try_init(crate::native::push_provisioned)
            .await
            .copied()
    }

    fn report(&self, message: impl Into<String>) {
        post_event(PlatformEvent::HostError {
            message: message.into(),
        });
    }
}

pub(crate) fn start(cx: &mut Cx, shared: &Arc<Shared>, state: &mut PlatformState) {
    crate::native::start_services(cx, shared, state);
    #[cfg(target_os = "ios")]
    network_ios::start(shared.core.clone());
    let requests = shared.mobile.push_requests.subscribe();
    let task_shared = shared.clone();
    let task = shared.runtime.spawn(async move {
        let shared = &task_shared;
        run_push_sync(
            requests,
            |permission| sync_push(shared, permission),
            |message| shared.mobile.report(message),
        )
        .await;
    });
    *shared.mobile.push_task.lock().unwrap() = Some(task.abort_handle());
    // Links first (they do not depend on push provisioning), then a silent
    // push sync. The permission is only asked from in-context GUI flows.
    request_notifications(shared, false, None);
}

pub(crate) fn shutdown(shared: &Shared) {
    if let Some(task) = shared.mobile.push_task.lock().unwrap().take() {
        task.abort();
    }
    #[cfg(target_os = "ios")]
    network_ios::stop();
}

pub(crate) fn finish_event(shared: &Shared, event: &Event) {
    if let Event::Actions(actions) = event {
        for action in actions.iter() {
            if let Some(PlatformAction::Event(PlatformEvent::ConnectionLink { url })) =
                action.downcast_ref::<PlatformAction>()
            {
                shared.mobile.routing.lock().unwrap().link_delivered(url);
            }
        }
    }
}

pub(crate) fn handle_event(
    cx: &mut Cx,
    shared: &Arc<Shared>,
    state: &mut PlatformState,
    event: &Event,
) {
    if matches!(event, Event::Shutdown) {
        shutdown(shared);
        return;
    }
    let signals = std::mem::take(&mut *SIGNALS.lock().unwrap());
    for signal in signals {
        match signal {
            MobileSignal::Link(url) => shared.mobile.route(PlatformEvent::ConnectionLink { url }),
            MobileSignal::NotificationTap(id, data) => receive_tap(shared, id, data),
            #[cfg(target_os = "android")]
            MobileSignal::SharedImages(share) => {
                shared.mobile.route(PlatformEvent::SharedImages(share));
            }
            MobileSignal::Resumed => {
                // Original didChangeAppLifecycleState(resumed). The badge is not reset here:
                // the shell re-sends the count of sessions still waiting once it is visible.
                request_notifications(shared, false, None);
            }
            MobileSignal::Insets(insets) => {
                *shared.insets.lock().unwrap() = insets;
                post_event(PlatformEvent::Insets(insets));
            }
            MobileSignal::AudioInterrupted => state.audio.interrupted(cx),
            #[cfg(target_os = "android")]
            MobileSignal::TokenRefreshed => request_notifications(shared, false, None),
        }
    }
    crate::native::handle_event(cx, shared, state, event);
}

/// Original `_receiveTap` + `_navigateNotification`.
fn receive_tap(shared: &Arc<Shared>, id: String, data: Map<String, Value>) {
    if id.is_empty()
        || !shared
            .mobile
            .handled_notifications
            .lock()
            .unwrap()
            .insert(id)
    {
        return;
    }
    if let Some(route) = PlatformRoute::from_notification_data(&data) {
        // Other sessions may still wait: the shell keeps the badge at their count.
        shared.mobile.route(PlatformEvent::Route(route));
    }
}

/// Tracks connected hosts and their revocation capability, then registers.
pub(crate) fn observe_core_event(shared: &Arc<Shared>, event: &CoreEvent) {
    match event {
        CoreEvent::Connected {
            host_id,
            server_info,
        } => {
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
        let Some(token) = push.token.clone() else {
            return;
        };
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
            let saved = task_shared
                .mobile
                .prefs
                .set_string(&format!("{TOKEN_PREFIX}{host}"), &token);
            if let Err(error) = saved {
                task_shared
                    .mobile
                    .push
                    .lock()
                    .unwrap()
                    .registered
                    .remove(&host);
                task_shared.mobile.report(format!(
                    "Could not save the Expo token for later host revocation: {error}"
                ));
                return;
            }
            let still_current = {
                let push = task_shared.mobile.push.lock().unwrap();
                push.connected.contains_key(&host) && push.token.as_deref() == Some(token.as_str())
            };
            if still_current {
                task_shared.core.notify(
                    &host,
                    json!({"type": "register_push_token", "token": token}),
                );
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
    let Some(token) = shared.mobile.prefs.get_string(&key) else {
        return;
    };
    let revocable = shared
        .mobile
        .push
        .lock()
        .unwrap()
        .connected
        .get(host_id)
        .copied()
        .unwrap_or(false);
    if revocable {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let message = json!({"token": token});
        shared.core.operation(host_id, move |client| async move {
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                client.unregister_push_token(message),
            )
            .await;
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
    shared
        .mobile
        .push
        .lock()
        .unwrap()
        .registered
        .remove(host_id);
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

/// Reads the notification permission (`request` shows the OS prompt when it
/// still can) and finishes `done` with it, then syncs push: device token, Expo
/// exchange, retirement of previous subscriptions, registration on connected
/// hosts. Builds without push credentials keep local notifications working.
pub(crate) fn request_notifications(
    shared: &Arc<Shared>,
    request_permission: bool,
    done: Option<Completion>,
) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        // An OS read must not replace a prompt's result with a permission
        // observed before the user answered it.
        let check = task_shared.mobile.permission_check.lock().await;
        let permission = crate::native::notification_permission(request_permission).await;
        if let Ok(permission) = &permission {
            let _ = task_shared
                .mobile
                .push_requests
                .send_replace(Some(*permission));
        }
        drop(check);
        match permission {
            Ok(permission) => {
                if let Some(done) = done {
                    done.ok(PlatformValue::Permission(permission));
                }
            }
            Err(error) => match done {
                Some(done) => done.fail(error),
                None => task_shared.mobile.report(error.message),
            },
        }
    });
}

#[derive(Debug)]
enum PushFailure {
    Permanent(PlatformError),
    Transient(PlatformError),
}

impl From<PlatformError> for PushFailure {
    fn from(error: PlatformError) -> Self {
        Self::Permanent(error)
    }
}

fn next_push_retry(delay: Duration) -> Duration {
    (delay * 2).min(PUSH_RETRY_MAX)
}

async fn run_push_sync<F>(
    mut requests: watch::Receiver<Option<PermissionState>>,
    sync: impl Fn(PermissionState) -> F,
    report: impl Fn(String),
) where
    F: Future<Output = Result<(), PushFailure>>,
{
    let mut changed = Ok(());
    while changed.is_ok() {
        let permission = *requests.borrow_and_update();
        let Some(permission) = permission else {
            changed = requests.changed().await;
            continue;
        };
        let mut delay = PUSH_RETRY_INITIAL;
        changed = loop {
            // Each run reads the current device token, including after a refresh.
            let result = sync(permission).await;
            if requests.has_changed().unwrap_or(true) {
                break requests.changed().await;
            }
            match result {
                Ok(()) => break requests.changed().await,
                Err(PushFailure::Permanent(error)) => {
                    report(error.message);
                    break requests.changed().await;
                }
                Err(PushFailure::Transient(error)) => {
                    if delay == PUSH_RETRY_INITIAL {
                        report(error.message);
                    }
                }
            }
            tokio::select! { biased;
                updated = requests.changed() => break updated,
                _ = tokio::time::sleep(delay) => delay = next_push_retry(delay),
            }
        };
    }
}

/// Original `requestNotifications` after the permission step.
async fn sync_push(shared: &Arc<Shared>, permission: PermissionState) -> Result<(), PushFailure> {
    if !shared.mobile.push_available().await? {
        return Ok(());
    }
    if !permission.allowed() {
        {
            let mut push = shared.mobile.push.lock().unwrap();
            push.token = None;
            push.device_token = None;
        }
        for (host, _) in saved_token_hosts(shared) {
            revoke(shared, &host).await;
        }
        return Ok(());
    }
    let device_token = crate::native::device_push_token().await.map_err(|error| {
        if error.code == "push_retryable" {
            PushFailure::Transient(error)
        } else {
            PushFailure::Permanent(error)
        }
    })?;
    if device_token.is_empty() {
        let error = PlatformError::native(
            "The operating system has not issued a push token. Check notification signing and Firebase configuration, then retry.",
        );
        return Err(error.into());
    }
    let (changed, has_token) = {
        let push = shared.mobile.push.lock().unwrap();
        (
            push.device_token.as_deref() != Some(device_token.as_str()),
            push.token.is_some(),
        )
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
        let _ =
            tokio::task::spawn_blocking(move || prefs_shared.mobile.prefs.remove(BACKGROUND_ERROR))
                .await;
    }
    if let Some(error) = background.or(stored) {
        shared.mobile.report(error);
    }
    Ok(())
}

/// Expo's FCM transport stores application data as JSON in `body`
/// (original `mobileNotificationData`).
#[cfg(target_os = "android")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_link_is_deduplicated_only_until_gui_delivery() {
        let url = "paseo://pair#offer=example";
        let link = || PlatformEvent::ConnectionLink { url: url.into() };
        let mut routing = Routing::default();
        assert!(routing.route(link()).is_none());
        assert!(routing.route(link()).is_none());
        assert_eq!(routing.window_ready().count(), 1);

        // The startup queue has drained, but its posted action is still pending.
        assert!(routing.route(link()).is_none());
        routing.link_delivered(url);
        assert!(matches!(
            routing.route(link()),
            Some(PlatformEvent::ConnectionLink { url: received }) if received == url
        ));
        assert!(routing.route(link()).is_none());
        routing.link_delivered(url);
        assert!(routing.route(link()).is_some());
    }

    #[tokio::test]
    async fn newer_push_requests_coalesce_and_replace_a_pending_retry() {
        use tokio::sync::{mpsc, oneshot};

        let (updates, requests) = watch::channel(None);
        let (performed, mut runs) = mpsc::unbounded_channel();
        let (reports, mut errors) = mpsc::unbounded_channel();
        let worker = tokio::spawn(run_push_sync(
            requests,
            move |permission| {
                let (done, result) = oneshot::channel();
                performed.send((permission, done)).unwrap();
                async move { result.await.unwrap() }
            },
            move |message| reports.send(message).unwrap(),
        ));
        let timeout = Duration::from_secs(1);
        let assert_next = async |runs: &mut mpsc::UnboundedReceiver<_>, expected| {
            let (permission, done) = tokio::time::timeout(timeout, runs.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(permission, expected);
            done
        };

        let _ = updates.send_replace(Some(PermissionState::Granted));
        let first = assert_next(&mut runs, PermissionState::Granted).await;
        let _ = updates.send_replace(Some(PermissionState::Denied));
        let _ = updates.send_replace(Some(PermissionState::Provisional));
        first.send(Ok(())).unwrap();

        let latest = assert_next(&mut runs, PermissionState::Provisional).await;
        latest
            .send(Err(PushFailure::Transient(PlatformError::native(
                "offline",
            ))))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(timeout, errors.recv())
                .await
                .unwrap()
                .as_deref(),
            Some("offline"),
        );
        let _ = updates.send_replace(Some(PermissionState::Blocked));
        let next = assert_next(&mut runs, PermissionState::Blocked).await;
        next.send(Ok(())).unwrap();

        drop(updates);
        worker.await.unwrap();
    }

    #[test]
    fn push_retry_backoff_doubles_until_five_minutes() {
        let waits: Vec<_> = std::iter::successors(Some(PUSH_RETRY_INITIAL), |wait| {
            Some(next_push_retry(*wait))
        })
        .take(9)
        .map(|wait| wait.as_secs())
        .collect();
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    }
}
