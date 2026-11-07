//! Non-UI runtime of the native Paseo GUI.
//!
//! [`GuiCore`] owns one dedicated core thread with a single-threaded Tokio runtime. Host
//! connections (direct, relay, IPC, SSH and connected-host routes), directory hydration,
//! persistence and request correlation run there; the GUI thread only enqueues commands through
//! [`CoreHandle`] and drains bounded [`CoreEvent`] batches with [`GuiCore::drain_events`]. It ports the Flutter
//! `core/client.dart`, the host and persistence parts of `core/app_store.dart`, and
//! `core/transport/connection_selection.dart`.
mod connection;
mod error;
mod events;
mod host_config;
mod ids;
mod legacy;
mod protocol;
mod runtime;
mod storage;

use std::{
    path::PathBuf,
    pin::Pin,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use serde_json::Value;
use tokio::sync::mpsc;

pub use error::CoreError;
pub use host_config::{HostConfigInfo, inspect_host_config};
pub use protocol::{response_error, response_type_for};

use events::EventQueue;

pub type RequestId = String;

/// Startup configuration supplied by the shell/platform layer.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// The Flutter-compatible application-support directory. The core owns
    /// `state_dir/paseo-native/` and `state_dir/shared_preferences.json`.
    pub state_dir: PathBuf,
    /// Daemon home used for desktop local-daemon discovery. `None` uses `$PASEO_HOME` or
    /// `~/.paseo` on desktop and disables discovery on Android/iOS.
    pub daemon_home: Option<PathBuf>,
    /// An endpoint that must be present as a saved host (development daemons).
    pub default_endpoint: Option<String>,
}

#[derive(Clone, Debug)]
pub enum CoreEvent {
    State {
        state: Arc<Value>,
    },
    Connected {
        host_id: String,
        server_info: Value,
    },
    Disconnected {
        host_id: String,
        error: Option<CoreError>,
    },
    HostEvent {
        host_id: String,
        event: Value,
    },
    Response {
        host_id: String,
        request_id: RequestId,
        result: Result<Value, CoreError>,
    },
    Error {
        host_id: Option<String>,
        error: CoreError,
    },
}

pub(crate) type OperationFuture = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>;
pub(crate) type Operation =
    Box<dyn FnOnce(Arc<paseo_client::DaemonClient>) -> OperationFuture + Send>;

pub(crate) enum Command {
    Request {
        host_id: String,
        request_id: RequestId,
        message: Value,
        response_type: String,
        timeout: Option<Duration>,
    },
    Observe {
        host_id: String,
        request_id: RequestId,
        message: Value,
        response_type: String,
    },
    Operation {
        host_id: String,
        request_id: RequestId,
        run: Operation,
    },
    Cancel {
        request_id: RequestId,
    },
    Notify {
        host_id: String,
        message: Value,
    },
    ConnectHost {
        config: Value,
    },
    DisconnectHost {
        host_id: String,
    },
    RemoveHost {
        host_id: String,
    },
    RemoveConnection {
        host_id: String,
        connection_id: String,
    },
    ProbeHost {
        request_id: RequestId,
        config: Value,
    },
    RefreshHost {
        host_id: String,
    },
    RefreshHosts,
    DiscoverLocalDaemon,
    SetAppVisible {
        visible: bool,
    },
    SaveState {
        key: String,
        value: Value,
        ack: Option<RequestId>,
    },
    Shutdown,
}

/// Cloneable, non-blocking command handle. Every method returns immediately.
#[derive(Clone)]
pub struct CoreHandle {
    commands: mpsc::UnboundedSender<Command>,
    state_dir: Arc<PathBuf>,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl CoreHandle {
    fn send(&self, command: Command) {
        // A closed channel means the core is shutting down; there is nobody left to answer.
        let _ = self.commands.send(command);
    }

    /// Sends one correlated request. A string `requestId` in `message` is used verbatim.
    pub fn request(&self, host_id: &str, message: Value, response_type: &str) -> RequestId {
        self.request_with_timeout(
            host_id,
            message,
            response_type,
            Some(DEFAULT_REQUEST_TIMEOUT),
        )
    }

    /// [`CoreHandle::request`] with an explicit deadline; `None` waits without a deadline.
    pub fn request_with_timeout(
        &self,
        host_id: &str,
        message: Value,
        response_type: &str,
        timeout: Option<Duration>,
    ) -> RequestId {
        let request_id = requested_id(&message);
        self.send(Command::Request {
            host_id: host_id.to_owned(),
            request_id: request_id.clone(),
            message,
            response_type: response_type.to_owned(),
            timeout,
        });
        request_id
    }

    /// Owned observation restored after every reconnect; release it with `cancel_request`.
    pub fn observe(&self, host_id: &str, message: Value, response_type: &str) -> RequestId {
        let request_id = ids::new_id();
        self.send(Command::Observe {
            host_id: host_id.to_owned(),
            request_id: request_id.clone(),
            message,
            response_type: response_type.to_owned(),
        });
        request_id
    }

    pub fn cancel_request(&self, request_id: &str) {
        self.send(Command::Cancel {
            request_id: request_id.to_owned(),
        });
    }

    /// Fire-and-forget session message without correlation.
    pub fn notify(&self, host_id: &str, message: Value) {
        self.send(Command::Notify {
            host_id: host_id.to_owned(),
            message,
        });
    }

    /// Adds a host, or replaces and activates the saved connection with the same `id`.
    pub fn connect_host(&self, config: Value) {
        self.send(Command::ConnectHost { config });
    }

    /// Closes the host's connection and stops reconnecting; the host stays saved.
    /// `connect_host` with one of its connections or `refresh_host` connects it again.
    pub fn disconnect_host(&self, host_id: &str) {
        self.send(Command::DisconnectHost {
            host_id: host_id.to_owned(),
        });
    }

    /// Removes the saved host (Flutter `removeHost`). Tab close guards are the caller's.
    pub fn remove_host(&self, host_id: &str) {
        self.send(Command::RemoveHost {
            host_id: host_id.to_owned(),
        });
    }

    /// Flutter `removeConnection`: the last connection removes the host.
    pub fn remove_connection(&self, host_id: &str, connection_id: &str) {
        self.send(Command::RemoveConnection {
            host_id: host_id.to_owned(),
            connection_id: connection_id.to_owned(),
        });
    }

    /// Tests a `HostConfig` with a throwaway connection that must answer hello (host dialog).
    /// Answered by `Response { host_id: "", Ok(server_info) | Err }`; nothing is saved.
    pub fn probe_host(&self, config: Value) -> RequestId {
        let request_id = ids::new_id();
        self.send(Command::ProbeHost {
            request_id: request_id.clone(),
            config,
        });
        request_id
    }

    /// The application-support directory the core persists into.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.as_ref().clone()
    }

