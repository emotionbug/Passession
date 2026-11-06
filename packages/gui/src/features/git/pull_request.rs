//! Pull request pane data (original git/pull-request-panel data.ts,
//! timeline.ts, activity-state.ts, checks-summary.ts, check-presentation.ts
//! and git/forges/gitlab.view.tsx).

use super::json::{array, flag, int, object, text};
use super::policy::url_path;
use serde_json::{Value, json};

/// Original `mapPrPaneData` number: the status number, else `/pull/<n>` of its URL.
pub fn pull_request_number(status: &Value) -> Option<i64> {
    match status {
        Value::Object(map) if map.is_empty() => return None,
        Value::Object(_) => {}
        _ => return None,
    }
    if let Some(number) = status.get("number").and_then(Value::as_f64) {
        return Some(number as i64);
    }
    let path = url_path(text(status, "url"))?;
    let segments: Vec<&str> = path.split('/').collect();
    segments.windows(2).find_map(|window| {
        (window[0] == "pull" && !window[1].is_empty() && window[1].bytes().all(|b| b.is_ascii_digit()))
            .then(|| window[1].parse().ok())
            .flatten()
    })
}

/// Inputs of the original `shouldFetchTimelineFrom`.
pub struct TimelineFetchInputs<'a> {
    pub connected: bool,
    pub timeline_enabled: bool,
    pub github_features_enabled: bool,
    pub cwd: &'a str,
    pub pr_number: Option<i64>,
    pub repo_owner: &'a str,
    pub repo_name: &'a str,
    pub unsupported: bool,
}

impl TimelineFetchInputs<'_> {
    /// Original `shouldFetchTimelineFrom`.
    pub fn should_fetch(&self) -> bool {
        self.connected
            && self.timeline_enabled
            && self.github_features_enabled
            && !self.cwd.is_empty()
            && self.pr_number.is_some()
            && !self.repo_owner.is_empty()
            && !self.repo_name.is_empty()
            && !self.unsupported
    }
}

/// Original timeline query retry: up to three retries after 1s, 2s and 4s,
/// none for a host that does not know the request.
pub fn timeline_retry_delay(failures: u32, unsupported: bool) -> Option<f64> {
    if unsupported || failures >= 3 {
        return None;
    }
    Some((1000u64 << failures).min(30_000) as f64 / 1000.0)
}

/// Original `isUnsupportedTimelineError`: the host rejected the request type
/// (`rpc_error` with `code: unknown_schema` naming this request).
pub fn is_unsupported_timeline_error(code: &str, request_type: &str) -> bool {
    code == "unknown_schema" && request_type == "pull_request_timeline_request"
}

/// Original `mapPrPaneData` activity: only a timeline of this PR counts.
pub fn timeline_items(timeline: Option<&Value>, number: i64) -> Vec<Value> {
    match timeline {
        Some(timeline) if timeline["prNumber"].as_i64() == Some(number) => array(timeline, "items").to_vec(),
        _ => Vec::new(),
    }
}

/// Original `mapChecks`: the status checks, else Gitea's combined commit
/// status as one "CI" check.
pub fn pull_request_checks(status: &Value) -> Vec<Value> {
    let checks = array(status, "checks");
    if !checks.is_empty() {
        return checks.to_vec();
    }
    let facts = object(status, "forgeSpecific");
    let Some(ci_status) = facts.get("ciStatus").and_then(Value::as_str) else {
        return Vec::new();
    };
    if text(facts, "forge") != "gitea" {
        return Vec::new();
    }
    let mut check = json!({
        "name": "CI",
        "status": map_gitea_commit_state(ci_status),
        "url": text(status, "url"),
    });
    if ci_status == "warning" {
        check["traits"] = json!(["warning"]);
    }
    vec![check]
}

/// Original protocol `mapGiteaCommitState`: warning is terminal and failing.
pub fn map_gitea_commit_state(state: &str) -> &'static str {
    match state.to_lowercase().as_str() {
        "success" => "success",
        "failure" | "error" | "warning" => "failure",
        "skipped" => "skipped",
        _ => "pending",
    }
}

/// Original `CheckPresentation` in `PRESENTATION_ORDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Check {
    ActionRequired,
    Warning,
    Failure,
    Pending,
    Manual,
    Success,
    Ignored,
}

