//! Opaque WebSocket relay, wire-compatible with the v1/v2 Cloudflare adapter.
//!
//! `/ws?serverId=...&role=server|client&v=1|2&connectionId=...` is a routing
//! interface, not authentication. Daemon identity and owner admission remain at
//! the encrypted endpoints. Never put credentials into routing IDs or logs.
//! TLS terminates at the deployment ingress; [`router`] also supports embedding.

pub use paseo_protocol::crypto;

use axum::{
    Json, Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

/// Limits apply across both protocol versions. Queue overflow closes the slow
/// socket instead of blocking every session behind an unreadable connection.
#[derive(Clone, Debug)]
pub struct Config {
    pub max_sessions: usize,
    pub max_sockets: usize,
    pub max_sockets_per_session: usize,
    pub max_message_bytes: usize,
    pub queue_frames: usize,
    pub queue_bytes: usize,
    pub pending_frames: usize,
    pub pending_bytes: usize,
    pub total_buffer_bytes: usize,
    pub write_timeout: Duration,
    pub control_nudge_after: Duration,
    pub control_reset_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_sessions: 4096,
            max_sockets: 8192,
            max_sockets_per_session: 256,
            max_message_bytes: crypto::MAX_WIRE_BYTES,
            queue_frames: 256,
            queue_bytes: 64 * 1024 * 1024,
            pending_frames: 200,
            pending_bytes: 64 * 1024 * 1024,
            total_buffer_bytes: 512 * 1024 * 1024,
            write_timeout: Duration::from_secs(10),
            control_nudge_after: Duration::from_secs(10),
            control_reset_after: Duration::from_secs(5),
        }
    }
}

#[derive(Clone)]
pub struct Relay(Arc<Inner>);
struct Inner {
    config: Config,
    sessions: Mutex<Registry>,
    buffer: Arc<Semaphore>,
}
#[derive(Default)]
struct Registry {
    sessions: HashMap<SessionKey, Session>,
    sockets: usize,
    next_id: u64,
}
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct SessionKey {
    version: u8,
    server: String,
}
#[derive(Default)]
struct Session {
    sockets: HashMap<u64, Peer>,
    pending: HashMap<String, Pending>,
}
#[derive(Default)]
struct Pending {
    frames: VecDeque<Buffered>,
    bytes: usize,
}
struct Peer {
    role: Role,
    connection: String,
    tx: mpsc::Sender<Queued>,
    close: watch::Sender<Option<Closure>>,
    bytes: Arc<Semaphore>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Role {
    Server,
    Client,
}
#[derive(Clone, Copy)]
struct Closure {
    code: u16,
    reason: &'static str,
}
#[derive(Clone)]
struct Buffered {
    message: Message,
    size: usize,
    _budget: Arc<OwnedSemaphorePermit>,
}
struct Queued {
    frame: Buffered,
    _budget: OwnedSemaphorePermit,
}
struct Connection {
    key: SessionKey,
    id: u64,
    rx: mpsc::Receiver<Queued>,
    close: watch::Receiver<Option<Closure>>,
}
#[derive(Deserialize)]
struct Params {
    role: Role,
    #[serde(rename = "serverId")]
    server: String,
    #[serde(rename = "connectionId", default)]
    connection: String,
    v: Option<String>,
}

impl Relay {
    pub fn new(config: Config) -> Result<Self, &'static str> {
        if config.max_sessions == 0
            || config.max_sockets == 0
            || config.max_sockets_per_session == 0
            || config.queue_frames == 0
            || config.pending_frames == 0
            || config.pending_bytes == 0
            || config.max_message_bytes == 0
            || config.max_message_bytes > crypto::MAX_WIRE_BYTES
            || config.queue_bytes < config.max_message_bytes
            || config.total_buffer_bytes < config.queue_bytes
            || config.total_buffer_bytes > u32::MAX as usize
            || config.write_timeout.is_zero()
        {
            return Err("invalid relay resource limits");
        }
        Ok(Self(Arc::new(Inner {
            buffer: Arc::new(Semaphore::new(config.total_buffer_bytes)),
            config,
            sessions: Mutex::new(Registry::default()),
        })))
    }

