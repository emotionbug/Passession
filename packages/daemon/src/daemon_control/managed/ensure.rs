//! Bootstrap and start a daemon without claiming its lifetime.
use super::{
    DaemonInstance, EXECUTABLE, Installation, Selection, blocking, clean_runtime_environment,
    image_path, lock, probe_executable, read_daemon_instance, run_standalone_launcher,
    same_daemon_instance,
};
use crate::{daemon_admin::update_operation, transport};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::Instant};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
mod systemd;

const READY: Duration = Duration::from_secs(70);
const POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(super) enum Registration {
    Detached,
    SystemdUser {
        unit: String,
        unit_file: PathBuf,
        reused: bool,
    },
}

#[derive(Serialize)]
pub(crate) struct EnsureResult {
    pub state: &'static str,
    pub owner: &'static str,
    pub pid: u32,
    pub listen: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration: Option<&'static str>,
}

pub(crate) async fn ensure_daemon(
    home: &Path,
    runtime: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<EnsureResult> {
    #[cfg(target_os = "linux")]
    let manager = systemd::Manager::default();
    let path = home.to_owned();
    let home = blocking(move || {
        if !path.exists() {
            lock::private_directory(&path)?;
        }
        Ok(path.canonicalize()?)
    })
    .await?;
    // A ready, non-legacy daemon needs no installation files or locks.
    if let Some(instance) = read_daemon_instance(&home).await?
        && !instance.desktop_managed
    {
        let instance = ready(&home, Some(&instance), cancel).await?;
        let owner = existing_owner(&home, &instance).await;
        return report("running", owner, instance, None);
    }
    let _ensure = acquire_lease(&home.join(".native-ensure.lock"), cancel).await?;

    // A daemon we did not install is never adopted, configured, or stopped.
    let legacy = if let Some(instance) = read_daemon_instance(&home).await? {
        let instance = ready(&home, Some(&instance), cancel).await?;
        if !instance.desktop_managed {
            let owner = existing_owner(&home, &instance).await;
            return report("running", owner, instance, None);
        }
        Some(instance)
    } else {
        None
    };

    let installation = Installation::for_home(&home)?;
    let lease = installation
        .try_lease()?
        .context("A native update is using this installation")?;
    #[cfg(target_os = "linux")]
    let mut registration_lease = None;
    let (selection, created) = match installation.load()? {
        Some(selection) => {
            installation.bind_home()?;
            (selection, false)
        }
        None => {
            let source = runtime.context("No managed daemon installation exists; pass --runtime with the bundled daemon runtime directory")?;
            let source = tokio::fs::canonicalize(source)
                .await
                .context("Cannot open the bundled daemon runtime")?;
            let mut active = installation.preserve_runtime(&source, None).await?;
            let probe = probe_executable(&active.root.join(EXECUTABLE)).await?;
            active.version = probe["version"]
                .as_str()
                .filter(|v| !v.is_empty())
                .context("Bundled runtime reports no version")?
                .to_owned();
            active.source_sha = probe["sourceSha"].as_str().unwrap_or_default().to_owned();
            ensure!(
                probe["legacyMigrationCredential"] == true,
                "Use a bundled runtime that supports independent installation and legacy credential handover"
            );
            let override_id = legacy
                .as_ref()
                .and_then(|instance| instance.server_id.as_deref());
            let server_id = transport::ensure_server_id(&home, override_id).await?;
            let control = installation.install_control().await?;
            #[cfg(target_os = "linux")]
            let registration = {
                let (registration, lease) = manager
                    .select(&home, &control.path, installation.root(), cancel)
                    .await?;
                registration_lease = lease;
                registration
            };
            #[cfg(not(target_os = "linux"))]
            let registration = Registration::Detached;
            let mut selection = Selection {
                schema_version: 1,
                home: home.clone(),
                server_id,
                control,
                active,
                trial: None,
                registration: Some(registration),
                service: None,
                adopted_from: None,
                updated_at: String::new(),
            };
            installation.bind_home()?;
            installation.write(&mut selection)?;
            (selection, true)
        }
    };
    if !created {
        ensure!(
            installation.control_intact(&selection)?,
            "Managed control entry is missing or changed"
        );
    }
    if legacy.is_none()
        && let Some(instance) = read_daemon_instance(&home).await?
    {
        // A non-ensure launcher may have won while the runtime was being copied.
        let instance = ready(&home, Some(&instance), cancel).await?;
        let owner = existing_owner(&home, &instance).await;
        return report("running", owner, instance, None);
    }
    #[cfg(target_os = "linux")]
    {
        manager.prepare(&selection).await?;
        drop(registration_lease);
    }

    if let Some(legacy) = legacy {
        ensure!(
            legacy.server_id.as_deref() == Some(&selection.server_id),
            "Managed installation belongs to another server identity"
        );
        ensure!(
            selection.registration.is_some(),
            "This installation was not prepared for legacy desktop migration"
        );
        if !update_operation::migrate_desktop_legacy(&home, &legacy, installation.root(), cancel)
            .await?
        {
            return report("running", "desktop-legacy", legacy, Some("pending"));
        }
    }

    // The launcher checks this lease to distinguish a live update trial from
    // one that needs bootstrap recovery. Never hold it across startup.
    drop(lease);
    let service = match &selection.registration {
        Some(Registration::SystemdUser { unit, .. }) => Some(unit.as_str()),
        Some(Registration::Detached) => None,
        None => selection
            .service
            .as_ref()
            .map(|service| service.unit.as_str()),
    };
    let owner = match service {
        #[cfg(target_os = "linux")]
        Some(unit) => {
            manager.start(&selection, unit, cancel).await?;
            "systemd-user"
        }
        #[cfg(not(target_os = "linux"))]
        Some(_) => bail!("The recorded systemd user owner is unavailable on this platform"),
        None => {
            let mut command = Command::new(&selection.control.path);
            command
                .args(["__launcher", "--home"])
                .arg(&home)
                .arg("--standalone")
                .current_dir(&home)
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            clean_runtime_environment(&mut command);
            #[cfg(windows)]
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
            run_standalone_launcher(&mut command, installation.root(), READY).await?;
            "detached"
        }
    };
    let instance = ready(&home, None, cancel).await?;
    ensure!(
        instance.server_id.as_deref() == Some(&selection.server_id),
        "Started daemon published a different server identity"
    );
    ensure!(
        !instance.desktop_managed,
        "A legacy desktop supervisor took this daemon home during startup"
    );
    let pid = instance.pid;
    let image = blocking(move || image_path(pid)).await?;
    if !image.starts_with(installation.runtimes()) {
        let owner = existing_owner(&home, &instance).await;
        return report("running", owner, instance, None);
    }
    report(
        if created { "installed" } else { "started" },
        owner,
        instance,
        None,
    )
}

fn report(
    state: &'static str,
    owner: &'static str,
    instance: DaemonInstance,
    migration: Option<&'static str>,
) -> Result<EnsureResult> {
    Ok(EnsureResult {
        state,
        owner,
        pid: instance.pid,
        listen: instance.listen.context("Daemon has no listen address")?,
        migration,
    })
}

async fn ready(
    home: &Path,
    expected: Option<&DaemonInstance>,
    cancel: &CancellationToken,
) -> Result<DaemonInstance> {
    let deadline = Instant::now() + READY;
    loop {
        let instance = read_daemon_instance(home).await?;
        if let Some(expected) = expected {
            ensure!(
                instance.as_ref().is_some_and(|actual| {
                    same_daemon_instance(expected, actual)
                        && actual.process_start_identity == expected.process_start_identity
                }),
                "Daemon changed while waiting for readiness"
            );
        }
        if let Some(instance) = instance
            && let Some(listen) = instance.listen.as_deref().filter(|v| !v.is_empty())
            && paseo_client::read_local_credential_for_target(home, listen)
                .await?
                .is_some()
        {
            return Ok(instance);
        }
        ensure!(
            Instant::now() < deadline,
            "Daemon did not publish a listen address and local credential for {}. Logs: {}",
            home.display(),
            super::super::daemon_log_path(home).display()
        );
        pause(cancel).await?;
    }
}

async fn pause(cancel: &CancellationToken) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => bail!("Operation cancelled"),
        _ = tokio::time::sleep(POLL) => Ok(()),
    }
}

