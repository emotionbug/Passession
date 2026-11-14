use super::{duration, error, pretty, set_path};
use crate::cli::{Context, flag, option, output::Output, parse_duration, positional, string, switch};
use crate::{
    client::ConnectOptions,
    daemon_admin::{
        self as admin,
        update_operation::{self, ResumeRequest, SubmitRequest, UpdateSource},
    },
    daemon_control::{self as control, DaemonInstance, StartOptions, StopOptions},
};
use paseo_client::{ClientError, DaemonClient, DaemonTarget};
use anyhow::{Context as _, Result, bail, ensure};
use clap::{ArgMatches, Command};
use serde_json::{Value, json};
use std::{path::{Path, PathBuf}, time::Duration};
use tokio_util::sync::CancellationToken;

const REMOVED_FLAGS: &[(&str, &str, bool)] = &[
    ("port", "daemon.listen", true),
    ("listen", "daemon.listen", true),
    ("relay", "daemon.relay.enabled", false),
    ("no-relay", "daemon.relay.enabled", false),
    ("relay-use-tls", "daemon.relay.useTls", false),
    ("no-mcp", "daemon.mcp.enabled", false),
    ("no-inject-mcp", "daemon.mcp.injectIntoAgents", false),
    ("hostnames", "daemon.hostnames", true),
    ("allowed-hosts", "daemon.hostnames", true),
    ("foreground", "", false),
];
fn launch_options(mut command: Command) -> Command {
    for &(key, _, value) in REMOVED_FLAGS {
        command = command.arg(if value {
            option(key, key).hide(true)
        } else {
            switch(key, key).hide(true)
        });
    }
    command
}
pub fn command() -> Command {
    Command::new("daemon")
        .about("Manage the Paseo daemon")
        .subcommand_required(true)
        .subcommand(launch_options(
            Command::new("start").arg(option("timeout", "timeout")),
        ))
        .subcommand(launch_options(Command::new("run")))
        .subcommand(Command::new("status"))
        .subcommand(
            Command::new("stop")
                .arg(option("timeout", "timeout"))
                .arg(option("kill-timeout", "kill-timeout"))
                .arg(switch("force", "force")),
        )
        .subcommand(launch_options(
            Command::new("restart").arg(option("timeout", "timeout")),
        ))
        .subcommand(Command::new("reload"))
        .subcommand(
            Command::new("update")
                .about("Replace the daemon runtime as a durable operation; status and wait only observe it")
                .after_help("Submit: paseo daemon update [--from <complete-runtime>] [--operation-id <uuid>] [--resume-agent <agent-id> --continuation-file <file>] [--idle-timeout <duration>] [--force-running]\nObserve: paseo daemon update status <operation-id>\n         paseo daemon update wait <operation-id>\n--from and --continuation-file read files on this computer and require a local --home. Without --from the latest official release is installed.")
                .arg(positional("action", false).value_parser(["status", "wait"]))
                .arg(positional("operation", false))
                .arg(option("from", "from"))
                .arg(option("operation-id", "operation-id"))
                .arg(option("resume-agent", "resume-agent").requires("continuation-file"))
                .arg(option("continuation-file", "continuation-file").requires("resume-agent"))
                .arg(option("idle-timeout", "idle-timeout"))
                .arg(switch("force-running", "force-running")),
        )
        .subcommand(Command::new("pair").arg(switch("relay", "relay")))
        .subcommand(Command::new("set-password"))
        .subcommand(
            Command::new("config")
                .subcommand_required(true)
                .subcommand(Command::new("get").arg(positional("path", false)))
                .subcommand(
                    Command::new("set")
                        .arg(positional("path", true))
                        .arg(positional("value", true))
                        .arg(switch("string", "string")),
                )
                .subcommand(Command::new("unset").arg(positional("path", true))),
        )
}
pub async fn execute(path: &[&str], m: &ArgMatches, ctx: &Context) -> Result<Output> {
    if matches!(path, ["start"] | ["run"] | ["restart"]) {
        for &(key, config, value) in REMOVED_FLAGS {
            if (value && string(m, key).is_some()) || (!value && flag(m, key)) {
                let guidance = if config.is_empty() {
                    "Use paseo daemon run --home <path> for foreground deployment.".to_owned()
                } else {
                    format!(
                        "Use paseo daemon config set {config} <value> --home <path>, then start or restart."
                    )
                };
                return Err(error(
                    "REMOVED_LAUNCH_OPTION",
                    format!(
                        "--{key} was removed. {guidance} Deployment environment overrides belong to paseo daemon run."
                    ),
                ));
            }
        }
    }
    match path {
        ["start"] | ["run"] => {
            let home = ctx.local_home()?;
            let foreground = path[0] == "run";
            let started = control::start_daemon_instance(
                StartOptions {
                    home: home.clone(),
                    foreground,
                    deployment: foreground,
                    timeout: duration(m, "timeout", 600.0)?,
                    ..Default::default()
                },
                &ctx.cancel,
            )
            .await?;
            let action = if !started.spawned {
                "already_running"
            } else if foreground {
                "exited"
            } else {
                "started"
            };
            let data = if foreground {
                json!({"action":action,"pid":started.instance.pid})
            } else {
                json!({"action":action,"home":home,"pid":started.instance.pid,"listen":started.instance.listen,"logPath":control::daemon_log_path(&home)})
            };
            let mut output = Output::new(data)
                .id("pid")
                .exit(started.exit_code.unwrap_or(0));
            if !foreground {
                output = output.human(format!(
                    "{}: PID {}{}\nLogs: {}",
                    if started.spawned {
                        "Started"
                    } else {
                        "Already running"
                    },
                    started.instance.pid,
                    started
                        .instance
                        .listen
                        .as_ref()
                        .map(|listen| format!(", listening on {listen}"))
                        .unwrap_or_else(|| ", not ready".into()),
                    control::daemon_log_path(&home).display()
                ));
            }
            Ok(output)
        }
        ["status"] => {
            let data = observe_status(&ctx.target, &ctx.cancel).await?;
            let human = status_text(&data)?;
            Ok(Output::new(data).quiet_text("daemon").human(human))
        }
        ["stop"] => {
            let timeout = duration(m, "timeout", 15.0)?;
            match &ctx.target {
                DaemonTarget::Instance { home } => {
                    let stopped = control::stop_daemon_instance(
                        home,
                        StopOptions {
                            force: flag(m, "force"),
                            timeout,
                            kill_timeout: duration(m, "kill-timeout", 3.0)?,
                            ..Default::default()
                        },
                        &ctx.cancel,
                    )
                    .await?;
                    let human = format!("{}: {}", stopped.action.replace('_', " "), home.display());
                    Ok(Output::new(json!({"action":stopped.action,"pid":stopped.pid,"forced":stopped.forced,"usedLifecycleRpc":stopped.used_lifecycle_rpc,"home":home})).id("action").human(human))
                }
                DaemonTarget::Endpoint { .. } => {
                    ensure!(
                        !flag(m, "force"),
                        "--force requires a local --home; an endpoint gives no remote process authority."
                    );
                    let client = ctx.connect().await?;
                    let result = client.shutdown(timeout).await;
                    client.close().await;
                    result?;
                    Ok(Output::new(
                        json!({"action":"shutdown_requested","host":ctx.target.description()}),
                    )
                    .id("action")
                    .human("Shutdown requested; remote process exit was not verified."))
                }
            }
        }
        ["restart"] => {
            let data = restart_confirmed(
                &ctx.target,
                None,
                "cli_restart",
                duration(m, "timeout", 600.0)?,
                &ctx.cancel,
            )
            .await?;
            let human = format!(
                "Restarted worker {} → {} at {}. Supervisor launch retained.",
                data["previousWorkerPid"],
                data["workerPid"],
                ctx.target.description()
            );
            Ok(Output::new(data).id("action").human(human))
        }
        ["update"] => update(m, ctx).await,
        ["reload"] => {
            let client = ctx.connect().await?;
            let result = reload(&client).await;
            client.close().await;
            let payload = result?;
            let mut data = json!({"appliedPaths":payload["appliedPaths"],"restartRequiredPaths":payload["restartRequiredPaths"],"overrideControlledPaths":payload["overrideControlledPaths"]});
            let selector = target_selector(&ctx.target)?;
            let restart = format!("paseo daemon restart {selector}");
            data["restartCommand"] = json!(restart);
            let mut human = "Configuration reloaded.".to_owned();
            for (field, heading) in [
                (
                    "restartRequiredPaths",
                    "These changes require a daemon restart:",
                ),
                (
                    "overrideControlledPaths",
                    "These settings are controlled by daemon launch overrides:",
                ),
            ] {
                if let Some(paths) = data[field].as_array().filter(|a| !a.is_empty()) {
                    human.push_str(&format!("\n\nWarning: {heading}\n"));
                    for path in paths {
                        human.push_str(&format!("  {}\n", path.as_str().unwrap_or("")));
                    }
                    if field == "restartRequiredPaths" {
                        human.push_str(&format!("\nRun: {restart}"));
                    }
                }
            }
            Ok(Output::new(data).quiet_text("daemon-config").human(human))
        }
        ["pair"] => {
            let offer = pairing(&ctx.target, flag(m, "relay"), &ctx.cancel).await?;
            if offer["relayEnabled"] != true || offer["url"].as_str().is_none_or(str::is_empty) {
                return Err(error(
                    "RELAY_DISABLED",
                    "Relay pairing is disabled for this daemon. Run paseo daemon pair --relay to enable it.",
                ));
            }
            let text = format!(
                "{}\n{}\n{}",
                offer["qr"].as_str().unwrap_or(""),
                offer["url"].as_str().unwrap_or(""),
                offer["connectionUri"].as_str().unwrap_or("")
            );
            Ok(Output::new(offer).human(text))
        }
        ["set-password"] => set_password(ctx).await,
        ["config", action] => configuration(action, m, ctx).await,
        _ => bail!("Unknown daemon command"),
    }
}
async fn set_password(ctx: &Context) -> Result<Output> {
    use std::io::IsTerminal;
    ensure!(
        std::io::stdin().is_terminal(),
        error(
            "PASSWORD_TTY_REQUIRED",
            "paseo daemon set-password needs a terminal to read the password. Run it in an interactive terminal, or set PASEO_PASSWORD in the daemon's environment instead."
        )
    );
    let home = ctx.local_home()?;
    let password = super::prompts::password(&ctx.cancel).await?;
    ensure!(
        !ctx.cancel.is_cancelled(),
        error("PASSWORD_CANCELLED", "Password update cancelled")
    );
    let mut config = crate::config::read_persisted(&home, false).await?;
    let hash = crate::transport::hash_daemon_password(&password).await?;
    set_path(&mut config, "daemon.auth.password", Some(json!(hash)))?;
    crate::config::save_persisted(&home, &config).await?;
    let restart = format!(
        "paseo daemon restart --home {}",
        serde_json::to_string(&home)?
    );
    let message = format!(
        "Password written to {}\nRestart the daemon for the change to take effect.\nRun: {restart}",
        home.join("config.json").display()
    );
    Ok(Output::new(json!({"action":"password_set","configPath":home.join("config.json"),"restartCommand":restart,"message":message})).id("action").human(message).color_column("action",|_,_|Some(32)))
}
async fn reload(client: &DaemonClient) -> Result<Value> {
    client.require_feature(
        "daemonConfigReload",
        "Update the Paseo daemon before reloading configuration.",
    )?;
    client
        .request(
            json!({"type":"daemon.config.reload.request"}),
            "daemon.config.reload.response",
            Duration::from_secs(30),
        )
        .await
}
fn target_selector(target: &DaemonTarget) -> Result<String> {
    Ok(match target {
        DaemonTarget::Instance { home } => format!("--home {}", serde_json::to_string(home)?),
        DaemonTarget::Endpoint { .. } => format!(
            "--host {}",
            serde_json::to_string(&target.description())?
        ),
    })
}

