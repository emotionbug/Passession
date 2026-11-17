//! Process-local, application-only Tailscale. Blocking methods belong on a
//! blocking worker, never the GUI or async reactor. Credentials are not settings.
mod native;

/// Installs the Android OS network monitor bridge before creating a node.
/// `enabled=false` unregisters callbacks after the native node has closed.
#[cfg(target_os = "android")]
pub fn register_android_network_monitor(
    provider: fn(bool) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    native::register_android_network_monitor(provider)
}

/// Delivers a real Android interface snapshot (or explicit enumeration error).
#[cfg(target_os = "android")]
pub fn android_network_update(snapshot: &str) -> anyhow::Result<()> {
    native::Api::load()?.android_network_update(snapshot)
}

/// Selects an authenticated immutable desktop component image before any native
/// initialization. No environment variable can select a shared library.
///
/// # Safety
/// The caller must validate installation binding, archive/image digest, publisher
/// and store ownership, and call this before spawning threads. The Go runtime is
/// never unloaded or replaced inside a process.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub unsafe fn select_validated_library(path: std::path::PathBuf) -> anyhow::Result<()> {
    unsafe { native::select_validated_library(path) }
}

/// Validates the selected library's actual C ABI without creating a node.
pub fn validate_native_abi() -> anyhow::Result<()> {
    native::Api::load().map(|_| ())
}

/// Reports compiled identity from the loaded Go image (not update metadata).
pub fn native_identity() -> anyhow::Result<serde_json::Value> {
    native::Api::load()?.identity()
}

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub trait KeyStore: fmt::Debug + Send + Sync {
    fn load_or_create(&self, namespace: &Path) -> Result<[u8; 32]>;
    fn delete(&self, namespace: &Path) -> Result<()>;
}

/// Takes the state lease without waiting; `WouldBlock` means another holder.
/// Rust 1.93's `File::try_lock` is unsupported on Android, so Unix uses the
/// same `flock` primitive directly. Closing the file releases it.
#[cfg(unix)]
fn try_lock_state(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the borrowed File keeps this descriptor open for the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_state(file: &File) -> std::io::Result<()> {
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock.into(),
        std::fs::TryLockError::Error(error) => error,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub state_dir: PathBuf,
    pub hostname: String,
    pub control_url: Option<String>,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.state_dir.is_absolute(),
            "Tailscale state directory must be absolute"
        );
        ensure!(
            !self.hostname.is_empty()
                && self.hostname.len() <= 63
                && self
                    .hostname
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                && !self.hostname.starts_with('-')
                && !self.hostname.ends_with('-'),
            "Tailscale hostname must be a DNS label (letters, numbers and interior hyphens, at most 63 characters)"
        );
        if let Some(raw) = self.control_url.as_deref().filter(|s| !s.is_empty()) {
            let url = url::Url::parse(raw).context("invalid Tailscale control URL")?;
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.host_str().is_some(),
                "control URL cannot contain credentials, query or fragment"
            );
            let loopback = match url.host() {
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                _ => false,
            };
            ensure!(
                url.scheme() == "https" || (url.scheme() == "http" && loopback),
                "control URL requires HTTPS; HTTP is allowed only for a literal loopback address"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Status {
    pub backend_state: String,
    pub account: Option<String>,
    pub tailnet: Option<String>,
    pub node_name: Option<String>,
    pub ips: Vec<String>,
    pub health: Vec<String>,
}

struct ProxyInner {
    endpoint: SocketAddr,
    username: String,
    password: Zeroizing<String>,
    generation: u64,
    cancelled: CancellationToken,
}
#[derive(Clone)]
pub struct Proxy(Arc<ProxyInner>);
impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("generation", &self.generation())
            .field("valid", &self.is_valid())
            .finish_non_exhaustive()
    }
}
impl Proxy {
    pub fn endpoint(&self) -> SocketAddr {
        self.0.endpoint
    }
    pub fn username(&self) -> &str {
        &self.0.username
    }
    pub fn password(&self) -> &str {
        &self.0.password
    }
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
    pub fn is_valid(&self) -> bool {
        !self.0.cancelled.is_cancelled()
    }
    pub async fn invalidated(&self) {
        self.0.cancelled.cancelled().await
    }
    /// An unreachable loopback route for embedders' tests; cancelling `node` closes it.
    #[cfg(feature = "test-support")]
    pub fn unconnected(node: CancellationToken) -> Self {
        Self(Arc::new(ProxyInner {
            endpoint: SocketAddr::from(([127, 0, 0, 1], 9)),
            username: String::new(),
            password: Zeroizing::new(String::new()),
            generation: 0,
            cancelled: node,
        }))
    }
}

