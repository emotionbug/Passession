//! The core thread: one `Core` on a single-threaded runtime (`LocalSet`), so host, request and
//! directory state live in `RefCell`s and no lock is ever held by the GUI thread.
//!
//! Borrow discipline: never hold a `RefCell` borrow across an `.await` or across a call into
//! another component; take what you need, drop the borrow, then act.
mod directory;
mod hosts;
mod http;
mod requests;
mod selector;
mod store;
mod tailnet;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{Command, CoreConfig, CoreError, CoreEvent, RequestId, events::EventQueue};

pub(crate) use hosts::Host;
pub(crate) use requests::{Conn, Observation};
use store::Store;

pub(crate) struct Core {
    pub config: CoreConfig,
    queue: Arc<EventQueue>,
    pub store: RefCell<Store>,
    pub hosts: RefCell<Vec<Rc<Host>>>,
    /// GUI requests and operations still owed a `Response`, by request id.
    pub requests: RefCell<HashMap<RequestId, CancellationToken>>,
    pub observations: RefCell<HashMap<RequestId, Observation>>,
    pub client_id: RefCell<String>,
    /// Shared outbound buffer budget of every connection (SDK default 256 MiB).
    pub buffers: Arc<Semaphore>,
    pub cancel: CancellationToken,
    /// Relay daemon keys seen protected in this process (Flutter `_protectedIdentities`).
    pub protected_keys: RefCell<HashSet<String>>,
    /// Routes being opened (`origin/server`), Flutter `_routing`.
    pub routing: RefCell<HashSet<String>>,
    pub initialized: Cell<bool>,
    pub closing: Cell<bool>,
    next_key: Cell<u64>,
    tailnet: tailnet::Tailnet,
    state_scheduled: Cell<bool>,
    last_state: Cell<Option<Instant>>,
}

/// Minimum spacing of `State` publications; directory bursts collapse into one.
const STATE_INTERVAL: Duration = Duration::from_millis(16);

impl Core {
    fn new(config: CoreConfig, queue: Arc<EventQueue>) -> Rc<Self> {
        Rc::new(Self {
            store: RefCell::new(Store::new(&config.state_dir)),
            tailnet: tailnet::Tailnet::new(config.tailnet_key_store.clone()),
            config,
            queue,
            hosts: RefCell::new(Vec::new()),
            requests: RefCell::new(HashMap::new()),
            observations: RefCell::new(HashMap::new()),
            client_id: RefCell::new(String::new()),
            buffers: Arc::new(Semaphore::new(256 * 1024 * 1024)),
            cancel: CancellationToken::new(),
            protected_keys: RefCell::new(HashSet::new()),
            routing: RefCell::new(HashSet::new()),
            initialized: Cell::new(false),
            closing: Cell::new(false),
            next_key: Cell::new(0),
            state_scheduled: Cell::new(false),
            last_state: Cell::new(None),
        })
    }

    pub fn emit(&self, event: CoreEvent) {
        self.queue.push(event);
    }

    pub fn respond(&self, host_id: &str, request_id: &str, result: Result<Value, CoreError>) {
        self.emit(CoreEvent::Response {
            host_id: host_id.to_owned(),
            request_id: request_id.to_owned(),
            result,
        });
    }

    pub fn error(&self, host_id: Option<&str>, error: CoreError) {
        self.emit(CoreEvent::Error {
            host_id: host_id.map(str::to_owned),
            error,
        });
    }

    pub fn host_event(&self, host_id: &str, event: Value) {
        self.emit(CoreEvent::HostEvent {
            host_id: host_id.to_owned(),
            event,
        });
    }

    pub fn next_key(&self) -> u64 {
        let key = self.next_key.get() + 1;
        self.next_key.set(key);
        key
    }

    /// Flutter `AppStore.hostFor`: server id, active config id or any saved connection id.
    pub fn host_for(&self, id: &str) -> Option<Rc<Host>> {
        self.hosts
            .borrow()
            .iter()
            .find(|host| host.matches(id))
            .cloned()
    }

    pub fn host_by_key(&self, key: u64) -> Option<Rc<Host>> {
        self.hosts
            .borrow()
            .iter()
            .find(|host| host.key == key)
            .cloned()
    }

    pub fn contains(&self, host: &Rc<Host>) -> bool {
        self.hosts.borrow().iter().any(|h| Rc::ptr_eq(h, host))
    }