const UPDATE_SUBMIT_OPTIONS: &[&str] = &[
    "from",
    "operation-id",
    "resume-agent",
    "continuation-file",
    "idle-timeout",
];
const MAX_CONTINUATION_BYTES: u64 = 256 * 1024;
const REMOTE_UPDATE_FEATURE: &str = "durableDaemonUpdates";
/// How long a remote submit keeps retrying the same operation ID after its outcome became unknown.
const REMOTE_SUBMIT_RETRY: Duration = Duration::from_secs(120);
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `daemon update` submits to the shared durable engine; `status` and `wait` only
/// observe it. Local homes use the operation journal directly, so status works while
/// the daemon is down and ending this process never cancels an admitted operation.
async fn update(m: &ArgMatches, ctx: &Context) -> Result<Output> {
    if let Some(action) = string(m, "action") {
        let submit_option = UPDATE_SUBMIT_OPTIONS
            .iter()
            .copied()
            .find(|key| string(m, key).is_some())
            .or_else(|| flag(m, "force-running").then_some("force-running"));
        if let Some(key) = submit_option {
            return Err(error(
                "UPDATE_OPTION_CONFLICT",
                format!(
                    "--{key} configures a new update; paseo daemon update {action} only observes an existing operation."
                ),
            ));
        }
        let raw = string(m, "operation").ok_or_else(|| {
            error(
                "OPERATION_ID_REQUIRED",
                format!("Usage: paseo daemon update {action} <operation-id>"),
            )
        })?;
        let id = admin::update_operation_id(raw)?;
        let operation = match (action, &ctx.target) {
            ("status", DaemonTarget::Instance { home }) => update_operation::status(home, &id).await?,
            ("status", DaemonTarget::Endpoint { .. }) => remote_status(ctx, &id).await?,
            (_, DaemonTarget::Instance { home }) => {
                match update_operation::wait(home, &id, &ctx.cancel).await {
                    Err(_) if ctx.cancel.is_cancelled() => {
                        return Err(error(
                            "UPDATE_OBSERVER_CANCELLED",
                            format!("Stopped observing update operation {id}; the update itself continues."),
                        ));
                    }
                    observed => observed?,
                }
            }
            (_, DaemonTarget::Endpoint { .. }) => remote_wait(ctx, &id).await?,
        };
        let code = if action == "wait" {
            wait_exit_code(&operation)
        } else {
            0
        };
        let human = update_text(&operation);
        return Ok(Output::new(operation)
            .id("operationId")
            .exit(code)
            .human(human));
    }
    // The identity exists before anything is sent, so a lost acknowledgement is
    // retried with the same ID instead of starting a second replacement.
    let operation_id = match string(m, "operation-id") {
        Some(raw) => admin::update_operation_id(raw)?,
        None => uuid::Uuid::new_v4().hyphenated().to_string(),
    };
    let idle = match string(m, "idle-timeout") {
        Some(raw) => {
            parse_duration(raw).map_err(|e| error("INVALID_IDLE_TIMEOUT", e.to_string()))?
        }
        None => admin::DEFAULT_UPDATE_IDLE_TIMEOUT,
    };
    let idle_timeout_ms = admin::update_idle_timeout_ms(idle)?;
    let force_running = flag(m, "force-running");
    let submitted = match &ctx.target {
        DaemonTarget::Endpoint { .. } => {
            if let Some(key) = ["from", "resume-agent", "continuation-file"]
                .into_iter()
                .find(|key| string(m, key).is_some())
            {
                return Err(error(
                    "LOCAL_HOME_REQUIRED",
                    format!(
                        "--{key} requires a local --home: runtimes and continuations are read from files on this computer. --host can only install official releases."
                    ),
                ));
            }
            remote_submit(ctx, &operation_id, idle_timeout_ms, force_running).await
        }
        DaemonTarget::Instance { home } => {
            let source = match string(m, "from") {
                // Keep the supplied leaf intact until admission rejects symlinks
                // and pins its bytes. Canonicalizing here would hide a symlink.
                Some(raw) => UpdateSource::Local(if Path::new(raw).is_absolute() {
                    PathBuf::from(raw)
                } else {
                    std::env::current_dir()?.join(raw)
                }),
                None => UpdateSource::Release,
            };
            let resume = match (string(m, "resume-agent"), string(m, "continuation-file")) {
                (Some(agent), Some(file)) => Some(ResumeRequest {
                    agent_id: resume_agent_id(agent)?,
                    text: continuation_text(Path::new(file)).await?,
                }),
                (None, None) => None,
                _ => {
                    return Err(error(
                        "CONTINUATION_INCOMPLETE",
                        "--resume-agent and --continuation-file must be given together.",
                    ));
                }
            };
            let executor = std::env::current_exe()
                .context("Cannot locate this paseo executable for the update executor handoff")?;
            let request = SubmitRequest {
                operation_id: operation_id.clone(),
                source,
                resume,
                idle_timeout_ms,
                force_running,
            };
            update_operation::submit(home, &executor, request).await
        }
    };
    let operation = submitted.map_err(|failure| unconfirmed(&operation_id, failure))?;
    let selector = target_selector(&ctx.target)?;
    let human = format!(
        "{}\nObserve: paseo daemon update wait {operation_id} {selector}\nStatus:  paseo daemon update status {operation_id} {selector}",
        update_text(&operation)
    );
    Ok(Output::new(operation).id("operationId").human(human))
}
fn resume_agent_id(raw: &str) -> Result<String> {
    let id = raw.trim();
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(|c| c.is_whitespace() || c.is_control()),
        error("INVALID_RESUME_AGENT", "--resume-agent must name one existing Paseo agent ID.")
    );
    Ok(id.to_owned())
}
/// Frozen before submission: the operation owns this text even if the file later changes or disappears.
async fn continuation_text(path: &Path) -> Result<String> {
    let invalid = |reason: String| {
        error(
            "INVALID_CONTINUATION_FILE",
            format!("--continuation-file {}: {reason}", path.display()),
        )
    };
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| invalid(e.to_string()))?;
    if !metadata.is_file() {
        return Err(invalid("not a regular file".into()));
    }
    if metadata.len() > MAX_CONTINUATION_BYTES {
        return Err(invalid(format!("larger than {MAX_CONTINUATION_BYTES} bytes")));
    }
    let bytes = tokio::fs::read(path).await.map_err(|e| invalid(e.to_string()))?;
    if bytes.len() as u64 > MAX_CONTINUATION_BYTES {
        return Err(invalid(format!("larger than {MAX_CONTINUATION_BYTES} bytes")));
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid("not UTF-8 text".into()))?;
    if text.trim().is_empty() {
        return Err(invalid("empty".into()));
    }
    Ok(text)
}
/// The outcome of an unacknowledged submit is unknown; name the ID that makes a retry safe.
fn unconfirmed(operation_id: &str, failure: anyhow::Error) -> anyhow::Error {
    let code = failure
        .downcast_ref::<ClientError>()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| "UPDATE_SUBMIT_FAILED".into());
    ClientError::new(
        &code,
        format!(
            "{failure:#}\nOperation {operation_id}: retry with --operation-id {operation_id}, or inspect paseo daemon update status {operation_id}."
        ),
    )
    .details(json!({"operationId":operation_id}))
    .into()
}
async fn remote_client(ctx: &Context) -> Result<DaemonClient> {
    let mut options = ConnectOptions::new(ctx.target.clone());
    options.timeout = Duration::from_secs(5);
    let client = crate::client::connect(options, ctx.cancel.child_token()).await?;
    if client.server_info()["features"][REMOTE_UPDATE_FEATURE] != true {
        client.close().await;
        return Err(error(
            "DURABLE_UPDATE_UNSUPPORTED",
            "This daemon does not offer durable updates. Update it from its own host with paseo daemon update --home.",
        ));
    }
    Ok(client)
}
fn operation_payload(payload: Value) -> Result<Value> {
    payload
        .get("operation")
        .filter(|operation| operation.is_object())
        .cloned()
        .context("Daemon update response did not include the operation")
}
async fn pause(ctx: &Context, operation_id: &str) -> Result<()> {
    tokio::select! {
        _ = ctx.cancel.cancelled() => Err(error(
            "UPDATE_OBSERVER_CANCELLED",
            format!("Stopped observing update operation {operation_id}; the update itself continues."),
        )),
        _ = tokio::time::sleep(REMOTE_POLL_INTERVAL) => Ok(()),
    }
}
async fn remote_submit(
    ctx: &Context,
    operation_id: &str,
    idle_timeout_ms: u64,
    force_running: bool,
) -> Result<Value> {
    let message = json!({"type":"daemon.update.submit.request","operationId":operation_id,"source":"release","idleTimeoutMs":idle_timeout_ms,"forceRunning":force_running});
    let deadline = tokio::time::Instant::now() + REMOTE_SUBMIT_RETRY;
    let mut sent = false;
    let retry = |sent: bool, failure: &anyhow::Error| {
        sent && reconnect_failure(failure) && tokio::time::Instant::now() < deadline
    };
    loop {
        let client = match remote_client(ctx).await {
            Ok(client) => client,
            Err(failure) if retry(sent, &failure) => {
                pause(ctx, operation_id).await?;
                continue;
            }
            Err(failure) => return Err(failure),
        };
        sent = true;
        let result = client
            .request(message.clone(), "daemon.update.submit.response", REMOTE_REQUEST_TIMEOUT)
            .await;
        client.close().await;
        match result {
            Ok(payload) => return operation_payload(payload),
            // Same ID + same input returns the admitted operation instead of a second update.
            Err(failure) if retry(sent, &failure) => pause(ctx, operation_id).await?,
            Err(failure) => return Err(failure),
        }
    }
}
async fn remote_status(ctx: &Context, operation_id: &str) -> Result<Value> {
    Ok(remote_status_from(ctx, operation_id).await?.0)
}
/// The operation view and the server identity that answered for it.
async fn remote_status_from(ctx: &Context, operation_id: &str) -> Result<(Value, Value)> {
    let client = remote_client(ctx).await?;
    let result = client
        .request(
            json!({"type":"daemon.update.status.request","operationId":operation_id}),
            "daemon.update.status.response",
            REMOTE_REQUEST_TIMEOUT,
        )
        .await;
    let server_id = client.server_info()["serverId"].clone();
    client.close().await;
    Ok((operation_payload(result?)?, server_id))
}
/// Remote observer: survives the daemon's own restart by reconnecting, refuses a
/// different server identity, and never sends anything that could cancel the update.
async fn remote_wait(ctx: &Context, operation_id: &str) -> Result<Value> {
    let mut server: Option<Value> = None;
    loop {
        match remote_status_from(ctx, operation_id).await {
            Ok((operation, observed)) => {
                match &server {
                    Some(expected) if *expected != observed => {
                        return Err(error(
                            "DAEMON_REPLACED",
                            format!(
                                "The endpoint now answers as server {observed}, not {expected}; update operation {operation_id} cannot be observed there."
                            ),
                        ));
                    }
                    Some(_) => {}
                    None => server = Some(observed),
                }
                if admin::update_terminal(&operation) {
                    return Ok(operation);
                }
            }
            Err(failure) if reconnect_failure(&failure) => {}
            Err(failure) => return Err(failure),
        }
        pause(ctx, operation_id).await?;
    }
}
/// `wait` succeeds only for a committed replacement; session resume is reported separately.
fn wait_exit_code(operation: &Value) -> i32 {
    if operation["phase"] == "committed" { 0 } else { 1 }
}
fn runtime_text(runtime: &Value) -> Option<String> {
    let runtime = runtime.as_object()?;
    let version = runtime.get("version").and_then(Value::as_str).unwrap_or("unknown");
    let digest = runtime
        .get("executableSha256")
        .and_then(Value::as_str)
        .map(|sha| format!(" sha256:{}", sha.chars().take(12).collect::<String>()))
        .unwrap_or_default();
    let provenance = match runtime.get("release").and_then(Value::as_bool) {
        Some(true) => " (official release)",
        Some(false) => " (local build)",
        None => "",
    };
    Some(format!("{version}{digest}{provenance}"))
}
pub(crate) fn update_text(operation: &Value) -> String {
    let id = operation["operationId"].as_str().unwrap_or("unknown");
    let phase = operation["phase"].as_str().unwrap_or("unknown");
    let mut lines = vec![format!("Operation: {id}"), format!("Phase: {phase}")];
    lines.push(
        match phase {
            "committed" => "Update committed: the replacement daemon passed readiness and identity checks.",
            "rolled_back" => "Update rolled back: the previous runtime was restored.",
            "failed" => "Update failed; the original runtime, candidate and diagnostics are preserved.",
            _ if operation["accepted"] == true => "Update accepted; its executor completes it independently of this command.",
            _ => "Update not yet accepted.",
        }
        .to_owned(),
    );
    for (label, key) in [("Previous", "previous"), ("Candidate", "candidate")] {
        if let Some(text) = runtime_text(&operation[key]) {
            lines.push(format!("{label}: {text}"));
        }
    }
    if let Some(error) = operation["error"].as_str().filter(|e| !e.is_empty()) {
        lines.push(format!("Error: {error}"));
    }
    match &operation["rollback"] {
        Value::Null => {}
        Value::String(rollback) => lines.push(format!("Rollback: {rollback}")),
        rollback => lines.push(format!("Rollback: {rollback}")),
    }
    if let Some(continuation) = operation["continuation"].as_object() {
        let state = continuation.get("state").and_then(Value::as_str).unwrap_or("unknown");
        let mut line = format!(
            "Session resume: {state} (agent {}, message {})",
            continuation.get("agentId").and_then(Value::as_str).unwrap_or("unknown"),
            continuation.get("messageId").and_then(Value::as_str).unwrap_or("unknown")
        );
        if let Some(error) = continuation.get("error").and_then(Value::as_str) {
            line.push_str(&format!(": {error}"));
        }
        lines.push(line);
        if phase == "committed" && state != "accepted" {
            lines.push(format!(
                "The update is committed; the session resume is {} and does not change the update result.",
                if state == "pending" { "still pending" } else { "not confirmed" }
            ));
        }
    }
    lines.join("\n")
}
pub(crate) async fn assert_instance(home: &Path, expected: &DaemonInstance) -> Result<()> {
    let current = control::read_daemon_instance(home).await?;
    if !current
        .as_ref()
        .is_some_and(|current| control::same_daemon_instance(expected, current))
    {
        return Err(error(
            "DAEMON_REPLACED",
            "Daemon changed since confirmation; inspect its current home, PID, and start time.",
        ));
    }
    Ok(())
}
pub(crate) async fn observe_status(
    target: &DaemonTarget,
    cancel: &CancellationToken,
) -> Result<Value> {
    let instance = match target {
        DaemonTarget::Instance { home } => control::read_daemon_instance(home).await?,
        _ => None,
    };
    let mut local = match target {
        DaemonTarget::Instance { home } => {
            let config = crate::config::resolve_persisted_for_pairing(home).await?;
            let local_daemon = match &instance {
                None => "stopped",
                Some(instance) if instance.listen.is_some() => "running",
                Some(_) => "not_ready",
            };
            json!({"home":home,"pid":instance.as_ref().map(|i|i.pid),"startedAt":instance.as_ref().map(|i|&i.started_at),"listen":instance.as_ref().and_then(|i|i.listen.as_ref()),"serverId":instance.as_ref().and_then(|i|i.server_id.as_ref()),"hostname":instance.as_ref().map(|i|&i.hostname),"configuredListen":config["listen"],"localDaemon":local_daemon,"desktopManaged":instance.as_ref().is_some_and(|i|i.desktop_managed),"logPath":control::daemon_log_path(home)})
        }
        _ => json!({"host":target.description()}),
    };
    if matches!(target, DaemonTarget::Instance { .. })
        && instance.as_ref().is_none_or(|i| i.listen.is_none())
    {
        local["connectedDaemon"] = json!("not_probed");
        return Ok(local);
    }
    let mut options = ConnectOptions::new(target.clone());
    options.instance = instance.clone();
    options.timeout = Duration::from_millis(1500);
    let client = match crate::client::connect(options, cancel.child_token()).await {
        Ok(client) => client,
        Err(failure) => {
            if matches!(target, DaemonTarget::Endpoint { .. }) {
                return Err(failure);
            }
            let code = failure
                .downcast_ref::<ClientError>()
                .map(|e| e.code.as_str());
            local["connectedDaemon"] = json!(match code {
                Some("AUTH_REQUIRED") => "auth_required",
                Some("AUTH_FAILED") => "auth_failed",
                _ => "unreachable",
            });
            local["note"] = json!(failure.to_string());
            return Ok(local);
        }
    };
    let result = async {
        let status = client.daemon_status(Duration::from_millis(1500)).await;
        if let (DaemonTarget::Instance { home }, Some(instance)) = (target, instance.as_ref()) {
            assert_instance(home, instance).await?;
        }
        let info = client.server_info();
        local["serverId"] = info["serverId"].clone();
        local["daemonVersion"] = info["version"].clone();
        local["connectedDaemon"] = json!(if client.cancellation().is_cancelled() {
            "unreachable"
        } else {
            "reachable"
        });
        match status {
            Ok(status) => {
                for (key, source) in [
                    ("daemonVersion", "version"),
                    ("workerPid", "pid"),
                    ("daemonExecutable", "nodePath"),
                    ("providers", "providers"),
                    ("providersCached", "providersCached"),
                    ("relay", "relay"),
                ] {
                    if let Some(value) = status.get(source) {
                        local[key] = value.clone();
                    }
                }
            }
            Err(failure) => {
                if matches!(target, DaemonTarget::Endpoint { .. })
                    || client.cancellation().is_cancelled()
                    || failure
                        .downcast_ref::<ClientError>()
                        .is_none_or(|e| e.code != "DAEMON_REQUEST_TIMEOUT")
                {
                    return Err(failure);
                }
                local["note"] = json!(format!("Status details unavailable: {failure}"));
            }
        }
        Ok(local)
    }
    .await;
    client.close().await;
    result
}
fn reconnect_failure(error: &anyhow::Error) -> bool {
    let code = error
        .downcast_ref::<ClientError>()
        .map(|e| e.code.as_str())
        .or_else(|| {
            error
                .downcast_ref::<control::DaemonInstanceError>()
                .map(|e| e.code.as_str())
        });
    matches!(
        code,
        Some(
            "DAEMON_CONNECTION_LOST"
                | "DAEMON_REQUEST_TIMEOUT"
                | "DAEMON_UNREACHABLE"
                | "DAEMON_NOT_READY"
        )
    ) || error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}
