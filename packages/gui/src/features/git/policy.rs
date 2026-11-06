//! Forge presentation, merge capability and the git action policy (original
//! git/forge.ts, git/merge-capability.ts, git/forges/*.ts, git/policy.ts,
//! git/pr-hint.ts and git/worktree-archive-warning.ts).

use super::json::{flag, flag_or, int, object, opt_int, text};
use super::model::format_base_ref_label;
use super::text::Msg;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Forge presentation.

/// Original `normalizeForge`: an absent forge is GitHub (old daemons never send one).
pub fn normalize_forge(raw: Option<&str>) -> String {
    match raw {
        None | Some("") => "github".into(),
        Some(forge) => forge.into(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForgePresentation {
    pub forge: String,
    pub brand_label: String,
    pub change_request_abbrev: &'static str,
    pub number_prefix: &'static str,
    pub sign_in_cli: Option<&'static str>,
    pub sign_in_command: Option<&'static str>,
    pub hostname_flag: Option<&'static str>,
}

impl ForgePresentation {
    /// Original `changeRequestContext`: MR forges use the `_mr` copy variants.
    pub fn is_merge_request(&self) -> bool {
        self.change_request_abbrev == "MR"
    }
}

/// Original `getForgePresentation`: an unknown forge renders neutrally.
pub fn forge_presentation(raw: Option<&str>) -> ForgePresentation {
    let forge = normalize_forge(raw);
    match forge.as_str() {
        "github" => ForgePresentation {
            forge,
            brand_label: "GitHub".into(),
            change_request_abbrev: "PR",
            number_prefix: "#",
            sign_in_cli: Some("gh"),
            sign_in_command: Some("gh auth login"),
            hostname_flag: None,
        },
        "gitlab" => ForgePresentation {
            forge,
            brand_label: "GitLab".into(),
            change_request_abbrev: "MR",
            number_prefix: "!",
            sign_in_cli: Some("glab"),
            sign_in_command: Some("glab auth login"),
            hostname_flag: Some("--hostname"),
        },
        "gitea" | "forgejo" | "codeberg" => ForgePresentation {
            brand_label: match forge.as_str() {
                "gitea" => "Gitea",
                "forgejo" => "Forgejo",
                _ => "Codeberg",
            }
            .into(),
            forge,
            change_request_abbrev: "PR",
            number_prefix: "#",
            sign_in_cli: Some("tea"),
            sign_in_command: Some("tea login add"),
            hostname_flag: None,
        },
        _ => ForgePresentation {
            brand_label: forge.clone(),
            forge,
            change_request_abbrev: "PR",
            number_prefix: "#",
            sign_in_cli: None,
            sign_in_command: None,
            hostname_flag: None,
        },
    }
}

/// Original `buildForgeSignInCommand`.
pub fn forge_sign_in_command(forge: Option<&str>, host: Option<&str>) -> Option<String> {
    let presentation = forge_presentation(forge);
    let command = presentation.sign_in_command?;
    match (presentation.hostname_flag, host) {
        (Some(flag), Some(host)) if !host.is_empty() => Some(format!("{command} {flag} {host}")),
        _ => Some(command.to_owned()),
    }
}

/// Original `parseGitRemoteLocation(url).host` for the sign-in hostname.
pub fn parse_forge_host(remote_url: Option<&str>) -> Option<String> {
    let url = remote_url?.trim();
    if url.is_empty() {
        return None;
    }
    if !url.contains("://") {
        // `^(?:[^@/]+@)?([^:/]+):(?!//)`: scp-like `user@host:path` or `host:path`.
        let without_user = match url.find('@') {
            Some(at) if at > 0 && !url[..at].contains('/') => &url[at + 1..],
            _ => url,
        };
        let end = without_user.find([':', '/']).unwrap_or(without_user.len());
        let host = &without_user[..end];
        if !host.is_empty()
            && without_user[end..].starts_with(':')
            && !without_user[end + 1..].starts_with("//")
        {
            return Some(host.to_owned());
        }
    }
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = if host_port.starts_with('[') {
        host_port.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_owned()
    };
    if host.is_empty() { None } else { Some(host.to_lowercase()) }
}

/// Original `computeForgeSetupAction` + `buildForgeSetupMessage`.
pub fn forge_setup_message(
    forge: Option<&str>,
    auth_state: Option<&str>,
    forge_providers_supported: bool,
    remote_url: Option<&str>,
) -> Option<Msg> {
    let normalized = normalize_forge(forge);
    if normalized != "github" && !forge_providers_supported {
        return None;
    }
    let install = match auth_state {
        Some("cli_missing") => true,
        Some("unauthenticated") => false,
        _ => return None,
    };
    let presentation = forge_presentation(Some(&normalized));
    let Some(cli) = presentation.sign_in_cli else {
        return Some(Msg::new("workspace.git.forgeSetup.generic").arg("brand", &presentation.brand_label));
    };
    if install {
        return Some(
            Msg::new("workspace.git.forgeSetup.installCli")
                .arg("cli", cli)
                .arg("brand", &presentation.brand_label),
        );
    }
    let host = parse_forge_host(remote_url);
    Some(
        Msg::new("workspace.git.forgeSetup.signIn")
            .arg(
                "command",
                forge_sign_in_command(Some(&normalized), host.as_deref()).unwrap_or_default(),
            )
            .arg("brand", &presentation.brand_label),
    )
}

/// Original `normalizeCheckoutPrStatusPayload`: the legacy GitHub boolean maps
/// to an auth state for daemons that predate `authState`.
pub fn normalize_pr_status_payload(payload: &Value) -> Value {
    const KNOWN: [&str; 5] = ["authenticated", "unauthenticated", "cli_missing", "no_remote", "error"];
    let mut normalized = match payload {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let auth = payload.get("authState").and_then(Value::as_str);
    let state = match auth {
        Some(state) if KNOWN.contains(&state) => state.to_owned(),
        _ if flag(payload, "githubFeaturesEnabled") => "authenticated".into(),
        _ => "unauthenticated".into(),
    };
    normalized.insert("authState".into(), json!(state));
    Value::Object(normalized)
}

/// Original `PrHint`: the change request a checkout points at.
#[derive(Clone, Debug, PartialEq)]
pub struct PrHint {
    pub url: String,
    pub number: i64,
    /// `open`, `merged` or `closed`.
    pub state: &'static str,
    pub forge: String,
}

/// Original `selectPrHintFromStatus`: a hint only for a URL naming a change
/// request; merged wins over the raw state.
pub fn select_pr_hint(payload: Option<&Value>) -> Option<PrHint> {
    let payload = payload?;
    let status = object(payload, "status");
    let url = text(status, "url");
    if url.is_empty() {
        return None;
    }
    let path = url_path(url)?;
    let number = change_request_number(path)?;
    let state = if flag(status, "isMerged") || text(status, "state") == "merged" {
        "merged"
    } else if text(status, "state") == "open" {
        "open"
    } else {
        "closed"
    };
    let forge = match payload.get("forge").and_then(Value::as_str) {
        Some(forge) => forge.to_owned(),
        None => text(status, "forge").to_owned(),
    };
    Some(PrHint {
        url: url.to_owned(),
        number,
        state,
        forge: normalize_forge(Some(&forge)),
    })
}

/// The path of an absolute URL (`Uri.tryParse(url).path`).
pub fn url_path(url: &str) -> Option<&str> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => return Some(url.split(['?', '#']).next().unwrap_or("")),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    Some(rest.find('/').map(|slash| &rest[slash..]).unwrap_or(""))
}

/// `/(?:pull|pulls|merge_requests)/(\d+)(?:/|$)` on the URL path.
fn change_request_number(path: &str) -> Option<i64> {
    let segments: Vec<&str> = path.split('/').collect();
    for window in segments.windows(2) {
        if matches!(window[0], "pull" | "pulls" | "merge_requests")
            && !window[1].is_empty()
            && window[1].bytes().all(|b| b.is_ascii_digit())
            && let Ok(number) = window[1].parse()
        {
            return Some(number);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Merge capability.

#[derive(Clone, Debug, PartialEq)]
pub struct MergeCapability {
    pub direct_merge_ready: bool,
    pub can_enable_auto_merge: bool,
    pub auto_merge_enabled: bool,
    pub can_disable_auto_merge: bool,
    pub merge_blocked_by_queue: bool,
    pub allowed_methods: Vec<&'static str>,
    pub preferred_method: Option<&'static str>,
}

/// Original GitLab `GITLAB_ACTIVE_PIPELINE_STATUSES`.
pub const GITLAB_ACTIVE_PIPELINE_STATUSES: [&str; 7] = [
    "created",
    "waiting_for_resource",
    "preparing",
    "pending",
    "running",
    "canceling",
    "scheduled",
];

struct SchemaMismatch;

fn opt_str<'a>(facts: &'a Value, key: &str) -> Result<Option<&'a str>, SchemaMismatch> {
    match facts.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(SchemaMismatch),
    }
}

fn opt_bool(facts: &Value, key: &str) -> Result<Option<bool>, SchemaMismatch> {
    match facts.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(SchemaMismatch),
    }
}

fn opt_num(facts: &Value, key: &str) -> Result<(), SchemaMismatch> {
    match facts.get(key) {
        None | Some(Value::Null) | Some(Value::Number(_)) => Ok(()),
        Some(_) => Err(SchemaMismatch),
    }
}

/// Original `deriveMergeCapability`: the forge facts arm, the legacy
/// `status.github` facts when `forgeSpecific` is absent, and `None` for an
/// unknown or schema-mismatched family.
pub fn derive_merge_capability(forge_specific: Option<&Value>, legacy_github: Option<&Value>) -> Option<MergeCapability> {
    match forge_specific {
        None | Some(Value::Null) => match legacy_github {
            Some(Value::Object(map)) => {
                let mut facts = map.clone();
                facts.insert("forge".into(), json!("github"));
                // `{'forge': 'github', ...legacy}`: a legacy `forge` key wins.
                if let Some(forge) = map.get("forge") {
                    facts.insert("forge".into(), forge.clone());
                }
                forge_capability(&Value::Object(facts)).ok().flatten()
            }
            _ => None,
        },
        Some(facts @ Value::Object(_)) => forge_capability(facts).ok().flatten(),
        Some(_) => None,
    }
}

fn forge_capability(facts: &Value) -> Result<Option<MergeCapability>, SchemaMismatch> {
    match facts.get("forge").and_then(Value::as_str) {
        Some("github") => {
            let merge_state = opt_str(facts, "mergeStateStatus")?;
            let auto_merge_request = facts.get("autoMergeRequest").filter(|v| !v.is_null());
            if auto_merge_request.is_some_and(|v| !v.is_object()) {
                return Err(SchemaMismatch);
            }
            let repository = match facts.get("repository") {
                None | Some(Value::Null) => Value::Null,
                Some(value @ Value::Object(_)) => value.clone(),
                Some(_) => return Err(SchemaMismatch),
            };
            let repo = |key: &str| opt_bool(&repository, key).map(|v| v.unwrap_or(false));
            let mut methods = Vec::new();
            if repo("mergeCommitAllowed")? {
                methods.push("merge");
            }
            if repo("squashMergeAllowed")? {
                methods.push("squash");
            }
            if repo("rebaseMergeAllowed")? {
                methods.push("rebase");
            }
            let preferred = match opt_str(&repository, "viewerDefaultMergeMethod")? {
                Some("SQUASH") => Some("squash"),
                Some("MERGE") => Some("merge"),
                Some("REBASE") => Some("rebase"),
                _ => None,
            };
            let auto_merge_allowed = repo("autoMergeAllowed")?;
            let viewer_can_enable = opt_bool(facts, "viewerCanEnableAutoMerge")?.unwrap_or(false);
            let viewer_can_disable = opt_bool(facts, "viewerCanDisableAutoMerge")?.unwrap_or(false);
            let queue = opt_bool(facts, "isMergeQueueEnabled")?.unwrap_or(false)
                || opt_bool(facts, "isInMergeQueue")?.unwrap_or(false);
            Ok(Some(MergeCapability {
                direct_merge_ready: matches!(merge_state, Some("CLEAN" | "HAS_HOOKS")),
                can_enable_auto_merge: merge_state == Some("BLOCKED") && auto_merge_allowed && viewer_can_enable,
                auto_merge_enabled: auto_merge_request.is_some(),
                can_disable_auto_merge: viewer_can_disable,
                merge_blocked_by_queue: queue,
                allowed_methods: methods,
                preferred_method: preferred,
            }))
        }
        Some("gitlab") => {
            let detailed = opt_str(facts, "detailedMergeStatus")?;
            let legacy = opt_str(facts, "mergeStatus")?;
            let has_conflicts = opt_bool(facts, "hasConflicts")?.unwrap_or(false);
            let pipeline_status = opt_str(facts, "pipelineStatus")?;
            opt_num(facts, "pipelineId")?;
            opt_str(facts, "pipelineUrl")?;
            opt_bool(facts, "blockingDiscussionsResolved")?;
            opt_num(facts, "approvalsRequired")?;
            opt_num(facts, "approvalsGiven")?;
            let auto_merge_enabled = opt_bool(facts, "mergeWhenPipelineSucceeds")?.unwrap_or(false);
            let active = pipeline_status.is_some_and(|s| GITLAB_ACTIVE_PIPELINE_STATUSES.contains(&s));
            Ok(Some(MergeCapability {
                direct_merge_ready: match detailed {
                    Some(detailed) => detailed == "mergeable",
                    None => legacy == Some("can_be_merged") && !has_conflicts,
                },
                can_enable_auto_merge: !auto_merge_enabled && active,
                auto_merge_enabled,
                can_disable_auto_merge: auto_merge_enabled,
                merge_blocked_by_queue: false,
                allowed_methods: vec!["merge", "squash", "rebase"],
                preferred_method: None,
            }))
        }
        Some("gitea") => {
            let mergeable = opt_bool(facts, "mergeable")?.unwrap_or(false);
            let has_merged = opt_bool(facts, "hasMerged")?.unwrap_or(false);
            opt_str(facts, "ciStatus")?;
            Ok(Some(MergeCapability {
                direct_merge_ready: mergeable && !has_merged,
                can_enable_auto_merge: false,
                auto_merge_enabled: false,
                can_disable_auto_merge: false,
                merge_blocked_by_queue: false,
                allowed_methods: vec!["merge", "squash", "rebase"],
                preferred_method: None,
            }))
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Action policy.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ActionStatus {
    #[default]
    Idle,
    Pending,
    Success,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ActionRuntime {
    pub disabled: bool,
    pub status: ActionStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GitAction {
    pub id: &'static str,
    pub disabled: bool,
    pub status: ActionStatus,
    pub starts_group: bool,
    pub unavailable: Option<Msg>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitActions {
    pub primary: Option<GitAction>,
    pub secondary: Vec<GitAction>,
}

impl GitActions {
    /// Original `actionsWithoutPrimary`.
    pub fn without_primary(&self) -> Vec<GitAction> {
        let primary = self.primary.as_ref().map(|a| a.id);
        self.secondary
            .iter()
            .filter(|a| Some(a.id) != primary)
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ActionsInput {
    pub is_git: bool,
    pub github_features_enabled: bool,
    pub forge_brand_label: String,
    pub forge_change_request_noun: String,
    pub auto_merge_actions_enabled: bool,
    pub has_pull_request: bool,
    pub pull_request_url: Option<String>,
    /// `open`, `closed` or `None` (original `narrowPullRequestState`).
    pub pull_request_state: Option<&'static str>,
    pub pull_request_is_draft: bool,
    pub pull_request_is_merged: bool,
    pub pull_request_mergeable: String,
    pub merge_capability: Option<MergeCapability>,
    pub has_remote: bool,
    pub is_paseo_owned_worktree: bool,
    pub is_on_base_branch: bool,
    pub has_uncommitted_changes: bool,
    pub base_ref_available: bool,
    pub base_ref_label: String,
    pub ahead_count: i64,
    pub behind_base_count: i64,
    pub ahead_of_origin: Option<i64>,
    pub behind_of_origin: Option<i64>,
    pub should_promote_archive: bool,
    pub ship_default: &'static str,
    pub runtime: HashMap<&'static str, ActionRuntime>,
}

impl Default for ActionsInput {
    fn default() -> Self {
        Self {
            is_git: false,
            github_features_enabled: true,
            forge_brand_label: "GitHub".into(),
            forge_change_request_noun: "PR".into(),
            auto_merge_actions_enabled: false,
            has_pull_request: false,
            pull_request_url: None,
            pull_request_state: None,
            pull_request_is_draft: false,
            pull_request_is_merged: false,
            pull_request_mergeable: "UNKNOWN".into(),
            merge_capability: None,
            has_remote: false,
            is_paseo_owned_worktree: false,
            is_on_base_branch: false,
            has_uncommitted_changes: false,
            base_ref_available: false,
            base_ref_label: "base".into(),
            ahead_count: 0,
            behind_base_count: 0,
            ahead_of_origin: None,
            behind_of_origin: None,
            should_promote_archive: false,
            ship_default: "pr",
            runtime: HashMap::new(),
        }
    }
}

impl ActionsInput {
    fn runtime_of(&self, id: &str) -> ActionRuntime {
        self.runtime.get(id).copied().unwrap_or_default()
    }
}

const DIRECT_MERGE_MODELS: [(&str, &str, bool); 3] = [
    ("merge-pr-squash", "squash", true),
    ("merge-pr-merge", "merge", false),
    ("merge-pr-rebase", "rebase", false),
];
const AUTO_MERGE_MODELS: [(&str, &str, bool); 3] = [
    ("enable-pr-auto-merge-squash", "squash", true),
    ("enable-pr-auto-merge-merge", "merge", false),
    ("enable-pr-auto-merge-rebase", "rebase", false),
];
const PULL_REQUEST_ACTION_IDS: [&str; 8] = [
    "pr",
    "merge-pr-squash",
    "merge-pr-merge",
    "merge-pr-rebase",
    "enable-pr-auto-merge-squash",
    "enable-pr-auto-merge-merge",
    "enable-pr-auto-merge-rebase",
    "disable-pr-auto-merge",
];

/// Every action id, in the original runtime map order.
pub const ALL_ACTION_IDS: [&str; 14] = [
    "commit",
    "pull",
    "push",
    "pull-and-push",
    "pr",
    "merge-pr-squash",
    "merge-pr-merge",
    "merge-pr-rebase",
    "enable-pr-auto-merge-squash",
    "enable-pr-auto-merge-merge",
    "enable-pr-auto-merge-rebase",
    "disable-pr-auto-merge",
    "merge-branch",
    "merge-from-base",
];

/// Original `narrowPullRequestState`.
pub fn narrow_pull_request_state(state: Option<&str>) -> Option<&'static str> {
    match state {
        Some("open") => Some("open"),
        Some("closed") => Some("closed"),
        _ => None,
    }
}

const U: &str = "workspace.git.actions.unavailable";

fn unavailable(key: &str) -> Msg {
    Msg::new(format!("{U}.{key}"))
}

fn forge_args(input: &ActionsInput, key: &str) -> Msg {
    unavailable(key)
        .arg("brand", &input.forge_brand_label)
        .arg("noun", &input.forge_change_request_noun)
}

/// Original `buildGitActions`.
pub fn build_git_actions(input: &ActionsInput) -> GitActions {
    if !input.is_git {
        return GitActions::default();
    }
    let mut all: HashMap<&'static str, GitAction> = HashMap::new();
    let simple = |id: &'static str, starts_group: bool, unavailable: Option<Msg>| {
        let runtime = input.runtime_of(id);
        GitAction {
            id,
            disabled: runtime.disabled,
            status: runtime.status,
            starts_group,
            unavailable: if runtime.disabled { None } else { unavailable },
        }
    };
    all.insert("commit", simple("commit", false, None));
    all.insert("pull", simple("pull", false, pull_unavailable(input)));
    all.insert("push", simple("push", false, push_unavailable(input)));
    all.insert("pull-and-push", simple("pull-and-push", false, pull_and_push_unavailable(input)));
    all.insert("pr", pr_action(input));
    for (id, _, starts_group) in DIRECT_MERGE_MODELS {
        let runtime = input.runtime_of(id);
        all.insert(
            id,
            GitAction {
                id,
                disabled: runtime.disabled || !can_merge_pr(input),
                status: runtime.status,
                starts_group,
                unavailable: if runtime.disabled { None } else { merge_pr_unavailable(input) },
            },
        );
    }
    for (id, _, starts_group) in AUTO_MERGE_MODELS {
        let runtime = input.runtime_of(id);
        all.insert(
            id,
            GitAction {
                id,
                disabled: runtime.disabled,
                status: runtime.status,
                starts_group,
                unavailable: None,
            },
        );
    }
    let disable_runtime = input.runtime_of("disable-pr-auto-merge");
    let can_disable = input
        .merge_capability
        .as_ref()
        .is_some_and(|c| c.can_disable_auto_merge);
    all.insert(
        "disable-pr-auto-merge",
        GitAction {
            id: "disable-pr-auto-merge",
            disabled: disable_runtime.disabled || !can_disable,
            status: disable_runtime.status,
            starts_group: true,
            unavailable: if disable_runtime.disabled || can_disable {
                None
            } else {
                Some(unavailable("autoMergeCannotDisable"))
            },
        },
    );
    all.insert("merge-branch", simple("merge-branch", false, merge_branch_unavailable(input)));
    all.insert(
        "merge-from-base",
        simple("merge-from-base", true, merge_from_base_unavailable(input)),
    );
    all.insert("archive-workspace", simple("archive-workspace", true, None));

    let primary_id = primary_action_id(input);
    let mut secondary_ids: Vec<&'static str> = vec!["pull", "push", "pull-and-push"];
    if !input.is_on_base_branch {
        secondary_ids.push("merge-from-base");
        secondary_ids.push("merge-branch");
        secondary_ids.extend(
            PULL_REQUEST_ACTION_IDS
                .iter()
                .copied()
                .filter(|id| should_show_pull_request_action(input, id)),
        );
    }
    secondary_ids.push("archive-workspace");
    GitActions {
        primary: primary_id.and_then(|id| all.get(id).cloned()),
        secondary: secondary_ids
            .into_iter()
            .filter(|id| *id != "archive-workspace" || primary_id != Some("archive-workspace"))
            .filter_map(|id| all.get(id).cloned())
            .collect(),
    }
}

fn pr_action(input: &ActionsInput) -> GitAction {
    let runtime = input.runtime_of("pr");
    let viewing = input.has_pull_request && input.pull_request_url.is_some();
    let unavailable = if runtime.disabled {
        None
    } else if viewing {
        if input.github_features_enabled {
            None
        } else {
            Some(forge_args(input, "viewPrNoForge"))
        }
    } else {
        create_pr_unavailable(input)
    };
    GitAction {
        id: "pr",
        disabled: runtime.disabled,
        status: runtime.status,
        starts_group: false,
        unavailable,
    }
}

fn primary_action_id(input: &ActionsInput) -> Option<&'static str> {
    if input.should_promote_archive {
        return Some("archive-workspace");
    }
    if input.has_uncommitted_changes {
        return Some("commit");
    }
    if can_pull(input) {
        return Some("pull");
    }
    if can_push(input) {
        return Some("push");
    }
    if can_merge_pr(input) {
        return Some(preferred(input, &DIRECT_MERGE_MODELS).map_or(DIRECT_MERGE_MODELS[0].0, |m| m.0));
    }
    if can_enable_pr_auto_merge(input) {
        return Some(preferred(input, &AUTO_MERGE_MODELS).map_or(AUTO_MERGE_MODELS[0].0, |m| m.0));
    }
    if has_enabled_pr_auto_merge(input) {
        return Some("pr");
    }
    if input.ship_default == "pr" && can_use_pull_request_action_as_ship_default(input) {
        return Some("pr");
    }
    if !input.is_on_base_branch && input.ahead_count > 0 {
        return Some("merge-branch");
    }
    if !input.is_on_base_branch && can_merge_from_base(input) {
        return Some("merge-from-base");
    }
    if input.github_features_enabled && input.has_pull_request && input.pull_request_url.is_some() {
        return Some("pr");
    }
    if input.is_paseo_owned_worktree {
        return Some("archive-workspace");
    }
    None
}

fn method_allowed(input: &ActionsInput, method: &str) -> bool {
    input
        .merge_capability
        .as_ref()
        .is_none_or(|c| c.allowed_methods.contains(&method))
}

fn allowed<'a>(
    input: &ActionsInput,
    models: &'a [(&'static str, &'static str, bool)],
) -> Vec<&'a (&'static str, &'static str, bool)> {
    models.iter().filter(|m| method_allowed(input, m.1)).collect()
}

fn preferred<'a>(
    input: &ActionsInput,
    models: &'a [(&'static str, &'static str, bool)],
) -> Option<&'a (&'static str, &'static str, bool)> {
    let allowed = allowed(input, models);
    let preferred = input.merge_capability.as_ref().and_then(|c| c.preferred_method);
    allowed
        .iter()
        .find(|m| Some(m.1) == preferred)
        .or_else(|| allowed.first())
        .copied()
}

fn should_show_pull_request_action(input: &ActionsInput, id: &str) -> bool {
    if id == "pr" {
        return true;
    }
    if id == "disable-pr-auto-merge" {
        return input.auto_merge_actions_enabled
            && input.merge_capability.as_ref().is_some_and(|c| c.auto_merge_enabled);
    }
    if id.starts_with("merge-pr-") {
        return can_merge_pr(input) && allowed(input, &DIRECT_MERGE_MODELS).iter().any(|m| m.0 == id);
    }
    if id.starts_with("enable-pr-auto-merge-") {
        return can_enable_pr_auto_merge(input) && allowed(input, &AUTO_MERGE_MODELS).iter().any(|m| m.0 == id);
    }
    false
}

fn can_pull(input: &ActionsInput) -> bool {
    input.has_remote && !input.has_uncommitted_changes && input.behind_of_origin.unwrap_or(0) > 0
}

fn can_push(input: &ActionsInput) -> bool {
    input.has_remote && has_pushable_commits(input) && input.behind_of_origin.unwrap_or(0) == 0
}

fn has_pushable_commits(input: &ActionsInput) -> bool {
    if input.ahead_of_origin.unwrap_or(0) > 0 {
        return true;
    }
    // No-upstream Paseo worktrees are first-pushable: the daemon push sets upstream.
    input.is_paseo_owned_worktree && input.ahead_of_origin.is_none() && input.ahead_count > 0
}

fn can_merge_from_base(input: &ActionsInput) -> bool {
    !input.is_on_base_branch
        && input.base_ref_available
        && !input.has_uncommitted_changes
        && input.behind_base_count > 0
}

fn can_use_pull_request_action_as_ship_default(input: &ActionsInput) -> bool {
    if input.is_on_base_branch || !input.github_features_enabled {
        return false;
    }
    if input.has_pull_request {
        return input.pull_request_url.is_some();
    }
    input.ahead_count > 0
}

fn can_merge_pr(input: &ActionsInput) -> bool {
    let from_status = input.github_features_enabled
        && input.has_pull_request
        && input.pull_request_state == Some("open")
        && !input.pull_request_is_draft
        && !input.pull_request_is_merged
        && input.pull_request_mergeable != "CONFLICTING"
        && input.ahead_count > 0
        && !input.has_uncommitted_changes;
    if !from_status {
        return false;
    }
    match &input.merge_capability {
        None => {
            input.pull_request_mergeable == "MERGEABLE"
                && input.behind_of_origin == Some(0)
                && input.ahead_of_origin == Some(0)
                && !can_merge_from_base(input)
        }
        Some(capability) => {
            capability.direct_merge_ready
                && !capability.auto_merge_enabled
                && !capability.merge_blocked_by_queue
                && !allowed(input, &DIRECT_MERGE_MODELS).is_empty()
        }
    }
}

fn can_enable_pr_auto_merge(input: &ActionsInput) -> bool {
    let Some(capability) = &input.merge_capability else {
        return false;
    };
    input.github_features_enabled
        && input.auto_merge_actions_enabled
        && input.has_pull_request
        && input.pull_request_state == Some("open")
        && !input.pull_request_is_draft
        && !input.pull_request_is_merged
        && input.pull_request_mergeable != "CONFLICTING"
        && !capability.auto_merge_enabled
        && capability.can_enable_auto_merge
        && !capability.merge_blocked_by_queue
        && !allowed(input, &AUTO_MERGE_MODELS).is_empty()
}

fn has_enabled_pr_auto_merge(input: &ActionsInput) -> bool {
    input.github_features_enabled
        && input.has_pull_request
        && input.pull_request_url.is_some()
        && input.merge_capability.as_ref().is_some_and(|c| c.auto_merge_enabled)
}

fn pull_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.has_remote {
        return Some(unavailable("pullNoRemote"));
    }
    if input.has_uncommitted_changes {
        return Some(unavailable("pullDirty"));
    }
    match input.behind_of_origin {
        None => Some(unavailable("pullNoRemote")),
        Some(0) => Some(unavailable("pullUpToDate")),
        _ => None,
    }
}

fn push_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.has_remote {
        return Some(unavailable("pushNoRemote"));
    }
    if input.behind_of_origin.unwrap_or(0) > 0 {
        return Some(unavailable("pushBehind"));
    }
    if !has_pushable_commits(input) {
        return Some(unavailable("pushNothing"));
    }
    None
}

fn pull_and_push_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.has_remote {
        return Some(unavailable("pullAndPushNoRemote"));
    }
    if input.has_uncommitted_changes {
        return Some(unavailable("pullAndPushDirty"));
    }
    let Some(behind) = input.behind_of_origin else {
        return Some(unavailable("pullAndPushNoIncoming"));
    };
    if behind == 0 && input.ahead_of_origin == Some(0) {
        return Some(unavailable("pullAndPushInSync"));
    }
    if behind == 0 {
        return Some(unavailable("pullAndPushNoIncoming"));
    }
    if input.ahead_of_origin.unwrap_or(0) == 0 {
        return Some(unavailable("pullAndPushNothingToPush"));
    }
    None
}

