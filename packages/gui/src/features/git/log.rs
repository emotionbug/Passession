//! Git Log data for the `git_log` tab: the checkout's refs, its paged history
//! with the lane graph, and per-commit details, over `checkout.refs.request`,
//! `checkout.log.request` and `checkout.commit.get.request` (feature `gitLog`).
//!
//! A filter change reloads from the first page; a checkout change (an action's
//! invalidation, a status push that moved HEAD or the upstream counts, a
//! reconnect) reloads refs and as many rows as are shown (up to the host's
//! page limit) in place, keeping the selected commit unless the reload proved
//! it gone.

use super::graph::{Builder, GraphRow};
use super::host;
use super::json::{array, flag, opt_int, text};
use super::model::parse_iso8601_ms;
use super::store::GitStore;
use super::wire;
use crate::common::{PanelTarget, UiContext};
use paseo_gui_core::{CoreEvent, RequestId};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

/// Commits per `checkout.log.request` page.
pub const PAGE_SIZE: usize = 300;
/// The host's largest `limit` (an in-place reload asks for the shown rows).
pub const MAX_PAGE: usize = 1000;
/// Commit details kept besides the selected and loading ones.
const DETAIL_CACHE: usize = 64;
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
    /// History of one file or folder (checkout-relative).
    pub path: Option<String>,
}

impl Filters {
    /// Text, author and path filters drop commits from parent chains: no lanes.
    pub fn breaks_graph(&self) -> bool {
        !self.text.trim().is_empty() || self.author.is_some() || self.path.is_some()
    }

    pub fn is_active(&self) -> bool {
        self.revision.is_some() || self.breaks_graph()
    }

    /// The `revision` and `path` a `git_log` tab target asks for.
    pub fn from_target(data: &Value) -> Self {
        let field = |key: &str| data.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        Self {
            revision: field("revision").map(|name| RefFilter { name, kind: None }),
            path: data.get("path").and_then(Value::as_str).map(|path| path.trim_matches('/').to_owned()).filter(|p| !p.is_empty()),
            ..Self::default()
        }
    }
}

/// A search query that may name a SHA-1 or SHA-256 commit: 4–64 hex digits.
pub fn is_hash_query(query: &str) -> bool {
    (4..=64).contains(&query.len()) && query.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Disconnected,
    /// The host's daemon predates `gitLog`.
    Unsupported,
    Ready,
}

/// The answer to a hash lookup typed into the search field.
#[derive(Clone, Debug, PartialEq)]
pub enum Goto {
    /// `sha` (full) names a commit; its details are cached.
    Found { sha: String, explicit: bool },
    /// The query names no single commit.
    NotFound { query: String },
    /// A failed lookup must not silently become a message filter.
    Failed(String),
}

enum Pending {
    Refs { generation: u64 },
    Page { generation: u64, skip: usize },
    Commit(String),
    Goto { generation: u64, query: String, explicit: bool },
}

