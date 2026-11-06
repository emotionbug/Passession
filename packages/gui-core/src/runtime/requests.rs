//! Request correlation, owned observations and inbound message routing of one admitted
//! connection (Flutter `PaseoClient._request`, `_receive`, `observe`, `_startObservation`,
//! `_releaseObservation`), plus the GUI-facing request/operation/notify commands.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use paseo_client::{
    DaemonClient,
    transport::decode_frame,
};
use paseo_protocol::binary_frames::{
    TerminalOpcode, decode_terminal_frame, decode_terminal_resize, decode_terminal_snapshot,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Core, Host, directory, hosts};
use crate::{
    CoreError, Operation, RequestId,
    connection::{Control, Liveness, Opened, RouteIn, SlotText},
    error::{invalid_request, not_connected},
    ids,
    legacy::{LegacyWorkspaces, legacy_event_filter, legacy_matches, sorted_union},
    protocol::{
        invalid_response, is_response_type, response_error, response_type_for, rpc_error,
        validate_response,
    },
};

/// Flutter `PaseoClient.request` default deadline.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Flutter `_sendQueueTimeout`: a request waits this long for a connection attempt.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) type Accept = Box<dyn FnOnce(&Value)>;

struct Pending {
    request_type: String,
    response_type: String,
    fields: Value,
    reply: oneshot::Sender<Result<Value, CoreError>>,
    accept: Option<Accept>,
}

/// One admitted connection generation of a host.
pub(crate) struct Conn {
    pub generation: u64,
    pub client: Arc<DaemonClient>,
    pub control: mpsc::UnboundedSender<Control>,
    pub liveness: Rc<Liveness>,
    pub server_info: RefCell<Value>,
    pub legacy: RefCell<Option<LegacyWorkspaces>>,
    pending: RefCell<HashMap<String, Pending>>,
    /// Owned subscription id → observation id.
    pub subscriptions: RefCell<HashMap<String, RequestId>>,
    /// Connected-host routes carried by this connection, by route id.
    pub routes: RefCell<HashMap<String, mpsc::UnboundedSender<RouteIn>>>,
    /// Terminal slots bound by a core-correlated `subscribe_terminal_response`.
    terminal_slots: RefCell<HashMap<u8, SlotText>>,
    pub closed: Cell<bool>,
}

impl Conn {
    pub fn new(generation: u64, opened: &Opened) -> Rc<Self> {
        let info = opened.client.server_info();
        let legacy = (info["features"]["workspaceMultiplicity"] != true)
            .then(LegacyWorkspaces::default);
        Rc::new(Self {
            generation,
            client: opened.client.clone(),
            control: opened.control.clone(),
            liveness: opened.liveness.clone(),
            server_info: RefCell::new(info),
            legacy: RefCell::new(legacy),
            pending: RefCell::new(HashMap::new()),
            subscriptions: RefCell::new(HashMap::new()),
            routes: RefCell::new(HashMap::new()),
            terminal_slots: RefCell::new(HashMap::new()),
            closed: Cell::new(false),
        })
    }

    pub fn supports(&self, feature: &str) -> bool {
        self.server_info.borrow()["features"][feature] == true
    }

