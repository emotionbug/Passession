//! Binds the action policy to one checkout (original git_actions.dart
//! `CheckoutGitActions`): runtime state, labels, icons and handlers. Shared by
//! the workspace header split button and the compact Changes actions menu.

use super::host;
use super::json::{flag, int, object, opt_int, text};
use super::model::format_base_ref_label;
use super::policy::{
    ActionRuntime, ActionStatus, ActionsContext, GitAction, GitActions, actions_input_from, build_git_actions,
    forge_presentation, normalize_forge, resolve_risk_reasons, worktree_archive_risk_reasons, action_label_keys,
};
use super::overlay::{MenuEntry, MenuItem};
use super::store::{ActionFollowUp, GitStore, ReviewPlan, pr_status};
use super::text::{msg, t, tr, tr_forge};
use crate::common::{UiContext, UiEffect};
use makepad_widgets::Cx;
use serde_json::{Map, Value, json};

/// Git actions menu values of the last failure's controls.
const FAILURE_RETRY: &str = "failure-retry";
const FAILURE_COPY: &str = "failure-copy";
const FAILURE_DISMISS: &str = "failure-dismiss";
/// Menu value suffix of an action run without its review (Commit, Create PR).
const DIRECT_SUFFIX: &str = ":direct";

/// One checkout's identity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Checkout {
    pub host: String,
    pub workspace_id: String,
    pub cwd: String,
}

/// A confirmation the caller must show before the action runs.
#[derive(Clone, Debug)]
pub struct ArchiveConfirm {
    pub title: String,
    pub message: String,
    pub confirm: String,
    pub workspace_id: String,
}

