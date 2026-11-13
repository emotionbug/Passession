//! Git Log data for the `git_log` tab: the checkout's refs, its paged history
//! with the lane graph, and per-commit details, over `checkout.refs.request`,
//! `checkout.log.request` and `checkout.commit.get.request` (feature `gitLog`).
//!
//! A filter change reloads from the first page; a checkout change (an action's
//! invalidation, a status push that moved HEAD or the upstream counts, a
//! reconnect) reloads refs and the first page in place, keeping the shown rows
//! and the selected commit while it is still listed.

use super::graph::{self, GraphRow};
use super::host;
use super::json::{array, flag, opt_int, text};
use super::model::parse_iso8601_ms;
use super::store::GitStore;
use super::wire;
use crate::common::{PanelTarget, UiContext};
use paseo_gui_core::{CoreEvent, RequestId};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Commits per `checkout.log.request` page.
pub const PAGE_SIZE: usize = 300;
pub const FEATURE: &str = "gitLog";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefKind {
    /// A detached `HEAD` decoration.
    Head,
    Local,
    Remote,
    Tag,
}

impl RefKind {
    fn parse(kind: &str) -> Option<Self> {
        Some(match kind {
            "head" => RefKind::Head,
            "local" => RefKind::Local,
            "remote" => RefKind::Remote,
            "tag" => RefKind::Tag,
            _ => return None,
        })
    }
}

/// A `GitRefLabel` decoration.
#[derive(Clone, Debug, PartialEq)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
    pub is_head: bool,
}

