//! Starts the independent local daemon through the bundled CLI, never owning it.

use crate::action::PlatformError;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::sync::OnceCell;

/// Unpacked, platform-native release assets (never inside an archive).
pub(super) struct DaemonRuntime {
    executable: PathBuf,
    working_directory: PathBuf,
    environment: HashMap<String, String>,
}

impl DaemonRuntime {
    #[cfg(target_os = "macos")]
    async fn load_shell_path(&mut self, shell: &Path) -> anyhow::Result<()> {
        if self.environment.contains_key("PATH") {
            return Ok(());
        }
        // Finder supplies only the system PATH. Agent CLIs and their runtimes
        // are installed through the user's interactive login shell.
        let output = paseo_client::process::run(
            paseo_client::process::CommandSpec {
                program: shell.to_string_lossy().into_owned(),
                args: vec![
                    "-ilc".into(),
                    "printf '\\0PASEO_PATH=%s\\0' \"$PATH\"".into(),
                ],
                cwd: None,
                env: Default::default(),
                clear_env: false,
            },
            64 * 1024,
            Duration::from_secs(3),
            Default::default(),
        )
        .await?;
        anyhow::ensure!(output.status == 0, "Login shell PATH query failed");
        let path = output
            .stdout
            .split(|byte| *byte == 0)
            .find_map(|field| field.strip_prefix(b"PASEO_PATH="))
            .filter(|path| !path.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Login shell did not return PATH"))?;
        self.environment
            .insert("PATH".into(), String::from_utf8(path.to_vec())?);
        Ok(())
    }

    /// The selected bundled daemon component, an explicit development manifest,
    /// or the bundled desktop-runtime.json.
    fn locate() -> anyhow::Result<Self> {
        if let Some(store) = super::component_store::Store::discover()? {
            let selected = store.selected()?;
            let reference = &selected[&paseo_protocol::components::ComponentId::Daemon];
            let executable = store.image(reference)?;
            return Ok(Self {
                working_directory: executable
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Missing component runtime directory"))?
                    .to_owned(),
                executable,
                environment: HashMap::new(),
            });
        }
        let manifest = match std::env::var_os("PASEO_DESKTOP_RUNTIME") {
            Some(path) => PathBuf::from(path),
            None => {
                let executable = std::env::current_exe()?;
                let directory = executable.parent().unwrap_or(Path::new("."));
                if cfg!(target_os = "macos") {
                    directory
                        .join("..")
                        .join("Resources")
                        .join("runtime")
                        .join("desktop-runtime.json")
                } else {
                    directory.join("runtime").join("desktop-runtime.json")
                }
            }
        };
        let bytes = std::fs::read(&manifest).map_err(|error| {
            anyhow::anyhow!(
                "Bundled native runtime manifest is missing. Package the native Paseo runtime. ({}: {error})",
                manifest.display()
            )
        })?;
        let data: Value = serde_json::from_slice(&bytes)?;
        let base = std::path::absolute(manifest.parent().unwrap_or(Path::new(".")))?;
        let resolve = |key: &str| -> anyhow::Result<PathBuf> {
            let value = data
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Runtime manifest is missing {key}"))?;
            Ok(normalize(&base.join(value)))
        };
        let environment = data
            .get("environment")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            executable: resolve("executable")?,
            working_directory: resolve("workingDirectory")?,
            environment,
        })
    }
}

/// Lexical normalization (path package `p.normalize`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) fn home() -> Result<PathBuf, PlatformError> {
    let home = std::env::var_os("PASEO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".paseo")))
        .ok_or_else(|| PlatformError::new("DAEMON_HOME", "Cannot locate the local daemon home."))?;
    Ok(std::path::absolute(home)?)
}

fn daemon_command(
    runtime: &DaemonRuntime,
    args: impl IntoIterator<Item = OsString>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&runtime.executable);
    command
        .args(args)
        .current_dir(&runtime.working_directory)
        .envs(&runtime.environment)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .env_remove("PASEO_UPDATE_OPERATION_ID")
        .env_remove("PASEO_UPDATE_OPERATION_DIRECTORY")
        .env_remove("PASEO_UPDATE_NONCE")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
}

