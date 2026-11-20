use super::{Inner, Session, Transport, catalog, launch};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
};

pub(in crate::providers::pi_omp) const COMMANDS: &[(&str, &str, &str)] = &[
    ("login", "List OMP OAuth providers or log in", "[provider]"),
    (
        "logout",
        "List or remove stored OMP credentials",
        "[provider] [account number]",
    ),
    (
        "queue",
        "Inspect or change the OMP message queue",
        "[remove <n>|promote <n>|mode <steering|follow-up> <all|one>]",
    ),
    ("retry-stop", "Stop the pending OMP retry", ""),
    (
        "autoretry",
        "Show or set the OMP session's retry override",
        "[on|off]",
    ),
    ("subagents", "List native OMP subagents", ""),
    (
        "subagent-steer",
        "Send guidance to a native OMP subagent",
        "<id> <message>",
    ),
    ("subagent-cancel", "Cancel a native OMP subagent", "<id>"),
    (
        "bash",
        "Run a shell command and include the result in OMP context",
        "<command>",
    ),
    ("bash-stop", "Cancel OMP shell execution", ""),
    ("next-model", "Cycle to the next native OMP model", ""),
    (
        "next-thinking",
        "Cycle through the model's native OMP thinking levels",
        "",
    ),
    ("handoff", "Hand off with an OMP summary", "[instructions]"),
    (
        "fork",
        "Fork the native conversation into a new Passession agent",
        "[entry id]",
    ),
];

pub(super) fn slash(text: &str) -> Option<(&str, &str)> {
    let text = text.trim().strip_prefix('/')?;
    let (name, args) = text
        .split_once(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or((text, ""));
    Some((name, args.trim()))
}

pub(in crate::providers::pi_omp) fn identity_block(name: &str, args: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "resume" | "new" | "delete" | "branch" | "rewind" | "tree" => Some(
            "This OMP command can replace the conversation behind this agent. Open or import another Passession agent instead.",
        ),
        "fresh" => Some(
            "OMP /fresh changes the native session identity behind this agent. Use a new Passession agent instead.",
        ),
        "session" => {
            let verb = args.split_whitespace().next().unwrap_or("");
            (!matches!(verb, "" | "info" | "pin")).then_some(
                "Use /session info or the Account picker. Replacing or deleting the native OMP session here is unsafe.",
            )
        }
        name if name.starts_with("paseo-account-") => {
            Some("This is an internal Passession command.")
        }
        _ => None,
    }
}

pub(super) fn is_control(name: &str, args: &str) -> bool {
    identity_block(name, args).is_some()
        || COMMANDS.iter().any(|(command, _, _)| *command == name)
        || name == "todo"
            && args
                .split_whitespace()
                .next()
                .is_some_and(|verb| verb.eq_ignore_ascii_case("edit"))
}

pub(super) fn refresh_after_command(name: &str, args: &str) -> bool {
    matches!(
        name,
        "kiro-accounts" | "kiro-profile" | "kiro-profiles" | "kiro-model"
    ) || name == "session" && args.split_whitespace().next() == Some("pin")
}

fn rows<'a>(result: &'a Value, key: &str) -> Result<&'a [Value]> {
    result[key]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("OMP did not report {key}"))
}

fn label(row: &Value, key: &str) -> String {
    row[key]
        .as_str()
        .unwrap_or("unknown")
        .replace(['\r', '\n'], " ")
}

fn account_feature(provider: &str, snapshot: &Value) -> Option<Value> {
    let mut options = vec![json!({"id":"automatic","label":"Automatic"})];
    let mut selected = None;
    for account in snapshot["accounts"].as_array()? {
        let (Some(id), Some(label)) = (account["id"].as_u64(), account["label"].as_str()) else {
            continue;
        };
        let option = format!("{provider}/{id}");
        if snapshot["explicit"] == id {
            selected = Some(option.clone());
        }
        options.push(json!({"id":option,"label":label}));
    }
    (options.len() > 1).then(|| json!({"type":"select","id":"oauth_account","label":"Account","description":"Choose the OMP OAuth account for this session and provider.","value":selected.as_deref().unwrap_or("automatic"),"options":options}))
}

impl Inner {
    fn account_command(&self) -> Option<String> {
        self.assets
            .lock()
            .unwrap()
            .as_ref()?
            .account_command
            .clone()
    }

