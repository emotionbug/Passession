//! Read-only usage CLI for OMP's own accounts, not the omp-kiro HTTP store.
mod normalize;

use super::{
    DiscoveredAccounts, UsageContext,
    common::{UsageAccount, UsageRuntime, UsageScope, hash_account_key},
};
use anyhow::{Result, anyhow, bail};
use paseo_client::process::{self, CommandSpec};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{OnceCell, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const USAGE_TIMEOUT: Duration = Duration::from_secs(12);
const ACCOUNTS_TIMEOUT: Duration = Duration::from_secs(4);
const BATCH_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Default)]
pub(super) struct Runtime(Arc<Launches>);

#[derive(Default)]
struct Launches {
    global: Vec<Arc<Launch>>,
    agents: HashMap<String, (Arc<Launch>, String)>,
    by_id: HashMap<String, Arc<Launch>>,
    selected_agent: Option<String>,
    cache: Arc<BatchCache>,
    force: bool,
}

struct Launch {
    id: String,
    command: CommandSpec,
    present: bool,
    namespace: String,
    batch: OnceCell<Batch>,
}

type Batch = Result<Arc<normalize::Rows>, &'static str>;
type Completed = Option<(Instant, Batch)>;
type Subscriber = watch::Receiver<Completed>;

#[derive(Default)]
pub(super) struct BatchCache {
    slots: Mutex<HashMap<String, Subscriber>>,
}

impl BatchCache {
    async fn load(&self, launch: &Arc<Launch>, force: bool, cancel: &CancellationToken) -> Batch {
        if cancel.is_cancelled() {
            return Err("The OMP usage request was cancelled.");
        }
        let mut receiver = {
            let mut slots = self.slots.lock().unwrap();
            slots.retain(|_, receiver| match &*receiver.borrow() {
                Some((at, outcome)) => outcome.is_ok() && at.elapsed() < BATCH_TTL,
                None => receiver.has_changed().is_ok(),
            });
            if !force && let Some(receiver) = slots.get(&launch.id) {
                receiver.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                slots.insert(launch.id.clone(), receiver.clone());
                let launch = launch.clone();
                // Any subscriber can leave without cancelling another subscriber's CLI.
                tokio::spawn(async move {
                    let batch = launch.collect(&CancellationToken::new()).await;
                    sender.send_replace(Some((Instant::now(), batch)));
                });
                receiver
            }
        };
        loop {
            if let Some((_, batch)) = &*receiver.borrow_and_update() {
                return batch.clone();
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("The OMP usage request was cancelled."),
                result = receiver.changed() => {
                    if result.is_err() {
                        return Err("The OMP usage batch ended without a report.");
                    }
                }
            }
        }
    }
}

impl Runtime {
    pub(super) fn new(
        context: &UsageContext,
        home: &Path,
        selected_agent: Option<&str>,
        cache: Arc<BatchCache>,
        force: bool,
    ) -> Self {
        let mut by_id = HashMap::<String, Arc<Launch>>::new();
        let mut stores = HashSet::new();
        let mut present_providers = HashSet::new();
        let global = context
            .provider_specs
            .iter()
            .filter(|spec| spec.family == "omp" && context.enabled_provider_ids.contains(&spec.id))
            .filter_map(|spec| {
                let mut env = context.env.clone();
                env.extend(spec.env.clone());
                let launch = Launch::new(&spec.command, env, home.into(), home);
                if !launch.present {
                    return None;
                }
                present_providers.insert(spec.id.as_str());
                stores.insert(launch.namespace.clone()).then(|| {
                    let launch = Arc::new(launch);
                    by_id.insert(launch.id.clone(), launch.clone());
                    launch
                })
            })
            .collect();
        let agents = context
            .sessions
            .iter()
            .filter(|session| {
                session["providerFamily"] == "omp" && session["_runtimeUnavailable"] != true
            })
            .filter_map(|session| {
                let agent = session["agentId"].as_str()?;
                if selected_agent.is_some_and(|id| id != agent)
                    || (selected_agent.is_none()
                        && !session["provider"]
                            .as_str()
                            .is_some_and(|id| present_providers.contains(id)))
                {
                    return None;
                }
                let provider = session["model"].as_str()?.split_once('/')?.0;
                if !native_provider(provider) {
                    return None;
                }
                // These are the live session's command prefix, cwd and environment,
                // not the current provider registry's configuration.
                let command = serde_json::from_value::<Vec<String>>(session["command"].clone())
                    .ok()
                    .filter(|argv| !argv.is_empty())?;
                let env = session["env"]
                    .as_object()?
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect();
                let cwd = PathBuf::from(session["cwd"].as_str()?);
                let launch = Launch::new(&command, env, cwd, home);
                if selected_agent.is_none() && !launch.present {
                    return None;
                }
                let launch = by_id
                    .entry(launch.id.clone())
                    .or_insert_with(|| Arc::new(launch))
                    .clone();
                Some((agent.to_owned(), (launch, provider.to_owned())))
            })
            .collect();
        Self(Arc::new(Launches {
            global,
            agents,
            by_id,
            selected_agent: selected_agent.map(str::to_owned),
            cache,
            force,
        }))
    }

