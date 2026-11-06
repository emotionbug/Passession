//! One admitted daemon connection: the SDK transport (`transport::connector::Connection` for
//! direct/relay/IPC/SSH endpoints, or a connected-host route over a parent connection) feeding
//! a `paseo_client::DaemonClient` through an owned pump.
//!
//! The pump is the Flutter client boundary (`PaseoClient._receive` and `_AdmittedTransport`):
//! - the SDK hello is rewritten to the Flutter client identity: `clientType: "mobile"`, the
//!   Flutter capability set and the hello credential;
//! - invalid inbound frames are dropped instead of tearing the connection down; a malformed
//!   host-route binary frame resets it;
//! - the first `server_info` is checked against the expected server identity and the relay
//!   security floor before the client is admitted;
//! - every accepted frame is also delivered, in wire order, to the core (`Inbound`), which owns
//!   request correlation, observations and directory synchronization;
//! - envelope `ping`/`pong` drive the liveness heartbeat.
use std::{
    cell::Cell,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use paseo_client::{
    DaemonClient,
    crypto::SecurityMode,
    transport::{
        HostFrame,
        connector::{Connection, Sender as RawSender},
        decode_frame, encode_frame,
    },
};
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{CoreError, host_config};

/// Hello capabilities of the Flutter client (`client.dart` hello + `AppStore._newClient`).
/// Consumers are ported from that client, so the daemon must produce the same shapes.
const CAPABILITIES: [&str; 13] = [
    "hello_rejection",
    "owned_subscriptions",
    "selective_agent_timeline",
    "explicit_event_subscriptions",
    "terminal_reflowable_snapshot",
    "all_providers",
    "provider_subagents",
    "projected_subagent_timeline",
    "saved_history_reads",
    "timeline_replacement_invalidation",
    "reasoning_merge_enum",
    "timeline_notifications",
    "workspace_setup_observation",
];

pub(crate) const CLIENT_TYPE: &str = "mobile";

/// Frames the core consumes, in wire order.
pub(crate) enum Inbound {
    Message(Value),
    Binary(Bytes),
    Closed(CoreError),
}

/// Frames a parent connection delivers to one of its host routes.
pub(crate) enum RouteIn {
    Text(String),
    Binary(Bytes),
    Closed(CoreError),
}

pub(crate) enum Control {
    /// Sends one envelope `ping`; the reply fires on the next `pong`.
    Ping(oneshot::Sender<()>),
}

/// Inbound traffic since the last unanswered heartbeat (Flutter `_livenessFailures`).
#[derive(Default)]
pub(crate) struct Liveness {
    pub failures: Cell<u32>,
}

pub(crate) struct Opened {
    pub client: Arc<DaemonClient>,
    pub inbound: mpsc::UnboundedReceiver<Inbound>,
    pub control: mpsc::UnboundedSender<Control>,
    pub liveness: Rc<Liveness>,
    pub security: Option<SecurityMode>,
}

pub(crate) struct OpenRequest {
    pub client_id: String,
    pub expected_server_id: Option<String>,
    /// The daemon key was seen protected before (persisted floor or this process).
    pub protected_known: bool,
    pub timeout: Duration,
}

/// Admission checks on the first `server_info` (Flutter `_AdmittedTransport.messages`).
struct Admission {
    expected_server_id: Option<String>,
    relay: Option<(Option<SecurityMode>, bool)>,
}

impl Admission {
    fn verify(&self, info: &Value) -> Result<(), (CoreError, &'static str)> {
        if let Some(expected) = &self.expected_server_id
            && info["serverId"].as_str() != Some(expected.as_str()) {
                return Err((
                    CoreError::new(
                        "DAEMON_IDENTITY_MISMATCH",
                        "Connected daemon identity does not match the selected host",
                        false,
                    ),
                    "Daemon identity mismatch",
                ));
            }
        if let Some((mode, protected_known)) = self.relay {
            let advertised = info["features"]["relayProtectedSecurity"] == true;
            if mode != Some(SecurityMode::Protected) && (advertised || protected_known) {
                return Err((
                    CoreError::new(
                        "RELAY_SECURITY_DOWNGRADE",
                        "Relay protection downgrade detected",
                        false,
                    ),
                    "Relay security downgrade",
                ));
            }
        }
        Ok(())
    }
}

enum Raw {
    Direct {
        sender: RawSender,
        incoming: mpsc::Receiver<anyhow::Result<HostFrame>>,
        cancel: CancellationToken,
        task: Option<tokio::task::JoinHandle<()>>,
    },
    Routed {
        parent: Arc<DaemonClient>,
        route_id: String,
        incoming: mpsc::UnboundedReceiver<RouteIn>,
        unregister: Option<Box<dyn FnOnce()>>,
    },
}

fn transport_error(error: &anyhow::Error) -> CoreError {
    let classified = CoreError::from_anyhow(error);
    if classified.code == "CONNECTION_FAILED" {
        CoreError::new("DAEMON_CONNECTION_LOST", classified.message, true)
    } else {
        classified
    }
}

fn connection_closed() -> CoreError {
    CoreError::new("DAEMON_CONNECTION_LOST", "Daemon connection closed", true)
}

impl Raw {
    async fn send(&self, frame: HostFrame) -> anyhow::Result<()> {
        match self {
            Self::Direct { sender, .. } => sender.send(frame).await,
            Self::Routed {
                parent, route_id, ..
            } => match frame {
                HostFrame::Text(data) => {
                    parent
                        .send(json!({"type":"hosts.route.frame.request","routeId":route_id,"data":data,"binary":false}))
                        .await
                }
                HostFrame::Binary(bytes) => parent.send_binary(encode_frame(route_id, &bytes)?).await,
                HostFrame::Flush(reply) => {
                    let result = parent.flush().await;
                    let failure = result.as_ref().err().map(ToString::to_string);
                    let _ = reply.send(result);
                    match failure {
                        Some(failure) => anyhow::bail!(failure),
                        None => Ok(()),
                    }
                }
                HostFrame::Close { .. } => Ok(()),
            },
        }
    }

    async fn recv(&mut self) -> Option<Result<HostFrame, CoreError>> {
        match self {
            Self::Direct { incoming, .. } => incoming
                .recv()
                .await
                .map(|frame| frame.map_err(|error| transport_error(&error))),
            Self::Routed { incoming, .. } => incoming.recv().await.map(|frame| match frame {
                RouteIn::Text(text) => Ok(HostFrame::Text(text)),
                RouteIn::Binary(bytes) => Ok(HostFrame::Binary(bytes)),
                RouteIn::Closed(error) => Err(error),
            }),
        }
    }

    async fn close(self) {
        match self {
            Self::Direct { cancel, task, .. } => {
                cancel.cancel();
                if let Some(task) = task {
                    let _ = task.await;
                }
            }
            Self::Routed {
                parent,
                route_id,
                unregister,
                ..
            } => {
                if let Some(unregister) = unregister {
                    unregister();
                }
                if !parent.cancellation().is_cancelled() {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        parent.send(json!({"type":"hosts.route.close.request","routeId":route_id})),
                    )
                    .await;
                }
            }
        }
    }
}