    /// Useful for embedding and lifecycle monitoring; contains no routing IDs.
    pub fn counts(&self) -> (usize, usize) {
        let registry = self.0.sessions.lock().unwrap();
        (registry.sessions.len(), registry.sockets)
    }

    pub fn shutdown(&self) {
        let registry = self.0.sessions.lock().unwrap();
        for session in registry.sessions.values() {
            for peer in session.sockets.values() {
                peer.stop(1001, "Relay shutting down");
            }
        }
    }

    fn buffer(&self, message: Message) -> Option<Buffered> {
        let size = match &message {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
            _ => return None,
        };
        let permit = self
            .0
            .buffer
            .clone()
            .try_acquire_many_owned(size.max(1).try_into().ok()?)
            .ok()?;
        Some(Buffered {
            message,
            size,
            _budget: Arc::new(permit),
        })
    }

    fn notify(&self, session: &Session, value: serde_json::Value) {
        let Some(frame) = self.buffer(Message::Text(value.to_string().into())) else {
            for peer in session.sockets.values().filter(|p| p.control()) {
                peer.stop(1013, "Relay buffer full");
            }
            return;
        };
        for peer in session.sockets.values().filter(|p| p.control()) {
            peer.send(frame.clone());
        }
    }

    fn register(
        &self,
        key: SessionKey,
        role: Role,
        connection: String,
    ) -> Result<Connection, &'static str> {
        let config = &self.0.config;
        let mut registry = self.0.sessions.lock().unwrap();
        let replaces = |p: &Peer| {
            if key.version == 1 {
                p.role == role
            } else {
                role == Role::Server && p.role == role && p.connection == connection
            }
        };
        let replaced_count = registry
            .sessions
            .get(&key)
            .map_or(0, |s| s.sockets.values().filter(|p| replaces(p)).count());
        if registry.sockets - replaced_count >= config.max_sockets
            || (!registry.sessions.contains_key(&key)
                && registry.sessions.len() >= config.max_sessions)
            || registry
                .sessions
                .get(&key)
                .is_some_and(|s| s.sockets.len() - replaced_count >= config.max_sockets_per_session)
        {
            return Err("Relay connection limit reached");
        }
        registry.next_id = registry
            .next_id
            .checked_add(1)
            .ok_or("Relay identity space exhausted")?;
        let id = registry.next_id;
        registry.sockets += 1;
        let session = registry.sessions.entry(key.clone()).or_default();
        // Remove replaced peers immediately. Their eventual close callbacks must
        // not disconnect the replacement or its clients.
        session.sockets.retain(|_, peer| {
            if replaces(peer) {
                peer.stop(1008, "Replaced by new connection");
                false
            } else {
                true
            }
        });
        let (tx, rx) = mpsc::channel(config.queue_frames);
        let (close_tx, close) = watch::channel(None);
        session.sockets.insert(
            id,
            Peer {
                role,
                connection: connection.clone(),
                tx,
                close: close_tx,
                bytes: Arc::new(Semaphore::new(config.queue_bytes)),
            },
        );
        if key.version == 2 {
            if role == Role::Client {
                self.notify(
                    session,
                    json!({"type":"connected", "connectionId":connection}),
                );
            } else if connection.is_empty() {
                self.notify(
                    session,
                    json!({"type":"sync", "connectionIds":session.client_ids()}),
                );
            } else if let Some(frames) = session.pending.remove(&connection) {
                let peer = &session.sockets[&id];
                for frame in frames.frames {
                    peer.send(frame);
                }
            }
        }
        registry.sockets -= replaced_count;
        Ok(Connection { key, id, rx, close })
    }

    fn receive(&self, key: &SessionKey, id: u64, message: Message) {
        let mut registry = self.0.sessions.lock().unwrap();
        let Some(session) = registry.sessions.get_mut(key) else {
            return;
        };
        let Some(sender) = session.sockets.get(&id) else {
            return;
        };
        if key.version == 2 && sender.control() {
            if let Message::Text(text) = message {
                if serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|v| v.as_str()).map(|s| s == "ping"))
                    == Some(true)
                {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    if let Some(frame) = self.buffer(Message::Text(
                        json!({"type":"pong", "ts":ts}).to_string().into(),
                    )) {
                        sender.send(frame);
                    }
                }
            }
            return;
        }
        let Some(frame) = self.buffer(message) else {
            sender.stop(1013, "Relay buffer full");
            return;
        };
        let mut delivered = false;
        for target in session.sockets.values().filter(|p| {
            p.role != sender.role && (key.version == 1 || p.connection == sender.connection)
        }) {
            target.send(frame.clone());
            delivered = true;
        }
        if !delivered && key.version == 2 && sender.role == Role::Client {
            let pending = session
                .pending
                .entry(sender.connection.clone())
                .or_default();
            let config = &self.0.config;
            // Match the edge adapter's newest-200 buffering, adding a byte cap.
            while !pending.frames.is_empty()
                && (pending.frames.len() >= config.pending_frames
                    || pending.bytes + frame.size > config.pending_bytes)
            {
                pending.bytes -= pending.frames.pop_front().unwrap().size;
            }
            if frame.size <= config.pending_bytes {
                pending.bytes += frame.size;
                pending.frames.push_back(frame);
            } else {
                sender.stop(1009, "Pending frame exceeds buffer limit");
            }
        }
    }

    fn unregister(&self, key: &SessionKey, id: u64) {
        let mut registry = self.0.sessions.lock().unwrap();
        let Some(session) = registry.sessions.get_mut(key) else {
            return;
        };
        let Some(peer) = session.sockets.remove(&id) else {
            return;
        };
        let mut removed = 1;
        if key.version == 2 && !peer.connection.is_empty() {
            if peer.role == Role::Client && !session.has_client(&peer.connection) {
                session.pending.remove(&peer.connection);
                session.sockets.retain(|_, p| {
                    if p.role == Role::Server && p.connection == peer.connection {
                        p.stop(1001, "Client disconnected");
                        removed += 1;
                        false
                    } else {
                        true
                    }
                });
                self.notify(
                    session,
                    json!({"type":"disconnected", "connectionId":peer.connection}),
                );
            } else if peer.role == Role::Server {
                session.sockets.retain(|_, p| {
                    if p.role == Role::Client && p.connection == peer.connection {
                        p.stop(1012, "Server disconnected");
                        removed += 1;
                        false
                    } else {
                        true
                    }
                });
                session.pending.remove(&peer.connection);
                self.notify(
                    session,
                    json!({"type":"disconnected", "connectionId":peer.connection}),
                );
            }
        }
        if session.sockets.is_empty() {
            registry.sessions.remove(key);
        }
        registry.sockets -= removed;
    }

    async fn nudge(&self, key: SessionKey, id: u64) {
        tokio::time::sleep(self.0.config.control_nudge_after).await;
        for reset in [false, true] {
            {
                let registry = self.0.sessions.lock().unwrap();
                let Some(session) = registry.sessions.get(&key) else {
                    return;
                };
                let Some(client) = session.sockets.get(&id) else {
                    return;
                };
                if session
                    .sockets
                    .values()
                    .any(|p| p.role == Role::Server && p.connection == client.connection)
                {
                    return;
                }
                if reset {
                    for p in session.sockets.values().filter(|p| p.control()) {
                        p.stop(1011, "Control unresponsive");
                    }
                } else {
                    self.notify(
                        session,
                        json!({"type":"sync", "connectionIds":session.client_ids()}),
                    );
                }
            }
            if !reset {
                tokio::time::sleep(self.0.config.control_reset_after).await;
            }
        }
    }

    async fn socket(&self, socket: WebSocket, mut connection: Connection, nudge: bool) {
        let nudge_task = nudge.then(|| {
            let relay = self.clone();
            let key = connection.key.clone();
            let id = connection.id;
            tokio::spawn(async move {
                relay.nudge(key, id).await;
            })
        });
        let (mut sink, mut stream) = socket.split();
        let closure = loop {
            tokio::select! {
                _ = connection.close.changed() => {
                    break *connection.close.borrow();
                }
                message = stream.next() => match message {
                    Some(Ok(Message::Text(text))) => self.receive(&connection.key, connection.id, Message::Text(text)),
                    Some(Ok(Message::Binary(bytes))) => self.receive(&connection.key, connection.id, Message::Binary(bytes)),
                    Some(Ok(Message::Ping(bytes))) => {
                        if !matches!(tokio::time::timeout(self.0.config.write_timeout, sink.send(Message::Pong(bytes))).await, Ok(Ok(()))) { break None; }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        // Tungstenite queues the close reply while reading it.
                        // Flush that reply before dropping the transport.
                        let _ = tokio::time::timeout(self.0.config.write_timeout, sink.flush()).await;
                        break None;
                    }
                    _ => break None,
                },
                frame = connection.rx.recv() => {
                    let Some(frame) = frame else {
                        // Removing a peer drops both senders. The empty queue
                        // can win select before the close notification, but
                        // stop() has already stored the promised close frame.
                        break *connection.close.borrow();
                    };
                    if !matches!(tokio::time::timeout(self.0.config.write_timeout, sink.send(frame.frame.message)).await, Ok(Ok(()))) { break None; }
                }
            }
        };
        if let Some(close) = closure {
            let _ = tokio::time::timeout(
                self.0.config.write_timeout,
                sink.send(Message::Close(Some(CloseFrame {
                    code: close.code,
                    reason: close.reason.into(),
                }))),
            )
            .await;
        }
        if let Some(task) = nudge_task {
            task.abort();
        }
        self.unregister(&connection.key, connection.id);
    }
}