pub(crate) async fn restart_confirmed(
    target: &DaemonTarget,
    pinned: Option<DaemonInstance>,
    reason: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    let instance = match target {
        DaemonTarget::Instance { home } => Some(match pinned {
            Some(i) => i,
            None => control::read_daemon_instance(home)
                .await?
                .ok_or_else(|| error("DAEMON_NOT_RUNNING", "Daemon is not running."))?,
        }),
        _ => None,
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let remaining = || {
        deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .max(Duration::from_millis(1))
    };
    let connect = |timeout| {
        let mut options = ConnectOptions::new(target.clone());
        options.instance = instance.clone();
        options.timeout = timeout;
        crate::client::connect(options, cancel.child_token())
    };
    let client = connect(remaining()).await?;
    let previous = async {
        let worker = client.daemon_status(remaining()).await?;
        ensure!(
            worker["pid"].as_u64().is_some_and(|pid| pid > 0),
            "Daemon status did not identify the current worker PID"
        );
        let server_id = client.server_info()["serverId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Daemon greeting did not identify the server")?
            .to_owned();
        if let (DaemonTarget::Instance { home }, Some(instance)) = (target, instance.as_ref()) {
            assert_instance(home, instance).await?;
        }
        let ack = match client.restart(reason, remaining()).await {
            Ok(_) => true,
            Err(error) if reconnect_failure(&error) => false,
            Err(error) => return Err(error),
        };
        Ok((worker["pid"].clone(), json!(server_id), ack))
    }
    .await;
    client.close().await;
    let (worker, server_id, acknowledged) = previous?;
    let mut last = "No replacement worker observed".to_owned();
    while tokio::time::Instant::now() < deadline {
        if let (DaemonTarget::Instance { home }, Some(instance)) = (target, instance.as_ref()) {
            assert_instance(home, instance).await?;
        }
        let observed = match connect(remaining().min(Duration::from_secs(1))).await {
            Ok(client) => {
                let result = async {
                    ensure!(
                        client.server_info()["serverId"] == server_id,
                        "Connected peer identity changed."
                    );
                    let status = client.daemon_status(remaining()).await?;
                    ensure!(
                        status["pid"].as_u64().is_some_and(|pid| pid > 0),
                        "Daemon status did not identify the replacement worker PID"
                    );
                    Ok(status)
                }
                .await;
                client.close().await;
                result
            }
            Err(error) => Err(error),
        };
        match observed {
            Ok(status) if status["pid"] != worker => {
                if let (DaemonTarget::Instance { home }, Some(instance)) =
                    (target, instance.as_ref())
                {
                    assert_instance(home, instance).await?;
                }
                return Ok(
                    json!({"action":"restarted","target":target.description(),"supervisorPid":instance.as_ref().map(|i|i.pid),"previousWorkerPid":worker,"workerPid":status["pid"],"acknowledged":acknowledged}),
                );
            }
            Ok(_) => {}
            Err(error) if reconnect_failure(&error) => last = error.to_string(),
            Err(error) => return Err(error),
        }
        tokio::select! {_=cancel.cancelled()=>bail!("Restart cancelled"),_=tokio::time::sleep(remaining().min(Duration::from_millis(100)))=>{}}
    }
    Err(error(
        "RESTART_NOT_CONFIRMED",
        format!(
            "Replacement worker was not confirmed. Restart acknowledged: {acknowledged}. {last}"
        ),
    ))
}
async fn configuration(action: &str, m: &ArgMatches, ctx: &Context) -> Result<Output> {
    let home = ctx.local_home()?;
    let path = string(m, "path");
    if action == "get" {
        if let Some(path) = path {
            crate::config::validate_persisted_path(path)?;
        }
        let config = crate::config::read_persisted(&home, false).await?;
        let value = path
            .map(|p| p.split('.').try_fold(&config, |v, key| v.get(key)))
            .unwrap_or(Some(&config));
        let redacted = value
            .map(|v| redact(v, path.is_some_and(secret)))
            .unwrap_or(Value::Null);
        return Ok(pretty(
            json!({"source":"configured","path":path,"set":value.is_some(),"value":redacted}),
        )?
        .quiet_text("configuration"));
    }
    let path = path.context("Configuration path required")?;
    let value = if action == "unset" {
        None
    } else {
        let raw = string(m, "value").context("Configuration value required")?;
        Some(if flag(m, "string") {
            json!(raw)
        } else {
            serde_json::from_str(raw).unwrap_or(json!(raw))
        })
    };
    crate::config::edit_persisted(&home, path, value).await?;
    let running = control::read_daemon_instance(&home)
        .await?
        .is_some_and(|i| i.listen.is_some());
    if running
        && let Ok(client) = ctx.connect().await {
            let result = reload(&client).await;
            client.close().await;
            let mut result = result.map_err(|e| {
                error(
                    "CONFIG_SAVED_RELOAD_FAILED",
                    format!("Saved; reload failed: {e}"),
                )
            })?;
            result["action"] = json!("saved");
            return Ok(pretty(result)?.quiet_text("configuration"));
        }
    Ok(pretty(json!({"action":"saved","applied":false,"message":"Saved; not applied to a running daemon","nextCommand":format!("paseo daemon {} --home {}",if running{"reload"}else{"start"},serde_json::to_string(&home)?)}))?.quiet_text("configuration"))
}
fn secret(key: &str) -> bool {
    let key = key.to_lowercase().replace(['_', '-'], "");
    [
        "password",
        "apikey",
        "privatekey",
        "accesskey",
        "token",
        "secret",
        "authorization",
    ]
    .iter()
    .any(|part| key.contains(part))
}
fn redact(value: &Value, hidden: bool) -> Value {
    if hidden {
        return json!("[redacted]");
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact(v, secret(k))))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| redact(v, false)).collect()),
        _ => value.clone(),
    }
}
pub async fn pairing(
    target: &DaemonTarget,
    enable: bool,
    cancel: &CancellationToken,
) -> Result<Value> {
    if let DaemonTarget::Instance { home } = target
        && control::read_daemon_instance(home).await?.is_none() {
            if enable {
                let mut config = crate::config::read_persisted(home, true).await?;
                set_path(&mut config, "daemon.relay.enabled", Some(json!(true)))?;
                crate::config::save_persisted(home, &config).await?;
            }
            return pairing_result(crate::transport::offline_pairing_offer(home).await?);
        }
    let mut options = ConnectOptions::new(target.clone());
    options.timeout = Duration::from_millis(1500);
    let client = crate::client::connect(options, cancel.child_token()).await?;
    let result = async {
        client.require_feature(
            "daemonStatusRpc",
            "Update the Paseo daemon before pairing from this command.",
        )?;
        let request = || {
            client.request(
                json!({"type":"daemon.get_pairing_offer.request"}),
                "daemon.get_pairing_offer.response",
                Duration::from_millis(1500),
            )
        };
        let mut offer = request().await?;
        if offer["relayEnabled"] != true && enable {
            client.require_feature(
                "relayConfig",
                "Update the Paseo daemon before enabling relay from this command.",
            )?;
            client
                .request(
                    json!({"type":"set_daemon_config_request","config":{"relay":{"enabled":true}}}),
                    "set_daemon_config_response",
                    Duration::from_secs(30),
                )
                .await?;
            offer = request()
                .await
                .context("Relay configuration was saved, but fetching the pairing offer failed")?;
        }
        pairing_result(offer)
    }
    .await;
    client.close().await;
    result
}
fn pairing_result(offer: Value) -> Result<Value> {
    use base64::Engine;
    let url = offer["url"].as_str().filter(|s| !s.is_empty());
    let connection = if let Some((_, encoded)) = url.and_then(|url| url.split_once("#offer=")) {
        let payload: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.trim().trim_end_matches('='))?,
        )?;
        ensure!(payload["v"] == 2, "Invalid pairing offer version");
        let required = |key: &str| {
            payload[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Malformed pairing offer")
        };
        let endpoint = payload["relay"]["endpoint"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Missing relay endpoint")?;
        const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
            .remove(b'-')
            .remove(b'_')
            .remove(b'.')
            .remove(b'!')
            .remove(b'~')
            .remove(b'*')
            .remove(b'\'')
            .remove(b'(')
            .remove(b')');
        let encoded =
            percent_encoding::utf8_percent_encode(required("serverId")?, COMPONENT).to_string();
        let mut uri = url::Url::parse(&format!("relay://{endpoint}/{encoded}"))?;
        uri.query_pairs_mut()
            .append_pair("key", required("daemonPublicKeyB64")?);
        if payload["relay"]["useTls"] == true {
            uri.query_pairs_mut().append_pair("ssl", "true");
        }
        if let Some(security) = payload.get("minimumSecurityMode") {
            ensure!(security == "protected", "Invalid pairing security mode");
            uri.query_pairs_mut().append_pair("security", "protected");
        }
        Some(uri.to_string())
    } else {
        None
    };
    Ok(
        json!({"relayEnabled":offer["relayEnabled"],"url":url,"qr":offer["qr"],"connectionUri":connection}),
    )
}
pub async fn onboard(m: &ArgMatches, ctx: &Context) -> Result<Output> {
    let timeout = duration(m, "timeout", 600.0)?;
    let home = ctx.local_home()?;
    let existing = control::read_daemon_instance(&home).await?;
    let mut config = crate::config::read_persisted(&home, true).await?;
    if let Some(listen) = string(m, "listen")
        .map(str::to_owned)
        .or_else(|| string(m, "port").map(|p| format!("127.0.0.1:{p}")))
    {
        set_path(&mut config, "daemon.listen", Some(json!(listen)))?;
    }
    if flag(m, "relay") || flag(m, "no-relay") {
        set_path(
            &mut config,
            "daemon.relay.enabled",
            Some(json!(flag(m, "relay"))),
        )?;
    }
    if flag(m, "no-mcp") {
        set_path(&mut config, "daemon.mcp.enabled", Some(json!(false)))?;
    }
    if let Some(hosts) = string(m, "hostnames") {
        set_path(
            &mut config,
            "daemon.hostnames",
            Some(if hosts == "true" {
                json!(true)
            } else {
                json!(hosts.split(',').collect::<Vec<_>>())
            }),
        )?;
    }
    crate::config::save_persisted(&home, &config).await?;
    if existing.is_some() {
        let client = ctx.connect().await?;
        let result = reload(&client).await;
        client.close().await;
        result?;
    } else {
        control::start_daemon_instance(
            StartOptions {
                home: home.clone(),
                timeout,
                ..Default::default()
            },
            &ctx.cancel,
        )
        .await?;
    }
    let ready = control::wait_for_daemon_ready(&home, timeout, None, &ctx.cancel).await?;
    let mut offer = if flag(m, "no-relay") {
        Value::Null
    } else {
        pairing(&ctx.target, flag(m, "relay"), &ctx.cancel).await?
    };
    if !flag(m, "no-relay") && offer["relayEnabled"] != true && super::prompts::interactive() {
        eprintln!(
            "Your connection is end-to-end encrypted. Paseo cannot read your code or messages.\nLearn how it works: https://paseo.sh/docs/security"
        );
        if super::prompts::confirm("Enable relay to pair a device?", false, &ctx.cancel).await? {
            offer = pairing(&ctx.target, true, &ctx.cancel).await?;
        }
    }
    pretty(
        json!({"home":home,"pid":ready.pid,"listen":ready.listen,"pairing":offer,"logPath":control::daemon_log_path(&home)}),
    )
}

