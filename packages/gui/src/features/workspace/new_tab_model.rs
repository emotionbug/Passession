//! Pure parts of the new-tab / add-project form (`new_tab_panel.dart`): branch picker
//! items (`new-workspace-picker-item.ts`), the workspace source of a creation request and
//! the creation intent shape.

use serde_json::{json, Map, Value};

use super::rpc::str_of;

/// Original `new-workspace-picker-item.ts` branch `PickerItem`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchItem {
    /// `pickerItemLabel`: the branch name, `name (local)` when both refs show.
    pub name: String,
    /// Sent as the worktree `refName` (`refs/remotes/origin/x`, `refs/heads/x`).
    pub ref_name: String,
    /// The plain branch name (checkout source).
    pub branch_name: String,
    pub semantic_label: String,
    /// `+ahead −behind` of the local branch vs origin.
    pub divergence: Option<String>,
}

/// `branchNameFromRef`.
pub fn branch_name_from_ref(ref_name: &str) -> String {
    if let Some(rest) = ref_name.strip_prefix("refs/heads/") {
        return rest.to_owned();
    }
    if let Some(rest) = ref_name.strip_prefix("refs/remotes/") {
        return match rest.find('/') {
            Some(index) => rest[index + 1..].to_owned(),
            None => rest.to_owned(),
        };
    }
    ref_name.to_owned()
}