async fn acquire_lease(path: &Path, cancel: &CancellationToken) -> Result<fs::File> {
    let file = lock::open_private(path, true).or_else(|_| lock::open_private(path, false))?;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "Another process is still ensuring the daemon"
        );
        pause(cancel).await?;
    }
}

async fn existing_owner(home: &Path, instance: &DaemonInstance) -> &'static str {
    if let Some(owner) = registered_owner(home, instance).await {
        return owner;
    }
    #[cfg(unix)]
    {
        let pid = instance.pid;
        if blocking(move || super::process_facts(pid, false))
            .await
            .ok()
            .flatten()
            .is_some_and(|facts| facts.session == Some(pid))
        {
            return "detached";
        }
    }
    "existing"
}

/// A recorded owner is display evidence only after matching its home, live
/// supervisor birth and image, and actual systemd scope or detached session.
pub(crate) async fn registered_owner(
    home: &Path,
    instance: &DaemonInstance,
) -> Option<&'static str> {
    if instance.desktop_managed {
        return Some("desktop-legacy");
    }
    let home = home.to_owned();
    let instance = instance.clone();
    blocking(move || {
        let home = home.canonicalize()?;
        let Some(root) = super::read_pointer(&home)? else {
            return Ok(None);
        };
        let Some(selection) = Installation::at(root, home)?.load()? else {
            return Ok(None);
        };
        let pid = instance.pid;
        if instance.server_id.as_deref() != Some(selection.server_id.as_str())
            || !super::identity::verified(&instance)?
            || image_path(pid)? != selection.active.root.join(EXECUTABLE)
        {
            return Ok(None);
        }
        let unit = match selection.registration {
            Some(Registration::SystemdUser { unit, .. }) => Some(unit),
            Some(Registration::Detached) => None,
            None => selection.service.map(|service| service.unit),
        };
        if let Some(unit) = unit {
            #[cfg(target_os = "linux")]
            {
                let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
                let in_unit =
                    super::user_service_cgroup(&cgroup).is_some_and(|(_, name)| name == unit);
                let foreground = super::process_facts(pid, false)?
                    .is_some_and(|facts| facts.session != Some(pid));
                return Ok((in_unit && foreground).then_some("systemd-user"));
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = unit;
                return Ok(None);
            }
        }
        #[cfg(unix)]
        if super::process_facts(pid, false)?.is_none_or(|facts| facts.session != Some(pid)) {
            return Ok(None);
        }
        Ok(Some("detached"))
    })
    .await
    .ok()
    .flatten()
}