impl Checkout {
    fn status<'a>(&self, store: &'a GitStore) -> Option<&'a Value> {
        store.checkout(&self.host, &self.cwd).and_then(|s| s.status.as_ref())
    }

    fn pr_payload<'a>(&self, store: &'a GitStore) -> Option<&'a Value> {
        store.checkout(&self.host, &self.cwd).and_then(|s| s.pr_payload.as_ref())
    }

    fn workspace<'a>(&self, ui: &'a UiContext, store: &GitStore) -> Option<&'a Value> {
        let directory = self
            .status(store)
            .map(|s| text(s, "cwd").to_owned())
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| self.cwd.clone());
        host::workspace(ui, &self.host, &self.workspace_id, &directory)
    }

    /// Original `resolveWorkspaceArchiveRisk`: (isDirty, aheadOfOrigin, dirtyKnown, aheadKnown).
    fn archive_risk(&self, ui: &UiContext, store: &GitStore) -> (Option<bool>, Option<i64>, bool, bool) {
        let null = Value::Null;
        let git = self
            .status(store)
            .filter(|s| flag(s, "isGit"))
            .unwrap_or(&null);
        let runtime = self
            .workspace(ui, store)
            .map(|w| object(w, "gitRuntime"))
            .unwrap_or(&null);
        let dirty_known = git.get("isDirty").is_some_and(Value::is_boolean)
            || runtime.get("isDirty").is_some_and(Value::is_boolean)
            || runtime.get("isDirty").is_some();
        let ahead_known = git.get("aheadOfOrigin").is_some() || runtime.get("aheadOfOrigin").is_some();
        let is_dirty = git
            .get("isDirty")
            .and_then(Value::as_bool)
            .or_else(|| runtime.get("isDirty").and_then(Value::as_bool));
        let ahead = opt_int(git, "aheadOfOrigin").or_else(|| opt_int(runtime, "aheadOfOrigin"));
        (is_dirty, ahead, dirty_known, ahead_known)
    }

    fn can_archive(&self, ui: &UiContext, store: &GitStore) -> bool {
        let Some(workspace) = self.workspace(ui, store) else {
            return false;
        };
        if text(workspace, "workspaceKind") != "worktree" {
            return true;
        }
        let (_, _, dirty_known, ahead_known) = self.archive_risk(ui, store);
        dirty_known && ahead_known
    }

    fn is_archiving(&self, ui: &UiContext, store: &GitStore) -> bool {
        self.workspace(ui, store)
            .is_some_and(|w| w.get("archivingAt").is_some_and(|a| !a.is_null()))
            || store.archiving(&self.host, &self.cwd)
    }

    fn repo_root(&self, store: &GitStore) -> String {
        self.status(store).map(|s| text(s, "repoRoot").to_owned()).unwrap_or_default()
    }

    /// `preferences.changesShipDefault[repoRoot]`, `merge` or `pr`.
    pub fn ship_default(&self, ui: &UiContext, store: &GitStore) -> &'static str {
        let root = self.repo_root(store);
        if root.is_empty() {
            return "pr";
        }
        match host::preference(ui, "changesShipDefault").get(&root).and_then(Value::as_str) {
            Some("merge") => "merge",
            _ => "pr",
        }
    }

    fn persist_ship_default(&self, ui: &mut UiContext, store: &GitStore, next: &str) {
        let root = self.repo_root(store);
        if root.is_empty() {
            return;
        }
        let mut map = match host::preference(ui, "changesShipDefault") {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        };
        map.insert(root, json!(next));
        ui.push(UiEffect::SetPreference {
            key: "changesShipDefault".into(),
            value: Value::Object(map),
        });
    }

    fn auto_merge_actions_enabled(&self, ui: &UiContext) -> bool {
        host::supports(ui, &self.host, "checkoutForgeSetAutoMerge")
            || host::supports(ui, &self.host, "checkoutGithubSetAutoMerge")
    }

    fn forge(&self, store: &GitStore) -> String {
        normalize_forge(self.pr_payload(store).map(|p| text(p, "forge")))
    }

    fn merge_request(&self, store: &GitStore) -> bool {
        forge_presentation(Some(&self.forge(store))).is_merge_request()
    }

    fn has_pull_request(&self, store: &GitStore) -> bool {
        !text(pr_status(store.checkout(&self.host, &self.cwd)), "url").is_empty()
    }

    /// Original `build`.
    pub fn build(&self, ui: &UiContext, store: &GitStore) -> GitActions {
        let state = store.checkout(&self.host, &self.cwd);
        let base = t("workspace.git.diff.base");
        let context = ActionsContext {
            status_loading: state.is_some_and(|s| s.status_loading),
            post_ship_archive_suggested: store.post_ship_archive_suggested(&self.host, &self.cwd),
            auto_merge_actions_enabled: self.auto_merge_actions_enabled(ui),
            ship_default: self.ship_default(ui, store),
            base_fallback_label: &base,
        };
        let can_archive = self.can_archive(ui, store);
        let archiving = self.is_archiving(ui, store);
        let has_pull_request = self.has_pull_request(store);
        let mut runtime = |id: &'static str, actions_disabled: bool| {
            if id == "archive-workspace" {
                return ActionRuntime {
                    disabled: !can_archive || archiving,
                    status: if archiving { ActionStatus::Pending } else { ActionStatus::Idle },
                };
            }
            let store_id = if id == "pr" { "create-pr" } else { id };
            let status = store.action_status(&self.host, &self.cwd, store_id);
            ActionRuntime {
                disabled: actions_disabled || status == ActionStatus::Pending,
                status: if id == "pr" && has_pull_request { ActionStatus::Idle } else { status },
            }
        };
        let input = actions_input_from(
            state.and_then(|s| s.status.as_ref()),
            state.and_then(|s| s.pr_payload.as_ref()),
            &context,
            &mut runtime,
        );
        build_git_actions(&input)
    }

    /// Original `label`: the copy for the action's current status.
    pub fn label(&self, store: &GitStore, action: &GitAction) -> String {
        let base = format_base_ref_label(
            self.status(store).map(|s| text(s, "baseRef")).unwrap_or(""),
            &t("workspace.git.diff.base"),
        );
        let [idle, pending, success] = action_label_keys(action.id, self.has_pull_request(store), &base);
        let chosen = match action.status {
            ActionStatus::Idle => idle,
            ActionStatus::Pending => pending,
            ActionStatus::Success => success,
        };
        tr_forge(&chosen.key, self.merge_request(store), &chosen.args)
    }

    /// Original `icon`.
    pub fn icon(action: &GitAction) -> &'static str {
        match action.id {
            "commit" => "git-commit-horizontal",
            "pull" => "download",
            "push" => "upload",
            "pull-and-push" => "arrow-down-up",
            "merge-branch" => "git-merge",
            "merge-from-base" => "refresh-ccw",
            "archive-workspace" => "archive",
            _ => "git-pull-request",
        }
    }

    fn forge_label(&self, store: &GitStore, key: &str) -> String {
        tr_forge(key, self.merge_request(store), &[])
    }

    /// Original `resolveAutoMergeActionsRpc`.
    fn auto_merge_rpc(&self, ui: &UiContext) -> Option<&'static str> {
        if host::supports(ui, &self.host, "checkoutForgeSetAutoMerge") {
            Some("checkout.forge.set_auto_merge.request")
        } else if host::supports(ui, &self.host, "checkoutGithubSetAutoMerge") {
            Some("checkout.github.set_auto_merge.request")
        } else {
            None
        }
    }

    /// Runs a git actions menu answer: the last failure's Retry / Copy /
    /// Dismiss, an action without its review (`…:direct`), or an action.
    pub fn run_menu(&self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, actions: &GitActions, id: &str) -> Option<ArchiveConfirm> {
        match id {
            FAILURE_RETRY => store.retry_failure(cx, &self.host, &self.cwd),
            FAILURE_COPY => {
                if let Some(failure) = store.last_failure(&self.host, &self.cwd) {
                    let text = failure.message.clone();
                    ui.push(UiEffect::CopyText { text, toast: Some(t("workspace.git.actions.failure.copied")) });
                }
            }
            FAILURE_DISMISS => store.dismiss_failure(cx, &self.host, &self.cwd),
            _ => {
                let (action_id, review) = match id.strip_suffix(DIRECT_SUFFIX) {
                    Some(action_id) => (action_id, false),
                    None => (id, true),
                };
                let action = actions.primary.iter().chain(actions.secondary.iter()).find(|a| a.id == action_id)?.clone();
                return self.run_with(cx, ui, store, &action, review);
            }
        }
        None
    }

    /// Menu entries besides the actions: the last failure (title with the
    /// host's message, Retry, Copy, Dismiss) first, and Commit / Create PR
    /// without their review.
    pub fn menu_extras(&self, ui: &UiContext, store: &GitStore, actions: &GitActions) -> (Vec<MenuEntry>, Vec<MenuEntry>) {
        let mut failure_entries = Vec::new();
        if let Some(failure) = store.last_failure(&self.host, &self.cwd) {
            let pending = store.action_status(&self.host, &self.cwd, &failure.action_id) == ActionStatus::Pending;
            failure_entries.push(MenuEntry::Item(
                MenuItem::new("failure-message", format!("{}: {}", failure.title, failure.message)).icon("circle-alert").enabled(false),
            ));
            failure_entries.push(MenuEntry::Item(MenuItem::new(FAILURE_RETRY, t("workspace.git.actions.failure.retry")).icon("rotate-cw").enabled(failure.retryable && !pending && host::connected(ui, &self.host))));
            failure_entries.push(MenuEntry::Item(MenuItem::new(FAILURE_COPY, t("workspace.git.actions.failure.copy")).icon("copy")));
            failure_entries.push(MenuEntry::Item(MenuItem::new(FAILURE_DISMISS, t("workspace.git.actions.failure.dismiss")).icon("x")));
        }
        let mut direct = Vec::new();
        let all = || actions.primary.iter().chain(actions.secondary.iter());
        let ready = |id: &str| all().any(|a| a.id == id && !a.disabled && a.unavailable.is_none() && a.status == ActionStatus::Idle);
        if ready("commit") {
            direct.push(MenuEntry::Item(
                MenuItem::new(format!("commit{DIRECT_SUFFIX}"), t("workspace.git.actions.review.commitDirect")).icon("git-commit-horizontal"),
            ));
        }
        if ready("pr") && !self.has_pull_request(store) {
            direct.push(MenuEntry::Item(MenuItem::new(format!("pr{DIRECT_SUFFIX}"), self.forge_label(store, "workspace.git.actions.review.prDirect")).icon("git-pull-request")));
        }
        (failure_entries, direct)
    }

    /// Original `useGitActionRunner` + `_handle`. Commit and Create PR open
    /// their review first. A worktree archive returns the risk confirmation
    /// to show first (none when the worktree is safe).
    pub fn run(&self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, action: &GitAction) -> Option<ArchiveConfirm> {
        self.run_with(cx, ui, store, action, true)
    }

    fn run_with(&self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, action: &GitAction, review: bool) -> Option<ArchiveConfirm> {
        if action.disabled {
            return None;
        }
        if let Some(unavailable) = &action.unavailable {
            ui.toast(msg(unavailable), false);
            return None;
        }
        let base_ref = self.status(store).map(|s| text(s, "baseRef").to_owned()).unwrap_or_default();
        let (host, cwd) = (self.host.clone(), self.cwd.clone());
        let success = |store: &GitStore, key: &str| Some(self.forge_label(store, key));
        let plan = |success_label| ReviewPlan { host: host.clone(), cwd: cwd.clone(), success_label };
        match action.id {
            "commit" if review => {
                let label = success(store, "workspace.git.actions.commit.success");
                store.review_commit(cx, plan(label));
            }
            "commit" => {
                let label = success(store, "workspace.git.actions.commit.success");
                store.run_action(cx, &host, &cwd, "commit", vec![("checkout_commit_request", json!({ "addAll": true }))], "workspace.git.actions.toasts.failedCommit", label, ActionFollowUp::None);
            }
            "pull" => {
                let label = success(store, "workspace.git.actions.pull.success");
                store.run_action(cx, &host, &cwd, "pull", vec![("checkout_pull_request", json!({}))], "workspace.git.actions.toasts.failedPull", label, ActionFollowUp::None);
            }
            "push" => {
                let label = success(store, "workspace.git.actions.push.success");
                store.run_action(cx, &host, &cwd, "push", vec![("checkout_push_request", json!({}))], "workspace.git.actions.toasts.failedPush", label, ActionFollowUp::None);
            }
            "pull-and-push" => {
                let label = success(store, "workspace.git.actions.pullAndPush.success");
                store.run_action(
                    cx,
                    &host,
                    &cwd,
                    "pull-and-push",
                    vec![("checkout_pull_request", json!({})), ("checkout_push_request", json!({}))],
                    "workspace.git.actions.toasts.failedPullAndPush",
                    label,
                    ActionFollowUp::None,
                );
            }
            "pr" => {
                let url = text(pr_status(store.checkout(&host, &cwd)), "url").to_owned();
                if !url.is_empty() {
                    ui.push(UiEffect::OpenExternal { url });
                    return None;
                }
                self.persist_ship_default(ui, store, "pr");
                let label = success(store, "workspace.git.actions.createPr.success");
                if review {
                    let branch = self.status(store).map(|s| text(s, "currentBranch").to_owned()).unwrap_or_default();
                    store.review_pr(cx, ui, plan(label), base_ref, branch);
                } else {
                    store.run_action(cx, &host, &cwd, "create-pr", vec![("checkout_pr_create_request", json!({}))], "workspace.git.actions.toasts.failedCreatePr", label, ActionFollowUp::None);
                }
            }
            "disable-pr-auto-merge" => {
                let Some(rpc) = self.auto_merge_rpc(ui) else {
                    ui.toast(t("workspace.git.actions.toasts.autoMergeUnsupported"), true);
                    return None;
                };
                let label = success(store, "workspace.git.actions.autoMerge.disabled");
                store.run_action(cx, &host, &cwd, "disable-pr-auto-merge", vec![(rpc, json!({ "enabled": false }))], "workspace.git.actions.toasts.failedDisableAutoMerge", label, ActionFollowUp::None);
            }
            "merge-branch" => {
                if base_ref.is_empty() {
                    ui.toast(t("workspace.git.actions.toasts.baseRefUnavailable"), true);
                    return None;
                }
                self.persist_ship_default(ui, store, "merge");
                let label = success(store, "workspace.git.actions.mergeBranch.success");
                store.run_action(
                    cx,
                    &host,
                    &cwd,
                    "merge-branch",
                    vec![("checkout_merge_request", json!({ "baseRef": base_ref, "strategy": "merge", "requireCleanTarget": true }))],
                    "workspace.git.actions.toasts.failedMerge",
                    label,
                    ActionFollowUp::SuggestPostShipArchive,
                );
            }
            "merge-from-base" => {
                if base_ref.is_empty() {
                    ui.toast(t("workspace.git.actions.toasts.baseRefUnavailable"), true);
                    return None;
                }
                let label = success(store, "workspace.git.actions.mergeFromBase.success");
                store.run_action(
                    cx,
                    &host,
                    &cwd,
                    "merge-from-base",
                    vec![("checkout_merge_from_base_request", json!({ "baseRef": base_ref, "requireCleanTarget": true }))],
                    "workspace.git.actions.toasts.failedMergeFromBase",
                    label,
                    ActionFollowUp::None,
                );
            }
            "archive-workspace" => return self.archive(cx, ui, store),
            id if id.starts_with("merge-pr-") => {
                let method = &id["merge-pr-".len()..];
                self.persist_ship_default(ui, store, "pr");
                let label = success(store, "workspace.git.actions.mergePr.success");
                store.run_action(
                    cx,
                    &host,
                    &cwd,
                    action.id,
                    vec![("checkout_pr_merge_request", json!({ "mergeMethod": method }))],
                    "workspace.git.actions.toasts.failedMergePr",
                    label,
                    ActionFollowUp::SuggestPostShipArchive,
                );
            }
            id if id.starts_with("enable-pr-auto-merge-") => {
                let method = &id["enable-pr-auto-merge-".len()..];
                self.persist_ship_default(ui, store, "pr");
                let Some(rpc) = self.auto_merge_rpc(ui) else {
                    ui.toast(t("workspace.git.actions.toasts.autoMergeUnsupported"), true);
                    return None;
                };
                let label = success(store, "workspace.git.actions.autoMerge.enabled");
                store.run_action(
                    cx,
                    &host,
                    &cwd,
                    action.id,
                    vec![(rpc, json!({ "enabled": true, "mergeMethod": method }))],
                    "workspace.git.actions.toasts.failedEnableAutoMerge",
                    label,
                    ActionFollowUp::None,
                );
            }
            _ => {}
        }
        None
    }

    /// Original `useWorkspaceArchive`: worktrees confirm their risks first.
    fn archive(&self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore) -> Option<ArchiveConfirm> {
        let workspace = self.workspace(ui, store)?.clone();
        let workspace_id = text(&workspace, "id").to_owned();
        if text(&workspace, "workspaceKind") == "worktree" {
            let (is_dirty, ahead, _, _) = self.archive_risk(ui, store);
            let diff_stat = workspace.get("diffStat").filter(|s| s.is_object());
            let reasons = worktree_archive_risk_reasons(is_dirty, ahead, diff_stat);
            let lines = resolve_risk_reasons(&reasons, &msg);
            if !lines.is_empty() {
                let branch = self.status(store).map(|s| text(s, "currentBranch").to_owned()).unwrap_or_default();
                let name = match text(&workspace, "name") {
                    "" if !branch.is_empty() => branch,
                    "" => workspace_id.clone(),
                    name => name.to_owned(),
                };
                return Some(ArchiveConfirm {
                    title: tr("workspace.git.actions.archiveWarning.title", &[("workspaceName", name)]),
                    message: lines.join("\n"),
                    confirm: t("workspace.git.actions.archiveWarning.confirm"),
                    workspace_id,
                });
            }
        }
        store.archive_workspace(cx, ui, &self.host, &self.cwd, &workspace_id);
        None
    }

    /// The confirmed archive.
    pub fn archive_confirmed(&self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, workspace_id: &str) {
        store.archive_workspace(cx, ui, &self.host, &self.cwd, workspace_id);
    }
}

/// Original `gitActionMenuEntries`: separators before group starters (never
/// first); unavailable actions muted; a pending or succeeded row disabled.
pub fn menu_entries(checkout: &Checkout, store: &GitStore, actions: &[GitAction]) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        if action.starts_group && index > 0 {
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Item(
            MenuItem::new(action.id, checkout.label(store, action))
                .icon(Checkout::icon(action))
                .enabled(!action.disabled && action.status == ActionStatus::Idle)
                .muted(action.unavailable.is_some()),
        ));
    }
    entries
}

/// `aheadBehind.ahead` for the empty-state action.
pub fn ahead_count(status: &Value) -> i64 {
    int(object(status, "aheadBehind"), "ahead")
}
