//! Saved hosts and their client lifecycle: Flutter `PaseoClient` connect/reconnect/liveness and
//! `AppStore` add/update/remove, connected-host routes and local daemon discovery.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::{Conn, Core, directory, requests, selector::Selector, store};
use crate::{
    CoreError, RequestId,
    connection::{self, Control, Inbound, OpenRequest, Opened, RouteIn},
    error::host_unknown,
    host_config,
    ids,
    protocol::response_error,
    storage,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Status {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Error => "error",
        }
    }
}

/// Flutter `PaseoClient` connection state for one host.
struct Link {
    status: Status,
    conn: Option<Rc<Conn>>,
    generation: u64,
    retry: u32,
    reconnect: Option<tokio::task::AbortHandle>,
    /// Automatic (re)connection allowed; false after `disconnect_host` or removal.
    enabled: bool,
    waiters: Vec<oneshot::Sender<Result<Rc<Conn>, CoreError>>>,
    rtt: Option<Duration>,
    verifying: Option<u64>,
}

pub(crate) struct Host {
    pub key: u64,
    server_id: RefCell<String>,
    pub origin: Option<String>,
    pub config: RefCell<Value>,
    pub connections: RefCell<Vec<Value>>,
    /// Bumped whenever the host's client is replaced (Flutter `identical(host.client, …)`).
    pub client_epoch: Cell<u64>,
    pub detached_epoch: Cell<Option<u64>>,
    link: RefCell<Link>,
    pub dir: RefCell<directory::Directory>,
    pub selector: RefCell<Option<Rc<Selector>>>,
    pub error: RefCell<Option<CoreError>>,
    /// Last known `server_info`, kept while disconnected.
    pub server_info: RefCell<Value>,
}