fn create_pr_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.github_features_enabled {
        return Some(forge_args(input, "createPrNoForge"));
    }
    if input.ahead_count == 0 {
        return Some(unavailable("createPrNoCommits"));
    }
    None
}

fn merge_branch_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.base_ref_available {
        return Some(unavailable("mergeNoBase"));
    }
    if input.has_uncommitted_changes {
        return Some(unavailable("mergeDirty"));
    }
    if input.ahead_count == 0 {
        return Some(unavailable("mergeNothing"));
    }
    None
}

fn merge_from_base_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.base_ref_available {
        return Some(unavailable("updateNoBase"));
    }
    if input.has_uncommitted_changes {
        return Some(unavailable("updateDirty"));
    }
    if input.behind_base_count == 0 {
        return Some(unavailable("updateCurrent").arg("baseRef", &input.base_ref_label));
    }
    None
}

fn merge_pr_unavailable(input: &ActionsInput) -> Option<Msg> {
    if !input.github_features_enabled {
        return Some(forge_args(input, "mergePrNoForge"));
    }
    if !input.has_pull_request {
        return Some(unavailable("mergePrMissing"));
    }
    if input.pull_request_is_draft {
        return Some(unavailable("mergePrDraft"));
    }
    if input.pull_request_is_merged {
        return Some(unavailable("mergePrMerged"));
    }
    if input.pull_request_state == Some("closed") {
        return Some(unavailable("mergePrClosed"));
    }
    if input.pull_request_mergeable == "CONFLICTING" {
        return Some(unavailable("mergePrConflicts"));
    }
    let capability = input.merge_capability.as_ref()?;
    if capability.merge_blocked_by_queue {
        return Some(unavailable("mergePrQueue"));
    }
    if !capability.direct_merge_ready {
        return Some(forge_args(input, "mergePrNotReady"));
    }
    None
}

