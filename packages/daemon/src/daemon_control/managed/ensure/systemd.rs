use super::{Registration, Selection, acquire_lease};
use crate::daemon_control::managed::{
    DROP_IN, EXECUTABLE, TOOL_TIMEOUT, drop_in_content, exec_start_paths, parse_properties,
    quote_systemd, run_tool, sync_directory,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const PROPERTIES: &[&str] = &[
    "Id",
    "Type",
    "LoadState",
    "FragmentPath",
    "ExecStart",
    "Environment",
    "EnvironmentFiles",
    "NeedDaemonReload",
];

/// The user manager command is replaceable, and always resolved through the
/// caller's PATH. Fixtures use a private PATH and configuration directory.
pub(super) struct Manager {
    program: &'static str,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            program: "systemctl",
        }
    }
}

impl Manager {
    async fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push("--user".to_owned());
        full.extend(args.iter().map(|arg| (*arg).to_owned()));
        run_tool(self.program, &full, timeout).await
    }

    fn config_dir() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("User configuration directory is unavailable")?
            .join("systemd/user"))
    }

    pub(super) async fn select(
        &self,
        home: &Path,
        control: &Path,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<(Registration, Option<fs::File>)> {
        let Ok(environment) = self
            .run(&["show-environment"], Duration::from_secs(5))
            .await
        else {
            return Ok((Registration::Detached, None));
        };
        let config = Self::config_dir()?;
        fs::create_dir_all(&config)?;
        let lease = acquire_lease(&config.join(".paseo-registration.lock"), cancel).await?;
        let files = self
            .run(
                &[
                    "list-unit-files",
                    "--type=service",
                    "--no-legend",
                    "--no-pager",
                ],
                TOOL_TIMEOUT,
            )
            .await?;
        let names: Vec<_> = files
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| valid_unit(name) && !name.ends_with("@.service"))
            .collect();
        let default_home = effective_default_home(&environment);
        let mut existing = None;
        // Ask about installed files, not just loaded units (stopped units may
        // have been garbage collected by the user manager).
        for chunk in names.chunks(64) {
            let props = self.properties(chunk).await?;
            for unit in props {
                if !matches_home(&unit, home, control, default_home.as_deref()) {
                    continue;
                }
                let name = unit
                    .get("Id")
                    .filter(|name| valid_unit(name))
                    .context("Invalid systemd unit name")?;
                if matches!(&existing, Some(Registration::SystemdUser { unit, .. }) if unit == name)
                {
                    continue; // The unit is also listed under an alias.
                }
                let file = PathBuf::from(
                    unit.get("FragmentPath")
                        .context("Systemd unit has no fragment")?,
                );
                ensure!(
                    file.is_absolute(),
                    "Systemd unit has no persistent fragment"
                );
                let registration = Registration::SystemdUser {
                    unit: name.clone(),
                    unit_file: file,
                    reused: true,
                };
                ensure!(
                    existing.replace(registration).is_none(),
                    "Several user services launch this daemon home; keep only one before running ensure"
                );
            }
        }
        let registration = match existing {
            Some(registration) => registration,
            None => {
                let taken = |name: &str| {
                    names.contains(&name) || fs::symlink_metadata(config.join(name)).is_ok()
                };
                let unit = if taken("paseo.service") {
                    let suffix = root
                        .file_name()
                        .and_then(|part| part.to_str())
                        .context("Managed installation has no directory name")?;
                    format!("paseo-{suffix}.service")
                } else {
                    "paseo.service".to_owned()
                };
                ensure!(
                    valid_unit(&unit) && !taken(&unit),
                    "Systemd service name {unit} is already taken"
                );
                Registration::SystemdUser {
                    unit_file: config.join(&unit),
                    unit,
                    reused: false,
                }
            }
        };
        Ok((registration, Some(lease)))
    }

    async fn properties(&self, units: &[&str]) -> Result<Vec<BTreeMap<String, String>>> {
        let options: Vec<_> = PROPERTIES
            .iter()
            .map(|key| format!("--property={key}"))
            .collect();
        let mut args = vec!["show", "--no-pager"];
        args.extend(options.iter().map(String::as_str));
        args.push("--");
        args.extend_from_slice(units);
        Ok(self
            .run(&args, TOOL_TIMEOUT)
            .await?
            .split("\n\n")
            .filter(|text| !text.trim().is_empty())
            .map(parse_properties)
            .collect())
    }

    /// Write the binding before enabling it. This also repairs an interrupted
    /// first installation without overwriting any existing service text.
    pub(super) async fn prepare(&self, selection: &Selection) -> Result<()> {
        let Some(Registration::SystemdUser {
            unit,
            unit_file,
            reused,
        }) = &selection.registration
        else {
            return Ok(());
        };
        ensure!(valid_unit(unit), "Invalid recorded systemd unit name");
        let control = systemd_path(&selection.control.path)?;
        let home = systemd_path(&selection.home)?;
        let binding_changed = if *reused {
            let properties = self.properties(&[unit]).await?;
            let properties = properties
                .first()
                .context("Recorded systemd service is missing")?;
            ensure!(
                fragment(properties) == Some(unit_file),
                "Recorded systemd service fragment changed"
            );
            if !self.bound(selection, unit).await? {
                let environment = self
                    .run(&["show-environment"], Duration::from_secs(5))
                    .await?;
                let default_home = effective_default_home(&environment);
                ensure!(
                    matches_home(
                        properties,
                        &selection.home,
                        &selection.control.path,
                        default_home.as_deref()
                    ),
                    "Recorded systemd service changed; refusing to override its ExecStart"
                );
            }
            let path = Self::config_dir()?.join(format!("{unit}.d")).join(DROP_IN);
            // The registration is recorded only after inspecting the original
            // unit; adding this drop-in never edits that fragment.
            write_once(
                &path,
                &drop_in_content(&selection.control.path, &selection.home),
            )?
        } else {
            ensure!(
                unit_file.file_name().and_then(|s| s.to_str()) == Some(unit),
                "Invalid systemd unit path"
            );
            let content = format!(
                "# Installed by Paseo; manages only this daemon home.\n[Unit]\nDescription=Paseo daemon\n\n[Service]\nType=exec\nExecStart={} __launcher --home {}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
                quote_systemd(control),
                quote_systemd(home),
            );
            write_once(unit_file, &content)?
        };
        // A previous process may have exited after publishing the file but
        // before reloading it; test the effective binding before deciding.
        if binding_changed || !self.bound(selection, unit).await? {
            self.run(&["daemon-reload"], TOOL_TIMEOUT).await?;
        }
        ensure!(
            self.bound(selection, unit).await?,
            "systemd did not load the fixed control entry for {unit}"
        );
        Ok(())
    }

    async fn bound(&self, selection: &Selection, unit: &str) -> Result<bool> {
        // `show` may fail for a unit that has been written but not loaded.
        let Ok(properties) = self.properties(&[unit]).await else {
            return Ok(false);
        };
        let Some(properties) = properties.first() else {
            return Ok(false);
        };
        let Some(exec) = properties.get("ExecStart") else {
            return Ok(false);
        };
        let expected = &selection.control.path;
        let mut paths = exec_start_paths(exec).into_iter();
        let fixed = paths
            .next()
            .is_some_and(|path| Path::new(&path) == expected)
            && paths.next().is_none()
            && due_for_home(exec, expected, &selection.home, "__launcher");
        let same_fragment = match &selection.registration {
            Some(Registration::SystemdUser { unit_file, .. }) => {
                fragment(properties) == Some(unit_file)
            }
            _ => true, // Originally bound by the managed updater's ExecStart drop-in.
        };
        Ok(fixed
            && same_fragment
            && properties
                .get("LoadState")
                .is_some_and(|state| state == "loaded")
            && properties
                .get("NeedDaemonReload")
                .is_none_or(|state| state == "no"))
    }

    pub(super) async fn start(
        &self,
        selection: &Selection,
        unit: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "Operation cancelled");
        ensure!(
            valid_unit(unit) && self.bound(selection, unit).await?,
            "Recorded systemd service {unit} no longer launches this daemon home"
        );
        if let Some(Registration::SystemdUser { reused: false, .. }) = &selection.registration
            && self.run(&["is-enabled", unit], TOOL_TIMEOUT).await.is_err()
        {
            self.run(&["enable", unit], TOOL_TIMEOUT).await?;
        }
        self.run(&["start", "--no-block", unit], TOOL_TIMEOUT)
            .await?;
        Ok(())
    }
}