    pub(super) fn record_omp_commands(&self, commands: &[Value]) {
        let account_command = self.account_command();
        let mut state = self.state.lock().unwrap();
        state.account_command_available = account_command.as_ref().is_some_and(|name| {
            commands
                .iter()
                .any(|c| c["source"] == "extension" && c["name"] == name.as_str())
        });
        state.commands = Some(catalog::commands(commands, true));
    }

    pub(super) async fn discover_account_bridge(&self) {
        if self.state.lock().unwrap().native["model"]["provider"].is_null() {
            return;
        }
        let result = async {
            let (_, result) = self
                .runtime()?
                .request_with_timeout(
                    json!({"type":"get_available_commands"}),
                    Some(std::time::Duration::from_secs(3)),
                )
                .await?;
            self.record_omp_commands(rows(&result, "commands")?);
            self.refresh_account(None).await
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(%error, "OMP account picker unavailable");
        }
    }

    // Only called under the session's operation gate, or during initial setup.
    async fn refresh_account(&self, selection: Option<&Value>) -> Result<()> {
        let context = {
            let state = self.state.lock().unwrap();
            if state.account_command_available {
                state.native["model"]["provider"]
                    .as_str()
                    .zip(state.native["sessionId"].as_str())
                    .map(|(provider, id)| (provider.to_owned(), id.to_owned()))
            } else {
                None
            }
        };
        let Some(((provider, session_id), command)) = context.zip(self.account_command()) else {
            if selection.is_some() {
                bail!("Native OAuth account selection is unavailable for this OMP session");
            }
            let mut state = self.state.lock().unwrap();
            state.account = None;
            state.update_features();
            return Ok(());
        };
        let mut payload = json!({"provider":provider,"sessionId":session_id});
        if let Some(selection) = selection {
            payload["credentialId"] = if selection == "automatic" {
                Value::Null
            } else {
                let (selected_provider, id) = selection
                    .as_str()
                    .and_then(|s| s.rsplit_once('/'))
                    .context("Invalid OMP account option")?;
                ensure!(
                    selected_provider == provider,
                    "The current OMP provider has changed; choose an account again"
                );
                json!(id.parse::<u64>().context("Invalid OMP credential number")?)
            };
        }
        let result = self.extension(&command, payload).await?;
        let feature = account_feature(&provider, &result["result"]);
        if let Some(selection) = selection {
            ensure!(
                feature
                    .as_ref()
                    .is_some_and(|feature| feature["value"] == *selection),
                "OMP did not retain the requested account selection"
            );
        }
        let mut state = self.state.lock().unwrap();
        if state.native["sessionId"] == session_id && state.native["model"]["provider"] == provider
        {
            state.account = feature.map(|value| (provider, value));
            state.update_features();
        }
        Ok(())
    }

    pub(super) async fn set_oauth_account(&self, value: &Value) -> Result<()> {
        let _gate = self.operation.lock().await;
        let native = self.refresh().await?;
        self.require_idle(&native, "changing accounts")?;
        {
            let state = self.state.lock().unwrap();
            ensure!(
                state
                    .features
                    .iter()
                    .any(|feature| feature["id"] == "oauth_account"
                        && feature["options"].as_array().is_some_and(|options| options
                            .iter()
                            .any(|option| option["id"] == *value))),
                "That OMP account option is no longer available"
            );
        }
        self.refresh_account(Some(value)).await
    }

    pub(super) async fn refresh_control_state(&self) -> Result<Value> {
        let native = self.refresh().await?;
        self.publish_control_state(&native).await?;
        Ok(native)
    }

    async fn publish_control_state(&self, native: &Value) -> Result<()> {
        if self.require_idle(native, "refreshing accounts").is_ok() {
            let discover = self.state.lock().unwrap().commands.is_none();
            if discover {
                self.discover_account_bridge().await;
            } else if let Err(error) = self.refresh_account(None).await {
                let mut state = self.state.lock().unwrap();
                state.account = None;
                state.update_features();
                tracing::debug!(%error, "OMP account picker unavailable");
            }
        } else {
            self.state.lock().unwrap().refresh_after_prompt = true;
        }
        let event = {
            let mut state = self.state.lock().unwrap();
            let model = catalog::model_id(&state.native["model"]);
            let mut event = json!({"type":"model_changed","provider":self.spec.id,"runtimeInfo":state.info(&self.spec.id)});
            if let Some(model) = model.filter(|model| state.config["model"] != model.as_str()) {
                state.config["model"] = json!(model);
                event["model"] = state.config["model"].clone();
            }
            if state.config["thinkingOptionId"] != state.native["thinkingLevel"] {
                state.config["thinkingOptionId"] = state.native["thinkingLevel"].clone();
                event["thinkingOptionId"] = state.config["thinkingOptionId"].clone();
            }
            event
        };
        self.emit(event).await
    }

