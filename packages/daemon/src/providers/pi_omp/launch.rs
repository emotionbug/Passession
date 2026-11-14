use super::transport::Transport;
use paseo_client::process::CommandSpec;
use crate::providers::{
    api::{LaunchContext, ProviderSpec},
    registry::catalog::memo::{self, Loaded, Memo},
};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

/// Commands a Pi launch reports, shared across session opens of one provider
/// instance. The MCP integration depends on which extension provides `mcp`.
pub type CommandProbes = Memo<Arc<[Value]>>;

pub struct Assets {
    pub directory: tempfile::TempDir,
    pub mcp: bool,
}
pub fn external_env(spec: &ProviderSpec, context: &LaunchContext) -> BTreeMap<String, String> {
    let mut env = std::env::vars().collect::<BTreeMap<_, _>>();
    env.extend(spec.env.clone());
    env.extend(context.env.clone());
    for key in [
        "PASEO_NODE_ENV",
        "PASEO_DESKTOP_MANAGED",
        "PASEO_SUPERVISED",
        "ELECTRON_RUN_AS_NODE",
        "ELECTRON_NO_ATTACH_CONSOLE",
        "ESBUILD_BINARY_PATH",
    ] {
        env.remove(key);
    }
    env
}
pub fn command(
    spec: &ProviderSpec,
    config: &Value,
    context: &LaunchContext,
    session: Option<&str>,
    assets: Option<&Assets>,
) -> Result<CommandSpec> {
    validate_options(
        &spec.family,
        config.get("providerOptions").unwrap_or(&spec.options),
    )?;
    let mut argv = spec.command.clone();
    let omp = spec.family == "omp";
    let env = external_env(spec, context);
    if argv.is_empty() {
        argv.push(
            env.get(if omp { "OMP_COMMAND" } else { "PI_COMMAND" })
                .cloned()
                .or_else(|| {
                    if omp {
                        None
                    } else {
                        env.get("PI_ACP_PI_COMMAND").cloned()
                    }
                })
                .unwrap_or_else(|| spec.family.clone()),
        );
    }
    if !argv
        .iter()
        .any(|arg| arg == "--mode" || arg.starts_with("--mode="))
    {
        argv.extend(["--mode".into(), if omp { "rpc-ui" } else { "rpc" }.into()]);
    }
    let options = config.get("providerOptions").unwrap_or(&spec.options);
    if omp {
        let mode = config["modeId"].as_str().unwrap_or("full");
        let approval = match mode {
            "full" => "yolo",
            "write" => "write",
            "ask" => "always-ask",
            _ => bail!("Unsupported OMP mode '{mode}'"),
        };
        argv.extend(["--approval-mode".into(), approval.into()]);
        for (key, flag) in [
            ("smolModel", "--smol"),
            ("slowModel", "--slow"),
            ("planModel", "--plan"),
        ] {
            if let Some(value) = options[key].as_str().filter(|v| !v.is_empty()) {
                argv.extend([flag.into(), value.into()]);
            }
        }
    }
    // Resume follows the engine's stored model, then explicit overrides are applied
    // with set_model after initialization (the original resume omits --model).
    if session.is_none()
        && let Some(model) = config["model"].as_str().filter(|v| !v.is_empty()) {
            argv.extend(["--model".into(), model.into()]);
        }
    if let Some(level) = config["thinkingOptionId"]
        .as_str()
        .filter(|s| valid_thinking(s, omp))
    {
        argv.extend(["--thinking".into(), level.into()]);
    }
    if config["internal"] == true || config["persistSession"] == false {
        argv.push("--no-session".into());
    } else if let Some(path) = session {
        argv.extend(["--session".into(), path.into()]);
    }
    if omp {
        let prompt = system_prompt(config);
        if !prompt.is_empty() {
            argv.extend(["--append-system-prompt".into(), prompt]);
        }
    }
    if let Some(assets) = assets {
        let mcp = assets.directory.path().join("mcp.json");
        if mcp.exists() {
            argv.extend(["--mcp-config".into(), mcp.to_string_lossy().into_owned()]);
        }
        argv.extend([
            "--extension".into(),
            assets
                .directory
                .path()
                .join("paseo-integration.mjs")
                .to_string_lossy()
                .into_owned(),
        ]);
    }
    let program = argv.remove(0);
    Ok(CommandSpec {
        program,
        args: argv,
        cwd: Some(PathBuf::from(
            config["cwd"]
                .as_str()
                .ok_or_else(|| anyhow!("Provider session requires cwd"))?,
        )),
        env,
        clear_env: true,
    })
}
pub fn valid_thinking(level: &str, omp: bool) -> bool {
    matches!(
        level,
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) || (omp && level == "auto")
}
pub fn system_prompt(config: &Value) -> String {
    [
        config["systemPrompt"].as_str(),
        config["daemonAppendSystemPrompt"].as_str(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|v| !v.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}
pub fn timeout(spec: &ProviderSpec, config: &Value) -> Duration {
    Duration::from_millis(
        config
            .pointer("/providerOptions/rpcTimeoutMs")
            .or_else(|| spec.options.get("rpcTimeoutMs"))
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
            .unwrap_or(60000),
    )
}
pub fn ready_timeout(spec: &ProviderSpec, config: &Value) -> Duration {
    Duration::from_millis(
        config
            .pointer("/providerOptions/rpcTimeoutMs")
            .or_else(|| spec.options.get("rpcTimeoutMs"))
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
            .unwrap_or(20000),
    )
}
pub fn validate_options(family: &str, options: &Value) -> Result<()> {
    if options.is_null() {
        return Ok(());
    }
    let options = options
        .as_object()
        .ok_or_else(|| anyhow!("Provider options must be an object"))?;
    for (key, value) in options {
        match key.as_str() {
            "rpcTimeoutMs" => {
                if value.as_u64().is_none_or(|v| v == 0) {
                    bail!("rpcTimeoutMs must be a positive integer");
                }
            }
            "extensionTimeoutMs" if family == "pi" => {
                if value.as_u64().is_none_or(|v| v == 0) {
                    bail!("extensionTimeoutMs must be a positive integer");
                }
            }
            "sessionDir" => {
                if value.as_str().is_none_or(|v| v.is_empty()) {
                    bail!("sessionDir must be nonempty");
                }
            }
            "smolModel" | "slowModel" | "planModel" if family == "omp" => {
                if value.as_str().is_none_or(|v| v.is_empty()) {
                    bail!("{key} must be nonempty");
                }
            }
            _ => bail!("Unknown {family} provider option: {key}"),
        }
    }
    Ok(())
}
fn pi_agent_dir(env: &BTreeMap<String, String>) -> PathBuf {
    let home = env
        .get("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
    env.get("PI_CODING_AGENT_DIR")
        .map(|value| value.trim())
        .filter(|v| !v.is_empty())
        .map(|value| {
            if value == "~" {
                home.clone()
            } else if let Some(rest) = value.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(value)
            }
        })
        .unwrap_or_else(|| home.join(".pi/agent"))
}
/// The probe launch plus the extension and settings locations Pi loads
/// commands from, so installing or removing an extension changes the key.
async fn probe_key(launch: &CommandSpec) -> String {
    let agent = pi_agent_dir(&launch.env);
    let mut watched = vec![agent.join("extensions"), agent.join("settings.json")];
    if let Some(cwd) = &launch.cwd {
        watched.extend([cwd.join(".pi/extensions"), cwd.join(".pi/settings.json")]);
    }
    let fingerprint = memo::fingerprint(&watched).await;
    memo::key(&json!([
        launch.program,
        launch.args,
        launch.cwd.as_ref().map(|cwd| cwd.to_string_lossy()),
        memo::shared_env(&launch.env),
        fingerprint
    ]))
}
async fn extension_commands(
    spec: &ProviderSpec,
    config: &Value,
    context: &LaunchContext,
    probes: &CommandProbes,
) -> Result<Arc<[Value]>> {
    let probe_config = json!({"cwd":config["cwd"],"providerOptions":config["providerOptions"]});
    let launch = command(spec, &probe_config, context, None, None)?;
    let key = probe_key(&launch).await;
    probes
        .get_or_load(key, &context.cancel, || async move {
            let (probe, mut events) = Transport::spawn(
                launch,
                timeout(spec, config),
                context.cancel.child_token(),
            )
            .await?;
            let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
            let commands = probe.request(json!({"type":"get_commands"})).await;
            let _ = probe.close().await;
            drain.abort();
            let commands = commands?["commands"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            Ok(Loaded::Complete(commands.into()))
        })
        .await
}
pub async fn pi_assets(
    spec: &ProviderSpec,
    config: &Value,
    context: &LaunchContext,
    probes: &CommandProbes,
) -> Result<Assets> {
    let directory = tempfile::Builder::new()
        .prefix("paseo-pi-extension-")
        .tempdir()?;
    let mut builtin = json!({});
    let servers = context.mcp_servers.as_object();
    let mut supported = false;
    if servers.is_some_and(|s| !s.is_empty()) {
        let commands = match extension_commands(spec, config, context, probes).await {
            Ok(commands) => commands,
            Err(error) => {
                tracing::debug!(%error,"Pi MCP capability probe unavailable");
                Arc::from([])
            }
        };
        let adapter = commands.iter().any(|c| {
            c["source"] == "extension"
                && c["name"].as_str().is_some_and(|n| {
                    n == "mcp"
                        || n.strip_prefix("mcp:")
                            .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                })
                && (c.get("sourceInfo").is_none_or(Value::is_null)
                    || c["sourceInfo"].to_string().contains("pi-mcp-adapter"))
        });
        let built_in = commands.iter().any(|c| {
            c["source"] == "extension"
                && c["name"] == "mcp"
                && c.pointer("/sourceInfo/path") == Some(&json!("builtin:mcp"))
        });
        if adapter {
            let agent = pi_agent_dir(&external_env(spec, context));
            let path = agent.join("mcp.json");
            let mut global = match tokio::fs::read(&path).await {
                Ok(bytes) => serde_json::from_slice::<Value>(&bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
                Err(error) => return Err(error.into()),
            };
            let object = global
                .as_object_mut()
                .ok_or_else(|| anyhow!("Pi MCP config must contain an object"))?;
            let mut merged = object
                .remove("mcpServers")
                .and_then(|v| v.as_object().cloned())
                .or_else(|| {
                    object
                        .remove("mcp-servers")
                        .and_then(|v| v.as_object().cloned())
                })
                .unwrap_or_default();
            object.remove("mcp-servers");
            for (name, server) in servers.unwrap() {
                merged.insert(name.clone(), pi_server(server, true));
            }
            object.insert("mcpServers".into(), Value::Object(merged));
            private_write(
                &directory.path().join("mcp.json"),
                &serde_json::to_vec(&global)?,
            )?;
            supported = true;
        } else if built_in {
            for (name, server) in servers.unwrap() {
                if !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                {
                    bail!(
                        "Pi built-in MCP server names may only contain letters, digits, '_' and '-'"
                    );
                }
                if server["type"] == "sse" {
                    bail!(
                        "Pi built-in MCP cannot register SSE server '{name}'; load pi-mcp-adapter"
                    );
                }
                builtin[name] = pi_server(server, false);
            }
            supported = true;
        }
    }
    private_write(
        &directory.path().join("runtime.json"),
        &serde_json::to_vec(&json!({"systemPrompt":system_prompt(config),"mcpServers":builtin}))?,
    )?;
    private_write(
        &directory.path().join("paseo-integration.mjs"),
        include_bytes!("pi-integration.mjs"),
    )?;
    Ok(Assets {
        directory,
        mcp: supported,
    })
}
fn pi_server(server: &Value, adapter: bool) -> Value {
    let mut value = json!({});
    if server["type"] == "stdio" {
        for key in ["command", "args", "env"] {
            if let Some(field) = server.get(key) {
                value[key] = field.clone();
            }
        }
    } else {
        for key in ["url", "headers"] {
            if let Some(field) = server.get(key) {
                value[key] = field.clone();
            }
        }
        if adapter {
            value["auth"] = json!(false);
            value["oauth"] = json!(false);
        }
    }
    value
}
pub fn private_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{probe_key, validate_options};
    use paseo_client::process::CommandSpec;
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, path::Path};

    #[test]
    fn timeout_options_require_positive_u64_integers() {
        for (family, key) in [
            ("pi", "rpcTimeoutMs"),
            ("omp", "rpcTimeoutMs"),
            ("pi", "extensionTimeoutMs"),
        ] {
            for value in [json!(1), json!(u64::MAX)] {
                assert!(validate_options(family, &json!({key: value})).is_ok());
            }
            for value in [
                Value::Null,
                json!(false),
                json!("1"),
                json!(0),
                json!(-1),
                json!(1.0),
                json!(0.5),
                serde_json::from_str::<Value>("18446744073709551616").unwrap(),
            ] {
                assert!(
                    validate_options(family, &json!({key: value})).is_err(),
                    "{family} accepted {key}={value}"
                );
            }
        }
        assert!(validate_options("omp", &json!({"extensionTimeoutMs": 1})).is_err());
    }

    fn launch(cwd: &Path, agent: &str, agent_dir: &Path) -> CommandSpec {
        CommandSpec {
            program: "pi".into(),
            args: vec!["--mode".into(), "rpc".into()],
            cwd: Some(cwd.to_path_buf()),
            env: BTreeMap::from([
                ("PASEO_AGENT_ID".to_owned(), agent.to_owned()),
                (
                    "PI_CODING_AGENT_DIR".to_owned(),
                    agent_dir.to_string_lossy().into_owned(),
                ),
            ]),
            clear_env: true,
        }
    }

    #[tokio::test]
    async fn command_probe_is_shared_per_workspace_until_extensions_change() {
        let (project, other, agent_dir) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let first = probe_key(&launch(project.path(), "a", agent_dir.path())).await;
        assert_eq!(
            first,
            probe_key(&launch(project.path(), "b", agent_dir.path())).await
        );
        assert_ne!(
            first,
            probe_key(&launch(other.path(), "a", agent_dir.path())).await
        );
        std::fs::create_dir_all(project.path().join(".pi/extensions")).unwrap();
        let project_extension = probe_key(&launch(project.path(), "a", agent_dir.path())).await;
        assert_ne!(first, project_extension);
        std::fs::create_dir_all(agent_dir.path().join("extensions")).unwrap();
        assert_ne!(
            project_extension,
            probe_key(&launch(project.path(), "a", agent_dir.path())).await
        );
    }
}