    pub fn refresh_host(&self, host_id: &str) {
        self.send(Command::RefreshHost {
            host_id: host_id.to_owned(),
        });
    }

    pub fn refresh_hosts(&self) {
        self.send(Command::RefreshHosts);
    }

    pub fn discover_local_daemon(&self) {
        self.send(Command::DiscoverLocalDaemon);
    }

    pub fn set_app_visible(&self, visible: bool) {
        self.send(Command::SetAppVisible { visible });
    }

    pub fn save_state(&self, key: &str, value: Value) {
        self.send(Command::SaveState {
            key: key.to_owned(),
            value,
            ack: None,
        });
    }

    /// [`CoreHandle::save_state`] answered by a `Response { host_id: "" }` once durable.
    pub fn save_state_flushed(&self, key: &str, value: Value) -> RequestId {
        let request_id = ids::new_id();
        self.send(Command::SaveState {
            key: key.to_owned(),
            value,
            ack: Some(request_id.clone()),
        });
        request_id
    }

    /// Runs an SDK operation with the host's admitted client on the core runtime.
    pub fn operation<F, Fut>(&self, host_id: &str, f: F) -> RequestId
    where
        F: FnOnce(Arc<paseo_client::DaemonClient>) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<Value>> + Send + 'static,
    {
        let request_id = ids::new_id();
        let run: Operation = Box::new(move |client| Box::pin(f(client)));
        self.send(Command::Operation {
            host_id: host_id.to_owned(),
            request_id: request_id.clone(),
            run,
        });
        request_id
    }
}

fn requested_id(message: &Value) -> RequestId {
    match message.get("requestId") {
        Some(Value::String(id)) if !id.is_empty() => id.clone(),
        _ => ids::new_id(),
    }
}

/// The running core. Dropping it saves pending state, closes every connection (a daemon is
/// never stopped) and waits up to three seconds for the core thread.
pub struct GuiCore {
    handle: CoreHandle,
    queue: Arc<EventQueue>,
    thread: Option<std::thread::JoinHandle<()>>,
    done: std_mpsc::Receiver<()>,
}

impl GuiCore {
    pub fn start(config: CoreConfig, wake: Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<GuiCore> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .thread_name("paseo-gui-core-io")
            .build()?;
        let state_dir = config.state_dir.clone();
        let queue = Arc::new(EventQueue::new(wake));
        let (commands, receiver) = mpsc::unbounded_channel();
        let (done_tx, done) = std_mpsc::channel();
        let events = queue.clone();
        let thread = std::thread::Builder::new()
            .name("paseo-gui-core".into())
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, runtime::run(config, events, receiver));
                drop(local);
                rt.shutdown_timeout(Duration::from_millis(500));
                let _ = done_tx.send(());
            })?;
        Ok(GuiCore {
            handle: CoreHandle {
                commands,
                state_dir: Arc::new(state_dir),
            },
            queue,
            thread: Some(thread),
            done,
        })
    }

    pub fn handle(&self) -> CoreHandle {
        self.handle.clone()
    }

    /// Moves at most `limit` queued events into `output`, in arrival order.
    /// After a nonempty batch, schedule another GUI turn: the queue only emits
    /// an external wake when it transitions from empty to nonempty.
    pub fn drain_events(&mut self, output: &mut Vec<CoreEvent>, limit: usize) -> usize {
        self.queue.drain_into(output, limit)
    }
}

impl Drop for GuiCore {
    fn drop(&mut self) {
        self.handle.send(Command::Shutdown);
        if self.done.recv_timeout(Duration::from_secs(3)).is_ok()
            && let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
    }
}