struct Hello {
    auth: Option<Value>,
    sent: bool,
}

impl Hello {
    fn rewrite(&mut self, text: String) -> String {
        if self.sent {
            return text;
        }
        let Ok(Value::Object(mut hello)) = serde_json::from_str::<Value>(&text) else {
            return text;
        };
        if hello.get("type").and_then(Value::as_str) != Some("hello") {
            return text;
        }
        self.sent = true;
        let capabilities: Map<String, Value> = CAPABILITIES
            .iter()
            .map(|key| ((*key).to_owned(), Value::Bool(true)))
            .collect();
        hello.insert("clientType".into(), Value::String(CLIENT_TYPE.into()));
        hello.insert("capabilities".into(), Value::Object(capabilities));
        hello.remove("auth");
        if let Some(auth) = &self.auth {
            hello.insert("auth".into(), auth.clone());
        }
        Value::Object(hello).to_string()
    }
}

const REJECTIONS: [&str; 3] = [
    "password_required",
    "incorrect_password",
    "incompatible_protocol",
];

struct Pump {
    raw: Raw,
    outgoing: mpsc::Receiver<HostFrame>,
    client_in: mpsc::Sender<anyhow::Result<HostFrame>>,
    core: mpsc::UnboundedSender<Inbound>,
    control: mpsc::UnboundedReceiver<Control>,
    cancel: CancellationToken,
    hello: Hello,
    admission: Admission,
    liveness: Rc<Liveness>,
}

