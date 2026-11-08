//! Managed native installations. A fixed control entry selects immutable runtime
//! trees through `installation.json`; updates run in an executor outside the
//! daemon process tree and switch runtimes only through the real launch owner.
//! The operation journal (native-update.json) belongs to
//! `daemon_admin::update_operation`; the selection here is not a second journal.
use super::{
    DaemonInstance, StopOptions, error, identity, lock, read_daemon_instance,
    same_daemon_instance, stop_daemon_instance,
};
use crate::daemon_admin::update_operation::RuntimeIdentity;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::Instant};
use tokio_util::sync::CancellationToken;

const SELECTION: &str = "installation.json";
const LEASE: &str = "installation.lock";
/// Home-side pointer to the installation root (metadata only; no runtime files).
const POINTER: &str = "native-installation.json";
/// Path the legacy (pre-managed) updater and supervisor own. New code never writes it.
const LEGACY_UPDATE_RECORD: &str = "native-update.json";
/// Increase only when the launcher contract changes; ordinary updates keep the entry.
const CONTROL_PROTOCOL: u32 = 1;
const DROP_IN: &str = "zz-paseo-managed.conf";
const DROP_IN_HEADER: &str = "# Managed by Paseo: fixed control entry.";
#[cfg(target_os = "linux")]
const EXECUTOR_UNIT_PREFIX: &str = "paseo-update-";
const EXECUTABLE: &str = if cfg!(windows) { "paseo.exe" } else { "paseo" };
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

fn unsupported(message: impl Into<String>) -> anyhow::Error {
    error("OWNER_UNSUPPORTED", message)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work).await?
}

fn read_pointer(home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(POINTER);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 64 * 1024,
        "Invalid managed installation pointer"
    );
    let value: Value = serde_json::from_slice(&fs::read(&path)?)
        .context("Corrupt managed installation pointer")?;
    ensure!(value["schemaVersion"] == 1, "Unsupported installation pointer");
    let root = PathBuf::from(
        value["root"]
            .as_str()
            .context("Installation pointer has no root")?,
    );
    ensure!(root.is_absolute(), "Installation pointer root must be absolute");
    Ok(Some(root))
}

/// Internal `__managed-probe`: proves a copied image starts from its new
/// location (loader and linked libraries resolve without its runtime tree).
pub(crate) fn probe_report() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "sourceSha": option_env!("PASEO_BUILD_SOURCE_SHA").unwrap_or(""),
        "controlProtocol": CONTROL_PROTOCOL,
    })
}

async fn probe_executable(path: &Path) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(path)
            .arg("__managed-probe")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("{} did not answer its startup probe", path.display()))?
    .with_context(|| format!("Cannot execute {}", path.display()))?;
    ensure!(
        output.status.success(),
        "{} cannot run outside its runtime tree ({}): {}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let report: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{} returned an invalid startup probe", path.display()))?;
    ensure!(
        report["controlProtocol"].as_u64() >= Some(u64::from(CONTROL_PROTOCOL)),
        "{} does not implement the managed control protocol",
        path.display()
    );
    Ok(report)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}

fn host_platform() -> Result<(&'static str, &'static str)> {
    let platform = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => bail!("Managed native installations are unsupported on this platform"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => bail!("Managed native installations are unsupported on this architecture"),
    };
    Ok((platform, arch))
}

/// Operation IDs name units and directories; accept only UUID-like tokens.
fn validate_operation_id(operation_id: &str) -> Result<()> {
    ensure!(
        !operation_id.is_empty()
            && operation_id.len() <= 64
            && operation_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid update operation ID"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Process facts
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ProcessFacts {
    parent: Option<u32>,
    session: Option<u32>,
    cmd: Vec<String>,
    environ: Vec<(String, String)>,
    cwd: Option<PathBuf>,
}

fn process_facts(pid: u32, with_environment: bool) -> Result<Option<ProcessFacts>> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    let target = [sysinfo::Pid::from_u32(pid)];
    let mut kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_cwd(UpdateKind::Always);
    if with_environment {
        kind = kind.with_environ(UpdateKind::Always);
    }
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&target), true, kind);
    let Some(process) = system.process(target[0]) else {
        return Ok(None);
    };
    let cmd = process
        .cmd()
        .iter()
        .map(|arg| {
            arg.to_str()
                .map(str::to_owned)
                .context("Process arguments are not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    let environ = process
        .environ()
        .iter()
        .filter_map(|entry| {
            let entry = entry.to_str()?;
            let (key, value) = entry.split_once('=')?;
            (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect();
    Ok(Some(ProcessFacts {
        parent: process.parent().map(|pid| pid.as_u32()),
        session: process.session_id().map(|pid| pid.as_u32()),
        cmd,
        environ,
        cwd: process.cwd().map(Path::to_owned),
    }))
}

/// Path of the image a process is executing. Linux reports the mapped inode's
/// path and marks replaced images, which never match an expected runtime.
fn image_path(pid: u32) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let path = fs::read_link(format!("/proc/{pid}/exe"))
            .with_context(|| format!("Cannot inspect process {pid} image"))?;
        if path.to_string_lossy().ends_with(" (deleted)") {
            bail!("Process {pid} executes a replaced image");
        }
        Ok(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
        let target = [sysinfo::Pid::from_u32(pid)];
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&target),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        Ok(system
            .process(target[0])
            .and_then(|process| process.exe())
            .with_context(|| format!("Cannot inspect process {pid} image"))?
            .canonicalize()?)
    }
}

/// Digest of the running image. Linux reads the mapped executable itself, so a
/// later rename or replacement of the path cannot hide what is actually running.
fn image_sha256(pid: u32) -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        sha256_file(Path::new(&format!("/proc/{pid}/exe")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        sha256_file(&image_path(pid)?)
    }
}

/// Binds image facts to one process birth so PID reuse cannot satisfy them.
fn pinned_image(pid: u32, birth: &str) -> Result<(PathBuf, String)> {
    let alive = || -> Result<()> {
        ensure!(
            identity::start_identity(pid)?.as_deref() == Some(birth),
            "Process {pid} exited during image verification"
        );
        Ok(())
    };
    alive()?;
    let path = image_path(pid)?;
    let digest = image_sha256(pid)?;
    alive()?;
    Ok((path, digest))
}

fn is_paseo_run(cmd: &[String]) -> bool {
    cmd.windows(2).any(|pair| pair[0] == "daemon" && pair[1] == "run")
        || cmd.iter().any(|arg| arg == "__launcher")
}

fn home_argument(args: &[String]) -> Option<&str> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == "--home" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--home=")
        }
    })
}

fn container_marker() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        if Path::new("/.dockerenv").exists() {
            return Some("Docker");
        }
        if Path::new("/run/.containerenv").exists() {
            return Some("Podman");
        }
        if std::env::var_os("container").is_some() {
            return Some("container manager");
        }
    }
    None
}

// ---------------------------------------------------------------------------
// systemd user manager
// ---------------------------------------------------------------------------

