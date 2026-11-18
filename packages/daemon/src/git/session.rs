use super::{GitService, checkout_error, command, facts, mutations};
use crate::transport::RequestContext;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

impl GitService {
    pub(super) async fn handle_message(
        &self,
        context: &RequestContext,
        message: &Value,
    ) -> Result<bool> {
        let kind = message["type"].as_str().unwrap_or("");
        if kind == "unsubscribe_checkout_diff_request" {
            context
                .release_subscription(
                    message["subscriptionId"]
                        .as_str()
                        .ok_or_else(|| anyhow!("subscriptionId required"))?,
                )
                .await?;
            return Ok(true);
        }
        let response = match kind {
            "checkout.observe.request" => "checkout.observe.response",
            "checkout_status_request" => "checkout_status_response",
            "validate_branch_request" => "validate_branch_response",
            "branch_suggestions_request" => "branch_suggestions_response",
            "checkout.commits.list.request" => "checkout.commits.list.response",
            "checkout.commits.file_diff.request" => "checkout.commits.file_diff.response",
            "checkout.log.request" => "checkout.log.response",
            "checkout.refs.request" => "checkout.refs.response",
            "checkout.commit.get.request" => "checkout.commit.get.response",
            "checkout.diff.get.request" => "checkout.diff.get.response",
            "subscribe_checkout_diff_request" => "subscribe_checkout_diff_response",
            "checkout_switch_branch_request" => "checkout_switch_branch_response",
            "checkout.rename_branch.request" => "checkout.rename_branch.response",
            "checkout.discard_changes.request" => "checkout.discard_changes.response",
            "checkout_commit_request" => "checkout_commit_response",
            "checkout.commit_selected.request" => "checkout.commit_selected.response",
            "checkout_merge_request" => "checkout_merge_response",
            "checkout_merge_from_base_request" => "checkout_merge_from_base_response",
            "checkout_pull_request" => "checkout_pull_response",
            "checkout_push_request" => "checkout_push_response",
            "checkout.refresh.request" => "checkout.refresh.response",
            "stash_save_request" => "stash_save_response",
            "stash_pop_request" => "stash_pop_response",
            "stash_list_request" => "stash_list_response",
            "checkout_pr_create_request"
            | "checkout_pr_merge_request"
            | "checkout_pr_status_request"
            | "pull_request_timeline_request"
            | "checkout.forge.set_auto_merge.request"
            | "checkout.github.set_auto_merge.request"
            | "checkout.forge.get_check_details.request"
            | "checkout.github.get_check_details.request"
            | "forge.search.request"
            | "github_search_request" => "",
            _ => return Ok(false),
        };
        let raw_cwd = message["cwd"]
            .as_str()
            .ok_or_else(|| anyhow!("cwd required"))?;
        let cwd = if raw_cwd == "~" || raw_cwd.starts_with("~/") {
            let app = self
                .app
                .upgrade()
                .ok_or_else(|| anyhow!("Daemon shutting down"))?;
            app.user_home.join(raw_cwd.strip_prefix("~/").unwrap_or(""))
        } else {
            PathBuf::from(raw_cwd)
        };
        let cancel = context.cancellation();
        if kind == "checkout.observe.request" {
            if !context.is_modern()
                || !crate::transport::has_capability(context.capabilities(), "checkout_observation")
            {
                bail!("Checkout observation requires owned subscriptions");
            }
            let owner = context.begin_subscription("checkout", None, None)?;
            if let Err(error) = self.retain_observation(&cwd).await {
                owner.release().await?;
                return Err(error);
            }
            let bootstrap = async {
                context.send(json!({"type":"checkout.observe.response","payload":{
                    "cwd":raw_cwd,"subscriptionId":owner.response_id(),"requestId":message["requestId"]
                }})).await?;
                owner.activate().await
            }.await;
            if let Err(error) = bootstrap {
                self.release_observation(&cwd).await?;
                owner.release().await?;
                return Err(error);
            }
            let app = self.app.clone();
            let alias = PathBuf::from(raw_cwd);
            tokio::spawn(async move {
                let cancel = owner.cancellation();
                // Equality suppression is shared by the checkout, not by
                // subscribers. Even a fully warm cache must prime this owner.
                if let Some(app) = app.upgrade() {
                    let initial = async {
                        let snapshot = app.git.snapshot(&cwd, false, cancel.clone()).await?;
                        owner
                            .emit(super::checkout_status_update(&alias, &snapshot))
                            .await
                    }
                    .await;
                    if let Err(error) = initial
                        && !cancel.is_cancelled()
                    {
                        tracing::debug!(%error, cwd=%cwd.display(), "Initial checkout status unavailable");
                    }
                }
                cancel.cancelled().await;
                if let Some(app) = app.upgrade() {
                    let _ = app.git.release_observation(&cwd).await;
                }
                let _ = owner.release().await;
            });
            return Ok(true);
        }
        if kind == "subscribe_checkout_diff_request" {
            self.subscribe_diff(context, message, &cwd).await?;
            return Ok(true);
        }
        if response.is_empty() {
            match self.forge_message(context, message, &cwd).await {
                Ok(Some(mut frame)) => {
                    if frame["payload"].get("cwd").is_some() {
                        frame["payload"]["cwd"] = json!(raw_cwd);
                    }
                    context.send(frame).await?;
                }
                Ok(None) => return Ok(false),
                Err(error) => {
                    let (response, mut payload) = match kind {
                        "checkout_pr_create_request" => (
                            "checkout_pr_create_response",
                            json!({"url":null,"number":null}),
                        ),
                        "checkout_pr_merge_request" => {
                            ("checkout_pr_merge_response", json!({"success":false}))
                        }
                        _ => return Err(error),
                    };
                    payload["cwd"] = json!(raw_cwd);
                    payload["requestId"] = message["requestId"].clone();
                    payload["error"] = checkout_error(&error);
                    context
                        .send(json!({"type":response,"payload":payload}))
                        .await?;
                }
            }
            return Ok(true);
        }
        let activity = context.current_activity();
        let result = self
            .operation(
                kind,
                message,
                &cwd,
                activity["focusedAgentId"].as_str(),
                cancel,
            )
            .await;
        let mut payload = match result {
            Ok(value) => value,
            Err(error) => {
                let mut payload = match kind {
                    "checkout_status_request" => facts::non_git(&cwd),
                    "validate_branch_request" => {
                        json!({"exists":false,"resolvedRef":null,"isRemote":false})
                    }
                    "branch_suggestions_request" => json!({"branches":[],"branchDetails":[]}),
                    "checkout.commits.list.request" => json!({"baseRef":null,"commits":[]}),
                    "checkout.commits.file_diff.request" => {
                        json!({"sha":message["sha"],"path":message["path"],"file":null})
                    }
                    "checkout.log.request" => {
                        json!({"head":null,"commits":[],"hasMore":false})
                    }
                    "checkout.refs.request" => json!({
                        "head":{"sha":null,"branch":null,"detached":false},
                        "local":[],
                        "remote":[],
                        "tags":[],
                    }),
                    "checkout.commit.get.request" => json!({"commit":null}),
                    "checkout.diff.get.request" => json!({"files":[]}),
                    "stash_list_request" => json!({"entries":[]}),
                    "checkout_switch_branch_request" => {
                        json!({"success":false,"branch":message["branch"]})
                    }
                    "checkout.rename_branch.request" => {
                        json!({"success":false,"currentBranch":null})
                    }
                    _ => json!({"success":false}),
                };
                payload["error"] =
                    if kind == "validate_branch_request" || kind == "branch_suggestions_request" {
                        json!(error.to_string())
                    } else {
                        checkout_error(&error)
                    };
                payload
            }
        };
        if !matches!(
            kind,
            "validate_branch_request" | "branch_suggestions_request"
        ) {
            payload["cwd"] = json!(raw_cwd);
        }
        payload["requestId"] = message["requestId"].clone();
        context
            .send(json!({"type":response,"payload":payload}))
            .await?;
        Ok(true)
    }
    async fn operation(
        &self,
        kind: &str,
        message: &Value,
        cwd: &Path,
        focused_agent_id: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let env = self.environment()?;
        match kind {
            "checkout_status_request" => {
                return Ok(super::status_projection(
                    &self.snapshot(cwd, false, cancel).await?,
                ));
            }
            "checkout.diff.get.request" => {
                return self.read_diff(cwd, &message["compare"], cancel).await;
            }
            "checkout.commits.list.request" => {
                let mut value = super::diff::list_commits(cwd, &env, cancel).await?;
                value["error"] = Value::Null;
                return Ok(value);
            }
            "checkout.commits.file_diff.request" => {
                let sha = string(message, "sha")?;
                let path = string(message, "path")?;
                return Ok(
                    json!({"sha":sha,"path":path,"file":super::diff::commit_file(cwd,sha,path,&env,cancel).await?,"error":null}),
                );
            }
            "checkout.log.request" => {
                let text = |key: &str| message[key].as_str().filter(|s| !s.is_empty());
                let query = super::diff::LogQuery {
                    revision: text("revision"),
                    skip: message["skip"].as_u64(),
                    limit: message["limit"].as_u64(),
                    text: text("text"),
                    author: text("author"),
                    path: text("path"),
                };
                let mut value = super::diff::log(cwd, &query, &env, cancel).await?;
                value["error"] = Value::Null;
                return Ok(value);
            }
            "checkout.refs.request" => {
                let mut value = super::diff::refs(cwd, &env, cancel).await?;
                value["error"] = Value::Null;
                return Ok(value);
            }
            "checkout.commit.get.request" => {
                let mut value =
                    super::diff::commit(cwd, string(message, "sha")?, &env, cancel).await?;
                value["error"] = Value::Null;
                return Ok(value);
            }
            "validate_branch_request" => {
                let name = string(message, "branchName")?;
                command::request_ref(name, "branch")?;
                let resolution = facts::resolve_branch(cwd, name, &env, cancel).await?;
                let remote = resolution["kind"] == "remote-only";
                return Ok(
                    json!({"exists":resolution["kind"]!="not-found","resolvedRef":if remote{&resolution["remoteRef"]}else{&resolution["name"]},"isRemote":remote,"error":null}),
                );
            }
            "branch_suggestions_request" => {
                let branches = facts::branches(
                    cwd,
                    message["query"].as_str().unwrap_or(""),
                    message["limit"].as_u64().unwrap_or(50) as usize,
                    &env,
                    cancel,
                )
                .await?;
                return Ok(
                    json!({"branches":branches.iter().map(|v|v["name"].clone()).collect::<Vec<_>>(),"branchDetails":branches,"error":null}),
                );
            }
            "stash_list_request" => {
                return Ok(
                    json!({"entries":facts::stashes(cwd,message["paseoOnly"].as_bool().unwrap_or(true),&env,cancel).await?,"error":null}),
                );
            }
            "checkout.refresh.request" => {
                self.invalidate(cwd).await?;
                self.snapshot(cwd, false, cancel.clone()).await?;
                self.refresh_forge(cwd, cancel).await?;
                return Ok(json!({"success":true,"error":null}));
            }
            _ => {}
        }
        let missing = |error: &anyhow::Error| {
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        };
        // The source's merge handlers inspect the snapshot before touching git; a
        // missing directory has no git facts.
        let snapshot_git = if matches!(
            kind,
            "checkout_merge_request" | "checkout_merge_from_base_request"
        ) {
            match self.snapshot(cwd, false, cancel.clone()).await {
                Ok(snapshot) => Some(super::status_projection(&snapshot)),
                Err(error) if missing(&error) => Some(Value::Null),
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let (_, entry) = match self.checkout_entry(cwd).await {
            Ok(entry) => entry,
            // `requireGitRepo` treats a missing directory like any other non-repository.
            Err(error)
                if !matches!(kind, "stash_save_request" | "stash_pop_request")
                    && missing(&error) =>
            {
                return Err(command::NotGitRepo(cwd.display().to_string()).into());
            }
            Err(error) => return Err(error),
        };
        if kind == "checkout.commit_selected.request" {
            let paths = message["paths"]
                .as_array()
                .ok_or_else(|| anyhow!("paths required"))?
                .iter()
                .map(|path| {
                    path.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("Invalid path"))
                })
                .collect::<Result<Vec<_>>>()?;
            let text = string(message, "message")?.to_owned();
            let cwd = cwd.to_owned();
            let app = self.app.clone();
            // A socket closing aborts its request tasks. Keep the index transaction
            // and checkout mutex alive through commit settlement, not just submission.
            return tokio::spawn(async move {
                let _mutation = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => bail!("Git mutation cancelled before admission"),
                    guard = entry.mutation.lock() => guard,
                };
                let result = mutations::commit_selected(&cwd, &paths, &text, &env, cancel).await;
                if let Some(app) = app.upgrade()
                    && let Err(error) = app.git.notify_mutation(&cwd, true).await
                {
                    tracing::warn!(%error, "Selected commit settled; refresh failed");
                }
                result.map(|sha| json!({"success":true,"error":null,"commitSha":sha}))
            })
            .await?;
        }
        let _mutation = tokio::select! {_=cancel.cancelled()=>bail!("Git mutation cancelled before admission"),guard=entry.mutation.lock()=>guard};
        let mut result = json!({"success":true,"error":null});
        match kind {
            "checkout_switch_branch_request" => {
                let value =
                    mutations::checkout(cwd, string(message, "branch")?, &env, cancel.clone())
                        .await?;
                result["source"] = value["source"].clone();
                result["branch"] = message["branch"].clone();
            }
            "checkout.rename_branch.request" => {
                let value =
                    mutations::rename(cwd, string(message, "branch")?, &env, cancel.clone())
                        .await?;
                result["currentBranch"] = value["currentBranch"].clone();
            }
            "checkout.discard_changes.request" => {
                let paths = message["paths"]
                    .as_array()
                    .ok_or_else(|| anyhow!("paths required"))?
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| anyhow!("Invalid path"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                mutations::discard(cwd, &paths, &env, cancel.clone()).await?;
            }
            "checkout_commit_request" => {
                let generated;
                let text = match message["message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(value) => value,
                    None => {
                        generated = self
                            .generate_metadata(cwd, false, None, focused_agent_id, cancel.clone())
                            .await?;
                        string(&generated, "message")?
                    }
                };
                mutations::commit(
                    cwd,
                    text,
                    message["addAll"].as_bool().unwrap_or(true),
                    &env,
                    cancel.clone(),
                )
                .await?;
            }
            "checkout_merge_request" => {
                let git = snapshot_git.unwrap_or_default();
                require_git(&git, cwd)?;
                if message["requireCleanTarget"] == true && git["isDirty"] == true {
                    bail!("Working directory has uncommitted changes.");
                }
                let base = message["baseRef"]
                    .as_str()
                    .or_else(|| git["baseRef"].as_str())
                    .ok_or_else(|| anyhow!("Base branch is required for merge"))?;
                let target = mutations::merge_to_base(
                    cwd,
                    Some(base.strip_prefix("origin/").unwrap_or(base)),
                    message["strategy"] == "squash",
                    &env,
                    cancel.clone(),
                )
                .await?;
                if target != cwd {
                    self.notify_mutation(&target, message["strategy"] != "squash")
                        .await?;
                }
            }
            "checkout_merge_from_base_request" => {
                let require_clean = message["requireCleanTarget"].as_bool().unwrap_or(true);
                if require_clean && snapshot_git.unwrap_or_default()["isDirty"] == true {
                    bail!("Working directory has uncommitted changes.");
                }
                mutations::merge_from_base(
                    cwd,
                    message["baseRef"].as_str(),
                    require_clean,
                    &env,
                    cancel.clone(),
                )
                .await?
            }
            "checkout_pull_request" => mutations::pull(cwd, &env, cancel.clone()).await?,
            "checkout_push_request" => mutations::push(cwd, &env, cancel.clone()).await?,
            "stash_save_request" => {
                let branch = message["branch"].as_str().unwrap_or("").trim();
                mutations::execute(
                    cwd,
                    &[
                        "stash",
                        "push",
                        "--include-untracked",
                        "-m",
                        &format!(
                            "paseo-auto-stash: {}",
                            if branch.is_empty() { "unnamed" } else { branch }
                        ),
                    ],
                    &env,
                    cancel.clone(),
                )
                .await?;
            }
            "stash_pop_request" => {
                let index = message["stashIndex"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("Invalid stash index"))?;
                mutations::execute(
                    cwd,
                    &["stash", "pop", &format!("stash@{{{index}}}")],
                    &env,
                    cancel.clone(),
                )
                .await?;
            }
            _ => bail!("Unknown Git mutation {kind}"),
        }
        let refs = !(matches!(
            kind,
            "checkout.discard_changes.request" | "stash_save_request" | "stash_pop_request"
        ) || (kind == "checkout_merge_request" && message["strategy"] == "squash"));
        if let Err(error) = self.notify_mutation(cwd, refs).await {
            tracing::warn!(%error,"Mutation completed; refresh failed");
        }
        Ok(result)
    }
    async fn subscribe_diff(
        &self,
        context: &RequestContext,
        message: &Value,
        cwd: &Path,
    ) -> Result<()> {
        let requested = message["subscriptionId"].as_str();
        let slot = requested.map(|id| format!("diff:{id}"));
        let owner = context.begin_subscription("diffs", requested, slot.as_deref())?;
        let response_cwd = cwd.to_owned();
        let (cwd, entry) = match self.checkout_entry(cwd).await {
            Ok(entry) => entry,
            Err(error) => {
                owner.release().await?;
                return Err(error);
            }
        };
        let compare = message["compare"].clone();
        let diff = match Self::diff_entry(&entry, &compare).await {
            Ok(diff) => diff,
            Err(error) => {
                owner.release().await?;
                return Err(error);
            }
        };
        let mut changes = diff.changes.subscribe();
        if let Err(error) = self.retain_observation(&response_cwd).await {
            owner.release().await?;
            return Err(error);
        }
        let initial = match self
            .read_cached_diff(&cwd, &diff, &compare, owner.cancellation())
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = self.release_observation(&response_cwd).await;
                owner.release().await?;
                return Err(error);
            }
        };
        let mut payload = initial.clone();
        payload["cwd"] = json!(response_cwd);
        payload["subscriptionId"] = json!(owner.response_id());
        payload["requestId"] = message["requestId"].clone();
        let bootstrap = async {
            context
                .send(json!({"type":"subscribe_checkout_diff_response","payload":payload}))
                .await?;
            owner.activate().await
        }
        .await;
        if let Err(error) = bootstrap {
            let _ = self.release_observation(&response_cwd).await;
            let _ = owner.release().await;
            return Err(error);
        }
        let app = self.app.clone();
        tokio::spawn(async move {
            let cancel = owner.cancellation();
            let mut previous = initial;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    changed = changes.changed() => if changed.is_err() { break; }
                }
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {}
                }
                let Some(state) = app.upgrade() else { break };
                let next = state
                    .git
                    .read_cached_diff(&cwd, &diff, &compare, cancel.clone())
                    .await;
                drop(state);
                let mut value = match next {
                    Ok(value) => value,
                    Err(error) => json!({"cwd":cwd,"files":[],"error":checkout_error(&error)}),
                };
                if value == previous {
                    continue;
                }
                previous = value.clone();
                value["cwd"] = json!(response_cwd);
                value["subscriptionId"] = json!(owner.response_id());
                if owner
                    .emit(json!({"type":"checkout_diff_update","payload":value}))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            if let Some(app) = app.upgrade() {
                let _ = app.git.release_observation(&response_cwd).await;
            }
            let _ = owner.release().await;
        });
        Ok(())
    }
    async fn forge_message(
        &self,
        context: &RequestContext,
        message: &Value,
        cwd: &Path,
    ) -> Result<Option<Value>> {
        let env = self.environment()?;
        let cancel = context.cancellation();
        let mut input = message.clone();
        if message["type"] == "checkout_pr_create_request" {
            if self
                .forge
                .resolve(cwd, &env, cancel.clone())
                .await?
                .is_none()
            {
                bail!("No forge remote configured");
            }
            let activity = context.current_activity();
            let missing_title = input["title"].as_str().is_none_or(|s| s.trim().is_empty());
            let missing_body = input["body"].as_str().is_none_or(|s| s.trim().is_empty());
            if missing_title || missing_body {
                let generated = self
                    .generate_metadata(
                        cwd,
                        true,
                        input["baseRef"].as_str(),
                        activity["focusedAgentId"].as_str(),
                        cancel.clone(),
                    )
                    .await?;
                if missing_title {
                    input["title"] = generated["title"].clone();
                }
                if missing_body {
                    input["body"] = generated["body"].clone();
                }
            }
            let head = facts::current_branch(cwd, &env, cancel.clone())
                .await?
                .ok_or_else(|| anyhow!("Unable to determine head branch for PR"))?;
            let base =
                command::operation_base(cwd, input["baseRef"].as_str(), &env, cancel.clone())
                    .await?
                    .ok_or_else(|| anyhow!("Unable to determine base branch for PR"))?;
            command::safe_ref(&head)?;
            mutations::execute(cwd, &["push", "-u", "origin", &head], &env, cancel.clone()).await?;
            input["head"] = json!(head);
            input["base"] = json!(command::branch_name(&base));
        }
        let response = self
            .forge
            .handle_operation(&input, cwd, &env, cancel.clone())
            .await?;
        if matches!(
            message["type"].as_str(),
            Some(
                "checkout_pr_create_request"
                    | "checkout_pr_merge_request"
                    | "checkout.forge.set_auto_merge.request"
                    | "checkout.github.set_auto_merge.request"
            )
        ) {
            let _ = self.notify_mutation(cwd, true).await;
        }
        Ok(response)
    }
    pub async fn refresh_forge(&self, cwd: &Path, cancel: CancellationToken) -> Result<Value> {
        let env = self.environment()?;
        let payload = self.forge.status_payload(cwd, &env, cancel).await;
        let mut value = json!({"featuresEnabled":payload["githubFeaturesEnabled"],"pullRequest":payload["status"],"authState":payload["authState"],"error":payload["error"]});
        if let Some(forge) = payload.get("forge") {
            value["forge"] = forge.clone();
        }
        self.apply_forge_payload(cwd, &payload).await?;
        Ok(value)
    }
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().ok_or_else(|| anyhow!("{key} required"))
}

