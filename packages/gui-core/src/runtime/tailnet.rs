//! Process-wide embedded-node ownership. UI messages are local operations, never daemon RPCs.
use super::{Core, directory, hosts, store};
use crate::{CoreError, RequestId, host_config};
use paseo_tailnet::{Config, KeyStore, Manager, Proxy, Status};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

const SETTINGS: &str = "paseo.tailnet";

#[derive(Clone, Debug)]
struct Settings {
    enabled: bool,
    hostname: String,
    control_url: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname: "passession".into(),
            control_url: None,
        }
    }
}
impl Settings {
    fn parse(value: &Value) -> Result<Self, CoreError> {
        let enabled = value["enabled"]
            .as_bool()
            .ok_or_else(|| invalid("enabled must be a boolean"))?;
        let hostname = value["hostname"]
            .as_str()
            .ok_or_else(|| invalid("A device hostname is required"))?
            .trim();
        if hostname.is_empty()
            || hostname.len() > 63
            || !hostname
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !hostname.as_bytes()[0].is_ascii_alphanumeric()
            || !hostname.as_bytes()[hostname.len() - 1].is_ascii_alphanumeric()
        {
            return Err(invalid(
                "Device hostname must be a DNS label (1–63 letters, digits or hyphens, beginning and ending with a letter or digit).",
            ));
        }
        let control_url = match value.get("controlUrl") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if value.trim().is_empty() => None,
            Some(Value::String(value)) => {
                let url =
                    url::Url::parse(value.trim()).map_err(|_| invalid("Invalid control URL"))?;
                let loopback = url.host_str().is_some_and(|host| {
                    host.trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
                });
                if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(invalid(
                        "Control URL must use HTTPS (HTTP is allowed only for literal loopback IPs), without credentials, query or fragment.",
                    ));
                }
                Some(url.to_string())
            }
            _ => return Err(invalid("controlUrl must be a URL or null")),
        };
        Ok(Self {
            enabled,
            hostname: hostname.to_owned(),
            control_url,
        })
    }
    fn json(&self) -> Value {
        json!({"enabled":self.enabled,"hostname":self.hostname,"controlUrl":self.control_url})
    }
    fn runtime(&self, core: &Core) -> Config {
        Config {
            state_dir: core.config.state_dir.join("paseo-native").join("tailnet"),
            hostname: self.hostname.clone(),
            control_url: self.control_url.clone(),
        }
    }
}

/// Consecutive transient poll or focus-check failures a held route survives. A network rebind
/// briefly reports `Starting` and a stalled Go call times out after 15 s; neither ends the node.
const TOLERATED_FAILURES: u8 = 3;

/// Backend states that end the node's usable route at once.
fn terminal(backend_state: &str) -> bool {
    matches!(backend_state, "NeedsLogin" | "NeedsMachineAuth" | "Stopped")
}

/// The embedded node's blocking operations: `paseo_tailnet::Manager`, or a fake in tests.
trait Backend: Send + Sync {
    fn start(&self, config: Config) -> anyhow::Result<Status>;
    fn status(&self) -> anyhow::Result<Status>;
    fn login(&self) -> anyhow::Result<String>;
    fn proxy(&self) -> anyhow::Result<Proxy>;
    fn disconnect(&self) -> anyhow::Result<()>;
    fn logout(&self) -> anyhow::Result<()>;
}
impl Backend for Manager {
    fn start(&self, config: Config) -> anyhow::Result<Status> {
        Manager::start(self, config)
    }
    fn status(&self) -> anyhow::Result<Status> {
        Manager::status(self)
    }
    fn login(&self) -> anyhow::Result<String> {
        Manager::login(self)
    }
    fn proxy(&self) -> anyhow::Result<Proxy> {
        Manager::proxy(self)
    }
    fn disconnect(&self) -> anyhow::Result<()> {
        Manager::disconnect(self)
    }
    fn logout(&self) -> anyhow::Result<()> {
        Manager::logout(self)
    }
}

#[derive(Default)]
struct OperationResult {
    status: Option<Status>,
    proxy: Option<Proxy>,
    auth_url: Option<String>,
    /// A new node replaced whatever ran before; routes through the old one are dead.
    restarted: bool,
}

