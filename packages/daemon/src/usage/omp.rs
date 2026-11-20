//! Read-only usage CLI for OMP's own accounts, not the omp-kiro HTTP store.
mod normalize;

use super::{
    UsageContext,
    common::{UsageAccount, UsageRuntime, UsageScope, hash_account_key},
};
use anyhow::{Result, anyhow, bail};
use paseo_client::process::{self, CommandSpec};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const USAGE_TIMEOUT: Duration = Duration::from_secs(12);
const ACCOUNTS_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Default)]
pub(super) struct Runtime(Arc<Launches>);

#[derive(Default)]
struct Launches {
    global: Option<Launch>,
    agents: HashMap<String, Launch>,
    selected_agent: Option<String>,
}

struct Launch {
    command: CommandSpec,
    provider: Option<String>,
    namespace: String,
    batch: OnceCell<Result<normalize::Rows, &'static str>>,
}

impl Runtime {
    pub(super) fn new(context: &UsageContext, home: &Path, selected_agent: Option<&str>) -> Self {
        let global = context
            .provider_specs
            .iter()
            .find(|spec| spec.id == "omp" && spec.family == "omp")
            .map(|spec| {
                let mut env: BTreeMap<_, _> = std::env::vars().collect();
                env.extend(context.env.clone());
                env.extend(spec.env.clone());
                Launch::new(&spec.command, env, home.into(), None)
            });
        let agents = context
            .sessions
            .iter()
            .filter_map(|session| {
                let agent = session["agentId"].as_str()?;
                if selected_agent.is_some_and(|id| id != agent)
                    || session["_runtimeUnavailable"] == true
                {
                    return None;
                }
                let id = session["provider"].as_str()?;
                let spec = context
                    .provider_specs
                    .iter()
                    .find(|spec| spec.id == id && spec.family == "omp")?;
                let provider = session["model"].as_str()?.split_once('/')?.0;
                if !native_provider(provider) {
                    return None;
                }
                // usage_session records the exact sanitized OMP launch environment.
                let env = session["env"]
                    .as_object()?
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect();
                let cwd = session["cwd"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.into());
                Some((
                    agent.to_owned(),
                    Launch::new(&spec.command, env, cwd, Some(provider)),
                ))
            })
            .collect();
        Self(Arc::new(Launches {
            global,
            agents,
            selected_agent: selected_agent.map(str::to_owned),
        }))
    }

    fn launch(&self, agent: Option<&str>) -> Option<&Launch> {
        match agent {
            Some(agent) => self.0.agents.get(agent),
            None => self.0.global.as_ref(),
        }
    }
}