/// Inputs of the original `deriveGitActionsState` beyond status and PR payload.
pub struct ActionsContext<'a> {
    pub status_loading: bool,
    pub post_ship_archive_suggested: bool,
    pub auto_merge_actions_enabled: bool,
    pub ship_default: &'static str,
    pub base_fallback_label: &'a str,
}

/// Original `deriveGitActionsState` + the `useGitActions` input mapping.
pub fn actions_input_from(
    status: Option<&Value>,
    pr_payload: Option<&Value>,
    context: &ActionsContext<'_>,
    runtime: &mut dyn FnMut(&'static str, bool) -> ActionRuntime,
) -> ActionsInput {
    let git_status = status.filter(|s| flag(s, "isGit"));
    let is_git = git_status.is_some();
    let null = Value::Null;
    let git = git_status.unwrap_or(&null);
    let pr = pr_payload.map(|p| object(p, "status")).unwrap_or(&null);
    let pr_url = text(pr, "url");
    let forge = forge_presentation(pr_payload.map(|p| text(p, "forge")));
    let base_ref = text(git, "baseRef");
    let base_ref_label = format_base_ref_label(base_ref, context.base_fallback_label);
    let ahead_behind = object(git, "aheadBehind");
    let has_uncommitted_changes = flag(git, "isDirty");
    let actions_disabled = !is_git
        || status.is_some_and(|s| s.get("error").is_some_and(|e| !e.is_null()))
        || context.status_loading;
    let mut runtimes = HashMap::new();
    for id in ALL_ACTION_IDS.iter().copied().chain(["archive-workspace"]) {
        runtimes.insert(id, runtime(id, actions_disabled));
    }
    ActionsInput {
        is_git,
        github_features_enabled: pr_payload.is_none_or(|p| flag_or(p, "githubFeaturesEnabled", true)),
        forge_brand_label: forge.brand_label.clone(),
        forge_change_request_noun: forge.change_request_abbrev.to_owned(),
        auto_merge_actions_enabled: context.auto_merge_actions_enabled,
        has_pull_request: !pr_url.is_empty(),
        pull_request_url: if pr_url.is_empty() { None } else { Some(pr_url.to_owned()) },
        pull_request_state: narrow_pull_request_state(pr.get("state").and_then(Value::as_str)),
        pull_request_is_draft: flag(pr, "isDraft"),
        pull_request_is_merged: flag(pr, "isMerged"),
        pull_request_mergeable: pr
            .get("mergeable")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_owned(),
        merge_capability: derive_merge_capability(pr.get("forgeSpecific"), pr.get("github")),
        has_remote: flag(git, "hasRemote"),
        is_paseo_owned_worktree: flag(git, "isPaseoOwnedWorktree"),
        is_on_base_branch: is_git && text(git, "currentBranch") == base_ref_label,
        has_uncommitted_changes,
        base_ref_available: !base_ref.is_empty(),
        base_ref_label,
        ahead_count: int(ahead_behind, "ahead"),
        behind_base_count: int(ahead_behind, "behind"),
        ahead_of_origin: opt_int(git, "aheadOfOrigin"),
        behind_of_origin: opt_int(git, "behindOfOrigin"),
        should_promote_archive: !has_uncommitted_changes
            && (context.post_ship_archive_suggested || flag(pr, "isMerged")),
        ship_default: context.ship_default,
        runtime: runtimes,
    }
}

/// The catalog keys of an action's label for idle, pending and success
/// (original `getActionDisplayLabel`); returns `(key, args)` triples.
pub fn action_label_keys(id: &str, has_pull_request: bool, base_ref_label: &str) -> [Msg; 3] {
    let group = |name: &str| {
        [
            Msg::new(format!("workspace.git.actions.{name}.label")),
            Msg::new(format!("workspace.git.actions.{name}.pending")),
            Msg::new(format!("workspace.git.actions.{name}.success")),
        ]
    };
    match id {
        "commit" | "pull" | "push" => group(id),
        "pull-and-push" => group("pullAndPush"),
        "merge-branch" => group("mergeBranch"),
        "pr" if has_pull_request => [
            Msg::new("workspace.git.actions.viewPr"),
            Msg::new("workspace.git.actions.viewPr"),
            Msg::new("workspace.git.actions.viewPr"),
        ],
        "pr" => group("createPr"),
        "disable-pr-auto-merge" => [
            Msg::new("workspace.git.actions.autoMerge.enabled"),
            Msg::new("workspace.git.actions.autoMerge.disabling"),
            Msg::new("workspace.git.actions.autoMerge.disabled"),
        ],
        "merge-from-base" => [
            Msg::new("workspace.git.actions.mergeFromBase.label").arg("baseRef", base_ref_label),
            Msg::new("workspace.git.actions.mergeFromBase.pending"),
            Msg::new("workspace.git.actions.mergeFromBase.success"),
        ],
        "archive-workspace" => group("archive"),
        id if id.starts_with("merge-pr-") => [
            Msg::new(format!("workspace.git.actions.mergePr.{}", &id["merge-pr-".len()..])),
            Msg::new("workspace.git.actions.mergePr.pending"),
            Msg::new("workspace.git.actions.mergePr.success"),
        ],
        id => {
            let method = id.strip_prefix("enable-pr-auto-merge-").unwrap_or(id);
            let mut chars = method.chars();
            let capitalized = match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            };
            [
                Msg::new(format!("workspace.git.actions.autoMerge.enable{capitalized}")),
                Msg::new("workspace.git.actions.autoMerge.enabling"),
                Msg::new("workspace.git.actions.autoMerge.enabled"),
            ]
        }
    }
}