pub(crate) fn status_text(status: &Value) -> Result<String> {
    Ok(status
        .as_object()
        .context("Invalid daemon status")?
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}: {}",
                value
                    .as_str()
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|| std::borrow::Cow::Owned(value.to_string()))
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_restart_retries_socket_handoff_but_not_authority_errors() {
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let error = anyhow::Error::new(tokio_tungstenite::tungstenite::Error::Io(kind.into()))
                .context("WebSocket handshake failed");
            assert!(reconnect_failure(&error), "{error:#}");
        }
        for code in [
            "AUTH_REQUIRED",
            "AUTH_FAILED",
            "DAEMON_REPLACED",
            "INCOMPATIBLE_PROTOCOL",
        ] {
            assert!(!reconnect_failure(&error(
                code,
                "Not a temporary worker handoff"
            )));
        }
        assert!(!reconnect_failure(
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()
        ));
    }
    #[test]
    fn pairing_uri_preserves_protected_security_and_tls() {
        use base64::Engine;
        let encoded=base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"v":2,"serverId":"srv_example","daemonPublicKeyB64":"key+value=","relay":{"endpoint":"relay.example:443","useTls":true},"minimumSecurityMode":"protected"})).unwrap());
        let result=pairing_result(json!({"relayEnabled":true,"url":format!("https://app.example/#offer={encoded}"),"qr":null})).unwrap();
        assert_eq!(
            result["connectionUri"],
            "relay://relay.example:443/srv_example?key=key%2Bvalue%3D&ssl=true&security=protected"
        );
    }
    #[test]
    fn config_redaction_covers_nested_arrays_without_mutating_values() {
        let config = json!({"providers":[{"api_key":"secret","model":"model"}],"daemon":{"auth":{"password":"hash"}}});
        let redacted = redact(&config, false);
        assert_eq!(redacted["providers"][0]["api_key"], "[redacted]");
        assert_eq!(redacted["providers"][0]["model"], "model");
        assert_eq!(config["providers"][0]["api_key"], "secret");
    }
    fn update_matches(args: &[&str]) -> clap::error::Result<ArgMatches> {
        let parsed = crate::cli::build_command()
            .try_get_matches_from(["paseo", "daemon", "update"].iter().chain(args))?;
        let (_, daemon) = parsed.subcommand().unwrap();
        let (name, update) = daemon.subcommand().unwrap();
        assert_eq!(name, "update");
        Ok(update.clone())
    }
    fn code(failure: &anyhow::Error) -> &str {
        failure.downcast_ref::<ClientError>().map(|e| e.code.as_str()).unwrap_or("")
    }
    const ID: &str = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
    #[test]
    fn update_submits_without_a_subcommand_and_observes_by_operation_id() {
        let submit = update_matches(&["--from", "/opt/runtime", "--operation-id", ID]).unwrap();
        assert_eq!(string(&submit, "from"), Some("/opt/runtime"));
        assert_eq!(string(&submit, "action"), None);
        let status = update_matches(&["status", ID]).unwrap();
        assert_eq!(string(&status, "action"), Some("status"));
        assert_eq!(string(&status, "operation"), Some(ID));
        assert!(update_matches(&["wait", ID]).is_ok());
        assert!(update_matches(&["cancel", ID]).is_err());
        // A continuation names the agent and the frozen text together.
        assert!(update_matches(&["--resume-agent", "agent"]).is_err());
        assert!(update_matches(&["--continuation-file", "/tmp/c.txt"]).is_err());
    }
    #[tokio::test]
    async fn remote_update_rejects_local_paths_before_contacting_the_host() {
        let ctx = Context {
            target: DaemonTarget::Endpoint {
                host: "203.0.113.1:6767".into(),
            },
            output: crate::cli::output::OutputOptions::default(),
            cancel: CancellationToken::new(),
        };
        for args in [
            vec!["--from", "/opt/runtime"],
            vec!["--resume-agent", "agent", "--continuation-file", "/tmp/c.txt"],
        ] {
            let failure = update(&update_matches(&args).unwrap(), &ctx).await.err().unwrap();
            assert_eq!(code(&failure), "LOCAL_HOME_REQUIRED", "{args:?}");
        }
        let failure = update(&update_matches(&["status", ID, "--from", "/opt/runtime"]).unwrap(), &ctx)
            .await
            .err()
            .unwrap();
        assert_eq!(code(&failure), "UPDATE_OPTION_CONFLICT");
        let failure = update(&update_matches(&["--operation-id", "latest"]).unwrap(), &ctx)
            .await
            .err()
            .unwrap();
        assert_eq!(code(&failure), "INVALID_OPERATION_ID");
    }
    #[tokio::test]
    async fn continuation_text_is_frozen_verbatim_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("continuation.txt");
        std::fs::write(&file, "Continue the release.\n").unwrap();
        assert_eq!(continuation_text(&file).await.unwrap(), "Continue the release.\n");
        for content in [" \n".as_bytes().to_vec(), vec![0xff, 0xfe], vec![b'a'; MAX_CONTINUATION_BYTES as usize + 1]] {
            std::fs::write(&file, content).unwrap();
            assert_eq!(code(&continuation_text(&file).await.unwrap_err()), "INVALID_CONTINUATION_FILE");
        }
        assert_eq!(
            code(&continuation_text(directory.path()).await.unwrap_err()),
            "INVALID_CONTINUATION_FILE"
        );
    }
    #[test]
    fn update_report_separates_commit_from_session_resume() {
        let committed = json!({"operationId":ID,"phase":"committed","terminal":true,"accepted":true,"candidate":{"version":"1.2.0","executableSha256":"abcdef0123456789","release":false},"continuation":{"state":"unknown","agentId":"agent","messageId":"message"}});
        assert_eq!(wait_exit_code(&committed), 0);
        let text = update_text(&committed);
        assert!(text.contains("Update committed"), "{text}");
        assert!(text.contains("1.2.0 sha256:abcdef012345 (local build)"), "{text}");
        assert!(text.contains("Session resume: unknown (agent agent, message message)"), "{text}");
        assert!(text.contains("does not change the update result"), "{text}");
        let accepted = json!({"operationId":ID,"phase":"draining","terminal":false,"accepted":true});
        assert!(!update_text(&accepted).contains("committed"));
        for phase in ["rolled_back", "failed"] {
            let ended = json!({"operationId":ID,"phase":phase,"terminal":true,"accepted":true,"error":"readiness failed"});
            assert_eq!(wait_exit_code(&ended), 1);
            assert!(update_text(&ended).contains("Error: readiness failed"));
        }
    }
}