impl Pump {
    async fn run(mut self) {
        let mut awaiting_info = true;
        let mut ping: Option<oneshot::Sender<()>> = None;
        let mut control_open = true;
        let result: Result<(), CoreError> = loop {
            tokio::select! { biased;
                _ = self.cancel.cancelled() => break Ok(()),
                frame = self.outgoing.recv() => {
                    let Some(frame) = frame else { break Ok(()) };
                    let frame = match frame {
                        HostFrame::Text(text) => HostFrame::Text(self.hello.rewrite(text)),
                        other => other,
                    };
                    let closing = matches!(frame, HostFrame::Close { .. });
                    if let Err(error) = self.raw.send(frame).await {
                        break Err(transport_error(&error));
                    }
                    if closing {
                        break Ok(());
                    }
                }
                control = self.control.recv(), if control_open => {
                    match control {
                        Some(Control::Ping(reply)) => {
                            // One probe at a time; a newer probe replaces an abandoned one.
                            ping = Some(reply);
                            if let Err(error) = self.raw.send(HostFrame::Text("{\"type\":\"ping\"}".into())).await {
                                break Err(transport_error(&error));
                            }
                        }
                        // The owner dropped its control handle; keep pumping without heartbeats.
                        None => control_open = false,
                    }
                }
                frame = self.raw.recv() => {
                    let frame = match frame {
                        None => break Err(connection_closed()),
                        Some(Err(error)) => break Err(error),
                        Some(Ok(frame)) => frame,
                    };
                    match frame {
                        HostFrame::Binary(bytes) => {
                            if bytes.first() == Some(&0x30) && decode_frame(&bytes).is_err() {
                                break Err(CoreError::new("DAEMON_CONNECTION_LOST", "Invalid host routing frame", true));
                            }
                            self.liveness.failures.set(0);
                            let _ = self.core.send(Inbound::Binary(bytes.clone()));
                            if self.client_in.send(Ok(HostFrame::Binary(bytes))).await.is_err() {
                                break Ok(());
                            }
                        }
                        HostFrame::Text(text) => {
                            // Frames that are not JSON or fail envelope validation are dropped.
                            let Ok(root) = serde_json::from_str::<Value>(&text) else { continue };
                            match root["type"].as_str() {
                                Some("pong") => {
                                    self.liveness.failures.set(0);
                                    if let Some(reply) = ping.take() {
                                        let _ = reply.send(());
                                    }
                                    continue;
                                }
                                Some("hello.rejected") => {
                                    if !root["reason"].as_str().is_some_and(|r| REJECTIONS.contains(&r)) {
                                        continue;
                                    }
                                }
                                Some("session") if root["message"]["type"].is_string() => {
                                    let message = &root["message"];
                                    if awaiting_info && message["type"] == "status" && message["payload"]["status"] == "server_info" {
                                        awaiting_info = false;
                                        if let Err((error, reason)) = self.admission.verify(&message["payload"]) {
                                            let _ = self.raw.send(HostFrame::Close { code: 4002, reason: reason.into() }).await;
                                            break Err(error);
                                        }
                                    }
                                    let _ = self.core.send(Inbound::Message(message.clone()));
                                }
                                _ => continue,
                            }
                            self.liveness.failures.set(0);
                            if self.client_in.send(Ok(HostFrame::Text(text))).await.is_err() {
                                break Ok(());
                            }
                        }
                        HostFrame::Close { .. } | HostFrame::Flush(_) => {}
                    }
                }
            }
        };
        let error = result.err();
        if let Some(error) = &error {
            let _ = self.client_in.try_send(Err(anyhow::Error::new(error.clone())));
        }
        let _ = self.core.send(Inbound::Closed(error.unwrap_or_else(connection_closed)));
        self.cancel.cancel();
        self.raw.close().await;
    }
}

async fn admit(
    raw: Raw,
    request: OpenRequest,
    auth: Option<Value>,
    relay: Option<(Option<SecurityMode>, bool)>,
    cancel: CancellationToken,
) -> Result<Opened, CoreError> {
    let (out_tx, outgoing) = mpsc::channel(256);
    let (client_in, client_rx) = mpsc::channel(256);
    let (core_tx, inbound) = mpsc::unbounded_channel();
    let (control_tx, control) = mpsc::unbounded_channel();
    let liveness = Rc::new(Liveness::default());
    let security = relay.and_then(|(mode, _)| mode);
    let pump = Pump {
        raw,
        outgoing,
        client_in,
        core: core_tx,
        control,
        cancel: cancel.clone(),
        hello: Hello { auth, sent: false },
        admission: Admission {
            expected_server_id: request.expected_server_id.clone(),
            relay,
        },
        liveness: liveness.clone(),
    };
    let task = tokio::task::spawn_local(pump.run());
    let client = DaemonClient::from_transport(
        out_tx,
        client_rx,
        cancel,
        task,
        &request.client_id,
        CLIENT_TYPE,
        request.timeout,
    )
    .await
    .map_err(|error| CoreError::from_anyhow(&error))?;
    Ok(Opened {
        client: Arc::new(client),
        inbound,
        control: control_tx,
        liveness,
        security,
    })
}