// ---------------------------------------------------------------------------
// Worktree archive warning.

/// Original `buildWorktreeArchiveRiskReasons`.
pub fn worktree_archive_risk_reasons(
    is_dirty: Option<bool>,
    ahead_of_origin: Option<i64>,
    diff_stat: Option<&Value>,
) -> Vec<Msg> {
    let mut reasons = Vec::new();
    let additions = diff_stat.map(|s| int(s, "additions")).unwrap_or(0);
    let deletions = diff_stat.map(|s| int(s, "deletions")).unwrap_or(0);
    let has_diff_stat_changes = diff_stat.is_some() && (additions > 0 || deletions > 0);
    let uncommitted = is_dirty == Some(true) || (is_dirty.is_none() && has_diff_stat_changes);
    const W: &str = "workspace.git.actions.archiveWarning";
    if uncommitted {
        let mut parts = Vec::new();
        if additions > 0 {
            parts.push(
                Msg::new(if additions == 1 { format!("{W}.addedLine") } else { format!("{W}.addedLines") })
                    .arg("count", additions),
            );
        }
        if deletions > 0 {
            parts.push(
                Msg::new(if deletions == 1 { format!("{W}.deletedLine") } else { format!("{W}.deletedLines") })
                    .arg("count", deletions),
            );
        }
        if parts.is_empty() {
            reasons.push(Msg::new(format!("{W}.uncommittedChanges")));
        } else {
            // The diff stat parts are resolved by the reader and joined with ", ".
            let mut message = Msg::new(format!("{W}.uncommittedChangesWithDiff"));
            message.args.push(("diffStat", String::new()));
            reasons.push(message);
            reasons.extend(parts.into_iter().map(|mut part| {
                part.args.push(("__part", String::new()));
                part
            }));
        }
    }
    let ahead = ahead_of_origin.unwrap_or(0);
    if ahead > 0 {
        reasons.push(
            Msg::new(if ahead == 1 { format!("{W}.unpushedCommit") } else { format!("{W}.unpushedCommits") })
                .arg("count", ahead),
        );
    }
    reasons
}