/// The merge handlers read the snapshot instead of running git; a non-repository
/// still reports the typed `NOT_GIT_REPO` code (same message) clients recognize.
fn require_git(git: &Value, cwd: &Path) -> Result<()> {
    if git["isGit"] == true {
        Ok(())
    } else {
        Err(command::NotGitRepo(cwd.display().to_string()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{ChangedPaths, CheckoutChange};
    use crate::transport::{OutboundFrame, SourceConfig};
    use std::{
        collections::HashSet,
        sync::{Arc, RwLock, atomic::Ordering},
        time::Duration,
    };

    #[test]
    fn merge_snapshot_outside_a_repository_is_typed_not_git_repo() {
        let cwd = Path::new("/home/user");
        for git in [Value::Null, json!({"isGit": false})] {
            let error = require_git(&git, cwd).unwrap_err();
            assert_eq!(
                checkout_error(&error),
                json!({"code": "NOT_GIT_REPO", "message": "Not a git repository: /home/user"})
            );
        }
        assert!(require_git(&json!({"isGit": true}), cwd).is_ok());
    }

    /// The Commit tab's summary subscription on a plain folder: observation
    /// setup succeeds and the diff payload names the typed condition.
    #[tokio::test]
    async fn uncommitted_summary_outside_a_repository_reports_not_git_repo() -> Result<()> {
        let root = tempfile::tempdir()?;
        let folder = root.path().join("plain");
        tokio::fs::create_dir(&folder).await?;
        let app = crate::state::AppState::new(
            root.path().join("home"),
            root.path().into(),
            std::env::current_exe()?,
        );
        app.config.initialize().await?;
        let cancel = CancellationToken::new();
        app.git.retain_observation(&folder).await?;
        let compare = json!({"mode":"uncommitted","detail":"summary","ignoreWhitespace":false});
        let payload = app.git.read_diff(&folder, &compare, cancel.clone()).await?;
        assert_eq!(payload["files"], json!([]), "{payload}");
        assert_eq!(payload["error"]["code"], "NOT_GIT_REPO", "{payload}");
        app.git.release_observation(&folder).await?;
        let status = app
            .git
            .operation("checkout_status_request", &json!({}), &folder, None, cancel)
            .await?;
        assert_eq!(status["isGit"], false, "{status}");
        Ok(())
    }

    #[tokio::test]
    async fn observation_targets_diff_subscriptions_and_forge_generation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let folder = root.path().join("checkout");
        tokio::fs::create_dir(&folder).await?;
        let app = crate::state::AppState::new(
            root.path().join("home"),
            root.path().into(),
            std::env::current_exe()?,
        );
        app.config.initialize().await?;
        let cancel = CancellationToken::new();
        let env = app.git.environment()?;
        let git =
            |args: &'static [&'static str]| command::text(&folder, args, &env, cancel.clone());
        git(&["init", "-b", "main"]).await?;
        git(&["config", "user.name", "Observation test"]).await?;
        git(&["config", "user.email", "test@example.invalid"]).await?;
        git(&["config", "commit.gpgsign", "false"]).await?;
        git(&["config", "status.showUntrackedFiles", "all"]).await?;
        tokio::fs::write(folder.join("tracked"), "clean\n").await?;
        git(&["add", "tracked"]).await?;
        git(&["commit", "-m", "base"]).await?;
        tokio::fs::write(folder.join(".git/info/exclude"), "ignored\n").await?;

        app.git.snapshot(&folder, false, cancel.clone()).await?;
        let (_, checkout) = app.git.checkout_entry(&folder).await?;
        let base = GitService::diff_entry(&checkout, &json!({"mode":"base"})).await?;
        let all = GitService::diff_entry(&checkout, &json!({"mode":"all"})).await?;
        let uncommitted = GitService::diff_entry(&checkout, &json!({"mode":"uncommitted"})).await?;
        let base_change = base.changes.subscribe();
        let mut all_change = all.changes.subscribe();
        let mut uncommitted_change = uncommitted.changes.subscribe();
        let forge_generation = checkout.forge_generation.load(Ordering::Acquire);
        let paths = |path| CheckoutChange {
            paths: ChangedPaths::Paths([PathBuf::from(path)].into()),
            ..CheckoutChange::default()
        };

        tokio::fs::write(folder.join("ignored"), "ignored\n").await?;
        assert!(
            app.git
                .refresh_observed(&folder, paths("ignored"), cancel.clone())
                .await?
                .is_none()
        );
        assert!(!base_change.has_changed()?);
        assert!(!all_change.has_changed()?);
        assert!(!uncommitted_change.has_changed()?);

        tokio::fs::write(folder.join("tracked"), "dirty\n").await?;
        let changed = app
            .git
            .refresh_observed(&folder, paths("tracked"), cancel.clone())
            .await?
            .expect("dirty state should publish");
        assert_eq!(changed["git"]["isDirty"], true);
        assert!(!base_change.has_changed()?);
        assert!(all_change.has_changed()?);
        assert!(uncommitted_change.has_changed()?);
        all_change.borrow_and_update();
        uncommitted_change.borrow_and_update();
        assert_eq!(
            checkout.forge_generation.load(Ordering::Acquire),
            forge_generation
        );

        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]).await?;
        app.git
            .refresh_observed(
                &folder,
                CheckoutChange {
                    refs: true,
                    ..CheckoutChange::default()
                },
                cancel.clone(),
            )
            .await?;
        assert!(base_change.has_changed()?);
        assert!(all_change.has_changed()?);
        assert!(!uncommitted_change.has_changed()?);
        assert_eq!(
            checkout.forge_generation.load(Ordering::Acquire),
            forge_generation + 1
        );
        assert_eq!(
            app.git.cached_snapshot(&folder).await.unwrap()["git"]["diffStat"],
            Value::Null
        );
        assert_eq!(
            app.git.directory_snapshot(&folder).await["git"]["diffStat"],
            json!({"additions":1,"deletions":1}),
        );

        app.git.retain_legacy_shortstat(&folder).await;
        tokio::fs::write(folder.join("tracked"), "dirty\nanother line\n").await?;
        let changed = app
            .git
            .refresh_observed(&folder, paths("tracked"), cancel)
            .await?
            .expect("legacy observers also publish line-total-only changes");
        assert_eq!(
            changed["git"]["diffStat"],
            json!({"additions":2,"deletions":1})
        );
        app.git.release_legacy_shortstat(&folder).await;
        Ok(())
    }

    #[tokio::test]
    async fn checkout_observe_primes_owner_from_prewarmed_cache() -> Result<()> {
        let root = tempfile::tempdir()?;
        let checkout = root.path().join("checkout");
        tokio::fs::create_dir(&checkout).await?;
        let app = crate::state::AppState::new(
            root.path().join("home"),
            checkout.clone(),
            std::env::current_exe()?,
        );
        app.config.initialize().await?;
        let cancel = CancellationToken::new();
        command::text(
            &checkout,
            &["init", "-b", "main"],
            &app.git.environment()?,
            cancel.clone(),
        )
        .await?;
        let cached = app.git.snapshot(&checkout, false, cancel.clone()).await?;
        assert_eq!(cached["git"]["currentBranch"], "main");

        let (source, mut frames) = RequestContext::attach(SourceConfig {
            id: "git-observer".into(),
            session_id: "git-session".into(),
            client_id: "git-client".into(),
            capabilities: json!({"owned_subscriptions":true,"checkout_observation":true}),
            principal: json!({"kind":"owner"}),
            permissions: Arc::new(RwLock::new(HashSet::from(["workspace.read".to_owned()]))),
            routed: false,
            app_version: None,
            cancel: cancel.clone(),
        });
        // Resolve the alias for Git reads, but echo exactly what the GUI keyed
        // this subscription on in both the response and initial update.
        let message = json!({"type":"checkout.observe.request","requestId":"observe","cwd":"~"});
        let request = source.for_request(message.clone());
        assert!(app.git.handle_message(&request, &message).await?);

        let (response, update) = tokio::time::timeout(Duration::from_secs(5), async {
            let response = frames.recv().await.unwrap();
            let update = frames.recv().await.unwrap();
            (response, update)
        })
        .await?;
        let parse = |frame: &OutboundFrame| -> Result<Value> {
            let OutboundFrame::Text(text) = frame else {
                bail!("Expected text frame");
            };
            Ok(serde_json::from_str::<Value>(text)?["message"].clone())
        };
        let response = parse(&response.frame)?;
        let update = parse(&update.frame)?;
        assert_eq!(response["type"], "checkout.observe.response");
        assert_eq!(response["payload"]["requestId"], "observe");
        assert!(response["payload"]["subscriptionId"].is_string());
        assert_eq!(update["type"], "checkout_status_update");
        assert_eq!(
            update["payload"]["subscriptionId"],
            response["payload"]["subscriptionId"]
        );
        assert_eq!(update["payload"]["cwd"], "~");
        assert_eq!(
            update["payload"]["currentBranch"],
            cached["git"]["currentBranch"]
        );
        assert_eq!(update["payload"]["prStatus"]["cwd"], "~");
        assert!(frames.try_recv().is_err());
        cancel.cancel();
        app.git.shutdown().await?;
        Ok(())
    }
}