    async fn refresh_models(&self, force: bool) -> Result<()> {
        let result = self
            .runtime()?
            .request(json!({"type":"get_available_models"}))
            .await?;
        let models = rows(&result, "models")?
            .iter()
            .filter(|model| model["provider"].is_string() && model["id"].is_string())
            .map(|model| catalog::model(model, &self.spec.id, true))
            .collect::<Vec<_>>();
        if !force && self.state.lock().unwrap().models.as_ref() == Some(&models) {
            return Ok(());
        }
        // The registry publishes this exact workspace's discovery, including providers
        // that an extension registered only in this process.
        self.emit(json!({"type":"omp_catalog_changed","provider":self.spec.id,"models":models}))
            .await?;
        self.state.lock().unwrap().models = Some(models);
        Ok(())
    }

    pub(super) fn schedule_control_refresh(self: &Arc<Self>, generation: u64, models: bool) {
        let inner = self.clone();
        tokio::spawn(async move {
            let _gate = inner.operation.lock().await;
            if inner.generation.load(Ordering::Acquire) != generation
                || inner.closed.load(Ordering::Acquire)
                || inner.state.lock().unwrap().native["sessionId"].is_null()
            {
                return;
            }
            if let Err(error) = inner.refresh_control_state().await {
                tracing::debug!(%error, "Could not refresh OMP session controls");
            }
            if models && let Err(error) = inner.refresh_models(false).await {
                tracing::debug!(%error, "Could not refresh OMP models");
            }
        });
    }

    fn require_idle(&self, native: &Value, action: &str) -> Result<()> {
        ensure!(
            self.state.lock().unwrap().active.is_none()
                && native["isStreaming"] != true
                && native["isCompacting"] != true,
            "Wait for the current OMP turn to end before {action}"
        );
        Ok(())
    }

    async fn after_authentication(&self) -> Result<()> {
        let (state, models) = tokio::join!(self.refresh_control_state(), self.refresh_models(true));
        state?;
        models
    }
}

