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

use crate::{Command, CoreConfig, CoreError, CoreEvent, CoreState, RequestId, events::EventQueue};

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
    pub app_visible: tokio::sync::watch::Sender<bool>,
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
    published: RefCell<CoreState>,
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
            app_visible: tokio::sync::watch::channel(true).0,
            protected_keys: RefCell::new(HashSet::new()),
            routing: RefCell::new(HashSet::new()),
            initialized: Cell::new(false),
            closing: Cell::new(false),
            next_key: Cell::new(0),
            state_scheduled: Cell::new(false),
            last_state: Cell::new(None),
            published: RefCell::new(CoreState::default()),
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

    /// Only small records are rebuilt. Large sections retain their allocation until changed.
    fn state(&self) -> CoreState {
        let hosts = self.hosts.borrow();
        let records: Vec<Value> = hosts.iter().map(|host| host.state_json()).collect();
        let store = self.store.borrow();
        let mut previous = self.published.borrow_mut();
        let mut state = Map::new();
        state.insert("initialized".into(), self.initialized.get().into());
        state.insert("clientId".into(), self.client_id.borrow().clone().into());
        state.insert(
            "storage".into(),
            json!({
                "error": store.error.as_ref().map(|error| &error.message),
                "errorCode": store.error.as_ref().map(|error| &error.code),
                "writable": store.writable,
            }),
        );
        state.insert(
            "preferences".into(),
            Value::Object(store.preferences.clone()),
        );
        state.insert(
            "suppressedRoutes".into(),
            Value::Array(store.suppressed.iter().cloned().map(Value::from).collect()),
        );
        state.insert("hosts".into(), Value::Array(records));
        state.insert("tailnet".into(), self.tailnet.state());
        let old_revisions = &previous.value["revisions"];
        let mut revisions = Map::new();
        for (section, fields) in [
            ("preferences", &["preferences"][..]),
            ("suppressedRoutes", &["suppressedRoutes"][..]),
            ("storage", &["storage"][..]),
            ("tailnet", &["tailnet"][..]),
            ("session", &["initialized", "clientId"][..]),
        ] {
            let changed = fields
                .iter()
                .any(|key| previous.value.get(*key) != state.get(*key));
            revisions.insert(
                section.into(),
                if changed {
                    directory::next_revision().into()
                } else {
                    old_revisions[section].as_u64().unwrap_or(0).into()
                },
            );
        }
        let host_set_changed = !previous.value["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|host| host["directoryKey"].as_u64())
            .eq(hosts.iter().map(|host| host.key));
        revisions.insert(
            "hostSet".into(),
            if host_set_changed {
                directory::next_revision().into()
            } else {
                old_revisions["hostSet"].as_u64().unwrap_or(0).into()
            },
        );
        let prefs_changed = old_revisions["prefs"].as_object() != Some(&store.prefs_revisions);
        let layouts_changed = old_revisions["layouts"].as_u64() != Some(store.layouts_revision);
        revisions.insert("prefs".into(), Value::Object(store.prefs_revisions.clone()));
        revisions.insert("layouts".into(), store.layouts_revision.into());
        let prefs = if prefs_changed {
            Arc::new(store.prefs.values.clone())
        } else {
            previous.prefs.clone()
        };
        let layouts = if layouts_changed {
            Arc::new(store.layouts.clone())
        } else {
            previous.layouts.clone()
        };
        let same_directories = previous.directories.len() == hosts.len()
            && hosts.iter().all(|host| {
                previous
                    .directories
                    .get(&host.key)
                    .is_some_and(|snapshot| snapshot.revision == host.dir.borrow().revision())
            });
        let directories = if same_directories {
            previous.directories.clone()
        } else {
            Arc::new(
                hosts
                    .iter()
                    .map(|host| (host.key, host.dir.borrow_mut().snapshot()))
                    .collect(),
            )
        };
        state.insert("revisions".into(), Value::Object(revisions));
        let published = CoreState {
            value: Value::Object(state),
            prefs,
            layouts,
            directories,
        };
        *previous = published.clone();
        published
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
    // Shutdown is FIFO with GUI saves. Do not drop the LocalSet while a preferences
    // replacement (including its fsync/rename) or an earlier flush barrier is pending.
    if let Err(error) = store::write_prefs(&core).await {
        core.error(None, error);
    }
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