fn divergence_label(ahead: i64, behind: i64) -> Option<String> {
    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(format!("+{ahead}"));
    }
    if behind > 0 {
        parts.push(format!("−{behind}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn commits(count: i64) -> String {
    format!("{count} {}", if count == 1 { "commit" } else { "commits" })
}

fn divergence_accessibility(ahead: i64, behind: i64, counterpart: &str) -> String {
    match (ahead, behind) {
        (0, 0) => format!("up to date with {counterpart}"),
        (a, b) if a > 0 && b > 0 => format!("{} ahead and {b} behind {counterpart}", commits(a)),
        (a, _) if a > 0 => format!("{} ahead of {counterpart}", commits(a)),
        (_, b) => format!("{} behind {counterpart}", commits(b)),
    }
}

/// `buildBranchPickerItems`: an origin row for remote branches and a local row when there
/// is no remote or the two refs differ.
pub fn build_branch_items(details: &[Value]) -> Vec<BranchItem> {
    let mut items = Vec::new();
    for detail in details {
        let name = str_of(detail, "name");
        if name.is_empty() {
            continue;
        }
        let known = detail.get("hasLocal").is_some_and(|v| !v.is_null()) || detail.get("hasRemote").is_some_and(|v| !v.is_null());
        if !known {
            items.push(BranchItem {
                name: name.into(),
                ref_name: name.into(),
                branch_name: name.into(),
                semantic_label: format!("{name}, branch"),
                divergence: None,
            });
            continue;
        }
        let has_local = detail.get("hasLocal") == Some(&Value::Bool(true));
        let has_remote = detail.get("hasRemote") == Some(&Value::Bool(true));
        let ahead = detail.get("localAhead").and_then(Value::as_f64).map(|v| v as i64);
        let behind = detail.get("localBehind").and_then(Value::as_f64).map(|v| v as i64);
        let divergence = match (has_local && has_remote, ahead, behind) {
            (true, Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        let refs_differ = divergence.is_some_and(|(a, b)| a > 0 || b > 0);
        let shows_both = has_local && has_remote && (divergence.is_none() || refs_differ);
        if has_remote {
            items.push(BranchItem {
                name: name.into(),
                ref_name: format!("refs/remotes/origin/{name}"),
                branch_name: name.into(),
                semantic_label: format!("{name}, origin branch"),
                divergence: None,
            });
        }
        if has_local && (shows_both || !has_remote) {
            items.push(BranchItem {
                name: if shows_both { format!("{name} (local)") } else { name.into() },
                ref_name: format!("refs/heads/{name}"),
                branch_name: name.into(),
                semantic_label: match divergence {
                    Some((a, b)) => format!("{name}, local branch, {}", divergence_accessibility(a, b, &format!("origin {name}"))),
                    None => format!("{name}, local branch"),
                },
                divergence: divergence.and_then(|(a, b)| divergence_label(a, b)),
            });
        }
    }
    items
}

/// `branch_suggestions_response`: `branchDetails`, else plain `branches` names.
pub fn branch_details(response: &Value) -> Vec<Value> {
    if let Some(Value::Array(details)) = response.get("branchDetails") {
        return details.clone();
    }
    response
        .get("branches")
        .and_then(Value::as_array)
        .map(|branches| {
            branches
                .iter()
                .map(|branch| match branch {
                    Value::Object(_) => json!({ "name": str_of(branch, "name") }),
                    Value::String(name) => json!({ "name": name }),
                    other => json!({ "name": other.to_string() }),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The default base item from `checkout_status_response` (upstream, else local branch).
pub fn default_ref(status: &Value) -> Option<BranchItem> {
    let current = str_of(status, "currentBranch");
    if current.is_empty() {
        return None;
    }
    let upstream = str_of(status, "upstreamRef");
    let ref_name = if upstream.is_empty() { format!("refs/heads/{current}") } else { upstream.to_owned() };
    let name = branch_name_from_ref(&ref_name);
    Some(BranchItem {
        semantic_label: format!("{name}, {} branch", if upstream.is_empty() { "local" } else { "upstream" }),
        name,
        ref_name,
        branch_name: current.to_owned(),
        divergence: None,
    })
}

/// Workspace sources of the form.
pub const SOURCES: [&str; 6] = ["directory", "branch-off", "checkout", "pr", "clone", "new-directory"];

pub struct SourceFields<'a> {
    pub source: &'a str,
    pub cwd: &'a str,
    pub ref_name: &'a str,
    pub default_ref: Option<&'a str>,
    pub branch: &'a str,
    pub pr: &'a str,
    pub forge: &'a str,
    pub project_path: &'a str,
}

/// The `source` of a `workspace.create` request.
pub fn workspace_source(fields: &SourceFields<'_>) -> Value {
    if fields.source == "directory" {
        return json!({ "kind": "directory", "path": fields.cwd });
    }
    let mut source = Map::new();
    source.insert("kind".into(), "worktree".into());
    source.insert("cwd".into(), fields.cwd.into());
    source.insert("action".into(), if fields.source == "branch-off" { "branch-off" } else { "checkout" }.into());
    let picked = fields.ref_name.trim();
    if matches!(fields.source, "branch-off" | "checkout") && !picked.is_empty() {
        source.insert("refName".into(), picked.into());
    } else if fields.source == "branch-off" {
        // Original: with no pick the default base item's ref is sent.
        if let Some(default) = fields.default_ref {
            source.insert("refName".into(), default.into());
        }
    }
    if fields.source == "branch-off" && !fields.branch.trim().is_empty() {
        source.insert("branchName".into(), fields.branch.trim().into());
    }
    if fields.source == "pr" {
        let mut checkout = Map::new();
        checkout.insert("kind".into(), "change_request".into());
        checkout.insert("number".into(), fields.pr.trim().parse::<i64>().unwrap_or(0).into());
        if !fields.forge.trim().is_empty() {
            checkout.insert("forge".into(), fields.forge.trim().into());
        }
        if !fields.project_path.trim().is_empty() {
            checkout.insert("projectPath".into(), fields.project_path.trim().into());
        }
        source.insert("checkoutSource".into(), Value::Object(checkout));
    }
    Value::Object(source)
}

/// `int.tryParse(pr) >= 1`.
pub fn valid_pr_number(text: &str) -> bool {
    text.trim().parse::<i64>().is_ok_and(|n| n >= 1)
}

/// Feature value of `feature` in `config` (`featureValues[id]`, else the offered value).
pub fn feature_value<'a>(config: &'a Value, feature: &'a Value) -> &'a Value {
    let id = str_of(feature, "id");
    config
        .get("featureValues")
        .and_then(|values| values.get(id))
        .or_else(|| feature.get("value"))
        .unwrap_or(&Value::Null)
}

/// `withFeature`: the config with one feature value set.
pub fn with_feature(config: &Value, id: &str, value: Value) -> Value {
    let mut next = config.as_object().cloned().unwrap_or_default();
    let mut values = next.get("featureValues").and_then(Value::as_object).cloned().unwrap_or_default();
    values.insert(id.to_owned(), value);
    next.insert("featureValues".into(), Value::Object(values));
    Value::Object(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_items_follow_provenance() {
        let items = build_branch_items(&[
            json!({"name": "main", "hasLocal": true, "hasRemote": true, "localAhead": 0, "localBehind": 0}),
            json!({"name": "dev", "hasLocal": true, "hasRemote": true, "localAhead": 2, "localBehind": 1}),
            json!({"name": "solo", "hasLocal": true, "hasRemote": false}),
            json!({"name": "plain"}),
        ]);
        let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["main", "dev", "dev (local)", "solo", "plain"]);
        assert_eq!(items[2].divergence.as_deref(), Some("+2 −1"));
        assert_eq!(items[0].ref_name, "refs/remotes/origin/main");
        assert_eq!(items[3].ref_name, "refs/heads/solo");
        assert!(items[2].semantic_label.contains("2 commits ahead and 1 behind origin dev"));
    }

    #[test]
    fn branch_name_from_refs() {
        assert_eq!(branch_name_from_ref("refs/remotes/origin/feat/x"), "feat/x");
        assert_eq!(branch_name_from_ref("refs/heads/main"), "main");
    }

    #[test]
    fn sources_match_request_shape() {
        let fields = SourceFields { source: "branch-off", cwd: "/r", ref_name: "", default_ref: Some("refs/heads/main"), branch: " b ", pr: "", forge: "", project_path: "" };
        assert_eq!(
            workspace_source(&fields),
            json!({"kind": "worktree", "cwd": "/r", "action": "branch-off", "refName": "refs/heads/main", "branchName": "b"})
        );
        let pr = SourceFields { source: "pr", cwd: "/r", ref_name: "", default_ref: None, branch: "", pr: "12", forge: "gitlab", project_path: "" };
        assert_eq!(workspace_source(&pr)["checkoutSource"], json!({"kind": "change_request", "number": 12, "forge": "gitlab"}));
        assert!(!valid_pr_number("0"));
        assert!(valid_pr_number(" 3 "));
    }
}