impl Session {
    pub(super) async fn command(&self, name: &str, args: &str) -> Result<()> {
        if let Some(reason) = identity_block(name, args) {
            bail!("{reason}");
        }
        let runtime = self.inner.runtime()?;
        let usage = || {
            let hint = COMMANDS
                .iter()
                .find(|(command, _, _)| *command == name)
                .map(|(_, _, hint)| *hint)
                .unwrap_or("");
            anyhow!(
                "Usage: /{}{}",
                name,
                if hint.is_empty() {
                    String::new()
                } else {
                    format!(" {hint}")
                }
            )
        };
        let mut words = [""; 5];
        let mut count = 0;
        for (index, word) in args.split_whitespace().take(words.len()).enumerate() {
            words[index] = word;
            count = index + 1;
        }
        let words = &words[..count];
        let output = match (name, words) {
            ("login", []) => {
                let providers = runtime
                    .request(json!({"type":"get_login_providers"}))
                    .await?;
                let mut output = "OMP OAuth providers (use /login <provider>):".to_owned();
                for provider in rows(&providers, "providers")? {
                    use std::fmt::Write as _;
                    write!(
                        output,
                        "\n• {} — {}{}",
                        label(provider, "id"),
                        label(provider, "name"),
                        if provider["available"] == false {
                            " (unavailable)"
                        } else if provider["authenticated"] == true {
                            " (signed in)"
                        } else {
                            ""
                        }
                    )?;
                }
                output
            }
            ("login", [provider]) => {
                let _gate = self.inner.operation.lock().await;
                let native = self.inner.refresh().await?;
                self.inner.require_idle(&native, "logging in")?;
                {
                    struct Login<'a>(&'a std::sync::atomic::AtomicBool);
                    impl Drop for Login<'_> {
                        fn drop(&mut self) {
                            self.0.store(false, Ordering::Release);
                        }
                    }
                    self.inner.login_active.store(true, Ordering::Release);
                    let _login = Login(&self.inner.login_active);
                    runtime
                        .request_with_timeout(json!({"type":"login","providerId":provider}), None)
                        .await?;
                }
                self.inner
                    .after_authentication()
                    .await
                    .context("Signed in, but could not refresh OMP session data")?;
                format!("Signed in to OMP provider {provider}.")
            }
            ("logout", [] | [_] | [_, _]) => {
                let _gate = self.inner.operation.lock().await;
                let native = self.inner.refresh().await?;
                let provider = words
                    .first()
                    .copied()
                    .or_else(|| native["model"]["provider"].as_str())
                    .context("No current OMP provider; specify /logout <provider>")?;
                let result = runtime
                    .request(json!({"type":"get_logout_accounts","providerId":provider}))
                    .await?;
                let accounts = rows(&result, "accounts")?;
                if let Some(number) = words.get(1) {
                    self.inner.require_idle(&native, "logging out")?;
                    let id = number.parse::<u64>().map_err(|_| usage())?;
                    let account = accounts.iter().find(|account| account["credentialId"] == id)
                        .ok_or_else(|| anyhow!("No stored account #{id} for OMP provider {provider}; run /logout {provider} to list accounts"))?;
                    runtime
                        .request(json!({"type":"logout","providerId":provider,"credentialId":id}))
                        .await?;
                    self.inner
                        .after_authentication()
                        .await
                        .context("Signed out, but could not refresh OMP session data")?;
                    format!(
                        "Signed out of OMP provider {provider}: {}.",
                        label(account, "label")
                    )
                } else if accounts.is_empty() {
                    format!("OMP has no stored credentials for {provider}.")
                } else {
                    use std::fmt::Write as _;
                    let mut output = format!(
                        "OMP credentials for {provider} (use /logout {provider} <account number>):"
                    );
                    for account in accounts {
                        if let Some(id) = account["credentialId"].as_u64() {
                            write!(
                                output,
                                "\n• #{id}: {}{}",
                                label(account, "label"),
                                if account["active"] == true {
                                    " (active)"
                                } else {
                                    ""
                                }
                            )?;
                        }
                    }
                    output
                }
            }
            ("queue", _) => {
                let _gate = self.inner.operation.lock().await;
                queue_command(&self.inner, &runtime, words).await?
            }
            ("retry-stop", []) => {
                runtime.request(json!({"type":"abort_retry"})).await?;
                "OMP retry stop requested.".into()
            }
            ("autoretry", []) => {
                let state = self.inner.refresh().await?;
                let value = state["autoRetryEnabled"].as_bool().or(self
                    .inner
                    .state
                    .lock()
                    .unwrap()
                    .auto_retry);
                match value {
                    Some(value) => format!("OMP automatic retry is {} for this session.", on_off(value)),
                    None => "This session uses OMP's configured retry default. Use /autoretry on or /autoretry off to override it.".into(),
                }
            }
            ("autoretry", ["on" | "off"]) => {
                let _gate = self.inner.operation.lock().await;
                let enabled = words[0] == "on";
                runtime
                    .request(json!({"type":"set_auto_retry","enabled":enabled}))
                    .await?;
                self.inner.state.lock().unwrap().auto_retry = Some(enabled);
                format!(
                    "OMP automatic retry is {} for this session.",
                    on_off(enabled)
                )
            }
            ("subagents", []) => {
                let result = runtime.request(json!({"type":"get_subagents"})).await?;
                let agents = rows(&result, "subagents")?;
                if agents.is_empty() {
                    "No native OMP subagents are reported for this session.".into()
                } else {
                    use std::fmt::Write as _;
                    let mut output = "OMP subagents:".to_owned();
                    for agent in agents {
                        write!(
                            output,
                            "\n• {} — {} ({})",
                            label(agent, "id"),
                            label(agent, "agent"),
                            label(agent, "status")
                        )?;
                    }
                    output
                }
            }
            ("subagent-steer", [id, _, ..]) => {
                let message = args[id.len()..].trim_start();
                runtime
                    .request(json!({"type":"steer_subagent","subagentId":id,"message":message}))
                    .await?;
                format!("Guidance sent to OMP subagent {id}.")
            }
            ("subagent-cancel", [id]) => {
                let result = runtime
                    .request(json!({"type":"cancel_subagent","subagentId":id}))
                    .await?;
                ensure!(
                    result["cancelled"] == true,
                    "OMP did not cancel subagent {id}; it may no longer be running"
                );
                format!("Cancelled OMP subagent {id}.")
            }
            ("bash", [_, ..]) => {
                let _gate = self.inner.operation.lock().await;
                let (_, result) = runtime
                    .request_with_timeout(json!({"type":"bash","command":args}), None)
                    .await?;
                let status = if result["cancelled"] == true {
                    "canceled"
                } else if result["exitCode"].as_i64().is_some_and(|code| code != 0) {
                    "failed"
                } else {
                    "completed"
                };
                return self.inner.emit(json!({"type":"timeline","provider":self.inner.spec.id,"item":{
                    "type":"tool_call","callId":format!("omp-bash-{}",uuid::Uuid::new_v4()),"name":"bash","status":status,
                    "detail":{"type":"shell","command":args,"output":result["output"],"exitCode":result["exitCode"]},"error":null
                }})).await;
            }
            ("bash-stop", []) => {
                runtime.request(json!({"type":"abort_bash"})).await?;
                "OMP shell stop requested.".into()
            }
            ("next-model" | "next-thinking", []) => {
                let _gate = self.inner.operation.lock().await;
                let native = self.inner.refresh().await?;
                self.inner
                    .require_idle(&native, "cycling models or thinking levels")?;
                let rpc = if name == "next-model" {
                    "cycle_model"
                } else {
                    let result = runtime
                        .request(json!({"type":"get_available_thinking_levels"}))
                        .await?;
                    ensure!(
                        rows(&result, "levels")?.len() > 1,
                        "The current OMP model has no alternate thinking levels"
                    );
                    "cycle_thinking_level"
                };
                let result = runtime.request(json!({"type":rpc})).await?;
                ensure!(
                    !result.is_null(),
                    "OMP did not report an available alternative"
                );
                let state = self.inner.refresh_control_state().await?;
                if name == "next-model" {
                    format!(
                        "OMP model: {}.",
                        catalog::model_id(&state["model"])
                            .context("OMP did not report the new model")?
                    )
                } else {
                    format!(
                        "OMP thinking level: {}.",
                        state["thinkingLevel"]
                            .as_str()
                            .context("OMP did not report the new thinking level")?
                    )
                }
            }
            ("handoff", _) => return self.native_handoff(&runtime, args).await,
            ("fork", [] | [_]) => return self.native_fork(words.first().copied()).await,
            ("todo", [verb, ..]) if verb.eq_ignore_ascii_case("edit") => {
                let edit = args
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, rest)| rest.trim());
                self.edit_todo(&runtime, edit).await?
            }
            _ => return Err(usage()),
        };
        self.inner.out_of_band_message(output).await
    }

    async fn native_handoff(&self, runtime: &Transport, instructions: &str) -> Result<()> {
        let _gate = self.inner.operation.lock().await;
        let before = self.inner.refresh().await?;
        self.inner.state.lock().unwrap().live = true;
        let mut command = json!({"type":"handoff"});
        if !instructions.is_empty() {
            command["customInstructions"] = json!(instructions);
        }
        let result = runtime.request_with_timeout(command, None).await;
        // 18.22.1 may adopt a new native transcript; newer builds compact in
        // place. get_state, not savedPath, describes the current live owner.
        let after = self.inner.refresh().await;
        if let Ok(after) = &after
            && (after["sessionId"] != before["sessionId"]
                || after["sessionFile"] != before["sessionFile"])
        {
            {
                let mut state = self.inner.state.lock().unwrap();
                state.branch = None;
                state.account = None;
                state.shown_queue = None;
                state.update_features();
            }
            self.inner.emit(json!({"type":"thread_started","provider":self.inner.spec.id,"sessionId":after["sessionId"]})).await?;
        }
        result.context("Failed to hand off the OMP conversation")?;
        let after = after.context("OMP handed off, but the new session state could not be read")?;
        self.inner.publish_control_state(&after).await
    }

    async fn edit_todo(&self, runtime: &Transport, args: &str) -> Result<String> {
        let _gate = self.inner.operation.lock().await;
        let mut native = self.inner.refresh().await?;
        let mut phases = native["todoPhases"].take();
        let phases_list = phases
            .as_array_mut()
            .context("This OMP runtime did not report its native todo list")?;
        if args.is_empty() {
            use std::fmt::Write as _;
            let mut listing =
                "OMP todos — rename a task with /todo edit <phase.task> <new text>:".to_owned();
            for (p, phase) in phases_list.iter().enumerate() {
                for (t, task) in rows(phase, "tasks")?.iter().enumerate() {
                    write!(
                        listing,
                        "\n{}.{} [{}] {}: {}",
                        p + 1,
                        t + 1,
                        label(task, "status"),
                        label(phase, "name"),
                        label(task, "content")
                    )?;
                }
            }
            return Ok(listing);
        }
        self.inner.require_idle(&native, "editing todos")?;
        let (address, text) = args
            .split_once(char::is_whitespace)
            .map(|(address, text)| (address, text.trim()))
            .filter(|(_, text)| !text.is_empty())
            .context("Usage: /todo edit <phase.task> <new text>")?;
        let ordinal = |n: &str| n.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
        let (p, t) = address
            .split_once('.')
            .and_then(|(p, t)| ordinal(p).zip(ordinal(t)))
            .context("Use the phase.task number from /todo edit")?;
        let task = phases_list
            .get_mut(p)
            .and_then(|phase| phase["tasks"].get_mut(t))
            .filter(|task| task["content"].is_string())
            .context("That OMP todo does not exist")?;
        task["content"] = json!(text);
        let mut result = runtime
            .request(json!({"type":"set_todos","phases":phases}))
            .await?;
        let phases = result["todoPhases"].take();
        ensure!(
            phases[p]["tasks"][t]["content"] == text,
            "OMP did not retain the requested todo edit"
        );
        if let Some(todo) = super::history::todo_from_phases(&phases) {
            self.inner
                .emit(json!({"type":"timeline","provider":self.inner.spec.id,"item":todo}))
                .await?;
        }
        self.inner.state.lock().unwrap().native["todoPhases"] = phases;
        Ok(format!("Updated OMP todo {address}."))
    }

    async fn native_fork(&self, entry: Option<&str>) -> Result<()> {
        let _gate = self.inner.operation.lock().await;
        let source = self.inner.refresh().await?;
        self.inner
            .require_idle(&source, "forking the conversation")?;
        ensure!(
            source["isSettled"] == true,
            "OMP has not settled the native session. Wait for native work to finish before forking"
        );
        let original = source["sessionFile"]
            .as_str()
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .context("Fork requires a persisted OMP session file")?;
        let revision = crate::providers::api::native_file_revision(original)
            .await
            .context("Could not verify the OMP source transcript")?;
        let source_id = source["sessionId"]
            .as_str()
            .context("OMP did not report its source session id")?;
        let runtime = self.inner.runtime()?;
        let lease = runtime
            .request(json!({"type":"get_session_lease","sessionIds":[source_id]}))
            .await?;
        ensure!(
            rows(&lease, "held")?.iter().any(|id| id == source_id),
            "OMP could not verify the source session lease; not opening a second writer"
        );
        let config = self.inner.state.lock().unwrap().config.clone();
        let original_name = original
            .to_str()
            .context("Invalid OMP source transcript path")?;
        let (fork, mut events) = Transport::spawn(
            launch::command(
                &self.inner.spec,
                &config,
                &self.inner.context,
                Some(original_name),
                None,
            )?,
            launch::timeout(&self.inner.spec, &config),
            self.inner.context.cancel.child_token(),
        )
        .await?;
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        let result = async {
            fork.negotiate(launch::ready_timeout(&self.inner.spec, &config)).await?;
            let initial = fork.request(json!({"type":"get_state"})).await?;
            ensure!(initial["sessionId"] == source_id && initial["sessionFile"] == original_name, "The fork worker did not open the source conversation");
            ensure!(initial["messageCount"] == source["messageCount"],
                "The OMP session file does not match the live transcript yet; the source was not forked");
            let mut command = json!({"type":"fork"});
            if let Some(entry) = entry {
                command["entryId"] = json!(entry);
            }
            let response = fork.request(command).await?;
            ensure!(response["cancelled"] == false, "OMP did not complete the native fork");
            let forked = fork.request(json!({"type":"get_state"})).await?;
            let file = forked["sessionFile"].as_str().map(Path::new).filter(|path| path.is_absolute() && *path != original)
                .context("OMP did not return a distinct native fork file")?;
            ensure!(forked["sessionId"].is_string() && forked["sessionId"] != source_id, "OMP did not create a new session id for the fork");
            Ok::<_, anyhow::Error>(file.to_path_buf())
        }.await;
        let close = fork.close().await;
        drain.abort();
        let file = result?;
        close?;
        let still_original = self.inner.refresh().await?;
        ensure!(
            still_original["sessionId"] == source_id
                && still_original["sessionFile"] == original_name
                && crate::providers::api::native_file_revision(original)
                    .await
                    .as_ref()
                    == Some(&revision),
            "The OMP source transcript changed while forking; the fork was not imported"
        );
        ensure!(
            tokio::fs::metadata(&file)
                .await
                .is_ok_and(|meta| meta.is_file() && meta.len() > 0),
            "OMP did not persist the native fork file"
        );
        self.inner
            .emit(json!({"type":"omp_fork_ready","provider":self.inner.spec.id,"sessionFile":file}))
            .await
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

async fn queue_command(inner: &Inner, runtime: &Transport, words: &[&str]) -> Result<String> {
    if let [
        "mode",
        queue @ ("steering" | "follow-up"),
        mode @ ("all" | "one"),
    ] = words
    {
        let method = if *queue == "steering" {
            "set_steering_mode"
        } else {
            "set_follow_up_mode"
        };
        let mode = if *mode == "one" {
            "one-at-a-time"
        } else {
            "all"
        };
        runtime.request(json!({"type":method,"mode":mode})).await?;
        return Ok(format!("OMP {queue} queue delivery: {mode}."));
    }
    ensure!(
        matches!(words, [] | ["remove" | "promote", _]),
        "Usage: /queue [remove <n>|promote <n>|mode <steering|follow-up> <all|one>]"
    );
    let state = runtime.request(json!({"type":"get_state"})).await?;
    let queued = &state["queuedMessages"];
    let steering = rows(queued, "steering")?;
    let follow = rows(queued, "followUp")?;
    let claimed = queued["liveSteered"].as_u64().unwrap_or(0) as usize;
    let listing = || {
        use std::fmt::Write as _;
        let mut output = format!(
            "OMP queue (steering: {}, follow-up: {}):",
            label(&state, "steeringMode"),
            label(&state, "followUpMode")
        );
        if steering.is_empty() && follow.is_empty() {
            output.push_str(" empty");
        }
        for (i, text) in steering.iter().chain(follow).enumerate() {
            let kind = if i < claimed {
                "steering, already delivering"
            } else if i < steering.len() {
                "steering"
            } else {
                "follow-up"
            };
            let text = text.as_str().unwrap_or_default().replace('\n', " ");
            let _ = write!(output, "\n{}. [{kind}] {text}", i + 1);
        }
        output
    };
    let [action, n] = words else {
        inner.state.lock().unwrap().shown_queue = Some(queued.clone());
        return Ok(listing());
    };
    let index = n
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .context("Queue entries are numbered starting from 1")?;
    ensure!(
        inner
            .state
            .lock()
            .unwrap()
            .shown_queue
            .as_ref()
            .is_none_or(|last| last == queued),
        "The OMP queue changed since it was listed; run /queue to refresh the entry numbers"
    );
    let (queue, contents, selected) = if index < steering.len() {
        ensure!(
            index >= claimed,
            "OMP is already delivering that steering entry"
        );
        (
            "steering",
            &steering[claimed.min(steering.len())..],
            &steering[index],
        )
    } else {
        let selected = follow
            .get(index - steering.len())
            .context("No such OMP queue entry; run /queue to refresh the list")?;
        ("followUp", follow, selected)
    };
    if *action == "promote" && queue != "followUp" {
        bail!("Only follow-up entries can be promoted to steering");
    }
    // OMP's RPC only targets the first matching text in a queue. Refuse an
    // ambiguous edit instead of silently changing a different entry or image.
    ensure!(
        contents
            .iter()
            .filter(|message| *message == selected)
            .take(2)
            .count()
            == 1,
        "OMP cannot distinguish entries with identical text in this queue; no entry was changed"
    );
    let (rpc, key) = if *action == "remove" {
        ("remove_queued_message", "removed")
    } else {
        ("promote_queued_message", "promoted")
    };
    let mut command = json!({"type":rpc,"message":selected});
    if *action == "remove" {
        command["queue"] = json!(queue);
    }
    let result = runtime.request(command).await?;
    ensure!(
        result[key] == true,
        "That OMP queue entry is no longer editable; run /queue again"
    );
    Ok(format!("OMP queue entry {n} {key}."))
}

#[cfg(all(test, unix))]
mod tests;