struct Node {
    api: &'static native::Api,
    handle: u64,
    config: Config,
    generation: u64,
    cancelled: CancellationToken,
    closed: AtomicBool,
    // Lease is retained until native Start/Close have really returned, even
    // when the caller timed out. A replacement cannot race old state writes.
    lease: Mutex<Option<File>>,
}

/// Go reports this while a slow shutdown continues; calling Close again waits on the same drain.
const STILL_DRAINING: &str = "still draining";
/// How long a disconnect waits for a slow native shutdown before handing it to a drop thread.
const CLOSE_WAIT: Duration = Duration::from_secs(30);
/// How long a start waits for a drop-thread drain; below `start`'s own 20 s deadline.
const DRAIN_WAIT: Duration = Duration::from_secs(15);

/// Nodes still closing on a drop thread, which holds their lease until Go really closed them.
/// Go admits one node per process, so a start must wait for these before creating another.
struct Drains {
    count: Mutex<usize>,
    done: Condvar,
}
impl Drains {
    fn begin(&self) {
        *self.count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }
    fn end(&self) {
        *self.count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.done.notify_all();
    }
    fn wait(&self, timeout: Duration) -> Result<()> {
        let count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        let (count, _) = self
            .done
            .wait_timeout_while(count, timeout, |count| *count > 0)
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            *count == 0,
            "the previous Tailscale node is still shutting down; try again shortly"
        );
        Ok(())
    }
}
static DRAINS: Drains = Drains {
    count: Mutex::new(0),
    done: Condvar::new(),
};

impl Node {
    fn close(&self) -> Result<()> {
        self.cancelled.cancel();
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        loop {
            match self.api.close(self.handle) {
                Ok(()) => break,
                Err(error)
                    if Instant::now() < deadline && error.to_string().contains(STILL_DRAINING) => {}
                Err(error) => return Err(error),
            }
        }
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        self.cancelled.cancel();
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let lease = self
            .lease
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let api = self.api;
        let handle = self.handle;
        DRAINS.begin();
        // Never block a UI drop or unlock a directory still owned by Go.
        std::thread::spawn(move || {
            while api.close(handle).is_err() {
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(lease);
            DRAINS.end();
        });
    }
}

struct Inner {
    keys: Arc<dyn KeyStore>,
    operation: Mutex<()>,
    node: Mutex<Option<Arc<Node>>>,
    generation: AtomicU64,
}
#[derive(Clone)]
pub struct Manager(Arc<Inner>);
impl fmt::Debug for Manager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manager").finish_non_exhaustive()
    }
}
impl Manager {
    pub fn new(keys: Arc<dyn KeyStore>) -> Self {
        static SHARED: LazyLock<Mutex<Weak<Inner>>> = LazyLock::new(|| Mutex::new(Weak::new()));
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(inner) = shared.upgrade() {
            return Self(inner);
        }
        let inner = Arc::new(Inner {
            keys,
            operation: Mutex::new(()),
            node: Mutex::new(None),
            generation: AtomicU64::new(0),
        });
        *shared = Arc::downgrade(&inner);
        Self(inner)
    }