pub struct LogState {
    pub host: String,
    pub workspace_id: String,
    pub cwd: String,
    pub refs: Option<Refs>,
    /// Bumped whenever `refs` is replaced (branches-panel row cache).
    pub refs_version: u64,
    pub refs_error: Option<String>,
    pub commits: Vec<LogCommit>,
    /// Row of each listed SHA.
    index: HashMap<String, usize>,
    /// The lane graph of `commits` (empty while filters break parent chains).
    pub rows: Vec<GraphRow>,
    graph: Builder,
    pub has_more: bool,
    pub page_loading: bool,
    pub page_error: Option<String>,
    /// The `skip` of the page that failed.
    failed_skip: usize,
    /// The first page of the current filters arrived.
    pub loaded: bool,
    /// Bumped whenever a first page replaces `commits` (scroll re-anchoring).
    pub replaced: u64,
    /// The first load of these filters still has to pick a commit.
    pub auto_select: bool,
    pub filters: Filters,
    /// Distinct author names seen in loaded commits, sorted case-insensitively.
    pub authors: Vec<String>,
    pub details: HashMap<String, Detail>,
    /// Detail SHAs, least recently requested first.
    detail_order: VecDeque<String>,
    pub selected: Option<String>,
    goto: Option<Goto>,
    goto_generation: u64,
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

/// Drops the least recently requested details beyond `cap`, never `keep` or
/// a loading one.
fn evict_details(order: &mut VecDeque<String>, details: &mut HashMap<String, Detail>, keep: Option<&str>, cap: usize) {
    while order.len() > cap {
        let Some(position) = order
            .iter()
            .position(|sha| Some(sha.as_str()) != keep && !matches!(details.get(sha), Some(Detail::Loading)))
        else {
            break;
        };
        if let Some(sha) = order.remove(position) {
            details.remove(&sha);
        }
    }
}

/// The selection after a first page replaced the list: kept while listed, or
/// while it sat past the end of the reloaded prefix (later pages may list it).
fn keeps_selection(previous_index: Option<usize>, listed: bool, reloaded: usize, has_more: bool) -> bool {
    listed || (has_more && previous_index.is_some_and(|index| index >= reloaded))
}

impl LogState {
    pub fn new(target: &PanelTarget, store: &GitStore) -> Self {
        Self {
            host: target.server_id.clone(),
            workspace_id: target.workspace_id.clone(),
            cwd: target.cwd.clone(),
            refs: None,
            refs_version: 0,
            refs_error: None,
            commits: Vec::new(),
            index: HashMap::new(),
            rows: Vec::new(),
            graph: Builder::default(),
            has_more: false,
            page_loading: false,
            page_error: None,
            failed_skip: 0,
            loaded: false,
            replaced: 0,
            auto_select: true,
            filters: Filters::from_target(&target.data),
            authors: Vec::new(),
            details: HashMap::new(),
            detail_order: VecDeque::new(),
            selected: None,
            goto: None,
            goto_generation: 0,
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
        self.goto_generation += 1;
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
    /// new page replaces them and asks for as many rows as are shown (checkout
    /// changes); a filter change clears them.
    pub fn reload(&mut self, ui: &UiContext, keep: bool) {
        self.generation += 1;
        let generation = self.generation;
        self.pending.retain(|_, pending| matches!(pending, Pending::Commit(_) | Pending::Goto { .. }));
        if !keep {
            self.commits.clear();
            self.index.clear();
            self.rows.clear();
            self.graph = Builder::default();
            self.loaded = false;
            self.has_more = false;
            self.auto_select = true;
            self.selected = None;
        }
        self.page_error = None;
        self.refs_error = None;
        self.dirty = true;
        self.page_loading = false;
        if !self.active || self.availability(ui) != Availability::Ready {
            self.stale = true;
            return;
        }
        self.stale = false;
        self.send(ui, "checkout.refs.request", json!({ "cwd": self.cwd }), Pending::Refs { generation });
        let limit = if keep { self.commits.len().clamp(PAGE_SIZE, MAX_PAGE) } else { PAGE_SIZE };
        self.request_page(ui, 0, limit);
    }

    fn request_page(&mut self, ui: &UiContext, skip: usize, limit: usize) {
        let mut fields = json!({ "cwd": self.cwd, "skip": skip, "limit": limit });
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
        if let Some(path) = &self.filters.path {
            fields["path"] = json!(path);
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
        self.request_page(ui, skip, PAGE_SIZE);
    }

    /// A later page failed (the footer row shows it).
    pub fn page_error_at_end(&self) -> bool {
        self.loaded && self.page_error.is_some() && self.failed_skip > 0
    }

    /// The first page failed while older rows stay shown (the banner shows it).
    pub fn refresh_failed(&self) -> bool {
        self.loaded && self.page_error.is_some() && self.failed_skip == 0
    }

    /// Retry of an error row or banner: the failed page again.
    pub fn retry(&mut self, ui: &UiContext) {
        if self.page_error_at_end() && self.refs_error.is_none() {
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
        self.cancel_goto();
        self.reload(ui, false);
    }

    fn touch_detail(&mut self, sha: &str) {
        if self.detail_order.back().is_some_and(|last| last == sha) {
            return;
        }
        if let Some(position) = self.detail_order.iter().position(|s| s == sha) {
            self.detail_order.remove(position);
        }
        self.detail_order.push_back(sha.to_owned());
        evict_details(&mut self.detail_order, &mut self.details, self.selected.as_deref(), DETAIL_CACHE);
    }

    /// Requests `sha`'s details once (cached per SHA, least recently used dropped).
    pub fn ensure_detail(&mut self, ui: &UiContext, sha: &str) {
        if sha.is_empty() {
            return;
        }
        if self.details.contains_key(sha) {
            self.touch_detail(sha);
            return;
        }
        if !self.active || self.availability(ui) != Availability::Ready {
            return;
        }
        self.details.insert(sha.to_owned(), Detail::Loading);
        self.touch_detail(sha);
        self.dirty = true;
        self.send(ui, "checkout.commit.get.request", json!({ "cwd": self.cwd, "sha": sha }), Pending::Commit(sha.to_owned()));
    }

    /// A failed detail load asks again.
    pub fn retry_detail(&mut self, ui: &UiContext, sha: &str) {
        self.details.remove(sha);
        self.ensure_detail(ui, sha);
    }

    /// Looks a typed hash up; the answer arrives through [`take_goto`](Self::take_goto).
    /// A newer lookup supersedes an older one.
    pub fn goto(&mut self, ui: &UiContext, query: &str, explicit: bool) {
        self.goto_generation += 1;
        let generation = self.goto_generation;
        let pending = Pending::Goto { generation, query: query.to_owned(), explicit };
        self.send(ui, "checkout.commit.get.request", json!({ "cwd": self.cwd, "sha": query }), pending);
    }

    /// Drops an unanswered hash lookup (the query changed).
    pub fn cancel_goto(&mut self) {
        self.goto_generation += 1;
        self.goto = None;
    }

    pub fn take_goto(&mut self) -> Option<Goto> {
        self.goto.take()
    }

    pub fn index_of(&self, sha: &str) -> Option<usize> {
        self.index.get(sha).copied()
    }

    pub fn commit(&self, sha: &str) -> Option<&LogCommit> {
        self.index_of(sha).and_then(|index| self.commits.get(index))
    }

    /// Indexes and lays out the commits from `from` on.
    fn extend_list(&mut self, from: usize) {
        let Self { commits, index, rows, graph, filters, .. } = self;
        let graphed = !filters.breaks_graph();
        for (position, commit) in commits.iter().enumerate().skip(from) {
            index.entry(commit.sha.clone()).or_insert(position);
            if graphed {
                rows.push(graph.push(&commit.sha, &commit.parents));
            }
        }
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
                    self.goto_generation += 1;
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
                        self.refs_version += 1;
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
                    let previous = self.selected.as_deref().and_then(|sha| self.index_of(sha));
                    self.commits = page;
                    self.index.clear();
                    self.rows.clear();
                    self.graph = Builder::default();
                    self.extend_list(0);
                    self.replaced += 1;
                    let listed = self.selected.as_deref().is_some_and(|sha| self.index.contains_key(sha));
                    if !keeps_selection(previous, listed, self.commits.len(), flag(&payload, "hasMore")) {
                        self.selected = None;
                    }
                    0
                } else if skip == self.commits.len() {
                    self.commits.extend(page);
                    self.extend_list(skip);
                    skip
                } else {
                    return;
                };
                self.has_more = flag(&payload, "hasMore");
                self.loaded = true;
                self.note_authors(from);
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
                evict_details(&mut self.detail_order, &mut self.details, self.selected.as_deref(), DETAIL_CACHE);
            }
            Pending::Goto { generation, query, explicit } => {
                if generation != self.goto_generation {
                    return;
                }
                let payload = match result {
                    Ok(payload) => payload,
                    Err(failure) => {
                        self.goto = Some(Goto::Failed(failure.message));
                        return;
                    }
                };
                let commit = match payload.get("commit") {
                    Some(commit @ Value::Object(_)) => Some(CommitDetail::parse(commit)),
                    _ => None,
                };
                self.goto = Some(match commit.filter(|c| !c.sha.is_empty()) {
                    Some(commit) => {
                        let sha = commit.sha.clone();
                        self.details.insert(sha.clone(), Detail::Ready(Box::new(commit)));
                        self.touch_detail(&sha);
                        Goto::Found { sha, explicit }
                    }
                    None => Goto::NotFound { query },
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> LogState {
        LogState::new(&PanelTarget::new("git_log", "host", "workspace", "/repo"), &GitStore::default())
    }

    #[test]
    fn failed_hash_lookup_does_not_become_a_message_search() {
        let mut state = state();
        state.on_response(
            Pending::Goto { generation: 0, query: "deadbeef".into(), explicit: false },
            Err(wire::Failure::message("Disconnected")),
        );
        assert_eq!(state.take_goto(), Some(Goto::Failed("Disconnected".into())));
    }

    #[test]
    fn settled_details_enforce_cache_limit_and_recent_access() {
        let mut state = state();
        for i in 0..=DETAIL_CACHE {
            let sha = i.to_string();
            state.details.insert(sha.clone(), Detail::Loading);
            state.touch_detail(&sha);
        }
        assert_eq!(state.details.len(), DETAIL_CACHE + 1);
        state.on_response(Pending::Commit("0".into()), Ok(json!({ "commit": null })));
        assert_eq!(state.details.len(), DETAIL_CACHE);
        assert!(!state.details.contains_key("0"));
        state.on_response(Pending::Commit("1".into()), Ok(json!({ "commit": null })));
        state.touch_detail("1");
        assert_eq!(state.detail_order.back().map(String::as_str), Some("1"));
    }

    #[test]
    fn stale_page_cannot_replace_current_filter_results() {
        let mut state = state();
        state.generation = 2;
        state.on_response(
            Pending::Page { generation: 1, skip: 0 },
            Ok(json!({ "commits": [{ "sha": "old" }], "hasMore": false })),
        );
        assert!(state.commits.is_empty());
        assert!(!state.loaded);
    }

    #[test]
    fn hash_queries_accept_sha1_and_sha256() {
        assert!(is_hash_query("abcd"));
        assert!(is_hash_query("0123456789abcdefABCDEF0123456789abcdef01"));
        assert!(!is_hash_query("abc"));
        assert!(!is_hash_query("fix bug"));
        assert!(!is_hash_query("deadbeefg"));
        assert!(is_hash_query(&"a".repeat(64)));
        assert!(!is_hash_query(&"a".repeat(65)));
    }

    #[test]
    fn eviction_keeps_selected_and_loading_details() {
        let mut details = HashMap::new();
        let mut order = VecDeque::new();
        for sha in ["a", "b", "c", "d"] {
            details.insert(sha.to_owned(), Detail::Missing);
            order.push_back(sha.to_owned());
        }
        details.insert("b".to_owned(), Detail::Loading);
        evict_details(&mut order, &mut details, Some("a"), 2);
        // a is selected and b is loading: c goes first, then d.
        assert_eq!(order, VecDeque::from(vec!["a".to_owned(), "b".to_owned()]));
        assert!(details.contains_key("a") && details.contains_key("b"));
        assert!(!details.contains_key("c") && !details.contains_key("d"));
    }

    #[test]
    fn eviction_stops_when_only_kept_entries_remain() {
        let mut details = HashMap::from([("a".to_owned(), Detail::Loading)]);
        let mut order = VecDeque::from(vec!["a".to_owned()]);
        evict_details(&mut order, &mut details, None, 0);
        assert_eq!(order.len(), 1);
    }

    #[test]
    fn reload_keeps_selection_listed_or_past_the_reloaded_prefix() {
        assert!(keeps_selection(Some(10), true, 300, true));
        // Selected at row 500, the reload listed 300 rows: still valid history.
        assert!(keeps_selection(Some(500), false, 300, true));
        // An exhaustive response proves that the former row 500 disappeared.
        assert!(!keeps_selection(Some(500), false, 300, false));
        // Inside the reloaded range but gone: dropped.
        assert!(!keeps_selection(Some(5), false, 300, true));
        // Never listed (a filter change cleared the rows): dropped.
        assert!(!keeps_selection(None, false, 300, true));
    }

    #[test]
    fn target_data_selects_revision_and_path() {
        let filters = Filters::from_target(&json!({ "revision": " main ", "path": "/src/lib.rs" }));
        assert_eq!(filters.revision, Some(RefFilter { name: "main".to_owned(), kind: None }));
        assert_eq!(filters.path.as_deref(), Some("src/lib.rs"));
        assert!(filters.breaks_graph());
        assert_eq!(Filters::from_target(&json!({ "path": " file " })).path.as_deref(), Some(" file "));
        assert_eq!(Filters::from_target(&json!({})), Filters::default());
    }
}