impl Check {
    pub const ALL: [Check; 7] = [
        Check::ActionRequired,
        Check::Warning,
        Check::Failure,
        Check::Pending,
        Check::Manual,
        Check::Success,
        Check::Ignored,
    ];

    /// Original `COUNTED_CHECK_PRESENTATIONS` order of the pipeline summary.
    pub const COUNTED: [Check; 6] = [
        Check::Success,
        Check::Failure,
        Check::Warning,
        Check::ActionRequired,
        Check::Manual,
        Check::Pending,
    ];

    /// The catalog name (`workspace.git.pr.checksOverview.count.<name>`).
    pub fn name(self) -> &'static str {
        match self {
            Check::ActionRequired => "actionRequired",
            Check::Warning => "warning",
            Check::Failure => "failure",
            Check::Pending => "pending",
            Check::Manual => "manual",
            Check::Success => "success",
            Check::Ignored => "ignored",
        }
    }
}

/// Original `mapCheckStatus`.
pub fn check_status(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "failure" => "failure",
        "pending" => "pending",
        "skipped" => "skipped",
        "cancelled" => "cancelled",
        _ => "pending",
    }
}

/// Original `classifyCheck`.
pub fn classify(raw_status: &str, traits: &[&str]) -> Check {
    let status = check_status(raw_status);
    if traits.contains(&"action_required") {
        return Check::ActionRequired;
    }
    if traits.contains(&"warning") {
        return Check::Warning;
    }
    if traits.contains(&"manual") {
        return Check::Manual;
    }
    match status {
        "success" => Check::Success,
        "failure" => Check::Failure,
        "skipped" | "cancelled" => Check::Ignored,
        _ => Check::Pending,
    }
}