/// Opens a direct, relay, IPC or SSH connection for one saved `HostConfig`.
pub(crate) async fn open_direct(
    config: &Value,
    request: OpenRequest,
    global: Arc<Semaphore>,
    parent: &CancellationToken,
) -> Result<Opened, CoreError> {
    let deadline = request.timeout;
    let cancel = parent.child_token();
    let work = async {
        let resolved = host_config::resolve(config)?;
        if cfg!(any(target_os = "android", target_os = "ios")) && resolved.ssh {
            return Err(CoreError::new(
                "invalid_connection",
                "SSH connections are available on desktop only.",
                false,
            ));
        }
        let auth = host_config::authentication(config, &resolved).await;
        let mut target = resolved.target.clone();
        // A password travels as a bearer header too (never over a relay); a local credential
        // only inside the application hello.
        target.password = auth
            .as_ref()
            .filter(|auth| auth["kind"] == "password" && !resolved.is_relay())
            .and_then(|auth| auth["password"].as_str().map(str::to_owned));
        let minimum = if resolved.require_protected || request.protected_known {
            SecurityMode::Protected
        } else {
            SecurityMode::Legacy
        };
        let connection = Connection::connect(&target, minimum, global, cancel.clone())
            .await
            .map_err(|error| CoreError::from_anyhow(&error))?;
        let relay = resolved
            .is_relay()
            .then_some((connection.security, request.protected_known));
        let raw = Raw::Direct {
            sender: connection.sender,
            incoming: connection.incoming,
            cancel: connection.cancel,
            task: Some(connection.task),
        };
        let request = OpenRequest {
            expected_server_id: request.expected_server_id.or(resolved.expected_server_id),
            ..request
        };
        admit(raw, request, auth, relay, cancel.clone()).await
    };
    match tokio::time::timeout(deadline, work).await {
        Ok(result) => {
            if result.is_err() {
                cancel.cancel();
            }
            result
        }
        Err(_) => {
            cancel.cancel();
            Err(CoreError::new(
                "DAEMON_UNREACHABLE",
                "Daemon connection timed out",
                false,
            ))
        }
    }
}

/// Opens a connected-host route already admitted by the parent (`hosts.route.open.response`).
pub(crate) async fn open_routed(
    parent: Arc<DaemonClient>,
    route_id: String,
    incoming: mpsc::UnboundedReceiver<RouteIn>,
    unregister: Box<dyn FnOnce()>,
    request: OpenRequest,
    parent_cancel: &CancellationToken,
) -> Result<Opened, CoreError> {
    let cancel = parent_cancel.child_token();
    let raw = Raw::Routed {
        parent,
        route_id,
        incoming,
        unregister: Some(unregister),
    };
    let deadline = request.timeout;
    match tokio::time::timeout(deadline, admit(raw, request, None, None, cancel.clone())).await {
        Ok(result) => result,
        Err(_) => {
            cancel.cancel();
            Err(CoreError::new(
                "DAEMON_UNREACHABLE",
                "Timed out waiting for the connected host",
                false,
            ))
        }
    }
}

/// Per-terminal-slot incremental UTF-8 decoding of output/restore bytes.
#[derive(Default)]
pub(crate) struct SlotText {
    carry: Vec<u8>,
}

impl SlotText {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.carry) {
            Ok(_) => self.carry.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(&self.carry).into_owned();
                self.carry.clear();
                return text;
            }
        };
        let rest = self.carry.split_off(valid);
        String::from_utf8(std::mem::replace(&mut self.carry, rest)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_is_rewritten_once_with_the_flutter_identity() {
        let mut hello = Hello {
            auth: Some(json!({"kind":"password","password":"p"})),
            sent: false,
        };
        let rewritten: Value = serde_json::from_str(&hello.rewrite(
            json!({"type":"hello","clientId":"cid","clientType":"cli","capabilities":{"project_updates":true},"protocolVersion":1}).to_string(),
        ))
        .unwrap();
        assert_eq!(rewritten["clientType"], "mobile");
        assert_eq!(rewritten["clientId"], "cid");
        assert_eq!(rewritten["capabilities"]["saved_history_reads"], true);
        assert!(rewritten["capabilities"].get("project_updates").is_none());
        assert_eq!(rewritten["auth"]["kind"], "password");
        let again = json!({"type":"hello"}).to_string();
        assert_eq!(hello.rewrite(again.clone()), again);
    }

    #[test]
    fn admission_rejects_identity_and_relay_downgrades() {
        let admission = Admission {
            expected_server_id: Some("srv".into()),
            relay: Some((Some(SecurityMode::Legacy), false)),
        };
        assert_eq!(
            admission.verify(&json!({"serverId":"other"})).unwrap_err().0.code,
            "DAEMON_IDENTITY_MISMATCH"
        );
        assert!(admission.verify(&json!({"serverId":"srv"})).is_ok());
        assert_eq!(
            admission
                .verify(&json!({"serverId":"srv","features":{"relayProtectedSecurity":true}}))
                .unwrap_err()
                .0
                .code,
            "RELAY_SECURITY_DOWNGRADE"
        );
    }

    #[test]
    fn terminal_text_keeps_split_utf8_sequences() {
        let mut slot = SlotText::default();
        let bytes = "가".as_bytes();
        assert_eq!(slot.decode(&bytes[..1]), "");
        assert_eq!(slot.decode(&bytes[1..]), "가");
    }
}