fn valid_unit(name: &str) -> bool {
    !name.starts_with(['.', '-'])
        && name.ends_with(".service")
        && name.len() <= 255
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'@'))
}

fn fragment(properties: &BTreeMap<String, String>) -> Option<&Path> {
    properties.get("FragmentPath").map(Path::new)
}

fn systemd_path(path: &Path) -> Result<&str> {
    let text = path.to_str().context("systemd paths must be valid UTF-8")?;
    ensure!(
        !text.chars().any(char::is_control),
        "systemd paths cannot contain control characters"
    );
    Ok(text)
}

fn argv<'a>(exec: &'a str, executable: &Path) -> Option<&'a str> {
    exec.split_once("argv[]=")?
        .1
        .split_once(" ;")?
        .0
        .strip_prefix(executable.to_str()?)?
        .strip_prefix(' ')
}

fn due_for_home(exec: &str, executable: &Path, home: &Path, command: &str) -> bool {
    let Some(args) = argv(exec, executable).and_then(|args| args.strip_prefix(command)) else {
        return false;
    };
    let Some(path) = args
        .strip_prefix(" --home ")
        .or_else(|| args.strip_prefix(" --home="))
    else {
        return false;
    };
    Path::new(path).canonicalize().ok().as_deref() == Some(home)
        || (path.contains("$$")
            && Path::new(&path.replace("$$", "$"))
                .canonicalize()
                .ok()
                .as_deref()
                == Some(home))
}