pub fn classify_check(check: &Value) -> Check {
    let traits: Vec<String> = array(check, "traits")
        .iter()
        .map(|t| match t {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    let traits: Vec<&str> = traits.iter().map(String::as_str).collect();
    classify(text(check, "status"), &traits)
}

/// Original `classifyGitlabPipelineJob`.
pub fn classify_gitlab_job(status: &str, allow_failure: bool) -> Check {
    let mapped = match status {
        "success" | "passed" => "success",
        "failed" => "failure",
        "canceled" | "cancelled" | "skipped" => "skipped",
        _ => "pending",
    };
    if status == "manual" {
        return classify(
            mapped,
            if allow_failure { &["manual"] } else { &["manual", "action_required"] },
        );
    }
    if mapped == "failure" && allow_failure {
        return classify(mapped, &["warning"]);
    }
    classify(mapped, &[])
}

/// The non-empty presentation groups in reading order.
pub fn check_groups(checks: &[Value]) -> Vec<(Check, Vec<Value>)> {
    Check::ALL
        .iter()
        .map(|&presentation| {
            (
                presentation,
                checks
                    .iter()
                    .filter(|c| classify_check(c) == presentation)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, matching)| !matching.is_empty())
        .collect()
}

/// Original `selectOutcome`: a failure outranks anything still running.
pub fn checks_outcome(checks: &[Value], groups: &[(Check, Vec<Value>)]) -> &'static str {
    if checks.is_empty() {
        "none"
    } else if groups.iter().any(|g| g.0 == Check::ActionRequired) {
        "actionRequired"
    } else if groups.iter().any(|g| matches!(g.0, Check::Failure | Check::Warning)) {
        "failure"
    } else if checks.iter().any(|c| check_status(text(c, "status")) == "pending") {
        "pending"
    } else {
        "success"
    }
}

/// Original `formatCheckTiming`: a running check says how long it has run.
pub fn check_timing(check: &Value) -> Option<String> {
    let duration = text(check, "duration");
    if duration.is_empty() {
        return None;
    }
    Some(if check_status(text(check, "status")) == "pending" {
        format!("running {duration}")
    } else {
        duration.to_owned()
    })
}

// ---------------------------------------------------------------------------
// Activity timeline.

/// Original `mapActivity`: blank comments and blank "commented" reviews drop out.
pub fn map_activity(item: &Value) -> Option<Value> {
    let body = text(item, "body");
    if text(item, "kind") == "review" {
        if text(item, "reviewState") == "commented" && body.trim().is_empty() {
            return None;
        }
        return Some(item.clone());
    }
    if body.trim().is_empty() {
        return None;
    }
    let mut mapped = item.clone();
    mapped["kind"] = json!("comment");
    Some(mapped)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub thread_id: String,
    pub location: Value,
    pub is_resolved: Option<bool>,
    pub comments: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Single(Value),
    Thread(Thread),
    Review { review: Value, threads: Vec<Thread> },
}

impl Entry {
    pub fn id(&self) -> String {
        match self {
            Entry::Single(activity) => text(activity, "id").to_owned(),
            Entry::Thread(thread) => format!("thread:{}", thread.thread_id),
            Entry::Review { review, .. } => text(review, "id").to_owned(),
        }
    }

    /// Original activity-state.ts: resolved/outdated threads start collapsed.
    pub fn collapsed_by_default(&self) -> bool {
        match self {
            Entry::Single(activity) => {
                let location = object(activity, "location");
                flag(location, "isResolved") || flag(location, "isOutdated")
            }
            Entry::Thread(thread) => thread_collapsed_by_default(thread),
            Entry::Review { .. } => false,
        }
    }
}

pub fn thread_collapsed_by_default(thread: &Thread) -> bool {
    thread.is_resolved == Some(true) || flag(&thread.location, "isOutdated")
}

pub fn thread_id(thread: &Thread) -> String {
    format!("thread:{}", thread.thread_id)
}

/// Original `buildPrTimeline`: comments sharing a thread id form one thread at
/// the first comment's position; threads whose root carries a review id nest
/// under that review; everything else stays standalone in order.
pub fn build_timeline(activities: &[Value]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut thread_index: Vec<(String, usize)> = Vec::new();
    for activity in activities {
        let location = object(activity, "location");
        let id = match activity.get("threadId").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => text(location, "threadId").to_owned(),
        };
        if id.is_empty() {
            entries.push(Entry::Single(activity.clone()));
            continue;
        }
        if let Some((_, index)) = thread_index.iter().find(|(tid, _)| *tid == id) {
            if let Entry::Thread(thread) = &mut entries[*index] {
                thread.comments.push(activity.clone());
            }
            continue;
        }
        let is_resolved = match (location.get("isResolved"), activity.get("threadIsResolved")) {
            (Some(Value::Bool(resolved)), _) => Some(*resolved),
            (_, Some(Value::Bool(resolved))) => Some(*resolved),
            _ => None,
        };
        thread_index.push((id.clone(), entries.len()));
        entries.push(Entry::Thread(Thread {
            thread_id: id,
            location: location.clone(),
            is_resolved,
            comments: vec![activity.clone()],
        }));
    }
    let review_ids: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Single(activity) if text(activity, "kind") == "review" => Some(text(activity, "id").to_owned()),
            _ => None,
        })
        .collect();
    let mut by_review: Vec<(String, Vec<Thread>)> = Vec::new();
    let mut nested: Vec<String> = Vec::new();
    for entry in &entries {
        let Entry::Thread(thread) = entry else { continue };
        let review_id = text(&thread.comments[0], "reviewId");
        if review_id.is_empty() || !review_ids.iter().any(|id| id == review_id) {
            continue;
        }
        nested.push(thread.thread_id.clone());
        match by_review.iter_mut().find(|(id, _)| id == review_id) {
            Some((_, threads)) => threads.push(thread.clone()),
            None => by_review.push((review_id.to_owned(), vec![thread.clone()])),
        }
    }
    if by_review.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Thread(thread) if nested.contains(&thread.thread_id) => None,
            Entry::Single(activity) => {
                let id = text(&activity, "id").to_owned();
                match by_review.iter().find(|(review, _)| *review == id) {
                    Some((_, threads)) => Some(Entry::Review { review: activity, threads: threads.clone() }),
                    None => Some(Entry::Single(activity)),
                }
            }
            other => Some(other),
        })
        .collect()
}

/// Original `formatPullRequestThreadPath`.
pub fn thread_path(location: &Value) -> String {
    let path = text(location, "path");
    let has_line = location.get("line").is_some_and(|l| !l.is_null());
    let has_start = location.get("startLine").is_some_and(|l| !l.is_null());
    if has_line && has_start {
        return format!("{path}:{}-{}", int(location, "startLine"), int(location, "line"));
    }
    if has_line {
        return format!("{path}:{}", int(location, "line"));
    }
    path.to_owned()
}