    pub(super) fn has_scoped_launch(&self) -> bool {
        self.0
            .selected_agent
            .as_ref()
            .is_some_and(|id| self.0.agents.contains_key(id))
    }

    async fn rows(
        &self,
        launch: &Arc<Launch>,
        cancel: &CancellationToken,
    ) -> Result<Arc<normalize::Rows>> {
        launch
            .batch
            .get_or_init(|| self.0.cache.load(launch, self.0.force, cancel))
            .await
            .clone()
            .map_err(|message| anyhow!(message))
    }

    pub(super) fn visible_global_login(&self, input: &Value) -> bool {
        input["launchId"]
            .as_str()
            .and_then(|id| self.0.by_id.get(id))
            .is_some_and(|launch| {
                self.0
                    .global
                    .iter()
                    .all(|global| global.namespace != launch.namespace || global.id == launch.id)
            })
    }
}

fn native_provider(provider: &str) -> bool {
    !provider.is_empty()
        && !provider.starts_with("kiro-")
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// OMP's CLI profile flag takes precedence over OMP_PROFILE, then PI_PROFILE.
fn store_namespace(
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
    home: &Path,
) -> String {
    let value = |key: &str| {
        env.get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    let mut explicit = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--profile" {
            explicit = args.next().map(String::as_str);
        } else if let Some(profile) = arg.strip_prefix("--profile=") {
            explicit = Some(profile);
        }
    }
    let normalized = |profile: &str| {
        let profile = profile.trim();
        (!matches!(profile, "" | "default")).then(|| profile.to_owned())
    };
    let profile = explicit
        .or_else(|| {
            env.get("OMP_PROFILE")
                .or_else(|| env.get("PI_PROFILE"))
                .map(String::as_str)
        })
        .and_then(normalized);
    let home = value(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(Path::new)
        .unwrap_or(home);
    let base = cwd.join(home).join(
        value("PI_CONFIG_DIR")
            .unwrap_or(".omp")
            .trim_start_matches('/'),
    );
    let profile_root = |base: &Path, profile: Option<&str>| {
        profile.map_or_else(
            || base.to_owned(),
            |profile| base.join("profiles").join(profile),
        )
    };
    let default_agent = profile_root(&base, profile.as_deref()).join("agent");
    let inherited_profile_dir = env
        .get("OMP_PROFILE")
        .and_then(|profile| normalized(profile))
        .or_else(|| {
            env.get("PI_PROFILE")
                .and_then(|profile| normalized(profile))
        })
        .map(|profile| profile_root(&base, Some(&profile)).join("agent"));
    let override_dir = value("PI_CODING_AGENT_DIR")
        .map(|path| cwd.join(path))
        .filter(|path| Some(path) != inherited_profile_dir.as_ref());
    let mut agent = if profile.is_none() {
        override_dir.unwrap_or_else(|| default_agent.clone())
    } else {
        default_agent.clone()
    };
    // OMP redirects default agent storage only after the XDG root has been created.
    let profile_root_dir = profile_root(&base, profile.as_deref());
    if let Some(identity) = broker_namespace(env, cwd, &agent, &profile_root_dir) {
        return identity;
    }
    if cfg!(any(target_os = "linux", target_os = "macos"))
        && agent == default_agent
        && let Some(data) = value("XDG_DATA_HOME")
    {
        let xdg = profile_root(&cwd.join(data).join("omp"), profile.as_deref());
        if xdg.exists() {
            agent = xdg;
        }
    }
    let db = agent.join("agent.db");
    let path = db
        .canonicalize()
        .unwrap_or_else(|_| agent.canonicalize().unwrap_or(agent).join("agent.db"));
    hash_account_key(&json!(["local", path]).to_string())
}

fn broker_namespace(
    env: &BTreeMap<String, String>,
    cwd: &Path,
    agent: &Path,
    profile_root: &Path,
) -> Option<String> {
    let value = |name: &str| env.get(name).map(String::as_str).filter(|v| !v.is_empty());
    let url_env = value("OMP_AUTH_BROKER_URL");
    let token_env = value("OMP_AUTH_BROKER_TOKEN");
    let config = if url_env.is_none() || token_env.is_none() {
        ["config.yml", "config.yaml"]
            .into_iter()
            .find_map(|name| {
                let path = agent.join(name);
                // OMP reads the first present file, not a merge of both.
                path.exists().then(|| {
                    read_identity_file(&path)
                        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
                })
            })
            .flatten()
    } else {
        None
    };
    let field = |name: &str, dotted: &str| {
        let config = config.as_ref()?;
        config
            .get("auth")
            .and_then(|auth| auth.get("broker"))
            .and_then(|broker| broker.get(name))
            .or_else(|| config.get(dotted))?
            .as_str()
            .filter(|v| !v.is_empty())
    };
    let resolve = |setting: &str| value(setting).unwrap_or(setting).to_owned();
    let url = url_env
        .map(str::to_owned)
        .or_else(|| field("url", "auth.broker.url").map(resolve))?;
    let token = token_env
        .map(str::to_owned)
        .or_else(|| field("token", "auth.broker.token").map(resolve))
        .or_else(|| {
            read_identity_file(&profile_root.join("auth-broker.token"))
                .map(|raw| raw.trim().to_owned())
        });
    let pool = value("OMP_AUTH_BROKER_ACCOUNT_POOL_FILE").map(|path| {
        let path = cwd.join(path.trim());
        path.canonicalize().unwrap_or(path)
    });
    // A command-resolved broker value is opaque to the daemon. Keep its settings
    // location and execution context distinct rather than evaluating that command.
    let dynamic =
        url.starts_with('!') || token.as_deref().is_some_and(|token| token.starts_with('!'));
    let dynamic_context = dynamic.then(|| json!([agent, cwd, env]));
    Some(hash_account_key(
        &json!(["broker", url, token, pool, dynamic_context]).to_string(),
    ))
}

fn read_identity_file(path: &Path) -> Option<String> {
    const MAX_CONFIG: u64 = 256 * 1024;
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG {
        return None;
    }
    let mut raw = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_CONFIG + 1)
        .read_to_string(&mut raw)
        .ok()?;
    (raw.len() <= MAX_CONFIG as usize).then_some(raw)
}