fn effective_default_home(environment: &str) -> Option<PathBuf> {
    environment
        .lines()
        .find_map(|line| line.strip_prefix("PASEO_HOME="))
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".paseo")))
}

fn matches_home(
    properties: &BTreeMap<String, String>,
    home: &Path,
    control: &Path,
    default_home: Option<&Path>,
) -> bool {
    if properties
        .get("LoadState")
        .is_none_or(|value| value != "loaded")
    {
        return false;
    }
    let Some(exec) = properties.get("ExecStart") else {
        return false;
    };
    let paths = exec_start_paths(exec);
    let [executable] = paths.as_slice() else {
        return false;
    };
    let executable = Path::new(executable);
    if executable == control {
        return due_for_home(exec, executable, home, "__launcher");
    }
    if executable.file_name().and_then(|s| s.to_str()) != Some(EXECUTABLE)
        || properties
            .get("Type")
            .is_some_and(|kind| !matches!(kind.as_str(), "simple" | "exec" | "idle"))
    {
        return false;
    }
    if due_for_home(exec, executable, home, "daemon run") {
        return true;
    }
    if argv(exec, executable) != Some("daemon run")
        || properties
            .get("EnvironmentFiles")
            .is_some_and(|value| !value.is_empty())
    {
        return false;
    }
    let environment = properties
        .get("Environment")
        .map(String::as_str)
        .unwrap_or_default();
    let configured = environment
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix("PASEO_HOME="));
    // Quoted or escaped values cannot be taken from the display grammar safely.
    if configured.is_none() && environment.contains("PASEO_HOME=") {
        return false;
    }
    configured
        .map(Path::new)
        .or(default_home)
        .and_then(|path| path.canonicalize().ok())
        .as_deref()
        == Some(home)
}

fn write_once(path: &Path, text: &str) -> Result<bool> {
    let check = || -> Result<bool> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file()
                && metadata.len() == text.len() as u64
                && fs::read(path)? == text.as_bytes(),
            "Refusing to overwrite existing systemd configuration {}",
            path.display()
        );
        Ok(true)
    };
    if check()? {
        return Ok(false);
    }
    let directory = path
        .parent()
        .context("Missing systemd configuration directory")?;
    fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => sync_directory(directory)?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists && check()? => {
            return Ok(false);
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(true)
}