/// Review summary counts: (approvals, changes requested, comments).
pub fn activity_counts(activities: &[Value]) -> (usize, usize, usize) {
    let review = |state: &str| {
        activities
            .iter()
            .filter(|a| text(a, "kind") == "review" && text(a, "reviewState") == state)
            .count()
    };
    let comments = activities
        .iter()
        .filter(|a| {
            text(a, "kind") == "comment" || (text(a, "kind") == "review" && text(a, "reviewState") == "commented")
        })
        .count();
    (review("approved"), review("changes_requested"), comments)
}

/// Original `ActivityAvatar` identity fill index (case-insensitive author hash).
pub fn avatar_fill_index(author: &str) -> usize {
    let mut hash: u32 = 0;
    for unit in author.to_lowercase().encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(unit as u32);
    }
    (hash % 10) as usize
}

/// Original `IDENTITY_COLORS` fills; order is load-bearing.
pub const AVATAR_FILLS: [u32; 10] = [
    0x7a6aa8, 0x3d7ea6, 0x388068, 0xa4673a, 0xb05c80, 0x6a70b8, 0x368080, 0xb06260, 0x8f7838, 0x5179b0,
];

/// Original `derivePrState`: (catalog state key, icon state).
pub fn pr_state(pr: &Value) -> &'static str {
    let state = text(pr, "state").to_lowercase();
    if flag(pr, "isMerged") || state == "merged" {
        "merged"
    } else if state != "open" {
        "closed"
    } else if flag(pr, "isDraft") {
        "draft"
    } else {
        "open"
    }
}

/// Original repo identity of the header: GitLab project path, else owner/name.
pub fn repo_identity(pr: &Value) -> String {
    let project = text(pr, "projectPath");
    if !project.is_empty() {
        return project.to_owned();
    }
    let (owner, name) = (text(pr, "repoOwner"), text(pr, "repoName"));
    if !owner.is_empty() && !name.is_empty() {
        format!("{owner}/{name}")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_comes_from_status_or_pull_url() {
        assert_eq!(pull_request_number(&json!({"number": 5})), Some(5));
        assert_eq!(pull_request_number(&json!({"url": "https://github.com/a/b/pull/12/files"})), Some(12));
        assert_eq!(pull_request_number(&json!({})), None);
    }

    #[test]
    fn threads_nest_under_their_review_and_keep_order() {
        let activities = vec![
            json!({"id": "r1", "kind": "review", "reviewState": "commented", "body": "see"}),
            json!({"id": "c1", "kind": "comment", "body": "a", "threadId": "t1", "reviewId": "r1"}),
            json!({"id": "c2", "kind": "comment", "body": "b", "threadId": "t1"}),
            json!({"id": "c3", "kind": "comment", "body": "c", "location": {"threadId": "t2", "isResolved": true}}),
        ];
        let entries = build_timeline(&activities);
        assert_eq!(entries.len(), 2);
        match &entries[0] {
            Entry::Review { threads, .. } => assert_eq!(threads[0].comments.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        assert!(entries[1].collapsed_by_default());
    }

    #[test]
    fn gitlab_jobs_classify_manual_and_allowed_failures() {
        assert_eq!(classify_gitlab_job("manual", false), Check::ActionRequired);
        assert_eq!(classify_gitlab_job("manual", true), Check::Manual);
        assert_eq!(classify_gitlab_job("failed", true), Check::Warning);
        assert_eq!(classify_gitlab_job("canceled", false), Check::Ignored);
    }

    #[test]
    fn gitea_ci_status_becomes_one_check() {
        let checks = pull_request_checks(&json!({"url": "u", "forgeSpecific": {"forge": "gitea", "ciStatus": "warning"}}));
        assert_eq!(checks.len(), 1);
        assert_eq!(classify_check(&checks[0]), Check::Warning);
        let groups = check_groups(&checks);
        assert_eq!(checks_outcome(&checks, &groups), "failure");
    }

    #[test]
    fn retries_back_off_then_stop() {
        assert_eq!(timeline_retry_delay(0, false), Some(1.0));
        assert_eq!(timeline_retry_delay(2, false), Some(4.0));
        assert_eq!(timeline_retry_delay(3, false), None);
        assert_eq!(timeline_retry_delay(0, true), None);
    }
}