/// A Running node's route; other states have none.
fn routed(
    manager: &dyn Backend,
    status: Status,
    proxy: Option<Proxy>,
) -> anyhow::Result<OperationResult> {
    let proxy = if status.backend_state == "Running" {
        Some(match proxy.filter(Proxy::is_valid) {
            Some(proxy) => proxy,
            None => manager.proxy()?,
        })
    } else {
        None
    };
    Ok(OperationResult {
        status: Some(status),
        proxy,
        ..OperationResult::default()
    })
}

pub(super) struct Tailnet {
    manager: Option<Arc<dyn Backend>>,
    operation: Mutex<()>,
    settings: RefCell<Settings>,
    status: RefCell<Option<Status>>,
    proxy: RefCell<Option<Proxy>>,
    error: RefCell<Option<String>>,
    busy: Cell<bool>,
    visible_busy: Cell<bool>,
    checked_at: Cell<Option<Instant>>,
    poll_changed: Rc<Notify>,
    /// The user paused an enabled node. Runtime-only: an app restart is never paused.
    paused: Cell<bool>,
    /// Consecutive transient failures while a route is held.
    failures: Cell<u8>,
    generation: Cell<u64>,
    sockets: RefCell<CancellationToken>,
}
impl Tailnet {
    pub fn new(key_store: Option<Arc<dyn KeyStore>>) -> Self {
        Self {
            manager: key_store.map(|keys| Arc::new(Manager::new(keys)) as Arc<dyn Backend>),
            operation: Mutex::new(()),
            settings: RefCell::new(Settings::default()),
            status: RefCell::new(None),
            proxy: RefCell::new(None),
            error: RefCell::new(None),
            busy: Cell::new(false),
            visible_busy: Cell::new(false),
            checked_at: Cell::new(None),
            poll_changed: Rc::new(Notify::new()),
            paused: Cell::new(false),
            failures: Cell::new(0),
            generation: Cell::new(0),
            sockets: RefCell::new(CancellationToken::new()),
        }
    }
    /// Enabled and not paused: the node should be running.
    fn wanted(&self) -> bool {
        self.settings.borrow().enabled && !self.paused.get()
    }
    pub fn state(&self) -> Value {
        let mut state = self.settings.borrow().json();
        let status = self.status.borrow();
        state["backendState"] = json!(if self.error.borrow().is_some() {
            "Error"
        } else if !self.settings.borrow().enabled {
            "Disabled"
        } else if self.paused.get() {
            "Paused"
        } else {
            status
                .as_ref()
                .map(|s| s.backend_state.as_str())
                .unwrap_or("Starting")
        });
        state["paused"] = json!(self.paused.get());
        // Without a platform secret store no node can ever run.
        state["available"] = json!(self.manager.is_some());
        state["account"] = json!(status.as_ref().and_then(|s| s.account.as_ref()));
        state["tailnet"] = json!(status.as_ref().and_then(|s| s.tailnet.as_ref()));
        state["nodeName"] = json!(status.as_ref().and_then(|s| s.node_name.as_ref()));
        state["ips"] = json!(
            status
                .as_ref()
                .map(|s| s.ips.as_slice())
                .unwrap_or_default()
        );
        state["health"] = json!(
            status
                .as_ref()
                .map(|s| s.health.as_slice())
                .unwrap_or_default()
        );
        state["error"] = json!(*self.error.borrow());
        state["busy"] = json!(self.visible_busy.get());
        state
    }
}

fn invalid(message: &str) -> CoreError {
    CoreError::new("TAILNET_CONFIG", message, false)
}
fn unavailable(message: &str) -> CoreError {
    CoreError::new("TAILNET_UNAVAILABLE", message, false)
}

/// The only authority for selecting a host's network. A missing node never means normal routing.
pub(super) fn route(
    core: &Core,
    config: &Value,
) -> Result<(Option<Proxy>, CancellationToken), CoreError> {
    if !host_config::uses_tailnet(config) {
        return Ok((None, core.cancel.clone()));
    }
    if !core.tailnet.settings.borrow().enabled {
        return Err(unavailable(
            "Embedded Tailscale is disabled or changing. This host will not use the normal network.",
        ));
    }
    if core.tailnet.paused.get() {
        return Err(unavailable(
            "Embedded Tailscale is paused. Resume it to reach this host; this host will not use the normal network.",
        ));
    }
    let proxy = core.tailnet.proxy.borrow().clone().filter(Proxy::is_valid)
        .ok_or_else(|| unavailable("Embedded Tailscale is not connected. Enable it and sign in in Settings; this host will not use the normal network."))?;
    Ok((Some(proxy), core.tailnet.sockets.borrow().clone()))
}