    pub fn server_id(&self) -> String {
        self.server_info.borrow()["serverId"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Ends this generation: every pending request and route learns the outcome.
    pub fn fail_all(&self, error: &CoreError) {
        self.closed.set(true);
        let pending: Vec<Pending> = self.pending.borrow_mut().drain().map(|(_, p)| p).collect();
        for pending in pending {
            let _ = pending.reply.send(Err(CoreError::new(
                "DAEMON_CONNECTION_LOST",
                "Connection lost; request outcome is unknown",
                true,
            )));
        }
        let routes: Vec<_> = self.routes.borrow_mut().drain().map(|(_, r)| r).collect();
        for route in routes {
            let _ = route.send(RouteIn::Closed(CoreError::new(
                "DAEMON_CONNECTION_LOST",
                if error.is_terminal() {
                    "Parent host connection failed"
                } else {
                    "Parent host disconnected"
                },
                true,
            )));
        }
        self.subscriptions.borrow_mut().clear();
    }

    /// Sends one session message without correlation.
    pub async fn send(&self, message: Value) -> Result<(), CoreError> {
        if self.closed.get() {
            return Err(not_connected());
        }
        self.client.send(message).await.map_err(|error| {
            CoreError::new(
                "DAEMON_CONNECTION_LOST",
                format!("Connection lost before message could be sent: {error}"),
                false,
            )
        })
    }

    /// One correlated request (Flutter `_request` on a connected client). Domain rejections
    /// are `Ok(payload)`; `accept` runs synchronously in wire order for accepted responses.
    pub async fn request(
        self: &Rc<Self>,
        message: Value,
        response_type: &str,
        timeout: Option<Duration>,
        cancel: Option<&CancellationToken>,
        accept: Option<Accept>,
    ) -> Result<Value, CoreError> {
        let kind = message["type"].as_str().unwrap_or_default().to_owned();
        if kind == "fetch_workspaces_request" && self.legacy.borrow().is_some() {
            return Box::pin(self.legacy_workspaces(message, timeout, cancel, accept)).await;
        }
        if kind == "checkout.diff.get.request" && !self.supports("ownedSubscriptions") {
            let subscription = format!("legacy:{}", ids::new_id());
            let mut legacy = message;
            legacy["type"] = "subscribe_checkout_diff_request".into();
            legacy["subscriptionId"] = subscription.clone().into();
            let result = Box::pin(self.request(
                legacy,
                "subscribe_checkout_diff_response",
                timeout,
                cancel,
                accept,
            ))
            .await;
            if !self.closed.get() {
                let _ = self
                    .send(json!({"type":"unsubscribe_checkout_diff_request","subscriptionId":subscription}))
                    .await;
            }
            return result;
        }
        self.send_request(message, kind, response_type, timeout, cancel, accept)
            .await
    }

    async fn legacy_workspaces(
        self: &Rc<Self>,
        message: Value,
        timeout: Option<Duration>,
        cancel: Option<&CancellationToken>,
        accept: Option<Accept>,
    ) -> Result<Value, CoreError> {
        let mut agents = json!({
            "type": "fetch_agents_request",
            "scope": "active",
            "sort": [{"key": "updated_at", "direction": "desc"}],
        });
        for key in ["requestId", "page", "subscribe"] {
            if !message[key].is_null() {
                agents[key] = message[key].clone();
            }
        }
        let first_page = message["page"]["cursor"].is_null();
        let converted: Rc<RefCell<Option<Value>>> = Rc::default();
        let conn = self.clone();
        let slot = converted.clone();
        let hook: Accept = Box::new(move |body| {
            let value = conn.convert_legacy(body, first_page);
            if let Some(accept) = accept {
                accept(&value);
            }
            *slot.borrow_mut() = Some(value);
        });
        let body = self
            .send_request(
                agents,
                "fetch_agents_request".into(),
                "fetch_agents_response",
                timeout,
                cancel,
                Some(hook),
            )
            .await?;
        let stored = converted.borrow_mut().take();
        Ok(stored.unwrap_or_else(|| self.convert_legacy(&body, first_page)))
    }

    fn convert_legacy(&self, body: &Value, reset: bool) -> Value {
        let entries = body["entries"].as_array().cloned().unwrap_or_default();
        let rows = self
            .legacy
            .borrow_mut()
            .as_mut()
            .map(|legacy| legacy.read(&entries, reset))
            .unwrap_or_default();
        let mut converted = body.clone();
        if converted.is_object() {
            converted["entries"] = Value::Array(rows);
            converted["emptyProjects"] = json!([]);
        }
        converted
    }

    async fn send_request(
        &self,
        mut message: Value,
        request_type: String,
        response_type: &str,
        timeout: Option<Duration>,
        cancel: Option<&CancellationToken>,
        accept: Option<Accept>,
    ) -> Result<Value, CoreError> {
        let id = match message.get("requestId") {
            Some(Value::String(id)) if !id.is_empty() => id.clone(),
            None | Some(Value::Null) => ids::new_id(),
            Some(_) => return Err(invalid_request("Request identity must be unique and nonempty")),
        };
        message["requestId"] = Value::String(id.clone());
        if self.closed.get() {
            return Err(not_connected());
        }
        let (reply, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.borrow_mut();
            if pending.contains_key(&id) {
                return Err(invalid_request("Request identity must be unique and nonempty"));
            }
            let mut fields = message.clone();
            if let Some(map) = fields.as_object_mut() {
                map.remove("type");
                map.remove("requestId");
            }
            pending.insert(
                id.clone(),
                Pending {
                    request_type: request_type.clone(),
                    response_type: response_type.to_owned(),
                    fields,
                    reply,
                    accept,
                },
            );
        }
        if let Err(error) = self.send(message).await {
            self.pending.borrow_mut().remove(&id);
            return Err(error);
        }
        let started = Instant::now();
        let deadline = async {
            match timeout {
                Some(timeout) if !timeout.is_zero() => tokio::time::sleep_until((started + timeout).into()).await,
                _ => std::future::pending::<()>().await,
            }
        };
        let cancelled = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let result = tokio::select! { biased;
            result = receiver => result.unwrap_or_else(|_| Err(CoreError::new(
                "DAEMON_CONNECTION_LOST",
                "Connection lost; request outcome is unknown",
                true,
            ))),
            _ = cancelled => Err(CoreError::new(
                "DAEMON_REQUEST_CANCELLED",
                "Daemon request cancelled; a submitted mutation may still complete",
                true,
            )),
            _ = deadline => Err(CoreError::new("DAEMON_REQUEST_TIMEOUT", "Daemon request timed out", true)),
        };
        self.pending.borrow_mut().remove(&id);
        result
    }

    fn take_matching(&self, id: &str, kind: &str) -> Option<(String, Pending)> {
        let mut pending = self.pending.borrow_mut();
        let matches = pending.get(id).is_some_and(|p| {
            kind == p.response_type || kind == "rpc_error" || is_response_type(kind)
        });
        if matches {
            pending.remove(id).map(|p| (id.to_owned(), p))
        } else {
            None
        }
    }

    fn settle(&self, id: &str, pending: Pending, message: &Value, kind: &str) {
        let has_payload = message.get("payload").is_some();
        let body = if has_payload {
            &message["payload"]
        } else {
            message
        };
        let result = if !body.is_object() || (has_payload && body["requestId"] != id) {
            Err(invalid_response("Malformed correlated response"))
        } else if kind == "rpc_error" {
            Err(rpc_error(body, &pending.request_type))
        } else if kind != pending.response_type {
            Err(invalid_response(format!(
                "Unexpected response {kind} for {}",
                pending.request_type
            )))
        } else {
            validate_response(body, &pending.request_type, &pending.response_type)
                .map(|_| body.clone())
        };
        if let Ok(body) = &result
            && response_error(body, &pending.request_type).is_none() {
                if kind == "subscribe_terminal_response"
                    && let Some(slot) = body["slot"].as_u64() {
                        self.terminal_slots
                            .borrow_mut()
                            .insert(slot as u8, SlotText::default());
                    }
                if let Some(accept) = pending.accept {
                    accept(body);
                }
            }
        let _ = pending.reply.send(result);
    }

    /// Legacy hosts answer `subscribe_terminals_request` with an uncorrelated broadcast.
    fn correlate_legacy_terminals(&self, message: &Value) {
        let cwd = &message["payload"]["cwd"];
        let ids: Vec<String> = self
            .pending
            .borrow()
            .iter()
            .filter(|(_, p)| p.request_type == "subscribe_terminals_request" && p.fields["cwd"] == *cwd)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let mut correlated = message.clone();
            correlated["payload"]["requestId"] = Value::String(id.clone());
            if let Some((id, pending)) = self.take_matching(&id, "terminals_changed") {
                self.settle(&id, pending, &correlated, "terminals_changed");
            }
        }
    }
}

/// Routes one inbound session message of `conn` (Flutter `_receive`).
pub(crate) fn handle_message(core: &Rc<Core>, host: &Rc<Host>, conn: &Rc<Conn>, message: Value) {
    if conn.closed.get() {
        return;
    }
    let message = {
        let legacy = conn.legacy.borrow();
        match legacy.as_ref() {
            Some(legacy) => legacy.normalize(message),
            None => message,
        }
    };
    let kind = message["type"].as_str().unwrap_or_default().to_owned();
    if kind == "terminals_changed"
        && !conn.supports("ownedSubscriptions")
        && message["payload"].is_object()
        && message["payload"]["requestId"].is_null()
    {
        conn.correlate_legacy_terminals(&message);
    }
    let body = if message.get("payload").is_some() {
        &message["payload"]
    } else {
        &message
    };
    let id = if body.is_object() {
        body["requestId"].as_str()
    } else {
        message["requestId"].as_str()
    }
    .map(str::to_owned);
    if let Some(id) = &id {
        if let Some((id, pending)) = conn.take_matching(id, &kind) {
            conn.settle(&id, pending, &message, &kind);
            // A permission resolution answers its response and is also the broadcast event.
            if kind != "agent_permission_resolved" {
                return;
            }
        } else if is_response_type(&kind) {
            // The reply to an SDK operation; that operation owns it.
            return;
        }
    }
    if kind == "status"
        && body["status"] == "server_info"
        && body["serverId"].as_str().is_some_and(|id| !id.is_empty())
    {
        *conn.server_info.borrow_mut() = body.clone();
    }
    if matches!(kind.as_str(), "hosts.route.frame" | "hosts.route.closed") {
        let route_id = body["subscriptionId"].as_str().unwrap_or_default();
        let route = conn.routes.borrow().get(route_id).cloned();
        if let Some(route) = route {
            let frame = if kind == "hosts.route.closed" {
                conn.routes.borrow_mut().remove(route_id);
                RouteIn::Closed(CoreError::new(
                    body["code"].as_str().unwrap_or("HOST_ROUTE_CLOSED"),
                    body["message"].as_str().unwrap_or("Host route closed"),
                    true,
                ))
            } else {
                match (&body["data"], &body["binary"]) {
                    (Value::String(data), Value::Bool(false)) => RouteIn::Text(data.clone()),
                    (Value::String(data), Value::Bool(true)) => {
                        use base64::Engine;
                        match base64::engine::general_purpose::STANDARD.decode(data) {
                            Ok(bytes) => RouteIn::Binary(bytes.into()),
                            Err(_) => RouteIn::Closed(invalid_response("Invalid host route frame")),
                        }
                    }
                    _ => RouteIn::Closed(invalid_response("Invalid host route frame")),
                }
            };
            let _ = route.send(frame);
            return;
        }
    }
    let derived = conn
        .legacy
        .borrow_mut()
        .as_mut()
        .map(|legacy| legacy.update(&message))
        .unwrap_or_default();
    directory::on_message(core, host, conn, &message);
    forward(core, host, conn, message);
    for update in derived {
        directory::on_message(core, host, conn, &update);
        forward(core, host, conn, update);
    }
}

fn forward(core: &Rc<Core>, host: &Rc<Host>, conn: &Rc<Conn>, mut message: Value) {
    let body = if message["payload"].is_object() {
        &message["payload"]
    } else {
        &message
    };
    let mut matched: Vec<Value> = Vec::new();
    if conn.supports("ownedSubscriptions") {
        if let Some(subscription) = body["subscriptionId"].as_str()
            && let Some(id) = conn.subscriptions.borrow().get(subscription) {
                matched.push(Value::String(id.clone()));
            }
    } else {
        let observations = core.observations.borrow();
        let mut ids: Vec<&RequestId> = observations
            .iter()
            .filter(|(_, o)| {
                o.host_key == host.key
                    && o.subscription_id.is_some()
                    && legacy_matches(&o.request_type, &o.sent_fields, &message)
            })
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        matched.extend(ids.into_iter().map(|id| Value::String(id.clone())));
    }
    if !matched.is_empty()
        && let Some(map) = message.as_object_mut() {
            map.insert("observations".into(), Value::Array(matched));
        }
    core.host_event(&host.server_id(), message);
}

/// Binary frames: connected-host routes and terminal streams bound through the core.
pub(crate) fn handle_binary(core: &Rc<Core>, host: &Rc<Host>, conn: &Rc<Conn>, bytes: Bytes) {
    if conn.closed.get() {
        return;
    }
    match bytes.first() {
        Some(0x30) => {
            let Ok((route_id, offset)) = decode_frame(&bytes) else {
                return;
            };
            let route = conn.routes.borrow().get(route_id).cloned();
            if let Some(route) = route {
                let _ = route.send(RouteIn::Binary(bytes.slice(offset..)));
            }
        }
        Some(1..=5) => {
            let Ok(frame) = decode_terminal_frame(&bytes) else {
                return;
            };
            let mut slots = conn.terminal_slots.borrow_mut();
            let Some(text) = slots.get_mut(&frame.slot) else {
                return;
            };
            let mut payload = json!({"slot": frame.slot});
            match frame.opcode {
                TerminalOpcode::Output | TerminalOpcode::Restore => {
                    payload["opcode"] = if frame.opcode == TerminalOpcode::Output {
                        "output"
                    } else {
                        "restore"
                    }
                    .into();
                    payload["text"] = text.decode(frame.payload).into();
                }
                TerminalOpcode::Snapshot => {
                    payload["opcode"] = "snapshot".into();
                    payload["snapshot"] = decode_terminal_snapshot(frame.payload).unwrap_or(Value::Null);
                }
                TerminalOpcode::Resize => {
                    let Ok(size) = decode_terminal_resize(frame.payload) else {
                        return;
                    };
                    payload["opcode"] = "resize".into();
                    payload["rows"] = size.rows.into();
                    payload["cols"] = size.cols.into();
                }
                TerminalOpcode::Input => return,
            }
            drop(slots);
            core.host_event(
                &host.server_id(),
                json!({"type":"terminal.frame","payload":payload}),
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// GUI commands

fn deadline_left(started: Instant, timeout: Option<Duration>) -> Result<Option<Duration>, CoreError> {
    match timeout {
        None => Ok(None),
        Some(timeout) => {
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                Err(CoreError::new(
                    "DAEMON_REQUEST_TIMEOUT",
                    "Daemon request timed out",
                    false,
                ))
            } else {
                Ok(Some(left))
            }
        }
    }
}

/// Waits for the host's admitted connection the way Flutter `sendWhenConnected` does.
async fn admitted(
    core: &Rc<Core>,
    host: &Rc<Host>,
    cancel: &CancellationToken,
) -> Result<Rc<Conn>, CoreError> {
    if let Some(conn) = host.conn() {
        return Ok(conn);
    }
    if !host.connecting() {
        return Err(not_connected());
    }
    let wait = hosts::connect(core, host);
    tokio::select! { biased;
        _ = cancel.cancelled() => Err(CoreError::new("DAEMON_REQUEST_CANCELLED", "Daemon request cancelled before submission", false)),
        _ = tokio::time::sleep(ADMISSION_TIMEOUT) => Err(CoreError::new("DAEMON_REQUEST_TIMEOUT", "Timed out waiting for connection to send message", false)),
        result = wait => result.map_err(|error| CoreError::new(
            "DAEMON_CONNECTION_LOST",
            format!("Connection lost before message could be sent: {}", error.message),
            false,
        )),
    }
}

pub(crate) fn gui_request(
    core: &Rc<Core>,
    host_id: String,
    request_id: RequestId,
    mut message: Value,
    response_type: String,
    timeout: Option<Duration>,
) {
    let Some(host) = core.host_for(&host_id) else {
        core.respond(&host_id, &request_id, Err(crate::error::host_unknown(&host_id)));
        return;
    };
    let Some(kind) = message["type"].as_str().map(str::to_owned) else {
        core.respond(
            &host_id,
            &request_id,
            Err(invalid_request("A request is a JSON object with a string type")),
        );
        return;
    };
    let response_type = if response_type.is_empty() {
        match response_type_for(&kind) {
            Some(response) => response,
            None => {
                core.respond(
                    &host_id,
                    &request_id,
                    Err(invalid_request(format!(
                        "An explicit responseType is required for {kind}"
                    ))),
                );
                return;
            }
        }
    } else {
        response_type
    };
    message["requestId"] = Value::String(request_id.clone());
    let token = CancellationToken::new();
    core.requests
        .borrow_mut()
        .insert(request_id.clone(), token.clone());
    let core = core.clone();
    tokio::task::spawn_local(async move {
        let started = Instant::now();
        let result = async {
            let conn = admitted(&core, &host, &token).await?;
            let left = deadline_left(started, timeout)?;
            conn.request(message, &response_type, left, Some(&token), None)
                .await
        }
        .await;
        if core.requests.borrow_mut().remove(&request_id).is_some() {
            core.respond(&host.server_id(), &request_id, result);
        }
    });
}

pub(crate) fn gui_operation(core: &Rc<Core>, host_id: String, request_id: RequestId, run: Operation) {
    let Some(host) = core.host_for(&host_id) else {
        core.respond(&host_id, &request_id, Err(crate::error::host_unknown(&host_id)));
        return;
    };
    let token = CancellationToken::new();
    core.requests
        .borrow_mut()
        .insert(request_id.clone(), token.clone());
    let core = core.clone();
    tokio::task::spawn_local(async move {
        let result = async {
            let conn = admitted(&core, &host, &token).await?;
            let future = run(conn.client.clone());
            // Dropping the future on cancellation releases what it owns (owned subscriptions).
            tokio::select! { biased;
                _ = token.cancelled() => Err(CoreError::new(
                    "DAEMON_REQUEST_CANCELLED",
                    "Operation cancelled; a submitted mutation may still complete",
                    true,
                )),
                result = future => result.map_err(|error| CoreError::from_anyhow(&error)),
            }
        }
        .await;
        if core.requests.borrow_mut().remove(&request_id).is_some() {
            core.respond(&host.server_id(), &request_id, result);
        }
    });
}

pub(crate) fn notify(core: &Rc<Core>, host_id: String, message: Value) {
    let Some(host) = core.host_for(&host_id) else {
        core.error(Some(&host_id), crate::error::host_unknown(&host_id));
        return;
    };
    if !message["type"].is_string() {
        core.error(
            Some(&host_id),
            invalid_request("A session message is a JSON object with a string type"),
        );
        return;
    }
    let Some(conn) = host.conn() else {
        core.error(Some(&host_id), not_connected());
        return;
    };
    let core = core.clone();
    tokio::task::spawn_local(async move {
        if let Err(error) = conn.send(message).await {
            core.error(Some(&host.server_id()), error);
        }
    });
}

pub(crate) fn cancel(core: &Rc<Core>, request_id: &str) {
    let token = core.requests.borrow().get(request_id).cloned();
    if let Some(token) = token {
        token.cancel();
        return;
    }
    release_observation(core, request_id);
}

// ---------------------------------------------------------------------------------------------
// Observations

pub(crate) struct Internal {
    /// Refresh attempt that installed it (Flutter `_observationLifecycle` epoch).
    pub epoch: u64,
    first: Option<oneshot::Sender<Result<Value, CoreError>>>,
}

pub(crate) struct Observation {
    pub host_key: u64,
    pub request_type: String,
    pub response_type: String,
    /// Subscribe fields as given (without `type`/`requestId`).
    fields: Value,
    /// Fields actually sent, including legacy adjustments.
    pub sent_fields: Value,
    pub subscription_id: Option<String>,
    starting: bool,
    has_snapshot: bool,
    pub internal: Option<Internal>,
}

impl Observation {
    /// Moves a GUI observation to the host a duplicate merged into; it restarts there.
    pub fn reassign(&mut self, host_key: u64) {
        self.host_key = host_key;
        self.subscription_id = None;
        self.starting = false;
    }

    fn new(host_key: u64, mut message: Value, response_type: String, internal: Option<Internal>) -> Self {
        let request_type = message["type"].as_str().unwrap_or_default().to_owned();
        if let Some(map) = message.as_object_mut() {
            map.remove("type");
            map.remove("requestId");
        }
        Self {
            host_key,
            request_type,
            response_type,
            sent_fields: message.clone(),
            fields: message,
            subscription_id: None,
            starting: false,
            has_snapshot: false,
            internal,
        }
    }
}

const DIRECTORY_REQUESTS: [&str; 3] = [
    "fetch_agents_request",
    "fetch_workspaces_request",
    "workspace.label.list.request",
];

pub(crate) fn gui_observe(
    core: &Rc<Core>,
    host_id: String,
    request_id: RequestId,
    message: Value,
    response_type: String,
) {
    let Some(host) = core.host_for(&host_id) else {
        core.respond(&host_id, &request_id, Err(crate::error::host_unknown(&host_id)));
        return;
    };
    let Some(kind) = message["type"].as_str().map(str::to_owned) else {
        core.respond(
            &host_id,
            &request_id,
            Err(invalid_request("A subscription is a JSON object with a string type")),
        );
        return;
    };
    let response_type = if response_type.is_empty() {
        match response_type_for(&kind) {
            Some(response) => response,
            None => {
                core.respond(
                    &host_id,
                    &request_id,
                    Err(invalid_request(format!(
                        "An explicit responseType is required for {kind}"
                    ))),
                );
                return;
            }
        }
    } else {
        response_type
    };
    core.observations.borrow_mut().insert(
        request_id.clone(),
        Observation::new(host.key, message, response_type, None),
    );
    let core = core.clone();
    tokio::task::spawn_local(async move {
        if host.conn().is_none()
            && let Err(error) = hosts::connect(&core, &host).await {
                if core.observations.borrow_mut().remove(&request_id).is_some() {
                    core.respond(&host.server_id(), &request_id, Err(error));
                }
                return;
            }
        start_observation(&core, &request_id, false).await;
    });
}

/// An observation owned by directory synchronization; resolves with the first snapshot.
pub(crate) async fn observe_internal(
    core: &Rc<Core>,
    host: &Rc<Host>,
    message: Value,
    response_type: &str,
    epoch: u64,
) -> Result<(RequestId, Value), CoreError> {
    let id = ids::new_id();
    let (first, receiver) = oneshot::channel();
    core.observations.borrow_mut().insert(
        id.clone(),
        Observation::new(
            host.key,
            message,
            response_type.to_owned(),
            Some(Internal {
                epoch,
                first: Some(first),
            }),
        ),
    );
    start_observation(core, &id, false).await;
    match receiver.await {
        Ok(Ok(snapshot)) => Ok((id, snapshot)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(CoreError::new(
            "DAEMON_CONNECTION_LOST",
            "Subscription released",
            true,
        )),
    }
}

fn legacy_members(core: &Core, host_key: u64, request_type: &str, key: &str) -> Vec<Value> {
    let observations = core.observations.borrow();
    sorted_union(
        observations
            .values()
            .filter(|o| o.host_key == host_key && o.request_type == request_type)
            .map(|o| &o.fields[key]),
    )
}

/// Flutter `_startObservation`; `restored` after a reconnect.
pub(crate) async fn start_observation(core: &Rc<Core>, id: &str, restored: bool) {
    let (host_key, request_type, response_type, mut fields) = {
        let mut observations = core.observations.borrow_mut();
        let Some(observation) = observations.get_mut(id) else {
            return;
        };
        if observation.starting || observation.subscription_id.is_some() {
            return;
        }
        observation.starting = true;
        (
            observation.host_key,
            observation.request_type.clone(),
            observation.response_type.clone(),
            observation.fields.clone(),
        )
    };
    let Some(host) = core.host_by_key(host_key) else {
        return;
    };
    let Some(conn) = host.conn() else {
        let mut observations = core.observations.borrow_mut();
        let internal_first = observations
            .get(id)
            .is_some_and(|o| o.internal.is_some() && !o.has_snapshot);
        if internal_first {
            // A directory refresh fails now; GUI observations start on the next admission.
            if let Some(mut internal) = observations.remove(id).and_then(|o| o.internal)
                && let Some(first) = internal.first.take() {
                    let _ = first.send(Err(not_connected()));
                }
        } else if let Some(observation) = observations.get_mut(id) {
            observation.starting = false;
        }
        return;
    };
    let legacy = !conn.supports("ownedSubscriptions");
    let local_id = format!("legacy:{}", ids::new_id());
    if DIRECTORY_REQUESTS.contains(&request_type.as_str()) && fields["subscribe"].is_null() {
        fields["subscribe"] = json!({});
    }
    if legacy {
        match request_type.as_str() {
            "fetch_agents_request" | "fetch_workspaces_request" | "workspace.label.list.request" => {
                fields["subscribe"] = json!({"subscriptionId": local_id});
            }
            "fs.file.subscribe.request" | "subscribe_checkout_diff_request" => {
                fields["subscriptionId"] = local_id.clone().into();
            }
            "agent.timeline.set_subscription.request" => {
                fields["agentIds"] = legacy_members(core, host_key, &request_type, "agentIds").into();
            }
            "session.events.set_subscription.request" => {
                fields["events"] =
                    legacy_event_filter(legacy_members(core, host_key, &request_type, "events")).into();
            }
            _ => {}
        }
    }
    if let Some(observation) = core.observations.borrow_mut().get_mut(id) {
        observation.sent_fields = fields.clone();
    }
    let broadcast = legacy
        && ((request_type == "agent.timeline.set_subscription.request"
            && !conn.supports("selectiveAgentTimeline"))
            || (request_type == "session.events.set_subscription.request"
                && !conn.supports("explicitEventSubscriptions")));
    let accept = {
        let core = core.clone();
        let conn = conn.clone();
        let host = host.clone();
        let id = id.to_owned();
        let request_type = request_type.clone();
        let sent = fields.clone();
        let local_id = local_id.clone();
        move |body: &Value| {
            let subscription = if legacy {
                Some(local_id)
            } else {
                body["subscriptionId"].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
            };
            let Some(subscription) = subscription else {
                return;
            };
            let found = match core.observations.borrow_mut().get_mut(&id) {
                Some(observation) => {
                    observation.subscription_id = Some(subscription.clone());
                    true
                }
                None => false,
            };
            if found {
                conn.subscriptions.borrow_mut().insert(subscription, id);
            } else {
                // Released while subscribing.
                let core = core.clone();
                tokio::task::spawn_local(async move {
                    release_subscription(&core, &host, &conn, &request_type, &sent, &subscription).await;
                });
            }
        }
    };
    let result = if broadcast {
        let mut snapshot = fields.clone();
        snapshot["requestId"] = local_id.clone().into();
        accept(&snapshot);
        Ok(snapshot)
    } else {
        let mut message = fields;
        message["type"] = request_type.clone().into();
        conn.request(
            message,
            &response_type,
            Some(REQUEST_TIMEOUT),
            None,
            Some(Box::new(accept)),
        )
        .await
    };
    if host.conn_generation() != Some(conn.generation) {
        // Lost meanwhile; the reconnect restores it.
        return;
    }
    let outcome = match result {
        Ok(body) => match response_error(&body, &request_type) {
            Some(error) => Err(error),
            None => Ok(body),
        },
        Err(error) => Err(error),
    };
    settle_start(core, &host, id, restored, outcome);
}

fn settle_start(core: &Rc<Core>, host: &Rc<Host>, id: &str, restored: bool, outcome: Result<Value, CoreError>) {
    enum Next {
        Respond(Result<Value, CoreError>),
        Restored(Value),
        RestoreFailed(CoreError),
        Internal(oneshot::Sender<Result<Value, CoreError>>, Result<Value, CoreError>),
        InternalRestored(u64),
        InternalFailed(CoreError),
        Nothing,
    }
    let next = {
        let mut observations = core.observations.borrow_mut();
        let Some(observation) = observations.get_mut(id) else {
            return;
        };
        observation.starting = false;
        let outcome = match outcome {
            Ok(body) => match &observation.subscription_id {
                Some(subscription) => {
                    let mut snapshot = body;
                    snapshot["subscriptionId"] = subscription.clone().into();
                    Ok(snapshot)
                }
                None => Err(invalid_response("Host omitted subscription identity")),
            },
            Err(error) => Err(error),
        };
        let first = !observation.has_snapshot;
        match (outcome, first) {
            (Ok(snapshot), true) => {
                observation.has_snapshot = true;
                match observation.internal.as_mut() {
                    Some(internal) => match internal.first.take() {
                        Some(sender) => Next::Internal(sender, Ok(snapshot)),
                        None => Next::Nothing,
                    },
                    None => Next::Respond(Ok(snapshot)),
                }
            }
            (Ok(mut snapshot), false) => match &observation.internal {
                Some(internal) if restored => Next::InternalRestored(internal.epoch),
                Some(_) => Next::Nothing,
                None => {
                    snapshot["observationId"] = id.into();
                    Next::Restored(snapshot)
                }
            },
            (Err(error), true) => {
                let removed = observations.remove(id).expect("observation present");
                match removed.internal {
                    Some(mut internal) => match internal.first.take() {
                        Some(sender) => Next::Internal(sender, Err(error)),
                        None => Next::Nothing,
                    },
                    None => Next::Respond(Err(error)),
                }
            }
            (Err(error), false) => match observation.internal {
                Some(_) => Next::InternalFailed(error),
                None => Next::RestoreFailed(error),
            },
        }
    };
    let host_id = host.server_id();
    match next {
        Next::Respond(result) => core.respond(&host_id, id, result),
        Next::Restored(snapshot) => core.host_event(
            &host_id,
            json!({"type":"subscription.restored","payload":snapshot}),
        ),
        Next::RestoreFailed(error) => core.host_event(
            &host_id,
            json!({"type":"subscription.error","payload":{"error":error.message,"code":error.code,"observationId":id}}),
        ),
        Next::Internal(sender, result) => {
            let _ = sender.send(result);
        }
        Next::InternalRestored(epoch) => directory::observation_restored(core, host, epoch),
        Next::InternalFailed(error) => directory::observation_failed(core, host, error),
        Next::Nothing => {}
    }
}

/// Restores every observation of a newly admitted connection.
pub(crate) fn restore_observations(core: &Rc<Core>, host: &Rc<Host>) {
    let mut ids: Vec<RequestId> = core
        .observations
        .borrow()
        .iter()
        .filter(|(_, o)| o.host_key == host.key)
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    for id in ids {
        let core = core.clone();
        tokio::task::spawn_local(async move { start_observation(&core, &id, true).await });
    }
}

/// Connection loss: subscription ids die with the connection; a first snapshot still owed fails.
pub(crate) fn observations_lost(core: &Rc<Core>, host: &Rc<Host>, error: &CoreError) {
    let mut failed = Vec::new();
    {
        let mut observations = core.observations.borrow_mut();
        let ids: Vec<RequestId> = observations
            .iter()
            .filter(|(_, o)| o.host_key == host.key)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let observation = observations.get_mut(&id).expect("listed");
            observation.subscription_id = None;
            observation.starting = false;
            if !observation.has_snapshot {
                let removed = observations.remove(&id).expect("listed");
                failed.push((id, removed));
            }
        }
    }
    let host_id = host.server_id();
    for (id, observation) in failed {
        match observation.internal {
            Some(mut internal) => {
                if let Some(first) = internal.first.take() {
                    let _ = first.send(Err(error.clone()));
                }
            }
            None => core.respond(&host_id, &id, Err(error.clone())),
        }
    }
}

/// Releases every directory observation of a host (Flutter `_releaseObservations`).
pub(crate) fn release_internal(core: &Rc<Core>, host: &Rc<Host>) {
    let ids: Vec<RequestId> = core
        .observations
        .borrow()
        .iter()
        .filter(|(_, o)| o.host_key == host.key && o.internal.is_some())
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        release_observation(core, &id);
    }
}

/// Releases every observation of a removed host; GUI owners get a cancellation response.
pub(crate) fn release_host(core: &Rc<Core>, host: &Rc<Host>) {
    let ids: Vec<RequestId> = core
        .observations
        .borrow()
        .iter()
        .filter(|(_, o)| o.host_key == host.key)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        release_observation(core, &id);
    }
}

/// `cancel_request` of an observation (Flutter `OwnedObservation.close`).
pub(crate) fn release_observation(core: &Rc<Core>, id: &str) {
    let Some(observation) = core.observations.borrow_mut().remove(id) else {
        return;
    };
    let Some(host) = core.host_by_key(observation.host_key) else {
        return;
    };
    if !observation.has_snapshot && observation.internal.is_none() {
        core.respond(
            &host.server_id(),
            id,
            Err(CoreError::new(
                "DAEMON_REQUEST_CANCELLED",
                "Subscription released",
                false,
            )),
        );
    }
    let Some(subscription) = observation.subscription_id else {
        return;
    };
    let Some(conn) = host.conn() else {
        return;
    };
    conn.subscriptions.borrow_mut().remove(&subscription);
    let core = core.clone();
    tokio::task::spawn_local(async move {
        release_subscription(
            &core,
            &host,
            &conn,
            &observation.request_type,
            &observation.sent_fields,
            &subscription,
        )
        .await;
    });
}

/// Flutter `_releaseObservation` on the wire.
async fn release_subscription(
    core: &Rc<Core>,
    host: &Rc<Host>,
    conn: &Rc<Conn>,
    request_type: &str,
    fields: &Value,
    subscription: &str,
) {
    if conn.closed.get() {
        return;
    }
    if conn.supports("ownedSubscriptions") {
        let _ = conn
            .request(
                json!({"type":"subscription.release.request","subscriptionId":subscription}),
                "subscription.release.response",
                Some(REQUEST_TIMEOUT),
                None,
                None,
            )
            .await;
        return;
    }
    let others = |key: &str| -> bool {
        core.observations.borrow().values().any(|o| {
            o.host_key == host.key && o.request_type == request_type && o.fields[key] == fields[key]
        })
    };
    let _ = match request_type {
        "fs.file.subscribe.request" => conn
            .request(
                json!({"type":"fs.file.unsubscribe.request","subscriptionId":subscription}),
                "fs.file.unsubscribe.response",
                Some(REQUEST_TIMEOUT),
                None,
                None,
            )
            .await
            .map(|_| ()),
        "subscribe_checkout_diff_request" => {
            conn.send(json!({"type":"unsubscribe_checkout_diff_request","subscriptionId":subscription}))
                .await
        }
        "agent.timeline.set_subscription.request" if conn.supports("selectiveAgentTimeline") => conn
            .request(
                json!({"type":request_type,"agentIds":legacy_members(core, host.key, request_type, "agentIds")}),
                "agent.timeline.set_subscription.response",
                Some(REQUEST_TIMEOUT),
                None,
                None,
            )
            .await
            .map(|_| ()),
        "session.events.set_subscription.request" if conn.supports("explicitEventSubscriptions") => conn
            .request(
                json!({"type":request_type,"events":legacy_event_filter(legacy_members(core, host.key, request_type, "events"))}),
                "session.events.set_subscription.response",
                Some(REQUEST_TIMEOUT),
                None,
                None,
            )
            .await
            .map(|_| ()),
        "subscribe_terminal_request" if !others("terminalId") => {
            conn.send(json!({"type":"unsubscribe_terminal_request","terminalId":fields["terminalId"]}))
                .await
        }
        "subscribe_terminals_request"
            if !core.observations.borrow().values().any(|o| {
                o.host_key == host.key
                    && o.request_type == request_type
                    && o.fields["cwd"] == fields["cwd"]
                    && o.fields["workspaceId"] == fields["workspaceId"]
            }) =>
        {
            let mut message = fields.clone();
            message["type"] = "unsubscribe_terminals_request".into();
            conn.send(message).await
        }
        _ => Ok(()),
    };
}