/// Resolves [`worktree_archive_risk_reasons`] into display lines: diff stat
/// parts (marked `__part`) fold into the preceding `diffStat` argument.
pub fn resolve_risk_reasons(reasons: &[Msg], resolve: &dyn Fn(&Msg) -> String) -> Vec<String> {
    let mut lines: Vec<(Msg, Vec<String>)> = Vec::new();
    for reason in reasons {
        if reason.args.iter().any(|(name, _)| *name == "__part") {
            let mut part = reason.clone();
            part.args.retain(|(name, _)| *name != "__part");
            if let Some((_, parts)) = lines.last_mut() {
                parts.push(resolve(&part));
            }
            continue;
        }
        lines.push((reason.clone(), Vec::new()));
    }
    lines
        .into_iter()
        .map(|(mut reason, parts)| {
            for arg in &mut reason.args {
                if arg.0 == "diffStat" {
                    arg.1 = parts.join(", ");
                }
            }
            resolve(&reason)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ActionsInput {
        ActionsInput {
            is_git: true,
            ..Default::default()
        }
    }

    #[test]
    fn dirty_checkout_promotes_commit_and_clean_behind_promotes_pull() {
        let dirty = ActionsInput { has_uncommitted_changes: true, ..input() };
        assert_eq!(build_git_actions(&dirty).primary.unwrap().id, "commit");
        let behind = ActionsInput { has_remote: true, behind_of_origin: Some(2), ahead_of_origin: Some(0), ..input() };
        assert_eq!(build_git_actions(&behind).primary.unwrap().id, "pull");
    }

    #[test]
    fn no_upstream_paseo_worktree_with_commits_is_first_pushable() {
        let worktree = ActionsInput {
            has_remote: true,
            is_paseo_owned_worktree: true,
            ahead_count: 2,
            ship_default: "merge",
            ..input()
        };
        assert_eq!(build_git_actions(&worktree).primary.unwrap().id, "push");
    }

    #[test]
    fn unavailable_pull_explains_itself_instead_of_being_hidden() {
        let actions = build_git_actions(&ActionsInput { has_remote: false, ..input() });
        let pull = actions.secondary.iter().find(|a| a.id == "pull").unwrap();
        assert_eq!(pull.unavailable.as_ref().unwrap().key, "workspace.git.actions.unavailable.pullNoRemote");
    }

    #[test]
    fn merge_pr_uses_forge_preferred_method_within_allowed_set() {
        let capability = derive_merge_capability(
            Some(&json!({
                "forge": "github",
                "mergeStateStatus": "CLEAN",
                "repository": {"squashMergeAllowed": false, "mergeCommitAllowed": true, "rebaseMergeAllowed": true, "viewerDefaultMergeMethod": "REBASE"}
            })),
            None,
        );
        let ready = ActionsInput {
            has_pull_request: true,
            pull_request_url: Some("https://github.com/a/b/pull/1".into()),
            pull_request_state: Some("open"),
            ahead_count: 1,
            merge_capability: capability,
            ..input()
        };
        let actions = build_git_actions(&ready);
        assert_eq!(actions.primary.unwrap().id, "merge-pr-rebase");
        assert!(!actions.secondary.iter().any(|a| a.id == "merge-pr-squash"));
    }

    #[test]
    fn merged_pr_without_local_changes_promotes_archive() {
        let mut runtime = |_: &'static str, disabled: bool| ActionRuntime { disabled, status: ActionStatus::Idle };
        let built = actions_input_from(
            Some(&json!({"isGit": true, "currentBranch": "feat", "baseRef": "main"})),
            Some(&json!({"status": {"url": "https://github.com/a/b/pull/1", "isMerged": true}})),
            &ActionsContext {
                status_loading: false,
                post_ship_archive_suggested: false,
                auto_merge_actions_enabled: false,
                ship_default: "pr",
                base_fallback_label: "base",
            },
            &mut runtime,
        );
        assert_eq!(build_git_actions(&built).primary.unwrap().id, "archive-workspace");
    }

    #[test]
    fn non_git_checkout_has_no_actions() {
        assert_eq!(build_git_actions(&ActionsInput::default()), GitActions::default());
    }

    #[test]
    fn gitlab_capability_and_schema_mismatch() {
        let gitlab = derive_merge_capability(
            Some(&json!({"forge": "gitlab", "detailedMergeStatus": "mergeable", "pipelineStatus": "running"})),
            None,
        )
        .unwrap();
        assert!(gitlab.direct_merge_ready && gitlab.can_enable_auto_merge);
        assert_eq!(derive_merge_capability(Some(&json!({"forge": "github", "mergeStateStatus": 3})), None), None);
        assert_eq!(derive_merge_capability(Some(&json!({"forge": "other"})), None), None);
    }

    #[test]
    fn pr_hint_parses_forge_urls_and_prefers_merged() {
        let hint = select_pr_hint(Some(&json!({"forge": "gitlab", "status": {"url": "https://gitlab.com/a/b/-/merge_requests/7", "state": "open", "isMerged": true}}))).unwrap();
        assert_eq!((hint.number, hint.state, hint.forge.as_str()), (7, "merged", "gitlab"));
        assert!(select_pr_hint(Some(&json!({"status": {"url": "https://github.com/a/b/issues/7"}}))).is_none());
        assert_eq!(select_pr_hint(Some(&json!({"status": {"url": "https://codeberg.org/a/b/pulls/3/files", "state": "closed"}}))).unwrap().number, 3);
    }

    #[test]
    fn worktree_reasons_use_diff_stat_only_when_dirtiness_is_unknown() {
        let stat = json!({"additions": 3, "deletions": 1});
        assert!(worktree_archive_risk_reasons(Some(false), Some(0), Some(&stat)).is_empty());
        let reasons = worktree_archive_risk_reasons(None, Some(2), Some(&stat));
        let lines = resolve_risk_reasons(&reasons, &|m: &Msg| {
            let args: Vec<String> = m.args.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{}({})", m.key, args.join(","))
        });
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("uncommittedChangesWithDiff(diffStat="));
        assert!(lines[0].contains("addedLines(count=3)"));
        assert!(lines[1].contains("unpushedCommits(count=2)"));
    }

    #[test]
    fn legacy_auth_state_maps_from_features_flag() {
        assert_eq!(normalize_pr_status_payload(&json!({"githubFeaturesEnabled": true}))["authState"], "authenticated");
        assert_eq!(normalize_pr_status_payload(&json!({"authState": "cli_missing"}))["authState"], "cli_missing");
        assert_eq!(normalize_pr_status_payload(&json!({}))["authState"], "unauthenticated");
    }

    #[test]
    fn sign_in_host_from_remote_urls() {
        assert_eq!(parse_forge_host(Some("git@gitlab.example.com:a/b.git")).as_deref(), Some("gitlab.example.com"));
        assert_eq!(parse_forge_host(Some("https://user@Gitlab.Example.com:8443/a/b")).as_deref(), Some("gitlab.example.com"));
        assert_eq!(forge_sign_in_command(Some("gitlab"), Some("h")).as_deref(), Some("glab auth login --hostname h"));
    }
}