impl Peer {
    fn control(&self) -> bool {
        self.role == Role::Server && self.connection.is_empty()
    }
    fn stop(&self, code: u16, reason: &'static str) {
        self.close.send_replace(Some(Closure { code, reason }));
    }
    fn send(&self, frame: Buffered) {
        let Ok(permit) = self
            .bytes
            .clone()
            .try_acquire_many_owned(frame.size.max(1) as u32)
        else {
            self.stop(1013, "Slow consumer");
            return;
        };
        if self
            .tx
            .try_send(Queued {
                frame,
                _budget: permit,
            })
            .is_err()
        {
            self.stop(1013, "Slow consumer");
        }
    }
}
impl Session {
    fn has_client(&self, connection: &str) -> bool {
        self.sockets
            .values()
            .any(|p| p.role == Role::Client && p.connection == connection)
    }
    fn client_ids(&self) -> Vec<&str> {
        let mut ids: Vec<_> = self
            .sockets
            .values()
            .filter(|p| p.role == Role::Client)
            .map(|p| p.connection.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

pub fn router(relay: Relay) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/ws", get(upgrade))
        .with_state(relay)
}

async fn upgrade(
    State(relay): State<Relay>,
    Query(params): Query<Params>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let version = match params.v.as_deref().map(str::trim).unwrap_or("") {
        "" | "1" => 1,
        "2" => 2,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid v parameter (expected 1 or 2)",
            )
                .into_response();
        }
    };
    // Durable Object tags allowed at most 256 characters. Bound identity data
    // before retaining it; no new authentication policy is imposed by the relay.
    if params.server.is_empty() || params.server.len() > 256 || params.connection.len() > 256 {
        return (
            StatusCode::BAD_REQUEST,
            "Missing or oversized routing identity",
        )
            .into_response();
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(_) => {
            return (StatusCode::UPGRADE_REQUIRED, "Expected WebSocket upgrade").into_response();
        }
    };
    let connection_id = if version == 1 {
        String::new()
    } else {
        let id = params.connection.trim();
        if params.role == Role::Client && id.is_empty() {
            format!("conn_{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
        } else {
            id.to_owned()
        }
    };
    let key = SessionKey {
        version,
        server: params.server,
    };
    let connection = match relay.register(key, params.role, connection_id) {
        Ok(connection) => connection,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    // Failed HTTP upgrades must release registrations too.
    let failed_relay = relay.clone();
    let failed_key = connection.key.clone();
    let failed_id = connection.id;
    ws.max_message_size(relay.0.config.max_message_bytes)
        .max_frame_size(relay.0.config.max_message_bytes)
        .on_failed_upgrade(move |_| failed_relay.unregister(&failed_key, failed_id))
        .on_upgrade(move |socket| async move {
            relay
                .socket(
                    socket,
                    connection,
                    version == 2 && params.role == Role::Client,
                )
                .await
        })
}