    /// Schedules one `State` publication after the current burst of changes.
    pub fn touch(self: &Rc<Self>) {
        if self.state_scheduled.replace(true) {
            return;
        }
        let core = self.clone();
        tokio::task::spawn_local(async move {
            tokio::task::yield_now().await;
            if let Some(last) = core.last_state.get() {
                let elapsed = last.elapsed();
                if elapsed < STATE_INTERVAL {
                    tokio::time::sleep(STATE_INTERVAL - elapsed).await;
                }
            }
            core.state_scheduled.set(false);
            core.last_state.set(Some(Instant::now()));
            let state = core.state();
            core.emit(CoreEvent::State {
                state: Arc::new(state),
            });
        });
    }

    /// The published state. Host records and store sections are moved or cloned into place:
    /// `json!` would re-serialize every row of every host once more into a new `Value`.
    fn state(&self) -> Value {
        let hosts: Vec<Value> = self.hosts.borrow().iter().map(|h| h.state_json()).collect();
        let store = self.store.borrow();
        let mut state = Map::new();
        state.insert("initialized".into(), self.initialized.get().into());
        state.insert("clientId".into(), self.client_id.borrow().clone().into());
        state.insert(
            "storage".into(),
            json!({
                "error": store.error.as_ref().map(|e| e.message.clone()),
                "errorCode": store.error.as_ref().map(|e| e.code.clone()),
                "writable": store.writable,
            }),
        );
        state.insert(
            "preferences".into(),
            Value::Object(store.preferences.clone()),
        );
        state.insert(
            "layout".into(),
            store
                .layouts
                .get(crate::storage::MAIN_WINDOW)
                .cloned()
                .unwrap_or(Value::Null),
        );
        state.insert(
            "windowLayouts".into(),
            Value::Object(
                store
                    .layouts
                    .iter()
                    .map(|(window, layout)| (window.clone(), layout.clone()))
                    .collect(),
            ),
        );
        state.insert(
            "suppressedRoutes".into(),
            Value::Array(store.suppressed.iter().cloned().map(Value::from).collect()),
        );
        state.insert("prefs".into(), Value::Object(store.prefs.values.clone()));
        state.insert("hosts".into(), Value::Array(hosts));
        state.insert("tailnet".into(), self.tailnet.state());
        Value::Object(state)
    }
}

pub(crate) async fn run(
    config: CoreConfig,
    queue: Arc<EventQueue>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let core = Core::new(config, queue);
    core.touch();
    hosts::initialize(&core).await;
    while let Some(command) = commands.recv().await {
        if matches!(command, Command::Shutdown) {
            break;
        }
        handle(&core, command).await;
    }
    hosts::shutdown(&core).await;
    tailnet::shutdown(&core).await;
}

async fn handle(core: &Rc<Core>, command: Command) {
    match command {
        Command::Tailnet {
            request_id,
            message,
        } => tailnet::request(core, request_id, message),
        Command::HostHttp {
            host_id,
            request_id,
            url,
            max_bytes,
            reply,
            cancel,
        } => http::request(core, host_id, request_id, url, max_bytes, reply, cancel),
        Command::Request {
            host_id,
            request_id,
            message,
            response_type,
            timeout,
        } => requests::gui_request(core, host_id, request_id, message, response_type, timeout),
        Command::Observe {
            host_id,
            request_id,
            message,
            response_type,
        } => requests::gui_observe(core, host_id, request_id, message, response_type),
        Command::Operation {
            host_id,
            request_id,
            run,
        } => requests::gui_operation(core, host_id, request_id, run),
        Command::Cancel { request_id } => requests::cancel(core, &request_id),
        Command::Notify { host_id, message } => requests::notify(core, host_id, message),
        Command::TerminalInput {
            host_id,
            subscription_id,
            data,
        } => requests::terminal_input(core, host_id, subscription_id, data),
        Command::ConnectHost { config } => hosts::connect_host(core, config).await,
        Command::DisconnectHost { host_id } => hosts::disconnect_host(core, &host_id).await,
        Command::RemoveHost { host_id } => hosts::remove_host(core, &host_id).await,
        Command::RemoveConnection {
            host_id,
            connection_id,
        } => hosts::remove_connection(core, &host_id, &connection_id).await,
        Command::RefreshHost { host_id } => match core.host_for(&host_id) {
            Some(host) => {
                host.enable();
                directory::refresh_host(core, &host);
            }
            None => core.error(Some(&host_id), crate::error::host_unknown(&host_id)),
        },
        Command::RefreshHosts => hosts::refresh_hosts(core).await,
        Command::DiscoverLocalDaemon => {
            hosts::discover_local_daemon(core).await;
            hosts::refresh_hosts(core).await;
        }
        Command::SetAppVisible { visible } => hosts::set_app_visible(core, visible),
        Command::ProbeHost { request_id, config } => hosts::probe_host(core, request_id, config),
        Command::SaveState { key, value, ack } => store::save_state(core, key, value, ack).await,
        Command::Shutdown => {}
    }
}