fn native_provider(provider: &str) -> bool {
    !provider.is_empty()
        && !provider.starts_with("kiro-")
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

impl Launch {
    fn new(
        prefix: &[String],
        mut env: BTreeMap<String, String>,
        cwd: PathBuf,
        provider: Option<&str>,
    ) -> Self {
        // Match pi_omp::launch::external_env; none of the runtime control flags
        // should propagate to a standalone OMP command.
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
        let (program, prefix_args) = if let Some((program, args)) = prefix.split_first() {
            (program.clone(), args)
        } else {
            (
                env.get("OMP_COMMAND")
                    .cloned()
                    .unwrap_or_else(|| "omp".into()),
                &[][..],
            )
        };
        let mut args = Vec::with_capacity(prefix_args.len() + 3);
        let mut prefix_args = prefix_args.iter().peekable();
        while let Some(arg) = prefix_args.next() {
            if arg == "--mode"
                && prefix_args
                    .peek()
                    .is_some_and(|v| matches!(v.as_str(), "rpc" | "rpc-ui"))
            {
                prefix_args.next();
            } else if !matches!(arg.as_str(), "--mode=rpc" | "--mode=rpc-ui") {
                args.push(arg.clone());
            }
        }
        // Only accounts with no public identity need their row id qualified by
        // the configured OMP profile/store. No environment values enter a report.
        let store_env = [
            "HOME",
            "USERPROFILE",
            "OMP_PROFILE",
            "PI_PROFILE",
            "PI_CONFIG_DIR",
            "PI_CODING_AGENT_DIR",
            "XDG_DATA_HOME",
        ]
        .map(|key| env.get(key));
        let namespace = hash_account_key(&json!([program, args, store_env]).to_string());
        args.push("usage".into());
        Self {
            command: CommandSpec {
                program,
                args,
                cwd: Some(cwd),
                env,
                clear_env: true,
            },
            provider: provider.map(str::to_owned),
            namespace,
            batch: OnceCell::new(),
        }
    }

    async fn command<T: DeserializeOwned>(
        &self,
        accounts: bool,
        cancel: &CancellationToken,
    ) -> Result<T, &'static str> {
        let mut command = self.command.clone();
        if accounts {
            command.args.push("accounts".into());
        }
        command
            .args
            .extend(["--json".into(), "--no-extensions".into()]);
        if let Some(provider) = &self.provider {
            command.args.extend(["--provider".into(), provider.clone()]);
        }
        let timeout = if accounts {
            ACCOUNTS_TIMEOUT
        } else {
            USAGE_TIMEOUT
        };
        let output = process::run(command, MAX_OUTPUT, timeout, cancel.clone())
            .await
            .map_err(|error| {
                if cancel.is_cancelled() {
                    "The OMP usage request was cancelled."
                } else if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
                }) {
                    "The configured OMP command was not found."
                } else if error
                    .chain()
                    .any(|cause| cause.to_string().contains("subprocess command timed out"))
                {
                    "The OMP usage command timed out."
                } else if error
                    .chain()
                    .any(|cause| cause.to_string().contains("stdout exceeded output limit"))
                {
                    "The OMP usage command exceeded its output limit."
                } else {
                    "The configured OMP usage command could not be run."
                }
            })?;
        if output.status != 0 {
            return Err(
                "The OMP usage command failed. Check that the configured OMP supports `usage --json`.",
            );
        }
        // Never forward stderr, provider errors, or a malformed JSON payload.
        serde_json::from_slice(&output.stdout)
            .map_err(|_| "OMP did not return recognized usage JSON. Update the configured OMP if it is older.")
    }

    async fn rows(&self, cancel: &CancellationToken) -> Result<&normalize::Rows> {
        self.batch
            .get_or_init(|| async {
                let (usage, accounts) =
                    tokio::join!(self.command(false, cancel), self.command(true, cancel));
                let usage = usage?;
                let accounts = match accounts {
                    Ok(accounts) => Some(accounts),
                    Err(_) if cancel.is_cancelled() => {
                        return Err("The OMP usage request was cancelled.");
                    }
                    Err(_) => {
                        // Some older OMPs support the report but not OAuth inventory.
                        tracing::warn!("OMP OAuth account inventory unavailable");
                        None
                    }
                };
                Ok(normalize::project(
                    usage,
                    accounts,
                    &self.namespace,
                    self.provider.as_deref(),
                ))
            })
            .await
            .as_ref()
            .map_err(|message| anyhow!(*message))
    }
}

pub(super) async fn discover(rt: &UsageRuntime, scope: &UsageScope) -> Result<Vec<UsageAccount>> {
    let agent = if scope.provider.is_some() {
        if scope.provider.as_deref() != Some("omp") {
            return Ok(Vec::new());
        }
        let Some(agent) = rt.omp.0.selected_agent.as_deref() else {
            return Ok(Vec::new());
        };
        Some(agent)
    } else {
        None
    };
    let Some(launch) = rt.omp.launch(agent) else {
        return Ok(Vec::new());
    };
    Ok(launch
        .rows(&rt.cancel)
        .await?
        .iter()
        .map(|(key, row)| UsageAccount {
            key: key.clone(),
            label: Some(row.label.clone()),
            harness: None,
            input: json!({"agentId":agent,"accountKey":key}),
        })
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input<'a> {
    agent_id: Option<&'a str>,
    account_key: &'a str,
}

pub(super) async fn fetch(rt: &UsageRuntime, input: &Value) -> Result<Value> {
    let input =
        Input::deserialize(input).map_err(|_| anyhow!("Invalid OMP usage account locator."))?;
    let Some(launch) = rt.omp.launch(input.agent_id) else {
        bail!("The configured OMP usage launch is no longer available.");
    };
    Ok(launch
        .rows(&rt.cancel)
        .await?
        .get(input.account_key)
        .map_or_else(
            || normalize::no_quota("OMP no longer lists this account."),
            |row| row.report.clone(),
        ))
}

#[cfg(test)]
mod tests;