fn invalidate(core: &Rc<Core>) -> u64 {
    let tailnet = &core.tailnet;
    let generation = tailnet.generation.get() + 1;
    tailnet.generation.set(generation);
    tailnet.sockets.borrow().cancel();
    *tailnet.sockets.borrow_mut() = core.cancel.child_token();
    tailnet.proxy.borrow_mut().take();
    let all = core.hosts.borrow().clone();
    let mut affected: HashSet<String> = all
        .iter()
        .filter(|host| host_config::uses_tailnet(&host.config.borrow()))
        .map(|host| host.server_id())
        .collect();
    // Connected-host routes are children of the selected transport, not independent fallback paths.
    loop {
        let old = affected.len();
        for host in &all {
            if host
                .origin
                .as_ref()
                .is_some_and(|origin| affected.contains(origin))
            {
                affected.insert(host.server_id());
            }
        }
        if affected.len() == old {
            break;
        }
    }
    for host in all {
        if affected.contains(&host.server_id()) {
            hosts::close_link(core, &host);
            let selector = host.selector.borrow().clone();
            if let Some(selector) = selector {
                selector.connections_changed(false);
            }
        }
    }
    generation
}

pub(super) fn initialize(core: &Rc<Core>) {
    let saved = core.store.borrow().prefs.values.get(SETTINGS).cloned();
    if let Some(saved) = saved {
        match Settings::parse(&saved) {
            Ok(settings) => *core.tailnet.settings.borrow_mut() = settings,
            Err(error) => *core.tailnet.error.borrow_mut() = Some(error.message),
        }
    }
    ensure(core);
    let weak = Rc::downgrade(core);
    let changed = core.tailnet.poll_changed.clone();
    tokio::task::spawn_local(async move {
        loop {
            let Some(core) = weak.upgrade() else {
                break;
            };
            if core.closing.get() {
                break;
            }
            // Manager has no status-change subscription. Poll only a wanted node, five
            // seconds after its last operation; an invalidated route wakes us sooner.
            let should_poll =
                core.tailnet.wanted() && core.tailnet.manager.is_some() && !core.tailnet.busy.get();
            let due =
                core.tailnet.checked_at.get().unwrap_or_else(Instant::now) + Duration::from_secs(5);
            let proxy = core.tailnet.proxy.borrow().clone().filter(Proxy::is_valid);
            let cancel = core.cancel.clone();
            drop(core);
            let poll_due = async {
                if !should_poll {
                    std::future::pending::<()>().await;
                }
                tokio::select! {
                    _ = tokio::time::sleep_until(due.into()) => {}
                    _ = async {
                        match proxy {
                            Some(proxy) => proxy.invalidated().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {}
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = changed.notified() => continue,
                _ = poll_due => {
                    if let Some(core) = weak.upgrade() {
                        poll(&core);
                    }
                }
            }
        }
    });
}

/// The periodic status check; never starts, restarts or wakes a paused node.
fn poll(core: &Rc<Core>) {
    if core.tailnet.wanted() && !core.tailnet.busy.get() {
        launch(core, None, "status".into(), None);
    }
}

/// Startup and focus regain: keeps a responsive node and its routes, and starts one only when
/// none is running. A paused node stays paused.
pub(super) fn ensure(core: &Rc<Core>) {
    if core.tailnet.wanted() && !core.tailnet.busy.get() {
        launch(core, None, "ensure".into(), None);
    }
}

pub(super) fn request(core: &Rc<Core>, id: RequestId, message: Value) {
    let action = message["action"].as_str().unwrap_or_default();
    let settings = match action {
        "configure" => match Settings::parse(&message) {
            Ok(settings) => Some(settings),
            Err(error) => {
                respond(core, &id, action, Err(error), None);
                return;
            }
        },
        "status" | "login" | "disconnect" | "logout" | "resume" => None,
        "pause" if !core.tailnet.settings.borrow().enabled => {
            respond(
                core,
                &id,
                action,
                Err(invalid("Embedded Tailscale is off")),
                None,
            );
            return;
        }
        "pause" => None,
        _ => {
            respond(
                core,
                &id,
                action,
                Err(invalid("Unknown embedded Tailscale action")),
                None,
            );
            return;
        }
    };
    launch(core, Some(id), action.to_owned(), settings);
}

fn respond(
    core: &Core,
    id: &str,
    action: &str,
    result: Result<(), CoreError>,
    auth_url: Option<String>,
) {
    let mut payload = json!({"requestId":id,"action":action,"success":result.is_ok(),"tailnet":core.tailnet.state()});
    if let Err(error) = result {
        payload["error"] = json!(error.message);
    }
    if let Some(url) = auth_url {
        payload["authUrl"] = json!(url);
    }
    core.respond(
        "",
        id,
        Ok(json!({"type":"tailnet.response","payload":payload})),
    );
}

fn launch(core: &Rc<Core>, id: Option<RequestId>, action: String, settings: Option<Settings>) {
    if action == "status" && core.tailnet.busy.get() {
        if let Some(id) = id {
            respond(core, &id, &action, Ok(()), None);
        }
        return;
    }
    let previous = core.tailnet.state();
    // Status, ensure and resume keep a healthy node's routes; every other action fences older
    // operations and closes the routes of the node it replaces or stops.
    let generation = if matches!(action.as_str(), "status" | "ensure" | "resume") {
        core.tailnet.generation.get()
    } else {
        invalidate(core)
    };
    if let Some(settings) = settings {
        *core.tailnet.settings.borrow_mut() = settings;
    }
    match action.as_str() {
        "disconnect" | "logout" => {
            core.tailnet.settings.borrow_mut().enabled = false;
            core.tailnet.paused.set(false);
        }
        "configure" | "login" | "resume" => core.tailnet.paused.set(false),
        "pause" => core.tailnet.paused.set(true),
        _ => {}
    }
    core.tailnet.busy.set(true);
    core.tailnet
        .visible_busy
        .set(id.is_some() && action != "status");
    if core.tailnet.visible_busy.get() {
        *core.tailnet.error.borrow_mut() = None;
    }
    if core.tailnet.state() != previous {
        core.touch();
    }
    core.tailnet.poll_changed.notify_one();
    let core = core.clone();
    tokio::task::spawn_local(async move {
        let _guard = core.tailnet.operation.lock().await;
        if core.closing.get() || generation != core.tailnet.generation.get() {
            if let Some(id) = id {
                respond(
                    &core,
                    &id,
                    &action,
                    Err(invalid("Operation superseded by a newer Tailscale setting")),
                    None,
                );
            }
            return;
        }
        let settings = core.tailnet.settings.borrow().clone();
        let enabled = settings.enabled;
        let wanted = core.tailnet.wanted();
        let config = settings.runtime(&core);
        let result: Result<OperationResult, CoreError> = async {
            let mut persistence_error =
                if matches!(action.as_str(), "configure" | "disconnect" | "logout") {
                    {
                        let mut store = core.store.borrow_mut();
                        let value = settings.json();
                        if store.prefs.values.get(SETTINGS) != Some(&value) {
                            store.prefs.values.insert(SETTINGS.into(), value);
                            store.bump_pref(SETTINGS);
                        }
                    }
                    store::write_prefs(&core).await.err()
                } else {
                    None
                };
            if matches!(action.as_str(), "status" | "ensure" | "resume") && !wanted {
                return Ok(OperationResult::default());
            }
            let manager = core.tailnet.manager.clone().ok_or_else(|| {
                unavailable("Embedded Tailscale requires an available platform secret store. Normal host connections are unaffected.")
            })?;
            let operation = action.clone();
            let proxy = core.tailnet.proxy.borrow().clone().filter(Proxy::is_valid);
            // A storage failure must not prevent disconnect/revocation. Never start a newly
            // enabled node whose intent could not be persisted.
            if action == "configure" && enabled
                && let Some(error) = persistence_error.take()
            {
                let _ = tokio::task::spawn_blocking(move || manager.disconnect()).await;
                return Err(error);
            }
            let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                match operation.as_str() {
                    "ensure" | "resume" => {
                        // A responsive node keeps its routes; only a missing, stopped or
                        // failing node is replaced.
                        match manager.status() {
                            Ok(status) if status.backend_state != "Stopped" => {
                                routed(&*manager, status, proxy)
                            }
                            _ => {
                                manager.disconnect()?;
                                let status = manager.start(config)?;
                                Ok(OperationResult { restarted: true, ..routed(&*manager, status, None)? })
                            }
                        }
                    }
                    "configure" => {
                        manager.disconnect()?;
                        if !enabled {
                            return Ok(OperationResult::default());
                        }
                        let status = manager.start(config)?;
                        Ok(OperationResult { restarted: true, ..routed(&*manager, status, None)? })
                    }
                    "disconnect" | "pause" => {
                        manager.disconnect()?;
                        Ok(OperationResult::default())
                    }
                    "logout" => {
                        manager.start(config)?;
                        manager.logout()?;
                        Ok(OperationResult::default())
                    }
                    "login" => {
                        anyhow::ensure!(enabled, "Enable embedded Tailscale before signing in");
                        manager.start(config)?;
                        let auth_url = manager.login()?;
                        let status = manager.status()?;
                        Ok(OperationResult { auth_url: Some(auth_url), ..routed(&*manager, status, None)? })
                    }
                    "status" => routed(&*manager, manager.status()?, proxy),
                    _ => unreachable!(),
                }
            })
            .await
            .map_err(|_| unavailable("Embedded Tailscale worker stopped"))?
            .map_err(|error| unavailable(&error.to_string()))?;
            if let Some(error) = persistence_error {
                return Err(error);
            }
            Ok(outcome)
        }.await;
        if core.closing.get() || generation != core.tailnet.generation.get() {
            if let Some(id) = id {
                respond(
                    &core,
                    &id,
                    &action,
                    Err(invalid("Operation superseded by a newer Tailscale setting")),
                    None,
                );
            }
            return;
        }
        let previous = core.tailnet.state();
        core.tailnet.busy.set(false);
        core.tailnet.visible_busy.set(false);
        core.tailnet.checked_at.set(Some(Instant::now()));
        core.tailnet.poll_changed.notify_one();
        // One stalled or rebinding check does not drop a held route; a terminal state, a
        // failure that persists or any other action applies at once.
        let held = core
            .tailnet
            .proxy
            .borrow()
            .as_ref()
            .is_some_and(Proxy::is_valid);
        let transient = match (action.as_str(), &result) {
            ("status", Err(_)) => true,
            ("status" | "ensure", Ok(outcome)) => {
                outcome.proxy.is_none()
                    && !outcome.restarted
                    && outcome
                        .status
                        .as_ref()
                        .is_some_and(|status| !terminal(&status.backend_state))
            }
            _ => false,
        };
        let failures = core.tailnet.failures.get() + 1;
        if held && transient && failures < TOLERATED_FAILURES {
            core.tailnet.failures.set(failures);
            if core.tailnet.state() != previous {
                core.touch();
            }
            if let Some(id) = id {
                respond(&core, &id, &action, Ok(()), None);
            }
            return;
        }
        core.tailnet.failures.set(0);
        let mut auth_url = None;
        let result = match result {
            Ok(OperationResult {
                status,
                proxy,
                auth_url: auth,
                restarted,
            }) => {
                if action != "status" || enabled {
                    *core.tailnet.error.borrow_mut() = None;
                }
                if restarted && core.tailnet.proxy.borrow().is_some() {
                    invalidate(&core);
                }
                let reconnect = proxy.is_some() && core.tailnet.proxy.borrow().is_none();
                // A failed backend cannot retain a previously usable route.
                if proxy.is_none() && core.tailnet.proxy.borrow().is_some() {
                    invalidate(&core);
                }
                *core.tailnet.status.borrow_mut() = status;
                *core.tailnet.proxy.borrow_mut() = proxy;
                auth_url = auth;
                if reconnect {
                    let hosts = core.hosts.borrow().clone();
                    for host in hosts {
                        if host.reconnect_enabled()
                            && host_config::uses_tailnet(&host.config.borrow())
                        {
                            directory::refresh_host(&core, &host);
                        }
                    }
                }
                Ok(())
            }
            Err(error) => {
                if core.tailnet.proxy.borrow().is_some() {
                    invalidate(&core);
                }
                *core.tailnet.error.borrow_mut() = Some(error.message.clone());
                Err(error)
            }
        };
        if core.tailnet.state() != previous {
            core.touch();
        }
        if let Some(id) = id {
            respond(&core, &id, &action, result, auth_url);
        }
    });
}

pub(super) async fn shutdown(core: &Rc<Core>) {
    invalidate(core);
    let _guard = core.tailnet.operation.lock().await;
    if let Some(manager) = core.tailnet.manager.clone() {
        let _ = tokio::task::spawn_blocking(move || manager.disconnect()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tailnet_settings_reject_unsafe_control_urls() {
        for control in [
            "http://example.com",
            "http://localhost:2345",
            "https://user:secret@example.com",
            "file:///tmp/control",
            "https://example.com/#secret",
            "https://example.com/?key=secret",
        ] {
            assert!(
                Settings::parse(&json!({"enabled":true,"hostname":"phone","controlUrl":control}))
                    .is_err()
            );
        }
        for control in [
            "https://control.example.com",
            "http://127.0.0.1:2345",
            "http://[::1]:2345",
        ] {
            assert!(
                Settings::parse(&json!({"enabled":true,"hostname":"phone","controlUrl":control}))
                    .is_ok()
            );
        }
    }
    #[test]
    fn tailnet_settings_and_public_state_never_persist_authentication() {
        let settings = Settings::parse(
            &json!({"enabled":true,"hostname":"phone","authUrl":"secret","password":"secret"}),
        )
        .unwrap();
        assert!(!settings.json().to_string().contains("secret"));
        let node = Tailnet::new(None);
        assert_eq!(node.state()["backendState"], "Disabled");
        assert!(!node.state().to_string().contains("authUrl"));
        *node.status.borrow_mut() = Some(Status {
            backend_state: "Running".into(),
            ..Status::default()
        });
        *node.error.borrow_mut() = Some("Logout was not confirmed; identity retained".into());
        assert_eq!(node.state()["backendState"], "Error");
        assert_eq!(node.state()["enabled"], false);
    }

    fn core(path: &std::path::Path) -> Rc<Core> {
        Core::new(
            crate::CoreConfig {
                state_dir: path.to_owned(),
                daemon_home: None,
                default_endpoint: None,
                tailnet_key_store: None,
            },
            Arc::new(crate::events::EventQueue::new(Arc::new(|| {}))),
        )
    }

    #[tokio::test]
    async fn tailnet_invalidation_closes_only_selected_hosts_and_fences_old_routes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let core = core(dir.path());
                let selected =
                    json!({"id":"tail","endpoint":"ws://peer:6767/ws","networkMode":"tailnet"});
                let normal = json!({"id":"normal","endpoint":"ws://localhost:6767/ws"});
                let tail = hosts::Host::new(
                    1,
                    selected.clone(),
                    "tail".into(),
                    None,
                    vec![selected.clone()],
                    None,
                );
                let direct = hosts::Host::new(
                    2,
                    normal.clone(),
                    "normal".into(),
                    None,
                    vec![normal.clone()],
                    None,
                );
                core.hosts
                    .borrow_mut()
                    .extend([tail.clone(), direct.clone()]);
                assert!(route(&core, &selected).is_err());
                assert!(route(&core, &normal).unwrap().0.is_none());
                let old = core.tailnet.sockets.borrow().clone();
                let direct_cancel = direct.resource_cancellation();
                let selected_cancel = tail.resource_cancellation();
                let generation = invalidate(&core);
                assert_eq!(generation, 1);
                assert!(old.is_cancelled());
                assert!(selected_cancel.is_cancelled());
                assert!(!direct_cancel.is_cancelled());
                assert!(
                    !tail.resource_cancellation().is_cancelled(),
                    "new unrelated public requests remain possible after disconnect"
                );
                assert!(!core.tailnet.sockets.borrow().is_cancelled());
                core.tailnet.settings.borrow_mut().enabled = true;
                assert!(
                    route(&core, &selected).is_err(),
                    "enabled without a live node must not imply a normal route"
                );
                assert!(
                    route(&core, &normal).unwrap().0.is_none(),
                    "ordinary hosts remain direct while the embedded node is enabled"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn tailnet_newer_request_supersedes_pending_enable_and_missing_keystore_is_explicit() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let core = core(dir.path());
                request(
                    &core,
                    "old".into(),
                    json!({"action":"configure","enabled":true,"hostname":"phone"}),
                );
                request(&core, "new".into(), json!({"action":"disconnect"}));
                let mut responses = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        core.queue.drain_into(&mut responses, 100);
                        if responses
                            .iter()
                            .filter(|event| matches!(event, crate::CoreEvent::Response { .. }))
                            .count()
                            == 2
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let old = responses
                    .iter()
                    .find_map(|event| match event {
                        crate::CoreEvent::Response {
                            request_id,
                            result: Ok(value),
                            ..
                        } if request_id == "old" => Some(value),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(old["payload"]["success"], false);
                assert!(
                    old["payload"]["error"]
                        .as_str()
                        .unwrap()
                        .contains("superseded")
                );
                assert!(!core.tailnet.settings.borrow().enabled);
                assert!(!core.tailnet.busy.get());
                assert!(
                    core.tailnet
                        .error
                        .borrow()
                        .as_deref()
                        .unwrap()
                        .contains("secret store")
                );
            })
            .await;
    }

    /// A scripted node. `state` is what status reports (`None`: no node; `"Stalled"`: the
    /// status call fails); `node` is the running node's lifetime, cancelled by a disconnect.
    #[derive(Default)]
    struct Fake {
        state: std::sync::Mutex<Option<&'static str>>,
        node: std::sync::Mutex<CancellationToken>,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }
    impl Fake {
        fn count(&self, call: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == call)
                .count()
        }
        fn set(&self, state: Option<&'static str>) {
            if state.is_none() {
                self.node.lock().unwrap().cancel();
            }
            *self.state.lock().unwrap() = state;
        }
        fn report(&self, state: &str) -> anyhow::Result<Status> {
            anyhow::ensure!(state != "Stalled", "status request timed out");
            Ok(Status {
                backend_state: state.into(),
                ..Status::default()
            })
        }
    }
    impl Backend for Fake {
        fn start(&self, _: Config) -> anyhow::Result<Status> {
            self.calls.lock().unwrap().push("start");
            let mut state = self.state.lock().unwrap();
            if state.is_none() {
                *state = Some("Running");
                *self.node.lock().unwrap() = CancellationToken::new();
            }
            self.report(state.unwrap())
        }
        fn status(&self) -> anyhow::Result<Status> {
            self.calls.lock().unwrap().push("status");
            let state = *self.state.lock().unwrap();
            self.report(state.ok_or_else(|| anyhow::anyhow!("embedded Tailscale is disconnected"))?)
        }
        fn login(&self) -> anyhow::Result<String> {
            Ok("https://login.example.com/a/once".into())
        }
        fn proxy(&self) -> anyhow::Result<Proxy> {
            Ok(Proxy::unconnected(self.node.lock().unwrap().clone()))
        }
        fn disconnect(&self) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("disconnect");
            self.set(None);
            Ok(())
        }
        fn logout(&self) -> anyhow::Result<()> {
            self.set(None);
            Ok(())
        }
    }

    /// An enabled core whose node is `Fake`, with an empty writable preference store.
    fn faked(path: &std::path::Path) -> (Rc<Core>, Arc<Fake>) {
        let mut core = core(path);
        let fake = Arc::new(Fake::default());
        Rc::get_mut(&mut core).unwrap().tailnet.manager = Some(fake.clone() as Arc<dyn Backend>);
        core.tailnet.settings.borrow_mut().enabled = true;
        core.store.borrow_mut().prefs.writable = true;
        (core, fake)
    }

    async fn settle(core: &Rc<Core>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while core.tailnet.busy.get() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }

    fn selected() -> Value {
        json!({"id":"tail","endpoint":"ws://peer:6767/ws","networkMode":"tailnet"})
    }

    #[tokio::test]
    async fn tailnet_focus_ensure_keeps_a_running_node_and_restarts_only_a_missing_one() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let (core, fake) = faked(dir.path());
                ensure(&core);
                settle(&core).await;
                assert_eq!((fake.count("start"), fake.count("disconnect")), (1, 1));
                assert!(route(&core, &selected()).unwrap().0.is_some());

                let tail =
                    hosts::Host::new(1, selected(), "tail".into(), None, vec![selected()], None);
                core.hosts.borrow_mut().push(tail.clone());
                let links = tail.resource_cancellation();
                let sockets = core.tailnet.sockets.borrow().clone();
                let generation = core.tailnet.generation.get();
                for _ in 0..3 {
                    ensure(&core);
                    settle(&core).await;
                }
                assert_eq!(
                    (fake.count("start"), fake.count("disconnect")),
                    (1, 1),
                    "focus regain never restarts a Running node"
                );
                assert_eq!(core.tailnet.generation.get(), generation);
                assert!(!sockets.is_cancelled() && !links.is_cancelled());
                assert!(route(&core, &selected()).unwrap().0.unwrap().is_valid());

                // A node that went away is started again, and the dead routes are closed.
                fake.set(None);
                ensure(&core);
                settle(&core).await;
                assert_eq!(fake.count("start"), 2);
                assert!(links.is_cancelled());
                assert!(route(&core, &selected()).unwrap().0.unwrap().is_valid());
            })
            .await;
    }

    #[tokio::test]
    async fn tailnet_pause_is_runtime_only_resume_restarts_and_stop_persists_off() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let (core, fake) = faked(dir.path());
                ensure(&core);
                settle(&core).await;

                request(&core, "pause".into(), json!({"action":"pause"}));
                settle(&core).await;
                let state = core.tailnet.state();
                assert_eq!(state["paused"], true);
                assert_eq!(state["enabled"], true);
                assert_eq!(state["backendState"], "Paused");
                assert!(
                    fake.state.lock().unwrap().is_none(),
                    "pause disconnects the node"
                );
                assert!(
                    route(&core, &selected())
                        .unwrap_err()
                        .message
                        .contains("paused")
                );
                assert!(
                    !core.store.borrow().prefs.values.contains_key(SETTINGS),
                    "pause is never persisted"
                );

                // Neither the poll nor focus regain wakes a paused node.
                poll(&core);
                ensure(&core);
                assert!(!core.tailnet.busy.get());
                assert_eq!(fake.count("start"), 1);

                request(&core, "resume".into(), json!({"action":"resume"}));
                settle(&core).await;
                assert_eq!(core.tailnet.state()["paused"], false);
                assert_eq!(fake.count("start"), 2);
                assert!(route(&core, &selected()).unwrap().0.unwrap().is_valid());

                request(&core, "pause".into(), json!({"action":"pause"}));
                settle(&core).await;
                request(&core, "stop".into(), json!({"action":"disconnect"}));
                settle(&core).await;
                let state = core.tailnet.state();
                assert_eq!(state["paused"], false, "stop clears a pause");
                assert_eq!(state["backendState"], "Disabled", "{state}");
                assert_eq!(
                    core.store.borrow().prefs.values[SETTINGS]["enabled"],
                    false,
                    "stop persists Embedded Tailscale off"
                );
                assert!(route(&core, &selected()).is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn tailnet_poll_keeps_routes_through_transient_failures_but_not_terminal_states() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let (core, fake) = faked(dir.path());
                ensure(&core);
                settle(&core).await;
                for transient in ["Starting", "Stalled"] {
                    let generation = core.tailnet.generation.get();
                    fake.set(Some(transient));
                    for _ in 1..TOLERATED_FAILURES {
                        poll(&core);
                        settle(&core).await;
                        assert!(
                            route(&core, &selected()).unwrap().0.is_some(),
                            "{transient}"
                        );
                        assert_eq!(core.tailnet.generation.get(), generation);
                    }
                    poll(&core);
                    settle(&core).await;
                    assert!(route(&core, &selected()).is_err(), "{transient} persisted");
                    assert_eq!(core.tailnet.generation.get(), generation + 1);

                    // Running again restores the route.
                    fake.set(Some("Running"));
                    poll(&core);
                    settle(&core).await;
                    assert!(
                        route(&core, &selected()).unwrap().0.is_some(),
                        "{transient}"
                    );
                    assert!(core.tailnet.error.borrow().is_none());
                }

                // One success resets the count; a terminal state applies at once.
                fake.set(Some("Starting"));
                poll(&core);
                settle(&core).await;
                fake.set(Some("Running"));
                poll(&core);
                settle(&core).await;
                fake.set(Some("Starting"));
                poll(&core);
                settle(&core).await;
                assert!(route(&core, &selected()).unwrap().0.is_some());
                fake.set(Some("NeedsLogin"));
                poll(&core);
                settle(&core).await;
                assert!(route(&core, &selected()).is_err());
                assert_eq!(core.tailnet.state()["backendState"], "NeedsLogin");
            })
            .await;
    }
}
