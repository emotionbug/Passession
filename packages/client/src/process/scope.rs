//! Transient systemd scopes for the owned processes of a systemd user service.
//!
//! systemd-oomd kills whole cgroups. A process left in its service's cgroup
//! takes the service down with it under memory pressure; one in its own scope
//! is killed alone. Each scope is bound to the service, so stopping the
//! service still stops every process it started.

use std::{
    ffi::OsStr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};

use super::CommandSpec;

/// What `systemd-run --user` needs to reach the user service manager.
const MANAGER_VARIABLES: [&str; 2] = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];

struct Policy {
    systemd_run: String,
    env: String,
    /// The service this process runs in; every scope is bound to it.
    unit: String,
    slice: Option<String>,
    manager_env: Vec<(String, String)>,
}

static POLICY: OnceLock<Policy> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Run later [`super::ManagedProcess`] children in their own transient scopes
/// when this process is a unit of a systemd user manager. Returns the service
/// unit name, or None when this process does not run as such a service. Fails
/// when the service manager refuses a probe scope; nothing is enabled then.
pub async fn enable_service_scopes() -> Result<Option<String>> {
    let cgroup =
        std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
    let Some((unit, slice)) = service_unit(&cgroup) else {
        return Ok(None);
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let (Some(systemd_run), Some(env)) = (find(&path, "systemd-run"), find(&path, "env")) else {
        return Ok(None);
    };
    let manager_env: Vec<_> = MANAGER_VARIABLES
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
        .collect();
    if manager_env.is_empty() {
        return Ok(None);
    }
    let policy = Policy {
        systemd_run,
        env,
        unit,
        slice,
        manager_env,
    };
    let probe = policy.options(&format!("paseo-probe-{}", std::process::id()));
    let output = tokio::process::Command::new(&policy.systemd_run)
        .args(&probe)
        .arg("--")
        .arg(&policy.env)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running systemd-run")?;
    if !output.status.success() {
        bail!(
            "systemd-run refused a probe scope ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let unit = policy.unit.clone();
    let _ = POLICY.set(policy);
    Ok(Some(unit))
}

/// `spec` started in its own scope, once [`enable_service_scopes`] succeeded.
/// None leaves `spec` to run directly: scopes are off, or its program cannot
/// be resolved the way a direct spawn would (that spawn then reports why).
pub fn scoped(spec: &CommandSpec) -> Option<CommandSpec> {
    POLICY.get()?.wrap(spec)
}

impl Policy {
    fn options(&self, name: &str) -> Vec<String> {
        let mut options = vec![
            "--user".to_owned(),
            "--scope".to_owned(),
            "--quiet".to_owned(),
            "--no-ask-password".to_owned(),
            "--collect".to_owned(),
            // Arguments are literal: never expand `${NAME}` in them.
            "--expand-environment=no".to_owned(),
            format!("--unit={name}"),
            format!("--property=BindsTo={}", self.unit),
        ];
        if let Some(slice) = &self.slice {
            options.push(format!("--slice={slice}"));
        }
        options
    }

    fn wrap(&self, spec: &CommandSpec) -> Option<CommandSpec> {
        let program = resolve(spec)?;
        // `env` reads a leading NAME=VALUE argument as an assignment.
        if program.contains('=') {
            return None;
        }
        let mut env = spec.env.clone();
        let mut added = Vec::new();
        if spec.clear_env {
            for (name, value) in &self.manager_env {
                if !env.contains_key(name) {
                    env.insert(name.clone(), value.clone());
                    added.push(name.clone());
                }
            }
        }
        let name = format!(
            "paseo-{}-{}-{}",
            unit_label(&program),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let mut args = self.options(&name);
        args.push("--".to_owned());
        // The command itself never sees variables added only for systemd-run.
        if !added.is_empty() {
            args.push(self.env.clone());
            for name in added {
                args.push("-u".to_owned());
                args.push(name);
            }
        }
        args.push(program);
        args.extend(spec.args.iter().cloned());
        Some(CommandSpec {
            program: self.systemd_run.clone(),
            args,
            cwd: spec.cwd.clone(),
            env,
            clear_env: spec.clear_env,
        })
    }
}

/// The user-manager service named by a cgroup v2 membership, with its slice.
fn service_unit(cgroup: &str) -> Option<(String, Option<String>)> {
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let manager = parts
        .iter()
        .position(|part| part.starts_with("user@") && part.ends_with(".service"))?;
    let (unit, between) = parts[manager + 1..].split_last()?;
    if !unit.ends_with(".service") || between.iter().any(|part| !part.ends_with(".slice")) {
        return None;
    }
    Some((
        (*unit).to_owned(),
        between.last().map(|slice| (*slice).to_owned()),
    ))
}

/// The program a direct spawn of `spec` would execute: a path as given, or
/// a name searched in the child's PATH, falling back to this process's PATH
/// when the child inherits its environment.
fn resolve(spec: &CommandSpec) -> Option<String> {
    if spec.program.contains('/') {
        return Some(spec.program.clone());
    }
    let path = match spec.env.get("PATH") {
        Some(path) => path.into(),
        None if spec.clear_env => return None,
        None => std::env::var_os("PATH")?,
    };
    find(&path, &spec.program)
}

fn find(path: &OsStr, name: &str) -> Option<String> {
    std::env::split_paths(path)
        .map(|directory| directory.join(name))
        .find(|candidate| executable(candidate))
        .and_then(|candidate| candidate.into_os_string().into_string().ok())
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// A unit-name-safe label naming the program, for `systemctl` and oomd logs.
fn unit_label(program: &str) -> String {
    let base = PathBuf::from(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label: String = base
        .chars()
        .take(32)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if label.is_empty() {
        "process".into()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn policy() -> Policy {
        Policy {
            systemd_run: "/usr/bin/systemd-run".into(),
            env: "/usr/bin/env".into(),
            unit: "paseo.service".into(),
            slice: Some("app.slice".into()),
            manager_env: vec![("XDG_RUNTIME_DIR".into(), "/run/user/1000".into())],
        }
    }

    #[test]
    fn only_a_user_manager_service_has_a_scope_owner() {
        assert_eq!(
            service_unit(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/paseo.service\n"
            ),
            Some(("paseo.service".into(), Some("app.slice".into())))
        );
        assert_eq!(
            service_unit("0::/user.slice/user-1000.slice/user@1000.service/paseo.service"),
            Some(("paseo.service".into(), None))
        );
        for foreign in [
            "0::/system.slice/paseo.service",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-paseo-42.scope",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/paseo.service/worker",
            "1:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/paseo.service",
        ] {
            assert_eq!(service_unit(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn a_cleared_environment_gains_only_what_systemd_run_needs_and_the_command_drops_it() {
        let spec = CommandSpec {
            program: "/opt/omp/bin/omp".into(),
            args: vec!["--mode".into(), "rpc ${HOME} $x".into()],
            cwd: Some("/work".into()),
            env: BTreeMap::from([("PATH".into(), "/opt/omp/bin".into())]),
            clear_env: true,
        };
        let scoped = policy().wrap(&spec).unwrap();
        assert_eq!(scoped.program, "/usr/bin/systemd-run");
        assert_eq!(scoped.cwd, spec.cwd);
        assert!(scoped.clear_env);
        assert_eq!(scoped.env["XDG_RUNTIME_DIR"], "/run/user/1000");
        let command = scoped.args.iter().position(|arg| arg == "--").unwrap();
        assert!(scoped.args[..command].contains(&"--scope".to_owned()));
        assert!(scoped.args[..command].contains(&"--expand-environment=no".to_owned()));
        assert!(scoped.args[..command].contains(&"--property=BindsTo=paseo.service".to_owned()));
        assert!(scoped.args[..command].contains(&"--slice=app.slice".to_owned()));
        assert!(
            scoped.args[..command]
                .iter()
                .any(|arg| arg.starts_with("--unit=paseo-omp-"))
        );
        assert_eq!(
            scoped.args[command + 1..],
            [
                "/usr/bin/env",
                "-u",
                "XDG_RUNTIME_DIR",
                "/opt/omp/bin/omp",
                "--mode",
                "rpc ${HOME} $x"
            ]
        );
    }

    #[test]
    fn an_inherited_environment_runs_the_resolved_program_directly() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("tool");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = CommandSpec {
            program: "tool".into(),
            args: vec!["arg".into()],
            cwd: None,
            env: BTreeMap::from([("PATH".into(), directory.path().display().to_string())]),
            clear_env: false,
        };
        let scoped = policy().wrap(&spec).unwrap();
        let command = scoped.args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            scoped.args[command + 1..],
            [program.display().to_string(), "arg".into()]
        );
        assert!(!scoped.env.contains_key("XDG_RUNTIME_DIR"));
    }

    #[test]
    fn an_unresolvable_program_is_left_to_the_direct_spawn() {
        let missing = CommandSpec {
            program: "paseo-test-no-such-program".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::from([("PATH".into(), "/nonexistent".into())]),
            clear_env: true,
        };
        assert!(policy().wrap(&missing).is_none());
        let without_path = CommandSpec {
            env: BTreeMap::new(),
            ..missing.clone()
        };
        assert!(policy().wrap(&without_path).is_none());
        let assignment = CommandSpec {
            program: "/opt/a=b/tool".into(),
            ..missing
        };
        assert!(policy().wrap(&assignment).is_none());
    }
}