    pub fn start(&self, config: Config) -> Result<Status> {
        config.validate()?;
        let manager = self.clone();
        let generation = self.0.generation.load(Ordering::Acquire);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = send.send(manager.start_inner(config, generation));
        });
        match receive.recv_timeout(Duration::from_secs(20)) {
            Ok(result) => result,
            Err(_) => {
                let _ = self.disconnect();
                Err(anyhow::anyhow!(
                    "Tailscale startup timed out; pending work was cancelled"
                ))
            }
        }
    }

    fn start_inner(&self, mut config: Config, generation: u64) -> Result<Status> {
        config.validate()?;
        let _operation = self
            .0
            .operation
            .lock()
            .map_err(|_| anyhow::anyhow!("Tailscale operation lock failed"))?;
        ensure!(
            generation == self.0.generation.load(Ordering::Acquire),
            "Tailscale start was cancelled"
        );
        std::fs::create_dir_all(&config.state_dir).context("create Tailscale state directory")?;
        config.state_dir = config
            .state_dir
            .canonicalize()
            .context("canonicalize Tailscale state directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let existing = self
            .0
            .node
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(node) = existing {
            ensure!(
                node.config == config,
                "disconnect Tailscale before changing configuration"
            );
            return self.read_status(&node);
        }
        DRAINS.wait(DRAIN_WAIT)?;
        ensure!(
            generation == self.0.generation.load(Ordering::Acquire),
            "Tailscale start was cancelled"
        );
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.state_dir.join("runtime.lock"))?;
        match try_lock_state(&lease) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => anyhow::bail!(
                "Tailscale state is in use by another process or a node still shutting down"
            ),
            Err(error) => return Err(error).context("lock Tailscale state"),
        }
        let key = Zeroizing::new(
            self.0
                .keys
                .load_or_create(&config.state_dir)
                .context("unlock Tailscale state in the operating-system credential store")?,
        );
        let api = native::Api::load()?;
        let handle = api.new_node(&config, &key)?;
        let node = Arc::new(Node {
            api,
            handle,
            config,
            generation,
            cancelled: CancellationToken::new(),
            closed: AtomicBool::new(false),
            lease: Mutex::new(Some(lease)),
        });
        {
            let mut slot = self.0.node.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                generation == self.0.generation.load(Ordering::Acquire),
                "Tailscale start was cancelled"
            );
            *slot = Some(node.clone());
        }
        let result = node
            .api
            .call(node.handle, 1)
            .and_then(|v| serde_json::from_value(v).map_err(Into::into));
        if result.is_err() || node.cancelled.is_cancelled() {
            let mut slot = self.0.node.lock().unwrap_or_else(|e| e.into_inner());
            if slot
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &node))
            {
                slot.take();
            }
            node.cancelled.cancel();
            return result.and_then(|_| Err(anyhow::anyhow!("Tailscale start was cancelled")));
        }
        result
    }

    fn node(&self) -> Result<Arc<Node>> {
        self.0
            .node
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("embedded Tailscale is disconnected")
    }
    fn read_status(&self, node: &Node) -> Result<Status> {
        let status = serde_json::from_value(node.api.call(node.handle, 2)?)?;
        ensure!(
            !node.closed.load(Ordering::Acquire),
            "embedded Tailscale is closed"
        );
        Ok(status)
    }
    pub fn status(&self) -> Result<Status> {
        let node = self.node()?;
        self.read_status(&node)
    }
    pub fn login(&self) -> Result<String> {
        let node = self.node()?;
        let url: String = serde_json::from_value(node.api.call(node.handle, 3)?)?;
        ensure!(
            !node.cancelled.is_cancelled(),
            "Tailscale login was cancelled"
        );
        let parsed = url::Url::parse(&url).context("invalid authentication URL")?;
        let local_control = node
            .config
            .control_url
            .as_deref()
            .and_then(|s| url::Url::parse(s).ok());
        let allowed_http = parsed.scheme() == "http"
            && local_control.as_ref().is_some_and(|control| {
                control.scheme() == "http"
                    && control.host() == parsed.host()
                    && control.port_or_known_default() == parsed.port_or_known_default()
            });
        ensure!(
            parsed.scheme() == "https" || allowed_http,
            "authentication URL requires HTTPS"
        );
        ensure!(
            parsed.username().is_empty() && parsed.password().is_none() && parsed.host().is_some(),
            "invalid authentication URL authority"
        );
        Ok(url)
    }
    pub fn proxy(&self) -> Result<Proxy> {
        #[derive(Deserialize)]
        struct Credentials {
            endpoint: SocketAddr,
            username: String,
            password: String,
        }
        let node = self.node()?;
        ensure!(
            !node.cancelled.is_cancelled(),
            "embedded Tailscale is disconnected"
        );
        let credentials: Credentials = serde_json::from_value(node.api.call(node.handle, 5)?)?;
        ensure!(
            credentials.endpoint.ip().is_loopback(),
            "native proxy must bind loopback"
        );
        ensure!(
            !node.cancelled.is_cancelled(),
            "embedded Tailscale is disconnected"
        );
        Ok(Proxy(Arc::new(ProxyInner {
            endpoint: credentials.endpoint,
            username: credentials.username,
            password: Zeroizing::new(credentials.password),
            generation: node.generation,
            cancelled: node.cancelled.clone(),
        })))
    }
    /// Waits out a slow native shutdown for a bounded time. An unfinished drain keeps its
    /// lease on a drop thread, and the next `start` waits for it instead of failing.
    pub fn disconnect(&self) -> Result<()> {
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        let node = self.0.node.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(node) = node {
            node.close()?;
        }
        Ok(())
    }
    pub fn logout(&self) -> Result<()> {
        let manager = self.clone();
        let generation = self.0.generation.load(Ordering::Acquire);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = send.send(manager.logout_inner(generation));
        });
        match receive.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(_) => {
                let _ = self.disconnect();
                Err(anyhow::anyhow!(
                    "Tailscale logout timed out; disconnect applied, pending cleanup retains its state lock"
                ))
            }
        }
    }

    fn logout_inner(&self, generation: u64) -> Result<()> {
        let _operation = self
            .0
            .operation
            .lock()
            .map_err(|_| anyhow::anyhow!("Tailscale operation lock failed"))?;
        ensure!(
            generation == self.0.generation.load(Ordering::Acquire),
            "Tailscale logout was cancelled"
        );
        let node = self.node()?;
        node.cancelled.cancel();
        if let Err(error) = node.api.call(node.handle, 4) {
            {
                let mut slot = self.0.node.lock().unwrap_or_else(|e| e.into_inner());
                if slot
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &node))
                {
                    slot.take();
                }
            }
            // Upstream clears its in-memory identity even when remote logout
            // fails. Close drains it and restores the encrypted checkpoint.
            node.close()
                .context("logout failed and encrypted state recovery is still pending")?;
            return Err(error)
                .context("logout was not confirmed; local identity was retained for retry");
        }
        node.close()?;
        // Keep the exclusive lease across state and credential deletion.
        match std::fs::remove_file(node.config.state_dir.join("tailscaled.enc")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("remove logged-out Tailscale state"),
        }
        self.0.keys.delete(&node.config.state_dir)?;
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        let mut slot = self.0.node.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &node))
        {
            slot.take();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug)]
    struct LockedKeyStore;
    impl KeyStore for LockedKeyStore {
        fn load_or_create(&self, _: &Path) -> Result<[u8; 32]> {
            anyhow::bail!("credential store is locked")
        }
        fn delete(&self, _: &Path) -> Result<()> {
            anyhow::bail!("must not delete a locked credential")
        }
    }
    #[test]
    fn locked_keystore_fails_before_native_start_and_releases_lease() {
        let directory = tempfile::tempdir().unwrap();
        let manager = Manager::new(Arc::new(LockedKeyStore));
        let config = Config {
            state_dir: directory.path().into(),
            hostname: "passession-test".into(),
            control_url: Some("http://127.0.0.1:1".into()),
        };
        let error = manager.start(config).unwrap_err();
        assert!(format!("{error:#}").contains("credential store is locked"));
        assert!(!directory.path().join("tailscaled.enc").exists());
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("runtime.lock"))
            .unwrap();
        try_lock_state(&lease).unwrap();
        manager.disconnect().unwrap();
    }
    #[test]
    fn state_lease_excludes_other_handles_until_it_is_closed() {
        let directory = tempfile::tempdir().unwrap();
        let open = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.path().join("runtime.lock"))
                .unwrap()
        };
        let held = open();
        let other = open();
        try_lock_state(&held).unwrap();
        assert_eq!(
            try_lock_state(&other).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(held);
        try_lock_state(&other).unwrap();
    }
    #[test]
    fn control_url_policy() {
        let mut config = Config {
            state_dir: std::env::temp_dir().join("tailnet-policy"),
            hostname: "passession".into(),
            control_url: None,
        };
        for url in [
            "https://control.example",
            "http://127.0.0.1:1234",
            "http://[::1]:1234",
        ] {
            config.control_url = Some(url.into());
            assert!(config.validate().is_ok(), "{url}");
        }
        for url in [
            "http://control.example",
            "http://localhost:1234",
            "https://user:secret@example.com",
            "file:///tmp/state",
            "https://example.com/?token=secret",
        ] {
            config.control_url = Some(url.into());
            assert!(config.validate().is_err(), "{url}");
        }
    }
    #[tokio::test]
    async fn proxy_invalidation_is_shared_and_not_lost() {
        let token = CancellationToken::new();
        let proxy = Proxy(Arc::new(ProxyInner {
            endpoint: "127.0.0.1:1234".parse().unwrap(),
            username: "tsnet".into(),
            password: Zeroizing::new("secret-never-debug".into()),
            generation: 7,
            cancelled: token.clone(),
        }));
        let clone = proxy.clone();
        assert!(!format!("{proxy:?}").contains("secret"));
        token.cancel();
        assert!(!clone.is_valid());
        tokio::time::timeout(std::time::Duration::from_secs(1), clone.invalidated())
            .await
            .unwrap();
    }
    #[test]
    fn start_waits_for_a_background_drain_instead_of_failing() {
        let drains = Drains {
            count: Mutex::new(0),
            done: Condvar::new(),
        };
        drains.wait(Duration::ZERO).unwrap();
        drains.begin();
        assert!(drains.wait(Duration::from_millis(20)).is_err());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                drains.end();
            });
            drains.wait(Duration::from_secs(5)).unwrap();
        });
    }
}