async fn run(runtime: &DaemonRuntime, args: Vec<OsString>) -> Result<Value, PlatformError> {
    let mut command = daemon_command(runtime, args);
    let output = tokio::time::timeout(Duration::from_secs(180), command.output())
        .await
        .map_err(|_| PlatformError::new("DAEMON_TIMEOUT", "Local daemon command timed out."))??;
    let result = match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(result) if output.status.success() => return Ok(result),
        result => result,
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = result
        .as_ref()
        .ok()
        .and_then(|value| {
            value["error"]
                .as_str()
                .or_else(|| value["error"]["message"].as_str())
        })
        .filter(|s| !s.is_empty())
        .or_else(|| (!stderr.trim().is_empty()).then(|| stderr.trim()))
        .unwrap_or("The bundled CLI did not return a valid local daemon status.");
    Err(PlatformError::new("DAEMON_COMMAND_FAILED", message))
}

async fn runtime(cached: &OnceCell<DaemonRuntime>) -> Result<&DaemonRuntime, PlatformError> {
    cached
        .get_or_try_init(|| async {
            let runtime = tokio::task::spawn_blocking(DaemonRuntime::locate)
                .await
                .map_err(|e| PlatformError::native(e.to_string()))?
                .map_err(|e| PlatformError::new("RUNTIME_MISSING", e.to_string()))?;
            #[cfg(target_os = "macos")]
            let runtime = {
                let mut runtime = runtime;
                let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
                if let Err(error) = runtime.load_shell_path(Path::new(&shell)).await {
                    makepad_widgets::log!("Login shell PATH unavailable: {error}");
                }
                runtime
            };
            Ok(runtime)
        })
        .await
}

fn arguments(command: &str, home: &Path, runtime: Option<&Path>) -> Vec<OsString> {
    let mut args = vec![
        "daemon".into(),
        command.into(),
        "--home".into(),
        home.into(),
    ];
    if let Some(runtime) = runtime {
        args.extend(["--runtime".into(), runtime.as_os_str().to_owned()]);
    }
    args.push("--json".into());
    args
}

pub(super) async fn status(
    cached: &OnceCell<DaemonRuntime>,
    home: &Path,
) -> Result<Value, PlatformError> {
    if !tokio::fs::try_exists(home.join("paseo.pid")).await? {
        return Ok(json!({"status":"stopped", "home":home}));
    }
    // Use the CLI's PID/boot/birth checks, not the presence of a stale lock file.
    let mut observed = run(runtime(cached).await?, arguments("status", home, None)).await?;
    observed["status"] = match observed["localDaemon"].as_str() {
        Some("running") => "running",
        Some("not_ready") => "starting",
        Some("stopped") => "stopped",
        _ => {
            return Err(PlatformError::new(
                "DAEMON_STATUS",
                "Invalid local daemon status.",
            ));
        }
    }
    .into();
    if observed["desktopManaged"] == true {
        observed["owner"] = "desktop-legacy".into();
        observed["migration"] = "pending".into();
    }
    Ok(observed)
}

pub(super) async fn ensure(
    cached: &OnceCell<DaemonRuntime>,
    home: &Path,
    enabled: bool,
    observed: Value,
    starting: impl FnOnce(),
) -> Result<Value, PlatformError> {
    if !should_ensure(enabled, &observed) {
        return Ok(observed);
    }
    let runtime = runtime(cached).await?;
    ensure_with(runtime, home, enabled, observed, starting, |args| {
        run(runtime, args)
    })
    .await
}

fn should_ensure(enabled: bool, observed: &Value) -> bool {
    enabled
        && (observed["status"] == "stopped"
            || (observed["status"] == "running" && observed["owner"] == "desktop-legacy"))
}