fn resolve_program(program: &str, env: &BTreeMap<String, String>, cwd: &Path) -> (String, bool) {
    let path = env.get("PATH");
    if let Ok(resolved) = which::which_in(program, path, cwd) {
        return (resolved.to_string_lossy().into_owned(), true);
    }
    // An existing but non-executable file is a broken configured launch, not
    // an absent optional CLI. Fall through to process::run for its error.
    let present = |path: &Path| match path.metadata() {
        Ok(metadata) => metadata.is_file(),
        Err(error) => error.kind() == io::ErrorKind::PermissionDenied,
    };
    let candidate = Path::new(program);
    let found = if candidate.components().count() > 1 || candidate.is_absolute() {
        present(&cwd.join(candidate))
    } else {
        path.is_some_and(|path| {
            std::env::split_paths(path).any(|dir| present(&cwd.join(dir).join(candidate)))
        })
    };
    (program.to_owned(), found)
}

impl Launch {
    fn new(
        prefix: &[String],
        mut env: BTreeMap<String, String>,
        cwd: PathBuf,
        home: &Path,
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
            "PASEO_AGENT_ID",
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
        let mut args = Vec::with_capacity(prefix_args.len() + 1);
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
        let (program, present) = resolve_program(&program, &env, &cwd);
        let namespace = if present {
            store_namespace(&args, &env, &cwd, home)
        } else {
            String::new()
        };
        let id = hash_account_key(&json!([program, args, cwd, env, namespace]).to_string());
        args.push("usage".into());
        Self {
            id,
            present,
            command: CommandSpec {
                program,
                args,
                cwd: Some(cwd),
                env,
                clear_env: true,
            },
            batch: OnceCell::new(),
            namespace,
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

    async fn collect(&self, cancel: &CancellationToken) -> Batch {
        let (usage, accounts) =
            tokio::join!(self.command(false, cancel), self.command(true, cancel));
        let usage = usage?;
        let accounts = match accounts {
            Ok(accounts) => Some(accounts),
            Err(_) if cancel.is_cancelled() => return Err("The OMP usage request was cancelled."),
            Err(_) => {
                // Older OMP may support usage without the OAuth inventory.
                tracing::warn!("OMP OAuth account inventory unavailable");
                None
            }
        };
        Ok(Arc::new(normalize::project(
            usage,
            accounts,
            &self.namespace,
        )))
    }
}

pub(super) async fn discover(rt: &UsageRuntime, scope: &UsageScope) -> Result<DiscoveredAccounts> {
    let (launches, provider) = if let Some(family) = &scope.provider {
        if family != "omp" {
            return Ok(Vec::new().into());
        }
        let Some((launch, provider)) = rt
            .omp
            .0
            .selected_agent
            .as_ref()
            .and_then(|id| rt.omp.0.agents.get(id))
        else {
            return Ok(Vec::new().into());
        };
        (std::slice::from_ref(launch), Some(provider.as_str()))
    } else {
        (rt.omp.0.global.as_slice(), None)
    };
    let batches = futures_util::future::join_all(
        launches
            .iter()
            .map(|launch| async move { (launch, rt.omp.rows(launch, &rt.cancel).await) }),
    )
    .await;
    let mut discovered = Vec::new();
    let mut failed_omp_launches = HashSet::new();
    for result in batches {
        let (launch, rows) = result;
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "OMP usage launch discovery failed");
                failed_omp_launches.insert(launch.id.clone());
                continue;
            }
        };
        discovered.extend(
            rows.iter()
                .filter(|(_, row)| provider.is_none_or(|id| row.provider.eq_ignore_ascii_case(id)))
                .map(|(key, row)| UsageAccount {
                    key: key.clone(),
                    label: Some(row.label.clone()),
                    harness: None,
                    input: json!({"launchId":launch.id,"accountKey":key}),
                }),
        );
    }
    Ok(DiscoveredAccounts {
        accounts: discovered,
        failed_omp_launches,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input<'a> {
    launch_id: &'a str,
    account_key: &'a str,
}

pub(super) async fn fetch(rt: &UsageRuntime, input: &Value) -> Result<Value> {
    let input =
        Input::deserialize(input).map_err(|_| anyhow!("Invalid OMP usage account locator."))?;
    let Some(launch) = rt.omp.0.by_id.get(input.launch_id) else {
        bail!("The configured OMP usage launch is no longer available.");
    };
    Ok(rt
        .omp
        .rows(launch, &rt.cancel)
        .await?
        .get(input.account_key)
        .map_or_else(
            || normalize::no_quota("OMP no longer lists this account."),
            |row| row.report.clone(),
        ))
}

#[cfg(test)]
mod tests;