/// Flutter `PaseoClient.reconnectDelay`: `min(1500ms * 2^attempt, 30s)`, no jitter.
pub(crate) fn reconnect_delay(attempt: u32) -> Duration {
    if attempt >= 15 {
        return Duration::from_millis(30_000);
    }
    Duration::from_millis((1500u64 << attempt).min(30_000))
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const LIVENESS_INTERVAL: Duration = Duration::from_secs(10);
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(15);
const LIVENESS_FAILURES: u32 = 2;

impl Host {
    pub fn new(
        key: u64,
        config: Value,
        server_id: String,
        origin: Option<String>,
        connections: Vec<Value>,
        replica: Option<storage::Replica>,
    ) -> Rc<Self> {
        let mut dir = directory::Directory::default();
        if let Some(replica) = replica {
            dir.apply_replica(replica);
        }
        Rc::new(Self {
            key,
            server_id: RefCell::new(server_id),
            origin,
            config: RefCell::new(config),
            connections: RefCell::new(connections),
            client_epoch: Cell::new(0),
            detached_epoch: Cell::new(None),
            link: RefCell::new(Link {
                status: Status::Disconnected,
                conn: None,
                generation: 0,
                retry: 0,
                reconnect: None,
                enabled: true,
                waiters: Vec::new(),
                rtt: None,
                verifying: None,
            }),
            dir: RefCell::new(dir),
            selector: RefCell::new(None),
            error: RefCell::new(None),
            server_info: RefCell::new(Value::Null),
        })
    }

    pub fn server_id(&self) -> String {
        self.server_id.borrow().clone()
    }

    pub fn set_server_id(&self, server_id: &str) {
        *self.server_id.borrow_mut() = server_id.to_owned();
    }

    pub fn config_id(&self) -> String {
        self.config.borrow()["id"].as_str().unwrap_or_default().to_owned()
    }

    pub fn matches(&self, id: &str) -> bool {
        *self.server_id.borrow() == id
            || self.config.borrow()["id"] == id
            || self.connections.borrow().iter().any(|c| c["id"] == id)
    }

    pub fn conn(&self) -> Option<Rc<Conn>> {
        self.link.borrow().conn.clone()
    }

    pub fn conn_generation(&self) -> Option<u64> {
        self.link.borrow().conn.as_ref().map(|conn| conn.generation)
    }

    pub fn connecting(&self) -> bool {
        self.link.borrow().status == Status::Connecting
    }

    pub fn status(&self) -> Status {
        self.link.borrow().status
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.link.borrow().rtt
    }

    pub fn enable(&self) {
        self.link.borrow_mut().enabled = true;
    }

    fn label(&self) -> String {
        match self.config.borrow()["label"].as_str() {
            Some(label) if !label.is_empty() => label.to_owned(),
            _ => self.server_id(),
        }
    }

    /// Flutter files `_localHost`: a desktop-managed daemon reached over a local socket.
    fn is_local(&self) -> bool {
        if cfg!(any(target_os = "android", target_os = "ios")) || self.origin.is_some() {
            return false;
        }
        if self.server_info.borrow()["desktopManaged"] != true {
            return false;
        }
        let config = self.config.borrow();
        let endpoint = config["endpoint"].as_str().unwrap_or_default();
        if endpoint.starts_with("unix:") || endpoint.starts_with("pipe:") || endpoint.starts_with('/') {
            return true;
        }
        let candidate = if endpoint.contains("://") {
            endpoint.to_owned()
        } else {
            format!("tcp://{endpoint}")
        };
        url::Url::parse(&candidate)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1"))
    }

    /// The saved `state.json` row.
    pub fn record_json(&self) -> Value {
        json!({
            "serverId": self.server_id(),
            "originServerId": self.origin,
            "config": *self.config.borrow(),
            "connections": *self.connections.borrow(),
        })
    }

    pub fn replica_json(&self) -> Value {
        let dir = self.dir.borrow();
        json!({
            "serverId": self.server_id(),
            "cachedAt": dir.cached_at,
            "projects": dir.projects,
            "workspaces": dir.workspaces,
            "agents": dir.agents,
        })
    }

    pub fn state_json(&self) -> Value {
        let link = self.link.borrow();
        let dir = self.dir.borrow();
        let connected = link.status == Status::Connected;
        let error = self.error.borrow().as_ref().map(CoreError::to_json);
        let live = connected
            && !dir.loading
            && error.is_none()
            && dir.hydrating.is_empty()
            && dir.list_errors.is_empty();
        let cached = !live
            && (!dir.projects.is_empty() || !dir.workspaces.is_empty() || !dir.agents.is_empty());
        let list_errors: Map<String, Value> = dir
            .list_errors
            .iter()
            .map(|(kind, error)| ((*kind).to_owned(), error.to_json()))
            .collect();
        let probes = self
            .selector
            .borrow()
            .as_ref()
            .map(|selector| selector.probes_json())
            .unwrap_or_else(|| json!({}));
        let server_id = self.server_id();
        json!({
            "id": server_id,
            "serverId": server_id,
            "originServerId": self.origin,
            "label": self.label(),
            "config": *self.config.borrow(),
            "connections": *self.connections.borrow(),
            "activeConnectionId": self.config_id(),
            "status": link.status.name(),
            "error": error,
            "loading": dir.loading,
            "loadingLists": dir.hydrating.iter().collect::<Vec<_>>(),
            "listErrors": list_errors,
            "live": live,
            "cached": cached,
            "cachedAt": dir.cached_at,
            "maintenanceOperationId": dir.maintenance,
            "serverInfo": *self.server_info.borrow(),
            "connectionGeneration": link.generation,
            "latencyMs": link.rtt.map(|rtt| rtt.as_millis() as u64),
            "probes": probes,
            "isLocal": self.is_local(),
            "projects": dir.projects,
            "workspaces": dir.workspaces,
            "agents": dir.agents,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Connection lifecycle

fn relay_key(config: &Value) -> Option<String> {
    host_config::resolve(config).ok().and_then(|resolved| resolved.relay_key)
}

fn protected_pref(key: &str) -> String {
    format!("paseo.relayProtected.{key}")
}

pub(crate) fn protected_known(core: &Core, config: &Value) -> bool {
    let Some(key) = relay_key(config) else {
        return false;
    };
    core.protected_keys.borrow().contains(&key)
        || core.store.borrow().prefs.values.get(&protected_pref(&key)) == Some(&Value::Bool(true))
}

/// Waits for an admitted connection, starting an attempt when none runs (Flutter `connect`).
pub(crate) async fn connect(core: &Rc<Core>, host: &Rc<Host>) -> Result<Rc<Conn>, CoreError> {
    if let Some(conn) = host.conn() {
        return Ok(conn);
    }
    let (waiter, receiver) = oneshot::channel();
    host.link.borrow_mut().waiters.push(waiter);
    if host.status() != Status::Connecting {
        start_attempt(core, host);
    }
    receiver.await.unwrap_or_else(|_| {
        Err(CoreError::new(
            "DAEMON_CONNECTION_LOST",
            "Connection attempt ended",
            true,
        ))
    })
}

pub(crate) fn start_attempt(core: &Rc<Core>, host: &Rc<Host>) {
    if core.closing.get() {
        return;
    }
    let generation = {
        let mut link = host.link.borrow_mut();
        if link.status == Status::Connecting || link.conn.is_some() {
            return;
        }
        if let Some(timer) = link.reconnect.take() {
            timer.abort();
        }
        link.generation += 1;
        link.status = Status::Connecting;
        link.generation
    };
    core.touch();
    let core = core.clone();
    let host = host.clone();
    tokio::task::spawn_local(async move {
        let result = open(&core, &host).await;
        let current = host.link.borrow().generation == generation && !core.closing.get();
        match result {
            Ok(opened) if current => admit(&core, &host, opened, generation).await,
            Ok(opened) => {
                let client = opened.client.clone();
                tokio::task::spawn_local(async move { client.close().await });
            }
            Err(error) if current => lost(&core, &host, generation, error),
            Err(_) => {}
        }
    });
}

async fn open(core: &Rc<Core>, host: &Rc<Host>) -> Result<Opened, CoreError> {
    let config = host.config.borrow().clone();
    let request = OpenRequest {
        client_id: core.client_id.borrow().clone(),
        expected_server_id: config["expectedServerId"].as_str().map(str::to_owned),
        protected_known: protected_known(core, &config),
        timeout: CONNECT_TIMEOUT,
    };
    match &host.origin {
        Some(origin) => open_route(core, host, origin, request).await,
        None => connection::open_direct(&config, request, core.buffers.clone(), &core.cancel).await,
    }
}

/// Flutter `_HostRouteTransport.open` + `connectHost`.
async fn open_route(
    core: &Rc<Core>,
    host: &Rc<Host>,
    origin_id: &str,
    request: OpenRequest,
) -> Result<Opened, CoreError> {
    let Some(origin) = core.host_for(origin_id) else {
        return Err(CoreError::new(
            "PARENT_CLIENT_CLOSED",
            "The host connection used for this route is unavailable.",
            false,
        ));
    };
    if origin.status() == Status::Error
        && let Some(error) = origin.error.borrow().clone().filter(CoreError::is_terminal) {
            return Err(error);
        }
    let parent = connect(core, &origin).await?;
    let scope = ids::new_id();
    let (route_tx, route_rx) = tokio::sync::mpsc::unbounded_channel::<RouteIn>();
    let registry = Rc::downgrade(&parent);
    let accept: requests::Accept = Box::new(move |body: &Value| {
        if let (Some(conn), Some(route)) = (registry.upgrade(), body["routeId"].as_str()) {
            conn.routes.borrow_mut().insert(route.to_owned(), route_tx);
        }
    });
    let response = parent
        .request(
            json!({"type":"hosts.route.open.request","serverId":host.server_id(),"scopeId":scope}),
            "hosts.route.open.response",
            Some(requests::REQUEST_TIMEOUT),
            None,
            Some(accept),
        )
        .await;
    let route_id = match response {
        Ok(body) => match response_error(&body, "hosts.route.open.request") {
            None => body["routeId"].as_str().unwrap_or_default().to_owned(),
            Some(error) => {
                let _ = parent
                    .send(json!({"type":"hosts.route.close.request","scopeId":scope}))
                    .await;
                return Err(error);
            }
        },
        Err(error) => {
            if !parent.closed.get() {
                let _ = parent
                    .send(json!({"type":"hosts.route.close.request","scopeId":scope}))
                    .await;
            }
            return Err(error);
        }
    };
    let registry = Rc::downgrade(&parent);
    let unregister_id = route_id.clone();
    let unregister: Box<dyn FnOnce()> = Box::new(move || {
        if let Some(conn) = registry.upgrade() {
            conn.routes.borrow_mut().remove(&unregister_id);
        }
    });
    let request = OpenRequest {
        expected_server_id: Some(host.server_id()),
        ..request
    };
    connection::open_routed(
        parent.client.clone(),
        route_id,
        route_rx,
        unregister,
        request,
        &core.cancel,
    )
    .await
}

async fn admit(core: &Rc<Core>, host: &Rc<Host>, opened: Opened, generation: u64) {
    let config = host.config.borrow().clone();
    if opened.security == Some(paseo_client::crypto::SecurityMode::Protected)
        && let Some(key) = relay_key(&config) {
            core.protected_keys.borrow_mut().insert(key.clone());
            let pref = protected_pref(&key);
            let known = core.store.borrow().prefs.values.get(&pref) == Some(&Value::Bool(true));
            if !known {
                core.store
                    .borrow_mut()
                    .prefs
                    .values
                    .insert(pref, Value::Bool(true));
                if let Err(error) = store::write_prefs(core).await {
                    let client = opened.client.clone();
                    tokio::task::spawn_local(async move { client.close().await });
                    lost(
                        core,
                        host,
                        generation,
                        CoreError::new(
                            "RELAY_SECURITY",
                            format!("Unable to persist relay security policy: {}", error.message),
                            false,
                        ),
                    );
                    return;
                }
            }
        }
    if host.link.borrow().generation != generation || core.closing.get() {
        let client = opened.client.clone();
        tokio::task::spawn_local(async move { client.close().await });
        return;
    }
    let conn = Conn::new(generation, &opened);
    let info = conn.server_info.borrow().clone();
    let waiters = {
        let mut link = host.link.borrow_mut();
        link.conn = Some(conn.clone());
        link.status = Status::Connected;
        link.retry = 0;
        link.rtt = None;
        std::mem::take(&mut link.waiters)
    };
    *host.server_info.borrow_mut() = info.clone();
    if host
        .error
        .borrow()
        .as_ref()
        .is_some_and(|error| error.code.starts_with("DAEMON_") || error.is_auth_failure())
    {
        *host.error.borrow_mut() = None;
    }
    core.emit(crate::CoreEvent::Connected {
        host_id: host.server_id(),
        server_info: info.clone(),
    });
    let Opened { inbound, .. } = opened;
    tokio::task::spawn_local(read(core.clone(), host.clone(), conn.clone(), inbound));
    tokio::task::spawn_local(heartbeat(core.clone(), host.clone(), conn.clone()));
    for waiter in waiters {
        let _ = waiter.send(Ok(conn.clone()));
    }
    directory::sync_maintenance(core, host, &info);
    requests::restore_observations(core, host);
    let refreshing = host.dir.borrow().refreshing;
    if !refreshing && core.contains(host) {
        directory::refresh_host(core, host);
    }
    core.touch();
}

async fn read(
    core: Rc<Core>,
    host: Rc<Host>,
    conn: Rc<Conn>,
    mut inbound: tokio::sync::mpsc::UnboundedReceiver<Inbound>,
) {
    let error = loop {
        let Some(item) = inbound.recv().await else {
            break CoreError::new("DAEMON_CONNECTION_LOST", "Daemon connection closed", true);
        };
        if conn.closed.get() {
            return;
        }
        match item {
            Inbound::Message(message) => requests::handle_message(&core, &host, &conn, message),
            Inbound::Binary(bytes) => requests::handle_binary(&core, &host, &conn, bytes),
            Inbound::Closed(error) => break error,
        }
    };
    lost(&core, &host, conn.generation, error);
}

/// Flutter liveness heartbeat: one envelope ping 10 s after the previous probe settles.
async fn heartbeat(core: Rc<Core>, host: Rc<Host>, conn: Rc<Conn>) {
    loop {
        tokio::time::sleep(LIVENESS_INTERVAL).await;
        if conn.closed.get() || host.conn_generation() != Some(conn.generation) {
            return;
        }
        let (reply, answered) = oneshot::channel();
        if conn.control.send(Control::Ping(reply)).is_err() {
            return;
        }
        let started = Instant::now();
        match tokio::time::timeout(LIVENESS_TIMEOUT, answered).await {
            Ok(Ok(())) => {
                host.link.borrow_mut().rtt = Some(started.elapsed());
            }
            Ok(Err(_)) => return,
            Err(_) => {
                if conn.closed.get() {
                    return;
                }
                let failures = conn.liveness.failures.get() + 1;
                conn.liveness.failures.set(failures);
                if failures >= LIVENESS_FAILURES {
                    conn.liveness.failures.set(0);
                    lost(
                        &core,
                        &host,
                        conn.generation,
                        CoreError::new(
                            "DAEMON_CONNECTION_LOST",
                            format!("Liveness check timed out ({}ms)", LIVENESS_TIMEOUT.as_millis()),
                            true,
                        ),
                    );
                    return;
                }
            }
        }
    }
}

/// Flutter `_lost`: the connection generation ended (or its attempt failed).
pub(crate) fn lost(core: &Rc<Core>, host: &Rc<Host>, generation: u64, error: CoreError) {
    let (conn, waiters, enabled) = {
        let mut link = host.link.borrow_mut();
        if link.generation != generation {
            return;
        }
        let conn = link.conn.take();
        if conn.is_none() && link.status != Status::Connecting {
            return;
        }
        link.generation += 1;
        link.status = if link.enabled {
            Status::Error
        } else {
            Status::Disconnected
        };
        (conn, std::mem::take(&mut link.waiters), link.enabled)
    };
    if let Some(conn) = conn {
        conn.fail_all(&error);
        let client = conn.client.clone();
        tokio::task::spawn_local(async move { client.close().await });
    }
    requests::observations_lost(core, host, &error);
    *host.error.borrow_mut() = Some(error.clone());
    for waiter in waiters {
        let _ = waiter.send(Err(error.clone()));
    }
    core.emit(crate::CoreEvent::Disconnected {
        host_id: host.server_id(),
        error: Some(error.clone()),
    });
    let selector = host.selector.borrow().clone();
    if let Some(selector) = selector {
        selector.report_active_failure(&host.config_id(), &error);
    }
    if enabled && !error.is_terminal() && !core.closing.get() && core.contains(host) {
        schedule_reconnect(core, host);
    }
    core.touch();
}

fn schedule_reconnect(core: &Rc<Core>, host: &Rc<Host>) {
    let delay = {
        let mut link = host.link.borrow_mut();
        let delay = reconnect_delay(link.retry);
        link.retry += 1;
        delay
    };
    let task_core = core.clone();
    let task_host = host.clone();
    let handle = tokio::task::spawn_local(async move {
        tokio::time::sleep(delay).await;
        task_host.link.borrow_mut().reconnect = None;
        if task_host.link.borrow().enabled && task_core.contains(&task_host) {
            start_attempt(&task_core, &task_host);
        }
    });
    let previous = host
        .link
        .borrow_mut()
        .reconnect
        .replace(handle.abort_handle());
    if let Some(previous) = previous {
        previous.abort();
    }
}

/// Flutter `close()`: an intentional end of the client; observations stay registered and are
/// restored by the next admitted connection.
pub(crate) fn close_link(core: &Rc<Core>, host: &Rc<Host>) {
    let (conn, waiters, was_active) = {
        let mut link = host.link.borrow_mut();
        if let Some(timer) = link.reconnect.take() {
            timer.abort();
        }
        let was_active = link.conn.is_some() || link.status == Status::Connecting;
        link.generation += 1;
        link.status = Status::Disconnected;
        link.rtt = None;
        (link.conn.take(), std::mem::take(&mut link.waiters), was_active)
    };
    let error = CoreError::new("DAEMON_CONNECTION_LOST", "Client closed", true);
    if let Some(conn) = conn {
        conn.fail_all(&error);
        let client = conn.client.clone();
        tokio::task::spawn_local(async move { client.close().await });
    }
    requests::observations_lost(core, host, &error);
    for waiter in waiters {
        let _ = waiter.send(Err(error.clone()));
    }
    if was_active {
        core.emit(crate::CoreEvent::Disconnected {
            host_id: host.server_id(),
            error: None,
        });
    }
    core.touch();
}

/// Flutter `_detach`: releases the directory feeds and closes the host's client.
pub(crate) fn detach(core: &Rc<Core>, host: &Rc<Host>) {
    directory::cancel_resync(host);
    host.detached_epoch.set(Some(host.client_epoch.get()));
    requests::release_internal(core, host);
    close_link(core, host);
}

/// Flutter `_attach` with a new client: reconnects are enabled again.
pub(crate) fn attach(core: &Rc<Core>, host: &Rc<Host>) {
    host.client_epoch.set(host.client_epoch.get() + 1);
    {
        let mut link = host.link.borrow_mut();
        link.enabled = true;
        link.retry = 0;
    }
    if host.origin.is_none() && host.selector.borrow().is_none() && !core.closing.get() {
        let selector = Selector::new(core, host);
        *host.selector.borrow_mut() = Some(selector.clone());
        selector.start();
    }
}

fn dispose_selector(host: &Rc<Host>) {
    if let Some(selector) = host.selector.borrow_mut().take() {
        selector.stop();
    }
}

/// Flutter `ensureConnected`: skips the reconnect backoff; `verify` pings a healthy-looking
/// connection because an OS suspension may have frozen the heartbeat.
fn ensure_connected(core: &Rc<Core>, host: &Rc<Host>, verify: bool) {
    let (status, enabled) = {
        let link = host.link.borrow();
        (link.status, link.enabled)
    };
    if !enabled || core.closing.get() {
        return;
    }
    match status {
        Status::Connected if verify => verify_connection(core, host),
        Status::Connected | Status::Connecting => {}
        Status::Disconnected | Status::Error => start_attempt(core, host),
    }
}

fn verify_connection(core: &Rc<Core>, host: &Rc<Host>) {
    let Some(conn) = host.conn() else {
        return;
    };
    {
        let mut link = host.link.borrow_mut();
        if link.verifying == Some(conn.generation) {
            return;
        }
        link.verifying = Some(conn.generation);
    }
    let core = core.clone();
    let host = host.clone();
    tokio::task::spawn_local(async move {
        let sent = chrono::Utc::now().timestamp_millis();
        let result = conn
            .request(
                json!({"type":"ping","clientSentAt":sent}),
                "pong",
                Some(Duration::from_secs(3)),
                None,
                None,
            )
            .await;
        if let Err(error) = result
            && host.conn_generation() == Some(conn.generation) {
                lost(&core, &host, conn.generation, error);
                ensure_connected(&core, &host, false);
            }
        let mut link = host.link.borrow_mut();
        if link.verifying == Some(conn.generation) {
            link.verifying = None;
        }
    });
}

pub(crate) fn set_app_visible(core: &Rc<Core>, visible: bool) {
    if !visible {
        return;
    }
    let hosts: Vec<Rc<Host>> = core.hosts.borrow().clone();
    for host in hosts {
        ensure_connected(core, &host, true);
    }
}

// ---------------------------------------------------------------------------------------------
// Host management

fn new_host_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn add_to_core(core: &Rc<Core>, host: &Rc<Host>) {
    core.hosts.borrow_mut().push(host.clone());
    attach(core, host);
}

pub(crate) async fn connect_host(core: &Rc<Core>, value: Value) {
    let (config, extra, server_hint) = if value["config"].is_object() {
        (
            value["config"].clone(),
            value["connections"].as_array().cloned().unwrap_or_default(),
            value["serverId"].as_str().map(str::to_owned),
        )
    } else {
        (value, Vec::new(), None)
    };
    if !storage::valid_config(&config) {
        core.error(
            None,
            crate::error::invalid_request("A host connection needs string id, label and endpoint"),
        );
        return;
    }
    let config_id = config["id"].as_str().unwrap_or_default().to_owned();
    let existing = core
        .hosts
        .borrow()
        .iter()
        .find(|host| host.connections.borrow().iter().any(|c| c["id"] == config_id.as_str()))
        .cloned();
    if let Some(host) = existing {
        update_host(core, &host, config).await;
        return;
    }
    let server_id = server_hint
        .or_else(|| config["expectedServerId"].as_str().map(str::to_owned))
        .unwrap_or_else(|| config_id.clone());
    let mut connections = vec![config.clone()];
    for candidate in extra {
        if storage::valid_config(&candidate) && !connections.iter().any(|c| c["id"] == candidate["id"]) {
            connections.push(candidate);
        }
    }
    let host = Host::new(core.next_key(), config, server_id, None, connections, None);
    add_to_core(core, &host);
    save(core).await;
    directory::refresh_host(core, &host);
}

/// Flutter `updateHost`.
async fn update_host(core: &Rc<Core>, host: &Rc<Host>, config: Value) {
    detach(core, host);
    let credentials_changed = {
        let mut connections = host.connections.borrow_mut();
        match connections.iter().position(|c| c["id"] == config["id"]) {
            Some(index) => {
                let changed = connections[index]["password"] != config["password"];
                connections[index] = config.clone();
                changed
            }
            None => {
                connections.push(config.clone());
                false
            }
        }
    };
    *host.config.borrow_mut() = config;
    attach(core, host);
    let selector = host.selector.borrow().clone();
    if let Some(selector) = selector {
        selector.connections_changed(credentials_changed);
    }
    save(core).await;
    directory::refresh_host(core, host);
}

/// Closes a host's connection and stops reconnecting, keeping it saved.
pub(crate) async fn disconnect_host(core: &Rc<Core>, id: &str) {
    let Some(host) = core.host_for(id) else {
        core.error(Some(id), host_unknown(id));
        return;
    };
    host.link.borrow_mut().enabled = false;
    close_link(core, &host);
    *host.error.borrow_mut() = None;
    core.touch();
}

/// Flutter `removeHost` (tab close guards are the shell's).
pub(crate) async fn remove_host(core: &Rc<Core>, id: &str) {
    let Some(host) = core.host_for(id) else {
        return;
    };
    if let Some(origin) = &host.origin {
        let route = format!("{origin}/{}", host.server_id());
        let mut store = core.store.borrow_mut();
        if !store.suppressed.contains(&route) {
            store.suppressed.push(route);
        }
    }
    core.hosts.borrow_mut().retain(|h| !Rc::ptr_eq(h, &host));
    host.link.borrow_mut().enabled = false;
    dispose_selector(&host);
    requests::release_host(core, &host);
    detach(core, &host);
    save(core).await;
    core.touch();
}

/// Flutter `removeConnection`.
pub(crate) async fn remove_connection(core: &Rc<Core>, id: &str, connection_id: &str) {
    let Some(host) = core.host_for(id) else {
        return;
    };
    if host.connections.borrow().len() == 1 {
        remove_host(core, id).await;
        return;
    }
    host.connections
        .borrow_mut()
        .retain(|c| c["id"] != connection_id);
    if host.config_id() == connection_id {
        let first = host.connections.borrow()[0].clone();
        update_host(core, &host, first).await;
    } else {
        let selector = host.selector.borrow().clone();
        if let Some(selector) = selector {
            selector.connections_changed(false);
        }
    }
    save(core).await;
}

/// Flutter `_activateConnection` (connection selector switch).
pub(crate) fn activate_connection(core: &Rc<Core>, host: &Rc<Host>, connection: Value) {
    if core.closing.get() || !core.contains(host) || host.selector.borrow().is_none() {
        return;
    }
    detach(core, host);
    *host.config.borrow_mut() = connection;
    attach(core, host);
    directory::refresh_host(core, host);
    store::schedule_save(core);
}

/// Merges a host that resolved to an already saved server (Flutter duplicate handling).
pub(crate) async fn merge_into(core: &Rc<Core>, host: &Rc<Host>, duplicate: &Rc<Host>, previous_id: &str) {
    {
        let connections = host.connections.borrow().clone();
        let mut target = duplicate.connections.borrow_mut();
        for config in connections {
            if !target.iter().any(|c| c["id"] == config["id"]) {
                target.push(config);
            }
        }
    }
    core.hosts.borrow_mut().retain(|h| !Rc::ptr_eq(h, host));
    dispose_selector(host);
    requests::release_internal(core, host);
    // GUI observations follow the surviving host and restart on its connection.
    for observation in core.observations.borrow_mut().values_mut() {
        if observation.host_key == host.key {
            observation.reassign(duplicate.key);
        }
    }
    detach(core, host);
    let selector = duplicate.selector.borrow().clone();
    if let Some(selector) = selector {
        selector.connections_changed(false);
    }
    core.host_event(
        &duplicate.server_id(),
        json!({"type":"host.merged","payload":{"previousHostId":previous_id,"hostId":duplicate.server_id()}}),
    );
    save(core).await;
    if duplicate.conn().is_some() {
        requests::restore_observations(core, duplicate);
    }
    directory::refresh_host(core, duplicate);
    core.touch();
}

async fn save(core: &Rc<Core>) {
    if let Err(error) = store::save_now(core).await {
        core.store.borrow_mut().error = Some(error.clone());
        core.error(None, error);
    }
}

/// Flutter `refreshHosts`: hosts another window or process saved, then every host.
pub(crate) async fn refresh_hosts(core: &Rc<Core>) {
    match store::shared_hosts(core).await {
        Ok(loaded) => {
            for row in loaded {
                let config_id = row.config["id"].as_str().unwrap_or_default().to_owned();
                if core.host_for(&config_id).is_some() || core.host_for(&row.server_id).is_some() {
                    continue;
                }
                let record = json!({
                    "serverId": row.server_id,
                    "originServerId": row.origin_server_id,
                    "config": row.config,
                    "connections": row.connections,
                });
                store::remember_saved_host(core, &config_id, record);
                let host = Host::new(
                    core.next_key(),
                    row.config,
                    row.server_id,
                    row.origin_server_id,
                    row.connections,
                    row.replica,
                );
                *host.error.borrow_mut() = row.replica_error;
                add_to_core(core, &host);
            }
        }
        Err(error) => {
            core.store.borrow_mut().error = Some(error.clone());
            core.error(None, error);
        }
    }
    let hosts: Vec<Rc<Host>> = core.hosts.borrow().clone();
    for host in hosts {
        host.enable();
        directory::refresh_host(core, &host);
    }
    core.touch();
}

/// Flutter `_discoverRoutes`: online connected hosts of an origin become routed hosts.
pub(crate) async fn discover_routes(core: &Rc<Core>, origin: &Rc<Host>, directory: Value) {
    let rows = directory["hosts"].as_array().cloned().unwrap_or_default();
    for row in rows {
        let Some(id) = row["serverId"].as_str().filter(|id| !id.is_empty()).map(str::to_owned) else {
            continue;
        };
        let route = format!("{}/{id}", origin.server_id());
        if core.host_for(&id).is_some()
            || row["enabled"] != true
            || row["status"] != "online"
            || core.store.borrow().suppressed.contains(&route)
            || !core.routing.borrow_mut().insert(route.clone())
        {
            continue;
        }
        let label = row["label"].as_str().filter(|l| !l.is_empty()).unwrap_or(&id).to_owned();
        let config = json!({
            "id": format!("{}/{id}", origin.config_id()),
            "label": label,
            "endpoint": "ws://routed.invalid/ws",
            "expectedServerId": id,
        });
        let host = Host::new(
            core.next_key(),
            config.clone(),
            id.clone(),
            Some(origin.server_id()),
            vec![config],
            None,
        );
        attach(core, &host);
        match connect(core, &host).await {
            Ok(_) if !core.closing.get() && core.host_for(&id).is_none() => {
                match store::read_replica(core, id.clone()).await {
                    Ok(Some(replica)) => host.dir.borrow_mut().apply_replica(replica),
                    Ok(None) => {}
                    Err(error) => *host.error.borrow_mut() = Some(error),
                }
                core.hosts.borrow_mut().push(host.clone());
                directory::refresh_host(core, &host);
                store::schedule_save(core);
            }
            Ok(_) => {
                host.link.borrow_mut().enabled = false;
                close_link(core, &host);
            }
            Err(error) => {
                host.link.borrow_mut().enabled = false;
                close_link(core, &host);
                *origin.error.borrow_mut() = Some(error);
            }
        }
        core.routing.borrow_mut().remove(&route);
        core.touch();
    }
}

/// Flutter `_discoverLocalDaemon` (desktop only): the local daemon's `paseo.pid` names its
/// listening socket; a saved host for that daemon gains the connection or credential path.
pub(crate) async fn discover_local_daemon(core: &Rc<Core>) {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return;
    }
    let home = core
        .config
        .daemon_home
        .clone()
        .or_else(|| std::env::var_os("PASEO_HOME").filter(|v| !v.is_empty()).map(Into::into))
        .or_else(|| dirs::home_dir().map(|home| home.join(".paseo")));
    let Some(home) = home else {
        return;
    };
    let Ok(bytes) = tokio::fs::read(home.join("paseo.pid")).await else {
        return;
    };
    let lock: Value = match serde_json::from_slice(&bytes) {
        Ok(lock) => lock,
        Err(error) => {
            core.error(
                None,
                CoreError::new(
                    "LOCAL_DAEMON_STATE",
                    format!("The local daemon state is unreadable: {error}"),
                    false,
                ),
            );
            return;
        }
    };
    let listen = lock["listen"].as_str().unwrap_or_default().to_owned();
    if listen.is_empty() || lock["pid"].as_f64().unwrap_or(0.0) <= 0.0 {
        return;
    }
    let credential = home.join("local-credential");
    let credential_path = match tokio::fs::read_to_string(&credential).await {
        Ok(token) if host_config::valid_credential(token.trim()) => {
            Some(credential.to_string_lossy().into_owned())
        }
        _ => None,
    };
    let listening = host_config::socket_identity(&listen);
    let local = |c: &Value| {
        let endpoint = c["endpoint"].as_str().unwrap_or_default();
        endpoint == listen
            || (listening.is_some() && host_config::socket_identity(endpoint) == listening)
    };
    let server_id = lock["serverId"].as_str().unwrap_or_default().to_owned();
    let existing = core
        .hosts
        .borrow()
        .iter()
        .find(|host| {
            host.connections.borrow().iter().any(&local)
                || (!server_id.is_empty()
                    && (host.server_id() == server_id
                        || host
                            .connections
                            .borrow()
                            .iter()
                            .any(|c| c["expectedServerId"] == server_id.as_str())))
        })
        .cloned();
    if let Some(host) = existing {
        let mut changed = false;
        {
            let mut connections = host.connections.borrow_mut();
            if !connections.iter().any(&local) {
                let config = host.config.borrow();
                let mut connection = json!({"id": new_host_id(), "label": config["label"], "endpoint": listen});
                if let Some(path) = &credential_path {
                    connection["localCredentialPath"] = path.clone().into();
                }
                if !server_id.is_empty() {
                    connection["expectedServerId"] = server_id.clone().into();
                }
                if config["color"].is_string() {
                    connection["color"] = config["color"].clone();
                }
                connections.push(connection);
                changed = true;
            } else if let Some(path) = &credential_path {
                for connection in connections.iter_mut() {
                    if connection["localCredentialPath"].is_null() && local(&*connection) {
                        connection["localCredentialPath"] = path.clone().into();
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            return;
        }
        let selected = {
            let config_id = host.config_id();
            host.connections
                .borrow()
                .iter()
                .find(|c| c["id"] == config_id.as_str())
                .cloned()
        };
        if let Some(selected) = selected
            && selected != *host.config.borrow() {
                detach(core, &host);
                *host.config.borrow_mut() = selected;
                attach(core, &host);
            }
        save(core).await;
        return;
    }
    // Original `upsertConnectionFromListen`: labelled with the advertised hostname.
    let hostname = lock["hostname"].as_str().unwrap_or_default().trim().to_owned();
    let mut config = json!({
        "id": new_host_id(),
        "label": if hostname.is_empty() { listen.clone() } else { hostname },
        "endpoint": listen,
    });
    if let Some(path) = credential_path {
        config["localCredentialPath"] = path.into();
    }
    let id = config["id"].as_str().unwrap_or_default().to_owned();
    let host = Host::new(core.next_key(), config.clone(), id, None, vec![config], None);
    add_to_core(core, &host);
    save(core).await;
}

/// Adds `CoreConfig::default_endpoint` when no saved connection reaches it.
async fn ensure_default_endpoint(core: &Rc<Core>) {
    let Some(endpoint) = core.config.default_endpoint.clone().filter(|e| !e.trim().is_empty()) else {
        return;
    };
    let identity = host_config::socket_identity(&endpoint);
    let present = core.hosts.borrow().iter().any(|host| {
        host.connections.borrow().iter().any(|c| {
            let saved = c["endpoint"].as_str().unwrap_or_default();
            saved == endpoint || (identity.is_some() && host_config::socket_identity(saved) == identity)
        })
    });
    if present {
        return;
    }
    let id = new_host_id();
    let config = json!({"id": id, "label": endpoint, "endpoint": endpoint});
    let host = Host::new(core.next_key(), config.clone(), id, None, vec![config], None);
    add_to_core(core, &host);
    save(core).await;
}

/// Flutter `ConnectionDialog` test: a throwaway connection that must answer hello.
pub(crate) fn probe_host(core: &Rc<Core>, request_id: RequestId, config: Value) {
    let core = core.clone();
    tokio::task::spawn_local(async move {
        let result = async {
            if !config["endpoint"].is_string() {
                return Err(crate::error::invalid_request("A host connection needs an endpoint"));
            }
            let request = OpenRequest {
                client_id: core.client_id.borrow().clone(),
                expected_server_id: config["expectedServerId"].as_str().map(str::to_owned),
                protected_known: protected_known(&core, &config),
                timeout: host_config::probe_connect_timeout(&config),
            };
            let opened =
                connection::open_direct(&config, request, core.buffers.clone(), &core.cancel).await?;
            let info = opened.client.server_info();
            let client = opened.client.clone();
            tokio::task::spawn_local(async move { client.close().await });
            Ok(info)
        }
        .await;
        core.respond("", &request_id, result);
    });
}

// ---------------------------------------------------------------------------------------------
// Startup and shutdown

/// Flutter `AppStore._initialize` + `refreshHosts`.
pub(crate) async fn initialize(core: &Rc<Core>) {
    let paths = core.store.borrow().paths.clone();
    let (mut loaded, prefs) = store::load(paths).await;
    let hosts = std::mem::take(&mut loaded.hosts);
    core.store.borrow_mut().apply_loaded(&mut loaded, prefs);
    let stored_id = core
        .store
        .borrow()
        .prefs
        .values
        .get("paseo.clientId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let client_id = match stored_id {
        Some(id) => id,
        None => {
            let id = ids::new_id();
            core.store
                .borrow_mut()
                .prefs
                .values
                .insert("paseo.clientId".into(), Value::String(id.clone()));
            if let Err(error) = store::write_prefs(core).await {
                core.error(
                    None,
                    CoreError::new(
                        "STORAGE_CLIENT_ID",
                        format!("Unable to persist client identity: {}", error.message),
                        false,
                    ),
                );
            }
            id
        }
    };
    *core.client_id.borrow_mut() = client_id;
    for row in hosts {
        let host = Host::new(
            core.next_key(),
            row.config,
            row.server_id,
            row.origin_server_id,
            row.connections,
            row.replica,
        );
        *host.error.borrow_mut() = row.replica_error;
        add_to_core(core, &host);
    }
    core.initialized.set(true);
    let storage_error = core.store.borrow().error.clone();
    if let Some(error) = storage_error {
        core.error(None, error);
    }
    core.touch();
    discover_local_daemon(core).await;
    ensure_default_endpoint(core).await;
    let hosts: Vec<Rc<Host>> = core.hosts.borrow().clone();
    for host in hosts {
        directory::refresh_host(core, &host);
    }
    core.touch();
}

/// Saves, then closes every connection. A daemon is never stopped.
pub(crate) async fn shutdown(core: &Rc<Core>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), save(core)).await;
    core.closing.set(true);
    let hosts: Vec<Rc<Host>> = core.hosts.borrow().clone();
    for host in &hosts {
        dispose_selector(host);
        directory::cancel_resync(host);
        host.link.borrow_mut().enabled = false;
        close_link(core, host);
    }
    // Give the spawned client closes a moment to send their close frames.
    tokio::time::sleep(Duration::from_millis(100)).await;
    core.cancel.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_is_exponential_and_capped() {
        assert_eq!(reconnect_delay(0), Duration::from_millis(1500));
        assert_eq!(reconnect_delay(1), Duration::from_millis(3000));
        assert_eq!(reconnect_delay(4), Duration::from_millis(24_000));
        assert_eq!(reconnect_delay(5), Duration::from_millis(30_000));
        assert_eq!(reconnect_delay(40), Duration::from_millis(30_000));
    }
}