async fn run_tool(program: &str, args: &[String], timeout: Duration) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", identity::uid()));
    }
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow!("{program} {} timed out", args.join(" ")))?
        .with_context(|| format!("Cannot run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn systemctl(args: &[&str], timeout: Duration) -> Result<String> {
    let mut full = vec!["--user".to_owned()];
    full.extend(args.iter().map(|arg| (*arg).to_owned()));
    run_tool("systemctl", &full, timeout).await
}

/// Executable paths named by a `systemctl show -p ExecStart` value.
fn exec_start_paths(value: &str) -> Vec<String> {
    value
        .split("path=")
        .skip(1)
        .filter_map(|rest| rest.split(" ;").next())
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
        .collect()
}

fn parse_properties(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

async fn unit_properties(unit: &str, properties: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut args = vec!["show", unit];
    let flags: Vec<String> = properties.iter().map(|p| format!("--property={p}")).collect();
    args.extend(flags.iter().map(String::as_str));
    Ok(parse_properties(&systemctl(&args, TOOL_TIMEOUT).await?))
}

/// Returns (cgroup path, unit) for a process in a systemd user-manager service.
#[cfg(any(target_os = "linux", test))]
fn user_service_cgroup(cgroup_file: &str) -> Option<(String, String)> {
    let path = cgroup_file
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim()
        .to_owned();
    let unit = path.rsplit('/').next()?.to_owned();
    let in_user_manager = path
        .split('/')
        .any(|part| part.starts_with("user@") && part.ends_with(".service"));
    (in_user_manager && unit.ends_with(".service") && !unit.starts_with("user@"))
        .then_some((path, unit))
}

fn quote_systemd(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn drop_in_content(control: &Path, home: &Path) -> String {
    format!(
        "{DROP_IN_HEADER}\n# The original unit is preserved; delete this file and run `systemctl --user daemon-reload` to restore it.\n[Service]\nExecStart=\nExecStart={} __launcher --home {}\n",
        quote_systemd(&control.to_string_lossy()),
        quote_systemd(&home.to_string_lossy())
    )
}

fn drop_in_directory(unit: &str) -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("User configuration directory is unavailable")?
        .join("systemd")
        .join("user")
        .join(format!("{unit}.d")))
}

#[cfg(target_os = "linux")]
async fn user_manager_available() -> bool {
    which::which("systemd-run").is_ok()
        && systemctl(&["show-environment"], Duration::from_secs(5))
            .await
            .is_ok()
}

// ---------------------------------------------------------------------------
// Launch owner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OwnerKind {
    SystemdUser,
    Standalone,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceOwner {
    pub unit: String,
    pub control_group: String,
    pub fragment_path: PathBuf,
    pub main_pid: u32,
    pub main_start_identity: Option<String>,
    pub exec_path: PathBuf,
    /// Executable path(s) the unit's ExecStart named at discovery.
    #[serde(default)]
    pub exec_start_paths: Vec<String>,
    /// Exact argv[1..] of the service main process; the launcher reuses it.
    pub launch_args: Vec<String>,
}

/// Private relaunch facts of a detached standalone supervisor. The fixed
/// launcher replays them through `paseo daemon start`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StandaloneLaunch {
    pub environment: Vec<(String, String)>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchOwner {
    pub kind: OwnerKind,
    pub home: PathBuf,
    pub server_id: String,
    pub version: String,
    /// Empty when the running daemon does not report its source.
    #[serde(default)]
    pub source_sha: String,
    pub supervisor: DaemonInstance,
    pub executable: PathBuf,
    pub executable_sha256: String,
    #[serde(default)]
    pub service: Option<ServiceOwner>,
    #[serde(default)]
    pub launch: Option<StandaloneLaunch>,
}

impl LaunchOwner {
    /// Status-safe view: no environment, options or tokens.
    pub(crate) fn public_summary(&self) -> Value {
        json!({
            "kind": self.kind,
            "unit": self.service.as_ref().map(|service| &service.unit),
            "home": self.home,
            "serverId": self.server_id,
            "version": self.version,
            "supervisorPid": self.supervisor.pid,
            "executable": self.executable,
            "executableSha256": self.executable_sha256,
        })
    }
}

async fn authenticated_status(
    home: &Path,
    instance: &DaemonInstance,
    cancel: &CancellationToken,
) -> Result<Value> {
    let mut connect = crate::client::ConnectOptions::new(paseo_client::DaemonTarget::Instance {
        home: home.to_owned(),
    });
    connect.instance = Some(instance.clone());
    connect.timeout = Duration::from_secs(5);
    let client = crate::client::connect(connect, cancel.clone()).await?;
    let status = client.daemon_status(Duration::from_secs(5)).await;
    client.close().await;
    status
}

/// Identifies who launched the running daemon. Never mutates anything; every
/// unsupported owner is rejected here, before installation or service changes.
pub(crate) async fn discover_owner(home: &Path) -> Result<LaunchOwner> {
    let home = home.canonicalize().context("Daemon home is unavailable")?;
    let instance = read_daemon_instance(&home).await?.ok_or_else(|| {
        error(
            "DAEMON_NOT_RUNNING",
            format!("Daemon is not running for {}", home.display()),
        )
    })?;
    if instance.desktop_managed {
        return Err(unsupported(
            "This daemon is managed by Paseo Desktop. Update Paseo Desktop on the host.",
        ));
    }
    let server_id = match (&instance.listen, &instance.server_id) {
        (Some(listen), Some(server_id)) if !listen.is_empty() && !server_id.is_empty() => {
            server_id.clone()
        }
        _ => return Err(error("DAEMON_NOT_READY", "Daemon is not ready")),
    };
    if instance.uid != identity::uid() || instance.hostname != hostname::get()?.to_string_lossy()
    {
        return Err(unsupported("Daemon belongs to another user or host"));
    }
    let checked = instance.clone();
    if !blocking(move || identity::verified(&checked)).await? {
        return Err(unsupported("Supervisor identity cannot be verified"));
    }
    if let Some(container) = container_marker() {
        return Err(unsupported(format!(
            "Managed updates inside a {container} are unsupported; update the container image"
        )));
    }
    let status = authenticated_status(&home, &instance, &CancellationToken::new()).await?;
    ensure!(
        status["serverId"].as_str() == Some(server_id.as_str()),
        "Daemon status server identity does not match its lock"
    );
    let version = status["version"]
        .as_str()
        .filter(|version| !version.is_empty())
        .context("Daemon status has no version")?
        .to_owned();
    let source_sha = status["sourceSha"].as_str().unwrap_or("").to_owned();
    let pid = instance.pid;
    let birth = instance
        .process_start_identity
        .clone()
        .context("Supervisor has no birth identity")?;
    let (executable, executable_sha256, facts) = blocking(move || {
        let (path, digest) = pinned_image(pid, &birth)?;
        let facts = process_facts(pid, true)?.context("Supervisor process disappeared")?;
        Ok((path, digest, facts))
    })
    .await?;
    if executable.file_name() != Some(std::ffi::OsStr::new(EXECUTABLE)) {
        return Err(unsupported(
            "Supervisor does not run from a native runtime entry",
        ));
    }
    let service = service_owner(&home, &instance, &facts).await?;
    let kind = if service.is_some() {
        OwnerKind::SystemdUser
    } else {
        standalone_owner(&instance, &facts).await?;
        OwnerKind::Standalone
    };
    let launch = (kind == OwnerKind::Standalone).then(|| StandaloneLaunch {
        environment: facts
            .environ
            .iter()
            .filter(|(key, _)| !key.starts_with("PASEO_NATIVE_") && key != "PASEO_HOST")
            .cloned()
            .collect(),
        working_directory: facts.cwd.clone(),
    });
    Ok(LaunchOwner {
        kind,
        home,
        server_id,
        version,
        source_sha,
        supervisor: instance,
        executable,
        executable_sha256,
        service,
        launch,
    })
}

/// A systemd user service owns the daemon only when its main process is the
/// supervisor's `paseo daemon run` parent. Detached daemons merely hosted in a
/// service cgroup remain standalone.
#[cfg(target_os = "linux")]
async fn service_owner(
    home: &Path,
    instance: &DaemonInstance,
    facts: &ProcessFacts,
) -> Result<Option<ServiceOwner>> {
    let cgroup = fs::read_to_string(format!("/proc/{}/cgroup", instance.pid))?;
    let Some((control_group, unit)) = user_service_cgroup(&cgroup) else {
        return Ok(None);
    };
    let properties = unit_properties(
        &unit,
        &["MainPID", "ControlGroup", "FragmentPath", "LoadState", "ExecStart"],
    )
    .await?;
    let main_pid: u32 = properties
        .get("MainPID")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if main_pid == 0 || facts.parent != Some(main_pid) {
        return Ok(None);
    }
    if unit.starts_with(EXECUTOR_UNIT_PREFIX) {
        return Err(unsupported("Daemon runs inside an update executor unit"));
    }
    if properties.get("ControlGroup") != Some(&control_group)
        || properties.get("LoadState").map(String::as_str) != Some("loaded")
    {
        return Err(unsupported(format!(
            "systemd unit {unit} does not own the supervisor cgroup"
        )));
    }
    let fragment_path = PathBuf::from(properties.get("FragmentPath").cloned().unwrap_or_default());
    if fragment_path.as_os_str().is_empty() {
        return Err(unsupported(format!("systemd unit {unit} is transient")));
    }
    let main = process_facts(main_pid, false)?.context("Service main process disappeared")?;
    if !is_paseo_run(&main.cmd) || main.cmd.len() < 2 {
        return Err(unsupported(format!(
            "systemd unit {unit} does not launch `paseo daemon run`"
        )));
    }
    if let Some(configured) = home_argument(&main.cmd[1..]) {
        if Path::new(configured).canonicalize().ok().as_deref() != Some(home) {
            return Err(unsupported(format!(
                "systemd unit {unit} launches a different daemon home"
            )));
        }
    }
    Ok(Some(ServiceOwner {
        unit,
        control_group,
        fragment_path,
        main_pid,
        main_start_identity: identity::start_identity(main_pid)?,
        exec_path: image_path(main_pid)?,
        exec_start_paths: exec_start_paths(properties.get("ExecStart").map(String::as_str).unwrap_or("")),
        launch_args: main.cmd[1..].to_vec(),
    }))
}

#[cfg(not(target_os = "linux"))]
async fn service_owner(
    _home: &Path,
    _instance: &DaemonInstance,
    _facts: &ProcessFacts,
) -> Result<Option<ServiceOwner>> {
    Ok(None)
}

/// Standalone means a detached supervisor started by `paseo daemon start`.
async fn standalone_owner(instance: &DaemonInstance, facts: &ProcessFacts) -> Result<()> {
    if let Some(parent) = facts.parent {
        let parent_facts = blocking(move || process_facts(parent, false)).await?;
        if parent_facts.is_some_and(|parent| is_paseo_run(&parent.cmd)) {
            return Err(unsupported(
                "Daemon is owned by a foreground `paseo daemon run` outside a supported service manager",
            ));
        }
    }
    #[cfg(unix)]
    if facts.session != Some(instance.pid) {
        return Err(unsupported(
            "Supervisor is not a detached standalone session; its launch owner is unknown",
        ));
    }
    #[cfg(target_os = "macos")]
    {
        let jobs = run_tool("launchctl", &["list".to_owned()], TOOL_TIMEOUT).await?;
        let pid = instance.pid.to_string();
        if jobs
            .lines()
            .any(|line| line.split_whitespace().next() == Some(pid.as_str()))
        {
            return Err(unsupported("Daemon is a launchd job; launchd updates are unsupported"));
        }
    }
    #[cfg(not(unix))]
    let _ = (instance, facts.session);
    Ok(())
}

// ---------------------------------------------------------------------------
// Installation selection
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlEntry {
    pub path: PathBuf,
    pub sha256: String,
    pub protocol: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Trial {
    pub operation_id: String,
    pub runtime: RuntimeIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceBinding {
    pub unit: String,
    pub drop_in: PathBuf,
    pub original: PathBuf,
    pub launch_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Selection {
    pub schema_version: u32,
    pub home: PathBuf,
    pub server_id: String,
    pub control: ControlEntry,
    pub active: RuntimeIdentity,
    #[serde(default)]
    pub previous: Option<RuntimeIdentity>,
    #[serde(default)]
    pub trial: Option<Trial>,
    #[serde(default)]
    pub service: Option<ServiceBinding>,
    /// Original runtime root the first active runtime was copied from; never modified.
    #[serde(default)]
    pub adopted_from: Option<PathBuf>,
    pub updated_at: String,
}

pub(crate) struct Installation {
    root: PathBuf,
    home: PathBuf,
}

/// Exclusive installation lease. A live executor holds it for its whole run;
/// the kernel releases it on process death, which is how bootstrap recovery
/// distinguishes an orphaned trial from one under active verification.
pub(crate) struct InstallationLease {
    _file: File,
    root: PathBuf,
}

impl Installation {
    /// The home records its installation root once, so CLI, worker, transient
    /// executor units and the service launcher agree regardless of their
    /// differing XDG/user environments.
    pub(crate) fn for_home(home: &Path) -> Result<Self> {
        let home = home.canonicalize().context("Daemon home is unavailable")?;
        if let Some(root) = read_pointer(&home)? {
            return Self::at(root, home);
        }
        let base = match std::env::var_os("PASEO_INSTALLATIONS_DIR") {
            Some(base) => {
                let base = PathBuf::from(base);
                ensure!(base.is_absolute(), "PASEO_INSTALLATIONS_DIR must be absolute");
                base
            }
            None => dirs::data_local_dir()
                .context("Local data directory is unavailable")?
                .join("paseo")
                .join("installations"),
        };
        fs::create_dir_all(&base)?;
        let id = hex::encode(Sha256::digest(home.as_os_str().as_encoded_bytes()));
        Self::at(base.canonicalize()?.join(&id[..16]), home)
    }

    /// Binds the home to this installation root; refuses to rebind silently.
    fn bind_home(&self) -> Result<()> {
        match read_pointer(&self.home)? {
            Some(root) if root == self.root => return Ok(()),
            Some(root) => bail!(
                "Daemon home is bound to another installation: {}",
                root.display()
            ),
            None => {}
        }
        let bytes = serde_json::to_vec_pretty(&json!({"schemaVersion":1,"root":self.root}))?;
        let mut file = tempfile::NamedTempFile::new_in(&self.home)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(self.home.join(POINTER))
            .map_err(|e| e.error)?;
        sync_directory(&self.home)
    }

    fn at(root: PathBuf, home: PathBuf) -> Result<Self> {
        ensure!(
            !root.starts_with(&home) && !home.starts_with(&root),
            "Managed installation root must be outside the daemon home"
        );
        Ok(Self { root, home })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn runtimes(&self) -> PathBuf {
        self.root.join("runtimes")
    }

    fn ensure_root(&self) -> Result<()> {
        if let Some(parent) = self.root.parent() {
            fs::create_dir_all(parent)?;
        }
        lock::private_directory(&self.root)
    }

    fn private_child(&self, name: &str) -> Result<PathBuf> {
        self.ensure_root()?;
        let path = self.root.join(name);
        lock::private_directory(&path)?;
        Ok(path)
    }

    pub(crate) fn try_lease(&self) -> Result<Option<InstallationLease>> {
        self.ensure_root()?;
        let path = self.root.join(LEASE);
        let file = match lock::open_private(&path, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                match lock::open_private(&path, true) {
                    Ok(file) => file,
                    Err(_) => lock::open_private(&path, false)?,
                }
            }
            Err(error) => return Err(error),
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(InstallationLease {
                _file: file,
                root: self.root.clone(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn held(&self, lease: &InstallationLease) -> Result<()> {
        ensure!(
            lease.root == self.root,
            "Installation lease belongs to another installation"
        );
        Ok(())
    }

    pub(crate) fn load(&self) -> Result<Option<Selection>> {
        let path = self.root.join(SELECTION);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 1 << 20,
            "Invalid managed installation selection"
        );
        let selection: Selection = serde_json::from_slice(&fs::read(&path)?)
            .context("Corrupt managed installation selection")?;
        self.validate(&selection)?;
        Ok(Some(selection))
    }

    fn validate(&self, selection: &Selection) -> Result<()> {
        ensure!(
            selection.schema_version == 1,
            "Unsupported installation selection schema"
        );
        ensure!(
            selection.home == self.home,
            "Managed installation belongs to another daemon home"
        );
        let runtimes = self.runtimes();
        let owned = |runtime: &RuntimeIdentity| runtime.root.parent() == Some(runtimes.as_path());
        ensure!(
            owned(&selection.active)
                && selection.previous.as_ref().is_none_or(owned)
                && selection.trial.as_ref().is_none_or(|t| owned(&t.runtime)),
            "Managed installation selects a runtime outside its runtime store"
        );
        ensure!(
            selection
                .trial
                .as_ref()
                .is_none_or(|t| t.runtime.root != selection.active.root),
            "Managed installation trial equals its active runtime"
        );
        Ok(())
    }

    fn write(&self, selection: &mut Selection) -> Result<()> {
        self.validate(selection)?;
        selection.updated_at =
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let bytes = serde_json::to_vec_pretty(selection)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(SELECTION))
            .map_err(|e| e.error)?;
        sync_directory(&self.root)
    }

    fn mutate(
        &self,
        lease: &InstallationLease,
        change: impl FnOnce(&mut Selection) -> Result<bool>,
    ) -> Result<Selection> {
        self.held(lease)?;
        let mut selection = self
            .load()?
            .context("Managed installation is not prepared")?;
        if change(&mut selection)? {
            self.write(&mut selection)?;
        }
        Ok(selection)
    }

    /// Absent destination for runtime preparation; staging shares the runtime
    /// store's filesystem so admission is one atomic rename.
    pub(crate) fn staging_path(&self, operation_id: &str) -> Result<PathBuf> {
        validate_operation_id(operation_id)?;
        let staging = self.private_child("staging")?.join(operation_id);
        lock::private_directory(&staging)?;
        Ok(staging.join("runtime"))
    }

    /// Records the running runtime as known-good before any service change. The
    /// first run copies the complete running tree (verified against the running
    /// image) into the immutable store and installs the fixed control entry.
    pub(crate) async fn prepare_previous(
        &self,
        lease: &InstallationLease,
        owner: &LaunchOwner,
    ) -> Result<Selection> {
        self.held(lease)?;
        ensure!(owner.home == self.home, "Launch owner manages another home");
        self.bind_home()?;
        if let Some(mut selection) = self.load()? {
            ensure!(
                selection.server_id == owner.server_id,
                "Managed installation belongs to another server identity"
            );
            ensure!(
                selection.trial.is_none(),
                "Managed installation has an unrecovered trial runtime"
            );
            let running = |runtime: &RuntimeIdentity| {
                runtime.executable_sha256 == owner.executable_sha256
                    && owner.executable == runtime.root.join(EXECUTABLE)
            };
            let adopted = selection.adopted_from.as_ref().is_some_and(|source| {
                owner.executable == source.join(EXECUTABLE)
                    && owner.executable_sha256 == selection.active.executable_sha256
            });
            ensure!(
                running(&selection.active) || adopted,
                "Running daemon image is not the managed installation's active runtime"
            );
            if selection.control.protocol < CONTROL_PROTOCOL || !self.control_intact(&selection)? {
                selection.control = self.install_control().await?;
                self.write(&mut selection)?;
            }
            return Ok(selection);
        }
        let source = owner
            .executable
            .parent()
            .context("Running runtime has no root")?
            .to_owned();
        ensure!(
            !source.starts_with(&self.home) && !source.starts_with(&self.root),
            "Running runtime overlaps the daemon home or installation store"
        );
        let staging = self.staging_path(&format!(
            "adopt-{}",
            uuid::Uuid::new_v4().simple()
        ))?;
        let runtimes = self.private_child("runtimes")?;
        let expected_sha = owner.executable_sha256.clone();
        let (from, to) = (source.clone(), staging.clone());
        let digest = blocking(move || {
            ensure!(
                sha256_file(&from.join(EXECUTABLE))? == expected_sha,
                "Running image differs from its runtime file; refusing to adopt it"
            );
            let before = crate::daemon_admin::updater::tree_digest(&from)?;
            ensure_capacity(&to, tree_size(&from)?)?;
            copy_tree(&from, &to)?;
            ensure!(
                crate::daemon_admin::updater::tree_digest(&to)? == before
                    && crate::daemon_admin::updater::tree_digest(&from)? == before,
                "Running runtime changed while it was preserved"
            );
            Ok(before)
        })
        .await?;
        let destination = runtimes.join(format!(
            "adopted-{}",
            &hex::encode(Sha256::digest(digest.as_bytes()))[..16]
        ));
        if destination.exists() {
            let (path, expected) = (destination.clone(), digest.clone());
            blocking(move || {
                ensure!(
                    crate::daemon_admin::updater::tree_digest(&path)? == expected,
                    "Preserved runtime store entry was modified"
                );
                Ok(())
            })
            .await?;
            fs::remove_dir_all(staging.parent().context("Missing staging directory")?)?;
        } else {
            fs::rename(&staging, &destination)?;
            sync_directory(&runtimes)?;
            let _ = fs::remove_dir(staging.parent().context("Missing staging directory")?);
        }
        let (platform, arch) = host_platform()?;
        let legacy = RuntimeIdentity {
            root: destination.clone(),
            version: owner.version.clone(),
            source_sha: owner.source_sha.clone(),
            platform: platform.into(),
            arch: arch.into(),
            release: false,
            executable_sha256: owner.executable_sha256.clone(),
            tree_digest: digest.clone(),
        };
        // Accept marker provenance only when it is a strict standalone runtime
        // describing exactly the running image; never a desktop/legacy marker.
        let active = match crate::daemon_admin::updater::inspect_runtime(&destination).await {
            Ok(identity)
                if identity.tree_digest == digest
                    && identity.executable_sha256 == owner.executable_sha256
                    && identity.version == owner.version =>
            {
                RuntimeIdentity {
                    root: destination.clone(),
                    ..identity
                }
            }
            _ => legacy,
        };
        let mut selection = Selection {
            schema_version: 1,
            home: self.home.clone(),
            server_id: owner.server_id.clone(),
            control: self.install_control().await?,
            active,
            previous: None,
            trial: None,
            service: None,
            adopted_from: Some(source),
            updated_at: String::new(),
        };
        self.write(&mut selection)?;
        Ok(selection)
    }

    fn control_intact(&self, selection: &Selection) -> Result<bool> {
        Ok(selection.control.path == self.root.join("bin").join(EXECUTABLE)
            && sha256_file(&selection.control.path).ok().as_deref()
                == Some(selection.control.sha256.as_str()))
    }

    /// Installs the fixed control entry from this executable by rename, never by
    /// overwriting an executing image in place, and only after the copy has
    /// proven it starts outside any runtime tree.
    async fn install_control(&self) -> Result<ControlEntry> {
        let target = self.private_child("bin")?.join(EXECUTABLE);
        let (digest, report) = install_image(
            std::env::current_exe()?.canonicalize()?,
            target.clone(),
        )
        .await?;
        ensure!(
            report["version"].as_str() == Some(env!("CARGO_PKG_VERSION")),
            "Installed control entry reports another build"
        );
        Ok(ControlEntry {
            path: target,
            sha256: digest,
            protocol: CONTROL_PROTOCOL,
        })
    }

    /// Moves a prepared runtime into its immutable content-addressed directory.
    pub(crate) async fn admit_runtime(
        &self,
        lease: &InstallationLease,
        staged: &RuntimeIdentity,
    ) -> Result<RuntimeIdentity> {
        self.held(lease)?;
        let staging = self.root.join("staging");
        ensure!(
            staged.root.parent().and_then(Path::parent) == Some(staging.as_path())
                && staged.root.file_name() == Some(std::ffi::OsStr::new("runtime")),
            "Prepared runtime is not inside this installation's staging area"
        );
        let runtimes = self.private_child("runtimes")?;
        let name: String = staged
            .version
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' })
            .take(48)
            .collect();
        let destination = runtimes.join(format!(
            "{name}-{}",
            &hex::encode(Sha256::digest(staged.tree_digest.as_bytes()))[..16]
        ));
        let source = staged.clone();
        let target = destination.clone();
        blocking(move || {
            let verify = |root: &Path| -> Result<()> {
                ensure!(
                    crate::daemon_admin::updater::tree_digest(root)? == source.tree_digest
                        && sha256_file(&root.join(EXECUTABLE))? == source.executable_sha256,
                    "Runtime files changed after preparation: {}",
                    root.display()
                );
                Ok(())
            };
            if target.exists() {
                return verify(&target);
            }
            verify(&source.root)?;
            fs::rename(&source.root, &target)?;
            sync_directory(target.parent().context("Missing runtime store")?)?;
            verify(&target)
        })
        .await?;
        Ok(RuntimeIdentity {
            root: destination,
            ..staged.clone()
        })
    }

    pub(crate) async fn select_trial(
        &self,
        lease: &InstallationLease,
        operation_id: &str,
        candidate: &RuntimeIdentity,
    ) -> Result<Selection> {
        validate_operation_id(operation_id)?;
        self.mutate(lease, |selection| {
            if let Some(trial) = &selection.trial {
                ensure!(
                    trial.operation_id == operation_id && trial.runtime.root == candidate.root,
                    "Another update trial is selected"
                );
                return Ok(false);
            }
            ensure!(
                selection.active.root != candidate.root,
                "Candidate runtime is already active"
            );
            selection.trial = Some(Trial {
                operation_id: operation_id.to_owned(),
                runtime: candidate.clone(),
            });
            Ok(true)
        })
    }

    pub(crate) async fn commit_trial(
        &self,
        lease: &InstallationLease,
        operation_id: &str,
        candidate: &RuntimeIdentity,
    ) -> Result<Selection> {
        self.mutate(lease, |selection| match selection.trial.take() {
            Some(trial) => {
                ensure!(
                    trial.operation_id == operation_id && trial.runtime.root == candidate.root,
                    "Selected trial belongs to another update"
                );
                let previous = std::mem::replace(&mut selection.active, trial.runtime);
                selection.previous = Some(previous);
                Ok(true)
            }
            None => {
                ensure!(
                    selection.active.root == candidate.root,
                    "No update trial is selected"
                );
                Ok(false)
            }
        })
    }

    /// Returns selection to the known-good active runtime. Idempotent.
    pub(crate) async fn abandon_trial(
        &self,
        lease: &InstallationLease,
        operation_id: Option<&str>,
    ) -> Result<Selection> {
        self.mutate(lease, |selection| {
            let Some(trial) = &selection.trial else {
                return Ok(false);
            };
            ensure!(
                operation_id.is_none_or(|id| id == trial.operation_id),
                "Selected trial belongs to another update"
            );
            selection.trial = None;
            Ok(true)
        })
    }

    /// Removes unreferenced runtime trees, staging and executor copies. Kept:
    /// selected runtimes, `retain_runtimes` (journal-referenced roots such as
    /// failed/rolled-back/unknown candidates and previous runtimes), staging and
    /// executor material of `retain_operations`, and every store entry that a
    /// running process executes from or uses as its working directory.
    pub(crate) async fn cleanup(
        &self,
        lease: &InstallationLease,
        retain_operations: &[String],
        retain_runtimes: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        self.held(lease)?;
        let selection = self
            .load()?
            .context("Managed installation is not prepared")?;
        let root = self.root.clone();
        let mut keep: Vec<PathBuf> = [&selection.active]
            .into_iter()
            .chain(selection.previous.iter())
            .chain(selection.trial.iter().map(|trial| &trial.runtime))
            .map(|runtime| runtime.root.as_path())
            .chain(retain_runtimes.iter().map(PathBuf::as_path))
            .filter_map(|path| owned_entry(&root, path))
            .collect();
        keep.extend(
            std::env::current_exe()?
                .canonicalize()
                .ok()
                .and_then(|exe| owned_entry(&root, &exe)),
        );
        let retain: Vec<String> = retain_operations.to_vec();
        blocking(move || {
            keep.extend(occupied_entries(&root));
            let mut removed = Vec::new();
            for area in ["runtimes", "staging", "executors"] {
                let directory = root.join(area);
                let entries = match fs::read_dir(&directory) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                for entry in entries {
                    let entry = entry?;
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let metadata = fs::symlink_metadata(&path)?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        continue;
                    }
                    let retained = keep.contains(&path)
                        || (area != "runtimes" && retain.contains(&name));
                    if !retained {
                        fs::remove_dir_all(&path)?;
                        removed.push(path);
                    }
                }
            }
            Ok(removed)
        })
        .await
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Maps a path inside the installation store to its top-level entry
/// (`<root>/<area>/<name>`), the unit cleanup removes or keeps.
fn owned_entry(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut parts = path.strip_prefix(root).ok()?.components();
    match (parts.next()?, parts.next()?) {
        (std::path::Component::Normal(area), std::path::Component::Normal(name)) => {
            Some(root.join(area).join(name))
        }
        _ => None,
    }
}

/// Store entries some process still executes from or runs in. Only process
/// image and working-directory paths are read; no user data is inspected.
fn occupied_entries(root: &Path) -> Vec<PathBuf> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    system
        .processes()
        .values()
        .flat_map(|process| [process.exe(), process.cwd()])
        .flatten()
        .filter_map(|path| owned_entry(root, path))
        .collect()
}

fn tree_size(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn ensure_capacity(destination: &Path, required: u64) -> Result<()> {
    let target = destination
        .ancestors()
        .find(|path| path.exists())
        .context("Destination has no existing ancestor")?
        .canonicalize()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let available = disks
        .iter()
        .filter(|disk| target.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
        .context("Cannot determine free space for the installation store")?;
    let margin = 64 * 1024 * 1024;
    if available < required.saturating_add(margin) {
        return Err(error(
            "INSUFFICIENT_SPACE",
            format!(
                "Preserving the running runtime needs {} bytes; {} available",
                required.saturating_add(margin),
                available
            ),
        ));
    }
    Ok(())
}

/// Copies a runtime tree exactly (names, bytes, permission bits), rejecting
/// symlinks and special files like the runtime digest does.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    ensure!(!destination.exists(), "Copy destination already exists");
    let mut directories = Vec::new();
    for entry in walkdir::WalkDir::new(source).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            fs::create_dir(&target)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
            }
            directories.push((target, metadata.permissions()));
        } else if metadata.is_file() {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(&target, metadata.permissions())?;
            File::open(&target)?.sync_all()?;
        } else {
            bail!(
                "Runtime contains a symlink or special file: {}",
                relative.display()
            );
        }
    }
    for (directory, permissions) in directories.into_iter().rev() {
        sync_directory(&directory)?;
        fs::set_permissions(&directory, permissions)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Owner cutover
// ---------------------------------------------------------------------------

/// Before stopping a unit by name, proves it is still the service this
/// operation discovered and that everything it runs belongs to this
/// installation (original runtime, control entry, or a store runtime —
/// including a never-ready trial). A changed or unrelated unit is rejected.
async fn verify_service_ownership(
    installation: &Installation,
    owner: &LaunchOwner,
    service: &ServiceOwner,
) -> Result<()> {
    let changed = |reason: &str| {
        error(
            "OWNER_CHANGED",
            format!("systemd unit {}: {reason}; refusing to stop it", service.unit),
        )
    };
    let properties = unit_properties(
        &service.unit,
        &["MainPID", "ControlGroup", "FragmentPath", "LoadState", "ExecStart"],
    )
    .await?;
    if properties.get("LoadState").map(String::as_str) != Some("loaded")
        || properties.get("FragmentPath").map(PathBuf::from).as_ref()
            != Some(&service.fragment_path)
    {
        return Err(changed("unit definition changed"));
    }
    let control = installation
        .load()?
        .map(|selection| selection.control.path);
    let mut entries: Vec<String> = service.exec_start_paths.clone();
    entries.push(service.exec_path.to_string_lossy().into_owned());
    entries.extend(control.iter().map(|path| path.to_string_lossy().into_owned()));
    let named = exec_start_paths(properties.get("ExecStart").map(String::as_str).unwrap_or(""));
    if named.is_empty() || named.iter().any(|path| !entries.contains(path)) {
        return Err(changed("ExecStart no longer launches this installation"));
    }
    let control_group = properties.get("ControlGroup").cloned().unwrap_or_default();
    if !control_group.is_empty() && control_group != service.control_group {
        return Err(changed("control group changed"));
    }
    let main_pid: u32 = properties
        .get("MainPID")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if main_pid != 0 {
        let (home, exec_path, runtimes) = (
            owner.home.clone(),
            service.exec_path.clone(),
            installation.runtimes(),
        );
        let owned = blocking(move || {
            let Some(birth) = identity::start_identity(main_pid)? else {
                return Ok(true);
            };
            let image = image_path(main_pid)?;
            let facts = process_facts(main_pid, false)?;
            ensure!(
                identity::start_identity(main_pid)?.as_deref() == Some(birth.as_str()),
                "Service main process changed during verification"
            );
            let ours = image == exec_path
                || control.as_ref() == Some(&image)
                || image.starts_with(&runtimes);
            let same_home = facts.is_none_or(|facts| {
                is_paseo_run(&facts.cmd)
                    && home_argument(facts.cmd.get(1..).unwrap_or(&[])).is_none_or(|configured| {
                        Path::new(configured).canonicalize().ok().as_deref() == Some(home.as_path())
                    })
            });
            Ok(ours && same_home)
        })
        .await?;
        if !owned {
            return Err(changed("its main process is not this installation's daemon"));
        }
    }
    if let Some(instance) = read_daemon_instance(&owner.home).await? {
        #[cfg(target_os = "linux")]
        {
            let cgroup = fs::read_to_string(format!("/proc/{}/cgroup", instance.pid))?;
            if user_service_cgroup(&cgroup).is_none_or(|(path, _)| path != service.control_group) {
                return Err(changed("the home's supervisor runs outside this unit"));
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = instance;
    }
    Ok(())
}

/// Stops the daemon through its real launch owner and confirms that no
/// supervisor for the home remains.
pub(crate) async fn stop_owner(
    installation: &Installation,
    owner: &LaunchOwner,
    deadline: Duration,
) -> Result<()> {
    match &owner.service {
        Some(service) => {
            verify_service_ownership(installation, owner, service).await?;
            systemctl(&["stop", service.unit.as_str()], deadline + Duration::from_secs(10)).await?;
            let properties = unit_properties(&service.unit, &["ActiveState"]).await?;
            if !matches!(
                properties.get("ActiveState").map(String::as_str),
                Some("inactive" | "failed")
            ) {
                return Err(error(
                    "STOP_NOT_CONFIRMED",
                    format!("systemd unit {} did not stop", service.unit),
                ));
            }
            if read_daemon_instance(&owner.home).await?.is_some() {
                return Err(error(
                    "STOP_NOT_CONFIRMED",
                    "A supervisor for this home is still running outside its service",
                ));
            }
            Ok(())
        }
        None => {
            let Some(current) = read_daemon_instance(&owner.home).await? else {
                return Ok(());
            };
            let original = same_daemon_instance(&current, &owner.supervisor)
                && current.process_start_identity == owner.supervisor.process_start_identity;
            if !original {
                let (pid, runtimes) = (current.pid, installation.runtimes());
                let managed = blocking(move || Ok(image_path(pid)?.starts_with(&runtimes))).await?;
                if current.desktop_managed || !managed {
                    return Err(error(
                        "OWNER_CHANGED",
                        "Another launcher replaced the daemon during the update",
                    ));
                }
            }
            stop_daemon_instance(
                &owner.home,
                StopOptions {
                    instance: Some(current),
                    force: true,
                    timeout: deadline,
                    kill_timeout: Duration::from_secs(5),
                },
                &CancellationToken::new(),
            )
            .await?;
            Ok(())
        }
    }
}

/// Points a systemd user service at the fixed control entry with an
/// ExecStart-only drop-in, keeping the unit's environment and options. The
/// original unit text and prior drop-ins are saved once for recovery.
pub(crate) async fn bind_service(
    installation: &Installation,
    lease: &InstallationLease,
    owner: &LaunchOwner,
) -> Result<bool> {
    installation.held(lease)?;
    let Some(service) = &owner.service else {
        return Ok(false);
    };
    let selection = installation
        .load()?
        .context("Managed installation is not prepared")?;
    let content = drop_in_content(&selection.control.path, &owner.home);
    let directory = drop_in_directory(&service.unit)?;
    let path = directory.join(DROP_IN);
    let original = installation.root().join("service-original");
    if !original.exists() {
        let unit_text = systemctl(&["cat", service.unit.as_str()], TOOL_TIMEOUT).await?;
        let (root, directory, launch) = (
            installation.root().to_owned(),
            directory.clone(),
            json!({"unit":service.unit,"fragmentPath":service.fragment_path,"execPath":service.exec_path,"launchArgs":service.launch_args}),
        );
        let target = original.clone();
        blocking(move || {
            let temporary = tempfile::Builder::new()
                .prefix(".service-original-")
                .tempdir_in(&root)?;
            fs::write(temporary.path().join("unit.txt"), unit_text)?;
            fs::write(
                temporary.path().join("launch.json"),
                serde_json::to_vec_pretty(&launch)?,
            )?;
            if let Ok(entries) = fs::read_dir(&directory) {
                let saved = temporary.path().join("drop-ins");
                fs::create_dir(&saved)?;
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_file() && entry.file_name() != DROP_IN {
                        fs::copy(entry.path(), saved.join(entry.file_name()))?;
                    }
                }
            }
            for entry in walkdir::WalkDir::new(temporary.path()) {
                let entry = entry?;
                if entry.file_type().is_file() {
                    File::open(entry.path())?.sync_all()?;
                }
            }
            fs::rename(temporary.keep(), &target)?;
            sync_directory(&root)
        })
        .await?;
    }
    if fs::read(&path).ok().as_deref() == Some(content.as_bytes())
        && selection.service.is_some()
    {
        return Ok(false);
    }
    let binding = ServiceBinding {
        unit: service.unit.clone(),
        drop_in: path.clone(),
        original,
        launch_args: service.launch_args.clone(),
    };
    // The launcher needs the preserved arguments before the unit can start it.
    installation.mutate(lease, |selection| {
        selection.service = Some(binding);
        Ok(true)
    })?;
    let (target, bytes) = (path.clone(), content.clone());
    blocking(move || {
        let directory = target.parent().context("Missing drop-in directory")?;
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(bytes.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        file.as_file().sync_all()?;
        file.persist(&target).map_err(|e| e.error)?;
        sync_directory(directory)
    })
    .await?;
    systemctl(&["daemon-reload"], TOOL_TIMEOUT).await?;
    let exec = unit_properties(&service.unit, &["ExecStart"]).await?;
    ensure!(
        exec.get("ExecStart")
            .is_some_and(|value| value.contains(&*selection.control.path.to_string_lossy())),
        "systemd did not apply the managed ExecStart"
    );
    Ok(true)
}

/// Emergency path: removes the managed drop-in so the original unit applies.
pub(crate) async fn restore_service(
    installation: &Installation,
    lease: &InstallationLease,
    owner: &LaunchOwner,
) -> Result<()> {
    installation.held(lease)?;
    let Some(service) = &owner.service else {
        return Ok(());
    };
    let path = drop_in_directory(&service.unit)?.join(DROP_IN);
    match fs::read_to_string(&path) {
        Ok(content) => {
            ensure!(
                content.starts_with(DROP_IN_HEADER),
                "Refusing to remove a drop-in Paseo did not write"
            );
            fs::remove_file(&path)?;
            sync_directory(path.parent().context("Missing drop-in directory")?)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    systemctl(&["daemon-reload"], TOOL_TIMEOUT).await?;
    installation.mutate(lease, |selection| Ok(selection.service.take().is_some()))?;
    Ok(())
}

/// Legacy runtimes (pre-managed supervisors) read `home/native-update.json` as
/// their own transaction on every worker readiness and abort on anything else.
/// The v2 operation store must therefore live elsewhere; this rejects a start
/// that would hand a legacy runtime a file it cannot parse. A genuine legacy
/// record is left for that legacy supervisor's own recovery, never rewritten.
pub(crate) fn assert_legacy_bootable(home: &Path) -> Result<()> {
    let path = home.join(LEGACY_UPDATE_RECORD);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let legacy = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .is_some_and(|record| legacy_transaction_shape(&record));
    if !legacy {
        return Err(error(
            "LEGACY_BOOT_BLOCKED",
            format!(
                "{} is not a legacy update transaction; a legacy runtime cannot start while it exists",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn legacy_transaction_shape(record: &Value) -> bool {
    record.get("schemaVersion").is_none()
        && [
            "target",
            "staged",
            "backup",
            "directory",
            "originalDigest",
            "newDigest",
            "version",
            "sourceSha",
        ]
        .iter()
        .all(|key| record.get(key).is_some_and(Value::is_string))
}

/// Starts the daemon through its launch owner. A service owner is only ever
/// started by its service manager, never as an unmanaged standalone daemon.
pub(crate) async fn start_owner(
    installation: &Installation,
    owner: &LaunchOwner,
    deadline: Duration,
) -> Result<()> {
    // Every runtime may be legacy (the adopted previous runtime is); never start
    // one into a guaranteed readiness rejection.
    assert_legacy_bootable(&owner.home)?;
    match &owner.service {
        Some(service) => {
            systemctl(&["start", service.unit.as_str()], deadline + Duration::from_secs(10)).await?;
            Ok(())
        }
        None => {
            // Relaunch through the fixed control entry so bootstrap recovery and
            // trial selection apply exactly as they would after a reboot.
            let selection = installation
                .load()?
                .context("Managed installation is not prepared")?;
            let launch = owner
                .launch
                .as_ref()
                .context("Standalone launch facts are missing")?;
            // An executor in a transient unit must not start the daemon inside its
            // own cgroup: unit teardown would kill it. A fresh scope survives.
            #[cfg(target_os = "linux")]
            let scope = fs::read_to_string("/proc/self/cgroup")
                .ok()
                .and_then(|cgroup| user_service_cgroup(&cgroup))
                .map(|_| which::which("systemd-run"))
                .transpose()
                .context("systemd-run is required to leave the update executor unit")?;
            #[cfg(not(target_os = "linux"))]
            let scope: Option<PathBuf> = None;
            let mut command = match &scope {
                Some(systemd_run) => {
                    let mut command = Command::new(systemd_run);
                    command
                        .args(["--user", "--scope", "--quiet", "--collect"])
                        .arg(format!(
                            "--unit=paseo-daemon-{}",
                            &uuid::Uuid::new_v4().simple().to_string()[..12]
                        ))
                        .arg("--")
                        .arg(&selection.control.path);
                    command
                }
                None => Command::new(&selection.control.path),
            };
            command
                .arg("__launcher")
                .arg("--home")
                .arg(&owner.home)
                .arg("--standalone")
                .env_clear()
                .envs(launch.environment.iter().map(|(key, value)| (key, value)))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            #[cfg(target_os = "linux")]
            if scope.is_some() && !launch.environment.iter().any(|(key, _)| key == "XDG_RUNTIME_DIR") {
                command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", identity::uid()));
            }
            if let Some(directory) = &launch.working_directory {
                command.current_dir(directory);
            }
            let output = tokio::time::timeout(deadline + Duration::from_secs(10), command.output())
                .await
                .map_err(|_| anyhow!("Standalone launcher did not finish before its deadline"))?
                .context("Cannot run the managed control entry")?;
            if !output.status.success() {
                return Err(error(
                    "DAEMON_START_FAILED",
                    format!(
                        "Managed launcher failed ({}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                ));
            }
            Ok(())
        }
    }
}

/// Identity expected after `restore_service`: the adopted runtime's bytes at
/// their original location.
pub(crate) fn original_runtime(owner: &LaunchOwner, adopted: &RuntimeIdentity) -> RuntimeIdentity {
    RuntimeIdentity {
        root: owner
            .service
            .as_ref()
            .and_then(|service| service.exec_path.parent())
            .unwrap_or_else(|| owner.executable.parent().unwrap_or(owner.executable.as_path()))
            .to_owned(),
        ..adopted.clone()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerEvidence {
    pub supervisor_pid: u32,
    pub supervisor_start_identity: String,
    pub worker_pid: u32,
    pub worker_start_identity: String,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub version: String,
    pub server_id: String,
    pub listen: String,
    pub service_main_pid: Option<u32>,
}

/// Commit gate: a new supervisor launched by the owner, authenticated status
/// for the same server, and supervisor plus worker executing the expected image.
pub(crate) async fn verify_ready(
    owner: &LaunchOwner,
    expected: &RuntimeIdentity,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<WorkerEvidence> {
    let deadline = Instant::now() + timeout;
    let mut last = anyhow!("Daemon did not publish readiness");
    loop {
        if cancel.is_cancelled() {
            bail!("Operation cancelled");
        }
        match observe_ready(owner, expected, cancel).await {
            Ok(Some(evidence)) => return Ok(evidence),
            Ok(None) => {}
            Err(observed) => last = observed,
        }
        if Instant::now() >= deadline {
            return Err(error(
                "READINESS_FAILED",
                format!("Updated daemon was not verified: {last:#}"),
            ));
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("Operation cancelled"),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

async fn observe_ready(
    owner: &LaunchOwner,
    expected: &RuntimeIdentity,
    cancel: &CancellationToken,
) -> Result<Option<WorkerEvidence>> {
    let Some(instance) = read_daemon_instance(&owner.home).await? else {
        return Ok(None);
    };
    if same_daemon_instance(&instance, &owner.supervisor)
        && instance.process_start_identity == owner.supervisor.process_start_identity
    {
        bail!("The previous supervisor is still running");
    }
    let Some(listen) = instance.listen.clone().filter(|listen| !listen.is_empty()) else {
        return Ok(None);
    };
    ensure!(
        instance.server_id.as_deref() == Some(owner.server_id.as_str()),
        "Server identity changed"
    );
    let status = authenticated_status(&owner.home, &instance, cancel).await?;
    ensure!(
        status["serverId"].as_str() == Some(owner.server_id.as_str()),
        "Authenticated server identity changed"
    );
    ensure!(
        status["version"].as_str() == Some(expected.version.as_str()),
        "Daemon reports version {} instead of {}",
        status["version"],
        expected.version
    );
    // Image digest already binds the build; sourceSha is cross-checked whenever
    // the runtime reports it and the expected identity records one.
    if let Some(source) = status["sourceSha"].as_str().filter(|source| !source.is_empty()) {
        ensure!(
            expected.source_sha.is_empty() || source == expected.source_sha,
            "Daemon reports source {source} instead of {}",
            expected.source_sha
        );
    }
    let worker_pid = status["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .context("Daemon status has no worker PID")?;
    let supervisor_birth = instance
        .process_start_identity
        .clone()
        .context("Supervisor has no birth identity")?;
    let executable = expected.root.join(EXECUTABLE);
    let (path, sha) = (executable.clone(), expected.executable_sha256.clone());
    let (supervisor_pid, birth) = (instance.pid, supervisor_birth.clone());
    let (worker_birth, supervisor_parent) = blocking(move || {
        let check = |pid: u32, birth: &str, role: &str| -> Result<()> {
            let (image, digest) = pinned_image(pid, birth)?;
            ensure!(image == path, "{role} executes {} instead of {}", image.display(), path.display());
            ensure!(digest == sha, "{role} image digest does not match the expected runtime");
            Ok(())
        };
        check(supervisor_pid, &birth, "Supervisor")?;
        let worker_birth =
            identity::start_identity(worker_pid)?.context("Worker exited during verification")?;
        let worker = process_facts(worker_pid, false)?.context("Worker exited during verification")?;
        ensure!(
            worker.parent == Some(supervisor_pid),
            "Reported worker is not a child of the published supervisor"
        );
        check(worker_pid, &worker_birth, "Worker")?;
        let supervisor =
            process_facts(supervisor_pid, false)?.context("Supervisor exited during verification")?;
        Ok((worker_birth, supervisor.parent))
    })
    .await?;
    let mut service_main_pid = None;
    if let Some(service) = &owner.service {
        #[cfg(target_os = "linux")]
        {
            let cgroup = fs::read_to_string(format!("/proc/{}/cgroup", instance.pid))?;
            ensure!(
                user_service_cgroup(&cgroup).is_some_and(|(_, unit)| unit == service.unit),
                "Supervisor is not running inside {}",
                service.unit
            );
        }
        let properties = unit_properties(&service.unit, &["MainPID"]).await?;
        let main_pid: u32 = properties
            .get("MainPID")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        ensure!(
            main_pid != 0 && supervisor_parent == Some(main_pid),
            "Supervisor was not launched by {}",
            service.unit
        );
        service_main_pid = Some(main_pid);
    }
    Ok(Some(WorkerEvidence {
        supervisor_pid: instance.pid,
        supervisor_start_identity: supervisor_birth,
        worker_pid,
        worker_start_identity: worker_birth,
        executable,
        executable_sha256: expected.executable_sha256.clone(),
        version: expected.version.clone(),
        server_id: owner.server_id.clone(),
        listen,
        service_main_pid,
    }))
}

// ---------------------------------------------------------------------------
// Independent executor
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutorHandle {
    pub kind: String,
    pub unit: Option<String>,
    pub helper: PathBuf,
    pub helper_sha256: String,
    pub pid: Option<u32>,
    pub start_identity: Option<String>,
}

fn executor_directory(installation: &Installation, operation_id: &str) -> Result<PathBuf> {
    validate_operation_id(operation_id)?;
    let directory = installation.private_child("executors")?.join(operation_id);
    lock::private_directory(&directory)?;
    Ok(directory)
}

/// Copies an image next to `target` under a temporary name. `None` when
/// `target` already holds identical bytes.
fn stage_image(source: &Path, target: &Path) -> Result<(Option<tempfile::TempPath>, String)> {
    let digest = sha256_file(source)?;
    if sha256_file(target).ok().as_deref() == Some(digest.as_str()) {
        return Ok((None, digest));
    }
    let directory = target.parent().context("Missing image directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".paseo-image-")
        .suffix(std::env::consts::EXE_SUFFIX)
        .tempfile_in(directory)?;
    std::io::copy(&mut File::open(source)?, temporary.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    temporary.as_file().sync_all()?;
    // Closing the write handle lets the copy execute for its startup probe.
    let staged = temporary.into_temp_path();
    ensure!(sha256_file(&staged)? == digest, "Copied image digest mismatch");
    Ok((Some(staged), digest))
}

/// Installs `source` at `target` by rename after the copy proves it starts on
/// its own (no dependency on the runtime tree it came from). Returns the digest
/// and the probe report.
async fn install_image(source: PathBuf, target: PathBuf) -> Result<(String, Value)> {
    let (staged, digest) = {
        let (source, target) = (source.clone(), target.clone());
        blocking(move || stage_image(&source, &target)).await?
    };
    let report = match staged {
        Some(staged) => {
            let report = probe_executable(&staged).await?;
            staged.persist(&target).map_err(|e| e.error)?;
            sync_directory(target.parent().context("Missing image directory")?)?;
            ensure!(
                sha256_file(&target)? == digest,
                "Installed image digest mismatch: {}",
                target.display()
            );
            report
        }
        None => probe_executable(&target).await?,
    };
    Ok((digest, report))
}

fn executor_arguments(home: &Path, operation_id: &str) -> Vec<std::ffi::OsString> {
    vec![
        "__update-executor".into(),
        "--home".into(),
        home.as_os_str().to_owned(),
        "--operation".into(),
        operation_id.into(),
    ]
}

/// Launches `__update-executor` outside the daemon's process tree. A systemd
/// owner always uses a transient user unit so stopping the service cannot stop
/// the executor; the call returns only after the process exists.
pub(crate) async fn spawn_executor(
    home: &Path,
    operation_id: &str,
    owner: &LaunchOwner,
    executable: &Path,
) -> Result<ExecutorHandle> {
    let installation = Installation::for_home(home)?;
    ensure!(installation.home == owner.home, "Launch owner manages another home");
    // Every later process (transient unit, launcher) resolves this same root
    // through the home pointer, whatever its XDG/user environment.
    installation.ensure_root()?;
    installation.bind_home()?;
    let directory = executor_directory(&installation, operation_id)?;
    // Copied so neither runtime cleanup nor control refresh can replace the
    // file it executes; the probe rejects images needing their runtime tree.
    let helper = directory.join(EXECUTABLE);
    let (helper_sha256, _) =
        install_image(executable.canonicalize()?, helper.clone()).await?;
    let helper = helper.canonicalize()?;
    #[cfg(target_os = "linux")]
    {
        if owner.service.is_some() || user_manager_available().await {
            return systemd_executor(operation_id, owner, helper, helper_sha256).await;
        }
    }
    #[cfg(unix)]
    {
        let (program, args, cwd) = (
            helper.clone(),
            executor_arguments(&owner.home, operation_id),
            owner.home.clone(),
        );
        blocking(move || spawn_detached(&program, &args, &cwd)).await?;
        Ok(ExecutorHandle {
            kind: "detached".into(),
            unit: None,
            helper,
            helper_sha256,
            pid: None,
            start_identity: None,
        })
    }
    #[cfg(windows)]
    {
        let mut command = Command::new(&helper);
        command
            .args(executor_arguments(&owner.home, operation_id))
            .current_dir(&owner.home)
            .env_remove("PASEO_NATIVE_SUPERVISOR")
            .env_remove("PASEO_NATIVE_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // DETACHED | NEW_PROCESS_GROUP | NO_WINDOW | BREAKAWAY_FROM_JOB: the
            // copied helper must outlive the worker job it was requested from.
            .creation_flags(0x00000008 | 0x00000200 | 0x08000000 | 0x01000000);
        let child = command
            .spawn()
            .context("Cannot launch the update executor outside the daemon job")?;
        let pid = child.id().context("Update executor has no PID")?;
        Ok(ExecutorHandle {
            kind: "windows-helper".into(),
            unit: None,
            helper,
            helper_sha256,
            pid: Some(pid),
            start_identity: identity::start_identity(pid)?,
        })
    }
}

#[cfg(target_os = "linux")]
async fn systemd_executor(
    operation_id: &str,
    owner: &LaunchOwner,
    helper: PathBuf,
    helper_sha256: String,
) -> Result<ExecutorHandle> {
    let unit = format!("{EXECUTOR_UNIT_PREFIX}{operation_id}.service");
    let state = unit_properties(&unit, &["ActiveState"]).await?;
    let running = matches!(
        state.get("ActiveState").map(String::as_str),
        Some("active" | "activating" | "deactivating")
    );
    if !running {
        let mut args = vec![
            "--user".to_owned(),
            "--quiet".to_owned(),
            "--collect".to_owned(),
            format!("--unit={unit}"),
            format!("--description=Paseo daemon update {operation_id}"),
            "--property=Type=exec".to_owned(),
            format!("--property=WorkingDirectory={}", owner.home.display()),
            format!("--setenv=PASEO_HOME={}", owner.home.display()),
        ];
        args.push("--".to_owned());
        args.push(helper.to_string_lossy().into_owned());
        args.extend(
            executor_arguments(&owner.home, operation_id)
                .into_iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        );
        run_tool("systemd-run", &args, TOOL_TIMEOUT).await?;
    }
    let properties = unit_properties(&unit, &["MainPID", "ControlGroup", "ActiveState"]).await?;
    let pid: u32 = properties
        .get("MainPID")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut handle = ExecutorHandle {
        kind: "systemd-run".into(),
        unit: Some(unit.clone()),
        helper: helper.clone(),
        helper_sha256,
        pid: None,
        start_identity: None,
    };
    if pid == 0 {
        // Already finished; the operation journal records what it did.
        return Ok(handle);
    }
    let control_group = properties.get("ControlGroup").cloned().unwrap_or_default();
    ensure!(
        control_group.ends_with(&format!("/{unit}"))
            && owner
                .service
                .as_ref()
                .is_none_or(|service| !control_group.starts_with(&service.control_group)),
        "Update executor is not isolated from the daemon service"
    );
    ensure!(
        image_path(pid)? == helper,
        "Update executor unit runs an unexpected image"
    );
    handle.pid = Some(pid);
    handle.start_identity = identity::start_identity(pid)?;
    Ok(handle)
}

/// Double-fork into a new session so the executor is reparented away from the
/// requesting daemon before the daemon is stopped.
#[cfg(unix)]
fn spawn_detached(program: &Path, args: &[std::ffi::OsString], cwd: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("PASEO_NATIVE_SUPERVISOR")
        .env_remove("PASEO_NATIVE_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| match libc::fork() {
            -1 => Err(std::io::Error::last_os_error()),
            0 => {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
            _ => libc::_exit(0),
        });
    }
    let status = command
        .spawn()
        .context("Cannot launch the update executor")?
        .wait()?;
    ensure!(status.success(), "Update executor launcher failed: {status}");
    Ok(())
}

pub(crate) fn executor_alive(handle: &ExecutorHandle) -> Result<bool> {
    let (Some(pid), Some(birth)) = (handle.pid, &handle.start_identity) else {
        return Ok(false);
    };
    Ok(identity::start_identity(pid)?.as_ref() == Some(birth))
}

/// The running executor's own identity; `execute` journals it as the handoff ACK.
pub(crate) fn executor_identity() -> Result<ExecutorHandle> {
    let helper = std::env::current_exe()?.canonicalize()?;
    let pid = std::process::id();
    #[cfg(target_os = "linux")]
    let unit = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|cgroup| user_service_cgroup(&cgroup))
        .map(|(_, unit)| unit)
        .filter(|unit| unit.starts_with(EXECUTOR_UNIT_PREFIX));
    #[cfg(not(target_os = "linux"))]
    let unit: Option<String> = None;
    Ok(ExecutorHandle {
        kind: if unit.is_some() {
            "systemd-run"
        } else if cfg!(windows) {
            "windows-helper"
        } else {
            "detached"
        }
        .into(),
        unit,
        helper_sha256: sha256_file(&helper)?,
        helper,
        pid: Some(pid),
        start_identity: identity::start_identity(pid)?,
    })
}

/// Internal `__update-executor`: only the copied helper of this operation runs it.
pub(crate) async fn run_executor(home: &Path, operation_id: &str) -> Result<i32> {
    validate_operation_id(operation_id)?;
    let installation = Installation::for_home(home)?;
    let expected = installation
        .root()
        .join("executors")
        .join(operation_id)
        .join(EXECUTABLE)
        .canonicalize()
        .context("Update executor copy is missing")?;
    ensure!(
        std::env::current_exe()?.canonicalize()? == expected,
        "Update executor must run from its operation's copied helper"
    );
    crate::daemon_admin::update_operation::execute(&installation.home, operation_id).await
}

/// Trial runtime only while a live executor verifies it; otherwise known-good
/// active, then previous.
fn launch_order(selection: &Selection, executor_alive: bool) -> Vec<&RuntimeIdentity> {
    let mut order = Vec::new();
    if executor_alive {
        if let Some(trial) = &selection.trial {
            order.push(&trial.runtime);
        }
    }
    order.push(&selection.active);
    order.extend(selection.previous.iter());
    order
}

/// Internal `__launcher`: the fixed service entry. Bootstrap recovery runs
/// first; the launcher itself never mutates the selection.
pub(crate) async fn run_launcher(home: &Path, standalone: bool) -> Result<i32> {
    let installation = Installation::for_home(home)?;
    if let Err(recovery) =
        crate::daemon_admin::update_operation::recover_bootstrap(&installation.home).await
    {
        eprintln!("Paseo update recovery failed; launching the known-good runtime: {recovery:#}");
    }
    let selection = installation
        .load()?
        .context("No managed installation exists for this daemon home")?;
    let executor_alive = installation.try_lease()?.is_none();
    let home_arg = installation.home.to_string_lossy().into_owned();
    let args = match (&selection.service, standalone) {
        (_, true) => vec!["daemon".into(), "start".into(), "--home".into(), home_arg],
        (Some(service), false) => service.launch_args.clone(),
        (None, false) => vec!["daemon".into(), "run".into(), "--home".into(), home_arg],
    };
    let mut failures = Vec::new();
    for runtime in launch_order(&selection, executor_alive) {
        let executable = runtime.root.join(EXECUTABLE);
        let probe = executable.clone();
        match blocking(move || sha256_file(&probe)).await {
            Ok(digest) if digest == runtime.executable_sha256 => {}
            Ok(_) => {
                failures.push(format!("{}: image digest mismatch", executable.display()));
                continue;
            }
            Err(failure) => {
                failures.push(format!("{}: {failure:#}", executable.display()));
                continue;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let failure = std::process::Command::new(&executable).args(&args).exec();
            failures.push(format!("{}: {failure}", executable.display()));
        }
        #[cfg(not(unix))]
        match Command::new(&executable).args(&args).status().await {
            Ok(status) => return Ok(status.code().unwrap_or(1)),
            Err(failure) => failures.push(format!("{}: {failure}", executable.display())),
        }
    }
    bail!("No managed runtime could be launched: {}", failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(root: &Path, version: &str) -> RuntimeIdentity {
        RuntimeIdentity {
            root: root.to_owned(),
            version: version.into(),
            source_sha: String::new(),
            platform: "linux".into(),
            arch: "x64".into(),
            release: false,
            executable_sha256: format!("{version}-sha"),
            tree_digest: format!("{version}-tree"),
        }
    }

    fn fixture() -> (tempfile::TempDir, Installation, InstallationLease, Selection) {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        fs::create_dir(&home).unwrap();
        let installation = Installation::at(
            base.path().canonicalize().unwrap().join("install"),
            home.canonicalize().unwrap(),
        )
        .unwrap();
        let lease = installation.try_lease().unwrap().unwrap();
        let runtimes = installation.private_child("runtimes").unwrap();
        let mut selection = Selection {
            schema_version: 1,
            home: installation.home.clone(),
            server_id: "srv".into(),
            control: ControlEntry {
                path: installation.root().join("bin").join(EXECUTABLE),
                sha256: "control".into(),
                protocol: CONTROL_PROTOCOL,
            },
            active: runtime(&runtimes.join("old"), "1.0.0"),
            previous: None,
            trial: None,
            service: None,
            adopted_from: None,
            updated_at: String::new(),
        };
        installation.write(&mut selection).unwrap();
        (base, installation, lease, selection)
    }

    #[test]
    fn installation_lease_is_exclusive_until_released() {
        let (_base, installation, lease, _) = fixture();
        assert!(installation.try_lease().unwrap().is_none());
        drop(lease);
        assert!(installation.try_lease().unwrap().is_some());
    }

    #[tokio::test]
    async fn trial_selection_commits_and_abandons_idempotently() {
        let (_base, installation, lease, selection) = fixture();
        let candidate = runtime(&installation.runtimes().join("new"), "1.1.0");
        let other = runtime(&installation.runtimes().join("other"), "1.2.0");
        installation.select_trial(&lease, "op-1", &candidate).await.unwrap();
        installation.select_trial(&lease, "op-1", &candidate).await.unwrap();
        assert!(installation.select_trial(&lease, "op-2", &other).await.is_err());
        assert!(installation.abandon_trial(&lease, Some("op-2")).await.is_err());

        let committed = installation.commit_trial(&lease, "op-1", &candidate).await.unwrap();
        assert_eq!(committed.active.root, candidate.root);
        assert_eq!(committed.previous.unwrap().root, selection.active.root);
        assert!(committed.trial.is_none());
        // A retried commit after the selection was already written is a no-op.
        let again = installation.commit_trial(&lease, "op-1", &candidate).await.unwrap();
        assert_eq!(again.active.root, candidate.root);
        assert!(installation.select_trial(&lease, "op-3", &candidate).await.is_err());

        installation.select_trial(&lease, "op-4", &other).await.unwrap();
        let abandoned = installation.abandon_trial(&lease, None).await.unwrap();
        assert!(abandoned.trial.is_none());
        assert_eq!(abandoned.active.root, candidate.root);
        installation.abandon_trial(&lease, Some("op-4")).await.unwrap();
    }

    #[tokio::test]
    async fn selection_rejects_runtimes_outside_the_store() {
        let (base, installation, lease, _) = fixture();
        let outside = runtime(&base.path().join("elsewhere"), "9.9.9");
        assert!(installation.select_trial(&lease, "op-1", &outside).await.is_err());
        assert!(installation.load().unwrap().unwrap().trial.is_none());
    }

    #[test]
    fn launcher_runs_trial_only_under_a_live_executor() {
        let (_base, installation, _lease, mut selection) = fixture();
        let trial = runtime(&installation.runtimes().join("new"), "1.1.0");
        let previous = runtime(&installation.runtimes().join("older"), "0.9.0");
        selection.trial = Some(Trial {
            operation_id: "op-1".into(),
            runtime: trial.clone(),
        });
        selection.previous = Some(previous.clone());
        let roots = |alive| {
            launch_order(&selection, alive)
                .into_iter()
                .map(|runtime| runtime.root.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(roots(true), [trial.root.clone(), selection.active.root.clone(), previous.root.clone()]);
        // Orphaned trial (executor died, e.g. power loss): known-good first.
        assert_eq!(roots(false), [selection.active.root.clone(), previous.root.clone()]);
    }

    #[tokio::test]
    async fn cleanup_keeps_selected_and_retained_material() {
        let (_base, installation, lease, _) = fixture();
        let runtimes = installation.runtimes();
        for name in ["old", "new", "stale", "failed", "busy"] {
            fs::create_dir_all(runtimes.join(name)).unwrap();
        }
        let candidate = runtime(&runtimes.join("new"), "1.1.0");
        installation.select_trial(&lease, "op-1", &candidate).await.unwrap();
        installation.staging_path("op-1").unwrap();
        installation.staging_path("op-0").unwrap();
        // A process still running from a store entry keeps it.
        #[cfg(unix)]
        let mut busy = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(runtimes.join("busy"))
            .spawn()
            .unwrap();
        #[cfg(not(unix))]
        fs::remove_dir(runtimes.join("busy")).unwrap();
        let removed = installation
            .cleanup(
                &lease,
                &["op-1".into()],
                &[runtimes.join("failed"), PathBuf::from("/outside/store")],
            )
            .await
            .unwrap();
        #[cfg(unix)]
        {
            busy.kill().unwrap();
            busy.wait().unwrap();
        }
        for kept in ["old", "new", "failed", "busy"] {
            assert_eq!(runtimes.join(kept).exists(), kept != "busy" || cfg!(unix), "{kept}");
        }
        assert!(!runtimes.join("stale").exists());
        assert!(installation.root().join("staging/op-1").exists());
        assert!(!installation.root().join("staging/op-0").exists());
        assert_eq!(removed.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn preserved_runtime_copy_matches_the_runtime_digest() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let source = base.path().join("runtime");
        fs::create_dir_all(source.join("lib/nested")).unwrap();
        fs::write(source.join(EXECUTABLE), b"binary").unwrap();
        fs::set_permissions(source.join(EXECUTABLE), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(source.join("lib/nested/data"), b"data").unwrap();
        fs::set_permissions(source.join("lib"), fs::Permissions::from_mode(0o500)).unwrap();
        let copy = base.path().join("copy");
        copy_tree(&source, &copy).unwrap();
        assert_eq!(
            crate::daemon_admin::updater::tree_digest(&copy).unwrap(),
            crate::daemon_admin::updater::tree_digest(&source).unwrap()
        );
        std::os::unix::fs::symlink("data", source.join("link")).unwrap();
        assert!(copy_tree(&source, &base.path().join("rejected")).is_err());
        fs::set_permissions(source.join("lib"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(copy.join("lib"), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn service_cgroups_and_launch_arguments_are_classified() {
        let service = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/paseo.service\n";
        assert_eq!(
            user_service_cgroup(service),
            Some((
                "/user.slice/user-1000.slice/user@1000.service/app.slice/paseo.service".into(),
                "paseo.service".into()
            ))
        );
        assert_eq!(user_service_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n"), None);
        assert_eq!(user_service_cgroup("0::/system.slice/paseo.service\n"), None);
        let args: Vec<String> = ["/opt/paseo", "--home", "/h", "daemon", "run"]
            .map(String::from)
            .into();
        assert!(is_paseo_run(&args));
        assert_eq!(home_argument(&args[1..]), Some("/h"));
        assert_eq!(home_argument(&["daemon".into(), "--home=/x".into()]), Some("/x"));
        assert!(!is_paseo_run(&["/opt/paseo".into(), "daemon".into(), "start".into()]));
        assert_eq!(
            parse_properties("MainPID=42\nControlGroup=/a=b\n").get("ControlGroup"),
            Some(&"/a=b".to_owned())
        );
    }

    #[test]
    fn drop_in_overrides_only_exec_start_with_escaped_paths() {
        let content = drop_in_content(Path::new("/i/b in/paseo"), Path::new("/h/100%$x"));
        assert!(content.starts_with(DROP_IN_HEADER));
        let directives: Vec<_> = content.lines().filter(|line| !line.starts_with('#')).collect();
        assert_eq!(
            directives,
            [
                "[Service]",
                "ExecStart=",
                "ExecStart=\"/i/b in/paseo\" __launcher --home \"/h/100%%$$x\""
            ]
        );
    }

    #[test]
    fn legacy_runtime_start_is_refused_while_a_v2_record_occupies_its_path() {
        let home = tempfile::tempdir().unwrap();
        assert!(assert_legacy_bootable(home.path()).is_ok());
        let record = home.path().join(LEGACY_UPDATE_RECORD);
        fs::write(&record, br#"{"schemaVersion":2,"operationId":"op","phase":"staged"}"#).unwrap();
        let refused = assert_legacy_bootable(home.path()).unwrap_err();
        assert_eq!(
            refused.downcast_ref::<super::super::DaemonInstanceError>().unwrap().code,
            "LEGACY_BOOT_BLOCKED"
        );
        fs::write(&record, b"not json").unwrap();
        assert!(assert_legacy_bootable(home.path()).is_err());
        // A genuine legacy transaction is left for the legacy supervisor itself.
        fs::write(
            &record,
            serde_json::to_vec(&json!({"target":"/r","staged":"/s","backup":"/b","directory":"/d",
                "originalDigest":"o","newDigest":"n","version":"1.0.0","sourceSha":"x","committed":false}))
            .unwrap(),
        )
        .unwrap();
        assert!(assert_legacy_bootable(home.path()).is_ok());
        assert!(record.exists());
    }

    #[test]
    fn operation_ids_cannot_escape_unit_or_directory_names() {
        assert!(validate_operation_id("0b5e4a5c-7c1d-4f7e-9a51-2f7d3c8e9b10").is_ok());
        let long = "a".repeat(65);
        for bad in ["", "../x", "a/b", "a b", "a.service", long.as_str()] {
            assert!(validate_operation_id(bad).is_err(), "{bad}");
        }
    }
}