fn ref_labels(value: &Value) -> Vec<RefLabel> {
    array(value, "refs")
        .iter()
        .filter_map(|label| {
            Some(RefLabel {
                name: text(label, "name").to_owned(),
                kind: RefKind::parse(text(label, "kind"))?,
                is_head: flag(label, "isHead"),
            })
        })
        .collect()
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    array(value, key).iter().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// One `LogCommit` row.
#[derive(Clone, Debug, PartialEq)]
pub struct LogCommit {
    pub sha: String,
    pub short_sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_ms: Option<i64>,
    pub subject: String,
    pub refs: Vec<RefLabel>,
}

impl LogCommit {
    fn parse(value: &Value) -> Self {
        Self {
            sha: text(value, "sha").to_owned(),
            short_sha: text(value, "shortSha").to_owned(),
            parents: strings(value, "parents"),
            author_name: text(value, "authorName").to_owned(),
            author_email: text(value, "authorEmail").to_owned(),
            author_ms: parse_iso8601_ms(text(value, "authorDate")),
            subject: text(value, "subject").to_owned(),
            refs: ref_labels(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalBranch {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: Option<i64>,
    pub behind: Option<i64>,
    pub gone: bool,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteBranch {
    /// `origin/main`.
    pub name: String,
    pub remote: String,
    /// `main`.
    pub branch: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Refs {
    pub head_sha: Option<String>,
    pub head_branch: Option<String>,
    pub detached: bool,
    pub local: Vec<LocalBranch>,
    pub remote: Vec<RemoteBranch>,
    pub tags: Vec<String>,
}

impl Refs {
    fn parse(payload: &Value) -> Self {
        let head = &payload["head"];
        let optional = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        Self {
            head_sha: optional(head, "sha"),
            head_branch: optional(head, "branch"),
            detached: flag(head, "detached"),
            local: array(payload, "local")
                .iter()
                .map(|b| LocalBranch {
                    name: text(b, "name").to_owned(),
                    upstream: optional(b, "upstream"),
                    ahead: opt_int(b, "ahead"),
                    behind: opt_int(b, "behind"),
                    gone: flag(b, "gone"),
                    current: flag(b, "isCurrent"),
                })
                .collect(),
            remote: array(payload, "remote")
                .iter()
                .map(|b| RemoteBranch {
                    name: text(b, "name").to_owned(),
                    remote: text(b, "remote").to_owned(),
                    branch: text(b, "branch").to_owned(),
                })
                .collect(),
            tags: array(payload, "tags").iter().map(|t| text(t, "name").to_owned()).collect(),
        }
    }

    /// The kind of the first ref named `name` (local, remote, tag, then `HEAD`).
    pub fn kind_of(&self, name: &str) -> Option<RefKind> {
        if self.local.iter().any(|b| b.name == name) {
            Some(RefKind::Local)
        } else if self.remote.iter().any(|b| b.name == name) {
            Some(RefKind::Remote)
        } else if self.tags.iter().any(|t| t == name) {
            Some(RefKind::Tag)
        } else if name == "HEAD" {
            Some(RefKind::Head)
        } else {
            None
        }
    }
}

/// A `checkout.commit.get` commit.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitDetail {
    pub sha: String,
    pub short_sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_ms: Option<i64>,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_ms: Option<i64>,
    pub subject: String,
    pub body: String,
    pub refs: Vec<RefLabel>,
    /// `{path, oldPath, additions, deletions, status}`.
    pub files: Vec<Value>,
}

impl CommitDetail {
    fn parse(commit: &Value) -> Self {
        Self {
            sha: text(commit, "sha").to_owned(),
            short_sha: text(commit, "shortSha").to_owned(),
            parents: strings(commit, "parents"),
            author_name: text(commit, "authorName").to_owned(),
            author_email: text(commit, "authorEmail").to_owned(),
            author_ms: parse_iso8601_ms(text(commit, "authorDate")),
            committer_name: text(commit, "committerName").to_owned(),
            committer_email: text(commit, "committerEmail").to_owned(),
            committer_ms: parse_iso8601_ms(text(commit, "committerDate")),
            subject: text(commit, "subject").to_owned(),
            body: text(commit, "body").trim().to_owned(),
            refs: ref_labels(commit),
            files: array(commit, "files").to_vec(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Detail {
    Loading,
    Ready(Box<CommitDetail>),
    /// The SHA no longer resolves to a commit.
    Missing,
    Failed(String),
}

/// The ref the log is limited to.
#[derive(Clone, Debug, PartialEq)]
pub struct RefFilter {
    pub name: String,
    /// Unknown until the refs load for a preselected revision.
    pub kind: Option<RefKind>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filters {
    pub revision: Option<RefFilter>,
    pub text: String,
    pub author: Option<String>,
}

impl Filters {
    /// Text and author filters drop commits from parent chains: no lanes.
    pub fn breaks_graph(&self) -> bool {
        !self.text.trim().is_empty() || self.author.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Disconnected,
    /// The host's daemon predates `gitLog`.
    Unsupported,
    Ready,
}

enum Pending {
    Refs { generation: u64 },
    Page { generation: u64, skip: usize },
    Commit(String),
}

pub struct LogState {
    pub host: String,
    pub workspace_id: String,
    pub cwd: String,
    pub refs: Option<Refs>,
    pub refs_error: Option<String>,
    pub commits: Vec<LogCommit>,
    /// The lane graph of `commits` (empty while filters break parent chains).
    pub rows: Vec<GraphRow>,
    pub has_more: bool,
    pub page_loading: bool,
    pub page_error: Option<String>,
    /// The `skip` of the page that failed.
    failed_skip: usize,
    /// The first page of the current filters arrived.
    pub loaded: bool,
    pub filters: Filters,
    /// Distinct author names seen in loaded commits, sorted case-insensitively.
    pub authors: Vec<String>,
    pub details: HashMap<String, Detail>,
    pub selected: Option<String>,
    generation: u64,
    pending: HashMap<RequestId, Pending>,
    active: bool,
    stale: bool,
    seen_invalidation: u64,
    status_print: Option<String>,
    /// Set whenever anything visible changed; the view redraws and clears it.
    pub dirty: bool,
}

/// What of a checkout status moves history or refs (the status has no HEAD SHA).
fn status_print(store: &GitStore, host: &str, cwd: &str) -> Option<String> {
    let status = store.checkout(host, cwd)?.status.as_ref()?;
    Some(
        ["currentBranch", "upstreamRef", "aheadBehind", "aheadOfOrigin", "behindOfOrigin"]
            .iter()
            .map(|key| status.get(*key).map(Value::to_string).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\u{1f}"),
    )
}

impl LogState {
    pub fn new(target: &PanelTarget, store: &GitStore) -> Self {
        let revision = target
            .data
            .get("revision")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|name| RefFilter { name: name.to_owned(), kind: None });
        Self {
            host: target.server_id.clone(),
            workspace_id: target.workspace_id.clone(),
            cwd: target.cwd.clone(),
            refs: None,
            refs_error: None,
            commits: Vec::new(),
            rows: Vec::new(),
            has_more: false,
            page_loading: false,
            page_error: None,
            failed_skip: 0,
            loaded: false,
            filters: Filters { revision, ..Filters::default() },
            authors: Vec::new(),
            details: HashMap::new(),
            selected: None,
            generation: 0,
            pending: HashMap::new(),
            active: false,
            stale: true,
            seen_invalidation: store.invalidation(&target.server_id, &target.cwd),
            status_print: status_print(store, &target.server_id, &target.cwd),
            dirty: true,
        }
    }

    pub fn availability(&self, ui: &UiContext) -> Availability {
        if !host::connected(ui, &self.host) {
            Availability::Disconnected
        } else if !host::supports(ui, &self.host, FEATURE) {
            Availability::Unsupported
        } else {
            Availability::Ready
        }
    }

    /// Releases every in-flight request.
    pub fn dispose(&mut self) {
        self.generation += 1;
        self.pending.clear();
    }

    /// A hidden tab sends nothing; what it missed reloads once shown.
    pub fn set_active(&mut self, ui: &UiContext, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active && self.stale {
            let keep = self.loaded;
            self.reload(ui, keep);
        }
    }

    fn send(&mut self, ui: &UiContext, kind: &str, fields: Value, pending: Pending) {
        let request = wire::send(&ui.core, &self.host, kind, fields);
        self.pending.insert(request, pending);
    }

    /// Reloads refs and the first page. `keep` leaves the shown rows until the
    /// new page replaces them (checkout changes); a filter change clears them.
    pub fn reload(&mut self, ui: &UiContext, keep: bool) {
        if !self.active || self.availability(ui) != Availability::Ready {
            self.stale = true;
            return;
        }
        self.stale = false;
        self.generation += 1;
        let generation = self.generation;
        self.pending.retain(|_, pending| matches!(pending, Pending::Commit(_)));
        if !keep {
            self.commits.clear();
            self.rows.clear();
            self.loaded = false;
            self.has_more = false;
        }
        self.page_error = None;
        self.refs_error = None;
        self.dirty = true;
        self.send(ui, "checkout.refs.request", json!({ "cwd": self.cwd }), Pending::Refs { generation });
        self.request_page(ui, 0);
    }

    fn request_page(&mut self, ui: &UiContext, skip: usize) {
        let mut fields = json!({ "cwd": self.cwd, "skip": skip, "limit": PAGE_SIZE });
        if let Some(revision) = &self.filters.revision {
            fields["revision"] = json!(revision.name);
        }
        let query = self.filters.text.trim();
        if !query.is_empty() {
            fields["text"] = json!(query);
        }
        if let Some(author) = &self.filters.author {
            fields["author"] = json!(author);
        }
        self.page_loading = true;
        self.page_error = None;
        self.dirty = true;
        let generation = self.generation;
        self.send(ui, "checkout.log.request", fields, Pending::Page { generation, skip });
    }

    /// The next page, once the list end comes near.
    pub fn load_more(&mut self, ui: &UiContext) {
        if !self.active || !self.loaded || !self.has_more || self.page_loading || self.page_error.is_some() {
            return;
        }
        let skip = self.commits.len();
        self.request_page(ui, skip);
    }

    /// The error row's retry: the failed page again.
    pub fn retry(&mut self, ui: &UiContext) {
        if self.loaded && self.failed_skip > 0 {
            self.page_error = None;
            self.load_more(ui);
        } else {
            let keep = self.loaded;
            self.reload(ui, keep);
        }
    }

    pub fn set_filters(&mut self, ui: &UiContext, filters: Filters) {
        if filters == self.filters {
            return;
        }
        self.filters = filters;
        self.reload(ui, false);
    }

    /// Requests `sha`'s details once (cached per SHA).
    pub fn ensure_detail(&mut self, ui: &UiContext, sha: &str) {
        if sha.is_empty() || matches!(self.details.get(sha), Some(Detail::Loading | Detail::Ready(_) | Detail::Missing)) {
            return;
        }
        if !self.active || self.availability(ui) != Availability::Ready {
            return;
        }
        self.details.insert(sha.to_owned(), Detail::Loading);
        self.dirty = true;
        self.send(ui, "checkout.commit.get.request", json!({ "cwd": self.cwd, "sha": sha }), Pending::Commit(sha.to_owned()));
    }

    /// A failed detail load asks again.
    pub fn retry_detail(&mut self, ui: &UiContext, sha: &str) {
        self.details.remove(sha);
        self.ensure_detail(ui, sha);
    }

    pub fn index_of(&self, sha: &str) -> Option<usize> {
        self.commits.iter().position(|c| c.sha == sha)
    }

    pub fn commit(&self, sha: &str) -> Option<&LogCommit> {
        self.commits.iter().find(|c| c.sha == sha)
    }

    fn rebuild_graph(&mut self) {
        self.rows = if self.filters.breaks_graph() {
            Vec::new()
        } else {
            graph::build(self.commits.iter().map(|c| (c.sha.as_str(), c.parents.as_slice())))
        };
    }

    fn note_authors(&mut self, from: usize) {
        for commit in &self.commits[from..] {
            let name = commit.author_name.trim();
            if name.is_empty() {
                continue;
            }
            let key = name.to_lowercase();
            if let Err(index) = self.authors.binary_search_by(|a| a.to_lowercase().cmp(&key).then_with(|| a.as_str().cmp(name))) {
                self.authors.insert(index, name.to_owned());
            }
        }
    }

    /// Applies this view's share of a core batch and the store's checkout
    /// changes. Returns true when anything visible changed.
    pub fn process(&mut self, ui: &UiContext, store: &GitStore) -> bool {
        for event in &ui.events {
            match event {
                CoreEvent::Connected { host_id, .. } if *host_id == self.host => {
                    let keep = self.loaded;
                    self.reload(ui, keep);
                }
                CoreEvent::Disconnected { host_id, .. } if *host_id == self.host => {
                    self.generation += 1;
                    self.pending.clear();
                    self.page_loading = false;
                    self.details.retain(|_, detail| !matches!(detail, Detail::Loading));
                    self.stale = true;
                    self.dirty = true;
                }
                CoreEvent::Response { request_id, result, .. } => {
                    if let Some(pending) = self.pending.remove(request_id) {
                        self.on_response(pending, wire::checkout_result(result));
                    }
                }
                _ => {}
            }
        }
        // A host whose connection or server info arrives later becomes ready here.
        if self.stale && self.active && self.availability(ui) == Availability::Ready {
            let keep = self.loaded;
            self.reload(ui, keep);
        }
        let invalidation = store.invalidation(&self.host, &self.cwd);
        let print = status_print(store, &self.host, &self.cwd);
        let moved = self.status_print.is_some() && print.is_some() && print != self.status_print;
        if print.is_some() {
            self.status_print = print;
        }
        if invalidation != self.seen_invalidation || moved {
            self.seen_invalidation = invalidation;
            let keep = self.loaded;
            self.reload(ui, keep);
        }
        std::mem::take(&mut self.dirty)
    }

    fn on_response(&mut self, pending: Pending, result: Result<Value, wire::Failure>) {
        let result = result.and_then(|payload| match wire::payload_error(&payload) {
            Some(message) => Err(wire::Failure::message(message)),
            None => Ok(payload),
        });
        self.dirty = true;
        match pending {
            Pending::Refs { generation } => {
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(payload) => {
                        let refs = Refs::parse(&payload);
                        if let Some(revision) = self.filters.revision.as_mut()
                            && revision.kind.is_none()
                        {
                            revision.kind = refs.kind_of(&revision.name);
                        }
                        self.refs = Some(refs);
                        self.refs_error = None;
                    }
                    Err(failure) => self.refs_error = Some(failure.message),
                }
            }
            Pending::Page { generation, skip } => {
                if generation != self.generation {
                    return;
                }
                self.page_loading = false;
                let payload = match result {
                    Ok(payload) => payload,
                    Err(failure) => {
                        self.page_error = Some(failure.message);
                        self.failed_skip = skip;
                        return;
                    }
                };
                let page: Vec<LogCommit> = array(&payload, "commits").iter().map(LogCommit::parse).collect();
                let from = if skip == 0 {
                    self.commits = page;
                    0
                } else if skip == self.commits.len() {
                    self.commits.extend(page);
                    skip
                } else {
                    return;
                };
                self.has_more = flag(&payload, "hasMore");
                self.loaded = true;
                self.note_authors(from);
                self.rebuild_graph();
                // The selection survives a reload only while it is still listed.
                if skip == 0
                    && let Some(selected) = &self.selected
                    && self.index_of(selected).is_none()
                {
                    self.selected = None;
                }
            }
            Pending::Commit(sha) => {
                let detail = match result {
                    Ok(payload) => match payload.get("commit") {
                        Some(commit @ Value::Object(_)) => Detail::Ready(Box::new(CommitDetail::parse(commit))),
                        _ => Detail::Missing,
                    },
                    Err(failure) => Detail::Failed(failure.message),
                };
                self.details.insert(sha, detail);
            }
        }
    }
}