async fn ensure_with(
    runtime: &DaemonRuntime,
    home: &Path,
    enabled: bool,
    observed: Value,
    starting: impl FnOnce(),
    run: impl AsyncFnOnce(Vec<OsString>) -> Result<Value, PlatformError>,
) -> Result<Value, PlatformError> {
    if !should_ensure(enabled, &observed) {
        return Ok(observed);
    }
    starting();
    let args = arguments("ensure", home, Some(&runtime.working_directory));
    let mut result = run(args).await?;
    match result["state"].as_str() {
        Some("running" | "started" | "installed") => {
            result["status"] = "running".into();
            result["home"] = json!(home);
            Ok(result)
        }
        _ => Err(PlatformError::new(
            "DAEMON_ENSURE_FAILED",
            result["error"]
                .as_str()
                .unwrap_or("Local daemon did not become ready."),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn login_shell_path_reaches_daemon_and_agent_clis() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let tools = directory.path().join("agent tools");
        std::fs::create_dir(&tools).unwrap();
        let shell = tools.join("login shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\n[ \"$1\" = '-ilc' ] || exit 7\nprintf 'shell startup banner\\n'\nprintf '\\0PASEO_PATH=%s\\0' \"${0%/*}\"\n",
        ).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        for cli in ["codex", "claude"] {
            let program = tools.join(cli);
            std::fs::write(&program, "#!/bin/sh\nprintf 'agent-version\\n'\n").unwrap();
            std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut runtime = DaemonRuntime {
            executable: "/usr/bin/env".into(),
            working_directory: directory.path().to_owned(),
            environment: HashMap::new(),
        };
        runtime.load_shell_path(&shell).await.unwrap();
        for cli in ["codex", "claude"] {
            let output = daemon_command(&runtime, [cli.into(), "--version".into()])
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"agent-version\n");
        }
        runtime
            .environment
            .insert("PATH".into(), "/explicit/runtime/path".into());
        runtime
            .load_shell_path(Path::new("/missing/shell"))
            .await
            .unwrap();
        assert_eq!(runtime.environment["PATH"], "/explicit/runtime/path");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn failed_shell_query_preserves_the_inherited_environment() {
        let directory = tempfile::tempdir().unwrap();
        let mut runtime = DaemonRuntime {
            executable: "/usr/bin/env".into(),
            working_directory: directory.path().to_owned(),
            environment: HashMap::from([("EXPLICIT".into(), "preserved".into())]),
        };
        assert!(
            runtime
                .load_shell_path(Path::new("/usr/bin/false"))
                .await
                .is_err()
        );
        assert!(!runtime.environment.contains_key("PATH"));
        assert_eq!(runtime.environment["EXPLICIT"], "preserved");
    }

    #[tokio::test]
    async fn startup_only_ensures_stopped_or_legacy_daemons_when_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("daemon-home");
        let bundled = directory.path().join("bundle");
        let runtime = DaemonRuntime {
            executable: bundled.join("paseo"),
            working_directory: bundled.clone(),
            environment: HashMap::new(),
        };
        for (enabled, status, owner, expect_ensure) in [
            (false, "stopped", "", false),
            (true, "running", "detached", false),
            (true, "starting", "", false),
            (false, "running", "desktop-legacy", false),
            (true, "running", "desktop-legacy", true),
            (true, "stopped", "", true),
        ] {
            let observed = json!({"status": status, "owner": owner});
            let starting = std::cell::Cell::new(false);
            let ran = std::cell::Cell::new(false);
            let result = ensure_with(
                &runtime,
                &home,
                enabled,
                observed.clone(),
                || starting.set(true),
                async |args| {
                    ran.set(true);
                    assert_eq!(args, [
                        "daemon".into(), "ensure".into(), "--home".into(),
                        home.as_os_str().to_owned(), "--runtime".into(),
                        bundled.as_os_str().to_owned(), "--json".into(),
                    ]);
                    Ok(json!({"state":"installed","pid":42,"owner":"detached","listen":"127.0.0.1:16767"}))
                },
            )
            .await
            .unwrap();
            assert_eq!(starting.get(), expect_ensure);
            assert_eq!(ran.get(), expect_ensure);
            if expect_ensure {
                assert_eq!(result["status"], "running");
                assert_eq!(result["home"], json!(home));
            } else {
                assert_eq!(result, observed);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn daemon_cli_console_fixture() {
        if std::env::var_os("PASEO_HELPER_CONSOLE_PROBE").is_none() {
            return;
        }
        use windows::Win32::System::Console::GetConsoleWindow;
        assert!(unsafe { GetConsoleWindow() }.0.is_null());
        let mut input = String::new();
        assert_eq!(std::io::stdin().read_line(&mut input).unwrap(), 0);
        println!("daemon-cli-output");
        eprintln!("daemon-cli-error");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn desktop_daemon_cli_starts_without_console_and_captures_output() {
        let executable = std::env::current_exe().unwrap();
        let runtime = DaemonRuntime {
            working_directory: executable.parent().unwrap().to_owned(),
            executable,
            environment: HashMap::new(),
        };
        let mut command = daemon_command(
            &runtime,
            [
                "--exact",
                "desktop::daemon_service::tests::daemon_cli_console_fixture",
                "--nocapture",
            ]
            .map(OsString::from),
        );
        command.env("PASEO_HELPER_CONSOLE_PROBE", "1");
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("daemon-cli-output"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("daemon-cli-error"));
    }
}
