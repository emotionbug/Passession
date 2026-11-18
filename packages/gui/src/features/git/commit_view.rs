//! Session-directory Commit tool window (IntelliJ's Commit): a changelist tree
//! — Changes and Unversioned Files, grouped by compacted directory chains —
//! with tri-state inclusion, above a message form. The tree observes summaries
//! only; committing sends the explicit selection and never includes unrelated
//! index entries.
use super::chrome::{
    ButtonSize, ButtonVariant, TOOLBAR_HEIGHT, button, button_height, measure_button, palette,
    toolbar_band, toolbar_button, toolbar_button_size, toolbar_hit, tree_row_height,
    tree_row_metrics, ui as ui_px,
};
use super::host;
use super::json::{array, flag, object, text};
use super::model::utf16_cmp;
use super::paint::{Font, GitPaint, Hits, Retained, rect, with_alpha};
use super::store::GitStore;
use super::text::{t, tr};
use super::wire;
use crate::common::layer::inputs;
use crate::common::overlay::tooltip_at;
use crate::common::paint::TextSpec;
use crate::common::{PanelTarget, UiContext};
use makepad_widgets::scroll_bars::TouchScroll;
use makepad_widgets::*;
use paseo_gui_core::{CoreError, CoreEvent, RequestId};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

pub const FEATURE: &str = "gitSelectedCommit";

/// The daemon's typed not-a-repository code (`checkout_error`).
const NOT_GIT_REPO: &str = "NOT_GIT_REPO";
/// Tooltip delay of the tree and toolbar.
const TOOLTIP_WAIT: f64 = 0.5;
/// Right inset of tree rows: the 3px scrollbar thumb sits 5px from the edge,
/// and the status column keeps clear of it.
const SCROLLBAR_GUTTER: f64 = 12.0;

// ---------------------------------------------------------------------------
// Changelist tree (pure).

/// IntelliJ's changelist roots: tracked changes and unversioned files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Bucket {
    Changes,
    Unversioned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Status {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

fn file_status(file: &Value) -> Status {
    if flag(file, "untracked") {
        Status::Untracked
    } else if flag(file, "isDeleted") || text(file, "status") == "deleted" {
        Status::Deleted
    } else if !text(file, "oldPath").is_empty() {
        Status::Renamed
    } else if flag(file, "isNew") || text(file, "status") == "added" {
        Status::Added
    } else {
        Status::Modified
    }
}

fn bucket_of(file: &Value) -> Bucket {
    if file_status(file) == Status::Untracked {
        Bucket::Unversioned
    } else {
        Bucket::Changes
    }
}

/// `old → new`, as paths when the directory changed and as names otherwise.
fn rename_hint(file: &Value) -> Option<String> {
    let old = text(file, "oldPath");
    if old.is_empty() {
        return None;
    }
    let new = text(file, "path");
    let split = |path: &str| -> (String, String) {
        match path.rfind('/') {
            Some(slash) => (path[..slash].to_owned(), path[slash + 1..].to_owned()),
            None => (String::new(), path.to_owned()),
        }
    };
    let ((old_dir, old_name), (new_dir, new_name)) = (split(old), split(new));
    Some(if old_dir == new_dir {
        format!("{old_name} → {new_name}")
    } else {
        format!("{old} → {new}")
    })
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent_dir(path: &str) -> &str {
    path.rfind('/').map_or("", |slash| &path[..slash])
}

/// Stable identity of a tree node across refreshes (collapse and focus state).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NodeId {
    Bucket(Bucket),
    /// The full path of the deepest directory of a compacted chain.
    Dir(Bucket, String),
    File(String),
}

#[derive(Clone, Debug, PartialEq, Hash)]
enum NodeKind {
    Bucket(Bucket),
    /// `label` is the compacted chain, e.g. `packages/gui/src`.
    Dir {
        label: String,
    },
    File {
        index: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Node {
    id: NodeId,
    kind: NodeKind,
    depth: usize,
    /// The node's files as a range of [`Tree::order`].
    files: Range<usize>,
    /// One past the last node of this node's subtree (preorder).
    end: usize,
}

impl Node {
    fn is_file(&self) -> bool {
        matches!(self.kind, NodeKind::File { .. })
    }
}

/// The preorder tree; every subtree's files are contiguous in `order`, so a
/// node's inclusion state is a range count.
#[derive(Clone, Debug, Default, PartialEq)]
struct Tree {
    nodes: Vec<Node>,
    /// File indices in tree order.
    order: Vec<usize>,
}

#[derive(Default)]
struct DirBuild {
    dirs: BTreeMap<String, DirBuild>,
    files: Vec<usize>,
}

impl DirBuild {
    fn insert(&mut self, segments: &[&str], index: usize) {
        match segments.split_first() {
            None => self.files.push(index),
            Some((first, rest)) => self
                .dirs
                .entry((*first).to_owned())
                .or_default()
                .insert(rest, index),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Check {
    Unchecked,
    Partial,
    Checked,
}

impl Check {
    fn of(selected: usize, total: usize) -> Self {
        if total > 0 && selected == total {
            Check::Checked
        } else if selected == 0 {
            Check::Unchecked
        } else {
            Check::Partial
        }
    }
}

impl Tree {
    /// Changes then Unversioned Files (empty roots omitted); directories before
    /// files in code-unit order, single-child directory chains compacted. A flat
    /// tree lists each root's files by path.
    fn build(files: &[Value], group: bool) -> Self {
        let mut tree = Tree::default();
        for bucket in [Bucket::Changes, Bucket::Unversioned] {
            let mut members: Vec<usize> = (0..files.len())
                .filter(|&index| bucket_of(&files[index]) == bucket)
                .collect();
            if members.is_empty() {
                continue;
            }
            let root = tree.open(NodeId::Bucket(bucket), NodeKind::Bucket(bucket), 0);
            if group {
                let mut dirs = DirBuild::default();
                for index in members {
                    let path = text(&files[index], "path");
                    let segments: Vec<&str> = path.split('/').collect();
                    dirs.insert(&segments[..segments.len() - 1], index);
                }
                tree.emit(&dirs, bucket, "", 1, files);
            } else {
                members
                    .sort_by(|&a, &b| utf16_cmp(text(&files[a], "path"), text(&files[b], "path")));
                for index in members {
                    tree.leaf(index, 1, files);
                }
            }
            tree.close(root);
        }
        tree
    }

    fn open(&mut self, id: NodeId, kind: NodeKind, depth: usize) -> usize {
        let start = self.order.len();
        self.nodes.push(Node {
            id,
            kind,
            depth,
            files: start..start,
            end: 0,
        });
        self.nodes.len() - 1
    }

    fn close(&mut self, at: usize) {
        self.nodes[at].files.end = self.order.len();
        self.nodes[at].end = self.nodes.len();
    }

    fn leaf(&mut self, index: usize, depth: usize, files: &[Value]) {
        let id = NodeId::File(text(&files[index], "path").to_owned());
        let at = self.open(id, NodeKind::File { index }, depth);
        self.order.push(index);
        self.close(at);
    }

    fn emit(
        &mut self,
        dir: &DirBuild,
        bucket: Bucket,
        prefix: &str,
        depth: usize,
        files: &[Value],
    ) {
        let mut names: Vec<&String> = dir.dirs.keys().collect();
        names.sort_by(|a, b| utf16_cmp(a, b));
        for name in names {
            let mut child = &dir.dirs[name];
            let mut label = name.clone();
            let mut path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next, grandchild) = child.dirs.iter().next().expect("one child");
                label = format!("{label}/{next}");
                path = format!("{path}/{next}");
                child = grandchild;
            }
            let at = self.open(
                NodeId::Dir(bucket, path.clone()),
                NodeKind::Dir { label },
                depth,
            );
            self.emit(child, bucket, &path, depth + 1, files);
            self.close(at);
        }
        let mut leaves = dir.files.clone();
        leaves.sort_by(|&a, &b| utf16_cmp(text(&files[a], "path"), text(&files[b], "path")));
        for index in leaves {
            self.leaf(index, depth, files);
        }
    }

    /// Node indices shown with `collapsed` subtrees hidden.
    fn visible(&self, collapsed: &HashSet<NodeId>) -> Vec<usize> {
        let mut rows = Vec::with_capacity(self.nodes.len());
        let mut index = 0;
        while index < self.nodes.len() {
            rows.push(index);
            index = if collapsed.contains(&self.nodes[index].id) {
                self.nodes[index].end
            } else {
                index + 1
            };
        }
        rows
    }

    /// The nearest preceding node whose subtree contains `index`.
    fn parent(&self, index: usize) -> Option<usize> {
        (0..index).rev().find(|&at| self.nodes[at].end > index)
    }

    fn find(&self, id: &NodeId) -> Option<usize> {
        self.nodes.iter().position(|node| &node.id == id)
    }

    fn paths<'a>(&'a self, index: usize, files: &'a [Value]) -> impl Iterator<Item = &'a str> + 'a {
        self.order[self.nodes[index].files.clone()]
            .iter()
            .map(move |&file| text(&files[file], "path"))
    }

    fn check(&self, index: usize, files: &[Value], selected: &HashSet<String>) -> Check {
        let count = self
            .paths(index, files)
            .filter(|path| selected.contains(*path))
            .count();
        Check::of(count, self.nodes[index].files.len())
    }

    /// Selected-file prefix counts over `order` for O(1) [`Check`]s while drawing.
    fn selected_prefix(
        &self,
        files: &[Value],
        selected: &HashSet<String>,
        prefix: &mut Vec<usize>,
    ) {
        prefix.clear();
        prefix.reserve(self.order.len() + 1);
        prefix.push(0);
        for &file in &self.order {
            let last = *prefix.last().expect("seeded");
            prefix.push(last + usize::from(selected.contains(text(&files[file], "path"))));
        }
    }

    /// A fully included node excludes every descendant; otherwise all are included.
    fn toggle(&self, index: usize, files: &[Value], selected: &mut HashSet<String>) {
        if self.check(index, files, selected) == Check::Checked {
            for path in self.paths(index, files) {
                selected.remove(path);
            }
        } else {
            selected.extend(self.paths(index, files).map(str::to_owned));
        }
    }
}

// ---------------------------------------------------------------------------
// Loading.

#[derive(Clone, Debug, PartialEq, Hash)]
enum LoadFailure {
    NotRepository,
    TooLarge,
    Other(String),
}

/// The daemon's `NotGitRepo` text. Current daemons also send the typed code;
/// daemons that predate it report only this message.
fn not_repository_message(message: &str) -> bool {
    message.starts_with("Not a git repository")
}

fn load_payload(payload: &Value) -> Result<Vec<Value>, LoadFailure> {
    if let Some(message) = wire::payload_error(payload) {
        return Err(
            if text(object(payload, "error"), "code") == NOT_GIT_REPO
                || not_repository_message(&message)
            {
                LoadFailure::NotRepository
            } else if flag(payload, "diffTooLarge") {
                LoadFailure::TooLarge
            } else {
                LoadFailure::Other(message)
            },
        );
    }
    if flag(payload, "diffTooLarge") {
        return Err(LoadFailure::TooLarge);
    }
    Ok(array(payload, "files").to_vec())
}

fn load_result(result: &Result<Value, CoreError>) -> Result<Vec<Value>, LoadFailure> {
    match result {
        Ok(payload) => load_payload(payload),
        Err(error) if error.code == NOT_GIT_REPO || not_repository_message(&error.message) => {
            Err(LoadFailure::NotRepository)
        }
        Err(error) => Err(LoadFailure::Other(error.message.clone())),
    }
}

#[derive(Default)]
struct Selection {
    files: Vec<Value>,
    selected: HashSet<String>,
    initialized: bool,
}

impl Selection {
    fn replace(&mut self, files: Vec<Value>) {
        let present: HashSet<&str> = files.iter().map(|file| text(file, "path")).collect();
        self.selected.retain(|path| present.contains(path.as_str()));
        // Only the first snapshot selects all. New files arriving while the user
        // writes a message must not silently enter that pending commit.
        if !self.initialized {
            self.selected.extend(present.into_iter().map(str::to_owned));
            self.initialized = true;
        }
        self.files = files;
    }

    fn paths(&self) -> Vec<String> {
        self.files
            .iter()
            .filter_map(|file| {
                let path = text(file, "path");
                self.selected.contains(path).then(|| path.to_owned())
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// View.

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Action {
    /// Files toggle inclusion; roots and directories expand or collapse.
    Row(NodeId),
    Check(NodeId),
    Refresh,
    ExpandAll,
    CollapseAll,
    GroupByDirectory,
    Commit,
    Splitter,
    /// The commit failure banner (tooltip with the full error).
    Failure,
}

#[derive(Clone, Debug, PartialEq, Hash)]
enum Display {
    Disconnected,
    Unsupported,
    Loading,
    Failed(LoadFailure),
    Empty,
    Files,
}

/// A centred state: icon (or spinner), title, body, muted detail and one action.
struct StateCopy {
    icon: Option<&'static str>,
    error: bool,
    title: String,
    body: String,
    detail: String,
    action: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Part {
    Background,
    State,
    StateAction,
    Toolbar,
    Summary,
    Tree,
    Scrollbar,
    Splitter,
    Banner,
    BannerAction,
    Hint,
    Editor,
    Submit,
}

#[derive(Default)]
struct RowPaint {
    row: Retained<Action>,
    icon: Retained<Action>,
    pending: bool,
    visible: bool,
}

pub struct CommitView {
    target: PanelTarget,
    selection: Selection,
    message: String,
    observation: Option<RequestId>,
    commit: Option<RequestId>,
    active: bool,
    loaded: bool,
    /// A successful snapshot is shown (stale content stays while reloading).
    has_snapshot: bool,
    failed: Option<LoadFailure>,
    commit_failure: Option<String>,
    feedback: Option<String>,
    uncertain: bool,
    refresh_requested: bool,
    input_initialized: bool,
    clear_input: bool,
    seen_invalidation: u64,
    branch: Option<String>,
    /// The store's checkout status reports `isGit: false`.
    not_git: bool,
    reload_status: bool,
    tree: Tree,
    selection_prefix: Vec<usize>,
    visible_nodes: Vec<usize>,
    visible_generation: Option<u64>,
    group_by_dir: bool,
    collapsed: HashSet<NodeId>,
    /// Keyboard cursor (desktop).
    focus: Option<NodeId>,
    reveal_focus: bool,
    /// Whether the message form was drawn (it owns the text input).
    form_visible: bool,
    /// Dragged desktop form height, and the height last drawn.
    form_height: Option<f64>,
    shown_form_height: f64,
    /// `(pointer y, form height)` at the splitter press.
    drag: Option<(f64, f64)>,
    compact: bool,
    hits: Hits<Action>,
    pressed: Option<Action>,
    hovered: Option<Action>,
    list: Rect,
    row_height: f64,
    offset: f64,
    max_scroll: f64,
    touch_scroll: TouchScroll,
    sections: HashMap<Part, Retained<Action>>,
    toolbar_actions: HashMap<Action, Retained<Action>>,
    rows: HashMap<NodeId, RowPaint>,
    rows_dirty: bool,
    tree_generation: u64,
    presentation: Display,
    display_changed: bool,
    message_generation: u64,
    editor_focused: bool,
    pub icons_pending: bool,
}

impl CommitView {
    pub fn new(cx: &mut Cx, ui: &UiContext, store: &mut GitStore, target: PanelTarget) -> Self {
        store.acquire_checkout(cx, ui, &target.server_id, &target.cwd);
        Self {
            seen_invalidation: store.invalidation(&target.server_id, &target.cwd),
            target,
            selection: Selection::default(),
            message: String::new(),
            observation: None,
            commit: None,
            active: false,
            loaded: false,
            has_snapshot: false,
            commit_failure: None,
            failed: None,
            feedback: None,
            uncertain: false,
            refresh_requested: true,
            input_initialized: false,
            clear_input: false,
            branch: None,
            not_git: false,
            reload_status: false,
            tree: Tree::default(),
            selection_prefix: vec![0],
            visible_nodes: Vec::new(),
            visible_generation: None,
            group_by_dir: true,
            collapsed: HashSet::new(),
            focus: None,
            reveal_focus: false,
            form_visible: false,
            form_height: None,
            shown_form_height: 0.0,
            drag: None,
            compact: false,
            hits: Hits::default(),
            pressed: None,
            hovered: None,
            list: Rect::default(),
            row_height: 28.0,
            offset: 0.0,
            max_scroll: 0.0,
            touch_scroll: TouchScroll::default(),
            sections: HashMap::new(),
            toolbar_actions: HashMap::new(),
            rows: HashMap::new(),
            rows_dirty: false,
            tree_generation: 0,
            presentation: Display::Loading,
            display_changed: false,
            message_generation: 0,
            editor_focused: false,
            icons_pending: false,
        }
    }

    fn part_key(&self, part: Part) -> u64 {
        let files = self.presentation == Display::Files;
        let available = !matches!(
            self.presentation,
            Display::Disconnected | Display::Unsupported
        );
        let committing = self.commit.is_some();
        let ready = available && self.commit_ready();
        match part {
            Part::Background => 0,
            Part::State => inputs((&self.presentation, &self.feedback, self.compact)),
            Part::StateAction => inputs((
                self.hovered == Some(Action::Refresh),
                committing,
                self.compact,
            )),
            Part::Toolbar => inputs(self.compact),
            Part::Summary => inputs((
                &self.branch,
                self.selection.selected.len(),
                files,
                self.compact,
            )),
            Part::Tree => inputs((
                self.tree_generation,
                self.offset.to_bits(),
                self.reveal_focus,
                self.compact,
            )),
            Part::Scrollbar => inputs((self.offset.to_bits(), self.max_scroll.to_bits())),
            Part::Splitter => inputs((
                self.hovered == Some(Action::Splitter),
                self.drag.is_some(),
                self.form_height.map(f64::to_bits),
                self.compact,
            )),
            Part::Banner => inputs((&self.commit_failure, self.uncertain, self.compact)),
            Part::BannerAction => inputs((self.hovered == Some(Action::Refresh), self.compact)),
            Part::Hint => {
                let kind = if self.feedback.is_some() {
                    0
                } else if committing || ready || self.commit_failure.is_some() || self.uncertain {
                    1
                } else if self.selection.selected.is_empty() {
                    2
                } else if self.message.trim().is_empty() {
                    3
                } else {
                    1
                };
                inputs((kind, &self.feedback, self.compact))
            }
            Part::Editor => inputs((
                self.message_generation,
                committing,
                self.editor_focused,
                self.compact,
            )),
            Part::Submit => inputs((
                self.selection.selected.len(),
                committing,
                ready,
                self.hovered == Some(Action::Commit),
                self.pressed == Some(Action::Commit),
                self.compact,
            )),
        }
    }

    fn toolbar_key(&self, action: &Action) -> u64 {
        inputs((
            self.hovered.as_ref() == Some(action),
            self.commit.is_none()
                && !matches!(
                    self.presentation,
                    Display::Disconnected | Display::Unsupported
                ),
            *action == Action::Refresh && self.reloading(),
            *action == Action::GroupByDirectory && self.group_by_dir,
            self.compact,
        ))
    }

    fn row_key(&self, index: usize, selected: usize) -> u64 {
        let node = &self.tree.nodes[index];
        let file = match &node.kind {
            NodeKind::File { index } => {
                let file = &self.selection.files[*index];
                Some((text(file, "path"), text(file, "oldPath"), file_status(file)))
            }
            _ => None,
        };
        let highlighted = self.hovered.as_ref().is_some_and(
            |action| matches!(action, Action::Row(id) | Action::Check(id) if id == &node.id),
        ) || self
            .pressed
            .as_ref()
            .is_some_and(|action| matches!(action, Action::Row(id) if id == &node.id));
        inputs((
            (&node.kind, node.depth, node.files.len(), file),
            (
                Check::of(selected, node.files.len()),
                self.collapsed.contains(&node.id),
            ),
            (
                !self.compact && self.focus.as_ref() == Some(&node.id),
                highlighted,
            ),
            (self.commit.is_none(), self.group_by_dir, self.compact),
            (self.offset.to_bits(), self.list.size.y.to_bits()),
        ))
    }

    fn part_visible(&self, part: Part) -> bool {
        let files = self.presentation == Display::Files;
        match part {
            Part::Background => true,
            Part::State => !files,
            Part::StateAction => matches!(self.presentation, Display::Failed(_)),
            Part::Toolbar | Part::Summary => files || self.presentation == Display::Empty,
            Part::Banner | Part::BannerAction => {
                files && (self.commit_failure.is_some() || self.uncertain)
            }
            _ => files,
        }
    }

    fn begin_part(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, part: Part, area: Rect) -> bool {
        let key = self.part_key(part);
        self.sections
            .entry(part)
            .or_default()
            .begin(cx, area, key, &mut self.hits, paint)
    }

    fn end_part(&mut self, cx: &mut Cx2d, part: Part) {
        let key = self.part_key(part);
        let section = self.sections.get_mut(&part).unwrap();
        section.end(cx, &self.hits);
        section.set_inputs(key);
    }

    /// Schedule only presentations whose inputs changed. The pane's composition
    /// then replays every unaffected sibling, including editor and row hit geometry.
    pub fn invalidate(&mut self, cx: &mut Cx) {
        let transition = std::mem::take(&mut self.display_changed);
        for (part, section) in &self.sections {
            if transition && *part != Part::Background {
                section.invalidate(cx);
            } else if self.part_visible(*part) {
                section.invalidate_if_changed(cx, self.part_key(*part));
            }
        }
        for (action, section) in &self.toolbar_actions {
            if transition {
                section.invalidate(cx);
            } else if self.part_visible(Part::Toolbar)
                && (self.presentation == Display::Files || *action == Action::Refresh)
            {
                section.invalidate_if_changed(cx, self.toolbar_key(action));
            }
        }
        if !std::mem::take(&mut self.rows_dirty) && !transition {
            return;
        }
        let prefix = &self.selection_prefix;
        for (index, node) in self.tree.nodes.iter().enumerate() {
            if let Some(row) = self.rows.get(&node.id)
                && (row.visible || transition)
            {
                if transition {
                    row.row.invalidate(cx);
                } else {
                    row.row.invalidate_if_changed(
                        cx,
                        self.row_key(index, prefix[node.files.end] - prefix[node.files.start]),
                    );
                }
            }
        }
    }

    pub fn invalidate_media(&mut self, cx: &mut Cx) {
        for (id, row) in &self.rows {
            if row.pending
                && let NodeId::File(path) = id
                && row.visible
                && crate::features::files::file_icon(file_name(path)).is_some()
            {
                row.icon.invalidate(cx);
            }
        }
    }

    pub fn dispose(&mut self, store: &mut GitStore) {
        if let Some(id) = self.observation.take()
            && let Some(core) = store.core()
        {
            core.cancel_request(&id);
        }
        // A submitted mutation continues on the host; closing a tab is not cancel.
        store.release_checkout(&self.target.server_id, &self.target.cwd);
    }

    fn available(&self, ui: &UiContext) -> bool {
        host::connected(ui, &self.target.server_id)
            && host::supports(ui, &self.target.server_id, FEATURE)
    }

    fn observe(&mut self, ui: &UiContext) {
        if let Some(id) = self.observation.take() {
            ui.core.cancel_request(&id);
        }
        self.loaded = false;
        self.failed = None;
        self.refresh_requested = false;
        self.observation = Some(ui.core.observe(
            &self.target.server_id,
            json!({
                "type":"subscribe_checkout_diff_request", "cwd":self.target.cwd,
                "compare":{"mode":"uncommitted", "detail":"summary", "ignoreWhitespace":false}
            }),
            "subscribe_checkout_diff_response",
        ));
    }

    fn snapshot(&mut self, result: Result<Vec<Value>, LoadFailure>) {
        self.rows_dirty = true;
        self.loaded = true;
        match result {
            Ok(files) => {
                self.failed = None;
                self.has_snapshot = true;
                self.selection.replace(files);
                let tree = Tree::build(&self.selection.files, self.group_by_dir);
                if tree != self.tree {
                    self.tree = tree;
                    self.tree_generation = self.tree_generation.wrapping_add(1);
                }
            }
            Err(failure) => {
                // Retrying shows loading, not the stale list beneath the failure.
                self.failed = Some(failure);
                self.has_snapshot = false;
            }
        }
        self.tree.selected_prefix(
            &self.selection.files,
            &self.selection.selected,
            &mut self.selection_prefix,
        );
        let ids: HashSet<_> = self.tree.nodes.iter().map(|node| &node.id).collect();
        self.rows.retain(|id, _| ids.contains(id));
    }

    pub fn process(
        &mut self,
        cx: &mut Cx,
        ui: &UiContext,
        store: &mut GitStore,
        visible: bool,
    ) -> bool {
        let mut changed = self.active != visible;
        if self.active != visible {
            self.active = visible;
            self.refresh_requested = true;
            if !visible {
                if let Some(id) = self.observation.take() {
                    ui.core.cancel_request(&id);
                }
                self.touch_scroll.reset();
                self.drag = None;
            }
        }
        for event in &ui.events {
            match event {
                CoreEvent::Response {
                    request_id, result, ..
                } if self.observation.as_ref() == Some(request_id) => {
                    self.snapshot(load_result(result));
                    changed = true;
                }
                CoreEvent::Response {
                    request_id, result, ..
                } if self.commit.as_ref() == Some(request_id) => {
                    self.commit = None;
                    self.rows_dirty = true;
                    match wire::git_result(result) {
                        Ok(_) => {
                            self.message.clear();
                            self.message_generation = self.message_generation.wrapping_add(1);
                            self.clear_input = true;
                            self.selection.selected.clear();
                            self.selection_prefix.fill(0);
                            self.feedback = Some(t("workspace.git.commit.success"));
                            self.failed = None;
                        }
                        Err(error) => {
                            self.commit_failure = Some(error.message);
                            // Transport failure can follow a committed mutation. Require
                            // an explicit refresh/inspection, never an automatic replay.
                            self.uncertain = match result {
                                Err(error) => error.outcome_unknown,
                                Ok(payload) => paseo_gui_core::response_error(payload, "commit")
                                    .is_some_and(|error| error.outcome_unknown),
                            };
                        }
                    }
                    store.invalidate(cx, &self.target.server_id, &self.target.cwd);
                    self.refresh_requested = true;
                    changed = true;
                }
                CoreEvent::HostEvent { host_id, event } if *host_id == self.target.server_id => {
                    let payload = &event["payload"];
                    let observed = self.observation.as_ref().is_some_and(|id| {
                        array(event, "observations")
                            .iter()
                            .any(|v| v.as_str() == Some(id.as_str()))
                            || payload["observationId"].as_str() == Some(id.as_str())
                    });
                    if observed {
                        if text(event, "type") == "subscription.error" {
                            let error = text(payload, "error").to_owned();
                            self.snapshot(Err(LoadFailure::Other(error)));
                        } else {
                            self.snapshot(load_payload(payload));
                        }
                        changed = true;
                    }
                }
                CoreEvent::Disconnected { host_id, .. } if *host_id == self.target.server_id => {
                    self.refresh_requested = true;
                    self.loaded = false;
                    changed = true;
                }
                CoreEvent::Connected { host_id, .. } if *host_id == self.target.server_id => {
                    self.refresh_requested = true;
                    changed = true;
                }
                _ => {}
            }
        }
        let status = store
            .checkout(&self.target.server_id, &self.target.cwd)
            .and_then(|state| state.status.as_ref());
        let branch = status
            .map(|status| text(status, "currentBranch").to_owned())
            .filter(|branch| !branch.is_empty());
        // The checkout status names a plain folder before (or instead of) the diff.
        let not_git = status.is_some_and(|status| status.get("isGit") == Some(&Value::Bool(false)));
        if branch != self.branch || not_git != self.not_git {
            self.branch = branch;
            self.not_git = not_git;
            changed = true;
        }
        if std::mem::take(&mut self.reload_status) {
            // "Check again" after `git init` must not keep a stale non-repository status.
            store.reload(
                cx,
                &super::store::key(&self.target.server_id, &self.target.cwd),
            );
        }
        let invalidation = store.invalidation(&self.target.server_id, &self.target.cwd);
        if invalidation != self.seen_invalidation {
            self.seen_invalidation = invalidation;
            self.refresh_requested = true;
        }
        if visible && self.available(ui) && self.refresh_requested && self.commit.is_none() {
            self.observe(ui);
            changed = true;
        }
        let display = self.display(ui);
        changed |= display != self.presentation;
        self.display_changed |= display != self.presentation;
        self.presentation = display;
        changed
    }

    fn display(&self, ui: &UiContext) -> Display {
        if !host::connected(ui, &self.target.server_id) {
            Display::Disconnected
        } else if !host::supports(ui, &self.target.server_id, FEATURE) {
            Display::Unsupported
        } else if self.failed == Some(LoadFailure::NotRepository)
            || (self.not_git && !self.has_snapshot)
        {
            // A loaded diff wins over a stale status (e.g. right after `git init`).
            Display::Failed(LoadFailure::NotRepository)
        } else if let Some(failure) = &self.failed {
            Display::Failed(failure.clone())
        } else if !self.has_snapshot {
            Display::Loading
        } else if self.selection.files.is_empty() {
            Display::Empty
        } else {
            Display::Files
        }
    }

    fn can_commit(&self, ui: &UiContext) -> bool {
        self.available(ui) && self.commit_ready()
    }

    fn commit_ready(&self) -> bool {
        self.loaded
            && self.failed.is_none()
            && self.commit_failure.is_none()
            && !self.uncertain
            && self.commit.is_none()
            && !self.selection.selected.is_empty()
            && !self.message.trim().is_empty()
    }

    fn toggle_node(&mut self, index: usize) {
        self.rows_dirty = true;
        self.tree
            .toggle(index, &self.selection.files, &mut self.selection.selected);
        self.tree.selected_prefix(
            &self.selection.files,
            &self.selection.selected,
            &mut self.selection_prefix,
        );
        self.feedback = None;
    }

    fn activate(&mut self, ui: &UiContext, action: Action) {
        if self.commit.is_some() {
            return;
        }
        self.rows_dirty |= matches!(
            &action,
            Action::Row(_)
                | Action::Check(_)
                | Action::ExpandAll
                | Action::CollapseAll
                | Action::GroupByDirectory
                | Action::Commit
        );
        match action {
            Action::Row(id) => {
                let Some(index) = self.tree.find(&id) else {
                    return;
                };
                if self.tree.nodes[index].is_file() {
                    self.toggle_node(index);
                } else {
                    if !self.collapsed.remove(&id) {
                        self.collapsed.insert(id.clone());
                    }
                    self.tree_generation = self.tree_generation.wrapping_add(1);
                }
                if !self.compact {
                    self.focus = Some(id);
                }
            }
            Action::Check(id) => {
                if let Some(index) = self.tree.find(&id) {
                    self.toggle_node(index);
                    if !self.compact {
                        self.focus = Some(id);
                    }
                }
            }
            Action::ExpandAll => {
                self.collapsed.clear();
                self.tree_generation = self.tree_generation.wrapping_add(1);
            }
            Action::CollapseAll => {
                self.collapsed = self
                    .tree
                    .nodes
                    .iter()
                    .filter(|node| !node.is_file())
                    .map(|node| node.id.clone())
                    .collect();
                self.tree_generation = self.tree_generation.wrapping_add(1);
            }
            Action::GroupByDirectory => {
                self.group_by_dir = !self.group_by_dir;
                self.tree = Tree::build(&self.selection.files, self.group_by_dir);
                self.tree_generation = self.tree_generation.wrapping_add(1);
                self.tree.selected_prefix(
                    &self.selection.files,
                    &self.selection.selected,
                    &mut self.selection_prefix,
                );
                let ids: HashSet<_> = self.tree.nodes.iter().map(|node| &node.id).collect();
                self.rows.retain(|id, _| ids.contains(id));
                self.reveal_focus = true;
            }
            Action::Refresh => {
                self.uncertain = false;
                self.reload_status = true;
                self.commit_failure = None;
                self.refresh_requested = true;
                if self.available(ui) {
                    self.observe(ui);
                }
                self.feedback = None;
            }
            Action::Commit if self.can_commit(ui) => {
                self.feedback = None;
                self.commit = Some(wire::send(
                    &ui.core,
                    &self.target.server_id,
                    "checkout.commit_selected.request",
                    json!({
                        "cwd": self.target.cwd, "paths": self.selection.paths(), "message": self.message,
                    }),
                ));
            }
            Action::Commit | Action::Splitter | Action::Failure => {}
        }
    }

    /// Desktop tree keys: arrows move, Left/Right collapse/expand or walk to
    /// the parent/first child, Space toggles inclusion.
    fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if self.compact || !self.form_visible || key.modifiers.is_primary() {
            return false;
        }
        let visible = self.tree.visible(&self.collapsed);
        if visible.is_empty() {
            return false;
        }
        let last = visible.len() - 1;
        let current = self.focus.as_ref().and_then(|id| {
            visible
                .iter()
                .position(|&index| &self.tree.nodes[index].id == id)
        });
        let next = match key.key_code {
            KeyCode::ArrowUp => current.map_or(0, |at| at.saturating_sub(1)),
            KeyCode::ArrowDown => current.map_or(0, |at| (at + 1).min(last)),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::ArrowLeft => {
                let Some(at) = current else { return false };
                let node = &self.tree.nodes[visible[at]];
                if !node.is_file() && !self.collapsed.contains(&node.id) {
                    self.collapsed.insert(node.id.clone());
                    self.tree_generation = self.tree_generation.wrapping_add(1);
                    at
                } else {
                    self.tree
                        .parent(visible[at])
                        .and_then(|parent| visible.iter().position(|&index| index == parent))
                        .unwrap_or(at)
                }
            }
            KeyCode::ArrowRight => {
                let Some(at) = current else { return false };
                let node = &self.tree.nodes[visible[at]];
                // Files stay; a collapsed node expands; an expanded one steps in.
                if node.is_file() {
                    at
                } else if self.collapsed.remove(&node.id) {
                    self.tree_generation = self.tree_generation.wrapping_add(1);
                    at
                } else {
                    (at + 1).min(last)
                }
            }
            KeyCode::Space => {
                let Some(at) = current else { return false };
                if self.commit.is_none() {
                    self.toggle_node(visible[at]);
                }
                at
            }
            _ => return false,
        };
        self.focus = Some(self.tree.nodes[visible[next]].id.clone());
        self.reveal_focus = true;
        self.rows_dirty = true;
        true
    }

    pub fn handle_input(&mut self, cx: &mut Cx, event: &Event, input: &mut TextInput) -> bool {
        if !self.form_visible {
            // The form is not drawn: its editor must not keep the keyboard.
            if cx.has_key_focus(input.area()) {
                cx.revert_key_focus();
            }
            return false;
        }
        input.set_is_read_only(cx, self.commit.is_some());
        let uid = input.widget_uid();
        let actions = cx.capture_actions(|cx| input.handle_event(cx, event, &mut Scope::empty()));
        let focused = cx.has_key_focus(input.area());
        let focus_changed = self.editor_focused != focused;
        self.editor_focused = focused;
        for action in &actions {
            if let Some(action) = action.as_widget_action()
                && action.widget_uid == uid
                && let Some(TextInputAction::Changed(value)) =
                    action.action.downcast_ref::<TextInputAction>()
            {
                self.message.clone_from(value);
                self.message_generation = self.message_generation.wrapping_add(1);
                self.feedback = None;
                return true;
            }
        }
        focus_changed
    }

    fn tooltip(&self, action: &Action) -> Option<String> {
        match action {
            Action::Refresh => Some(t("workspace.git.commit.refresh")),
            Action::ExpandAll => Some(t("workspace.git.commit.expandAll")),
            Action::CollapseAll => Some(t("workspace.git.commit.collapseAll")),
            Action::GroupByDirectory => Some(t("workspace.git.commit.groupByDirectory")),
            Action::Failure => self.commit_failure.clone(),
            Action::Row(NodeId::File(path)) => {
                let file = self
                    .selection
                    .files
                    .iter()
                    .find(|file| text(file, "path") == path)?;
                let status = t(status_key(file_status(file)));
                Some(match text(file, "oldPath") {
                    "" => format!("{status} · {path}"),
                    old => format!(
                        "{status} · {path}\n{}",
                        tr(
                            "workspace.git.commit.renamedFrom",
                            &[("path", old.to_owned())]
                        )
                    ),
                })
            }
            _ => None,
        }
    }

    pub fn handle_pointer(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        area: Area,
        ui: &UiContext,
    ) -> bool {
        let hit = event.hits(cx, area);
        if let Some((origin, start)) = self.drag {
            match &hit {
                Hit::FingerMove(e) => {
                    self.form_height = Some(start + origin - e.abs.y);
                    return true;
                }
                Hit::FingerUp(_) => {
                    self.drag = None;
                    return true;
                }
                _ => {}
            }
        }
        if let Some(delta) = self.touch_scroll.handle_event(cx, event, &hit) {
            self.rows_dirty = true;
            self.pressed = None;
            if self.list.contains(self.touch_scroll.origin()) {
                return self
                    .touch_scroll
                    .apply_delta(&mut self.offset, delta, self.max_scroll);
            }
            self.touch_scroll.reset();
        }
        match hit {
            Hit::FingerScroll(e) if self.list.contains(e.abs) => {
                self.rows_dirty = true;
                return self.touch_scroll.apply_delta(
                    &mut self.offset,
                    e.scroll.y,
                    self.max_scroll,
                );
            }
            Hit::FingerDown(e) => {
                tooltip_at(ui, None, Rect::default(), TOOLTIP_WAIT);
                self.pressed = self.hits.find(e.abs).map(|(_, action)| action);
                self.rows_dirty |= matches!(
                    self.pressed.as_ref(),
                    Some(Action::Row(_) | Action::Check(_))
                );
                if self.pressed == Some(Action::Splitter) {
                    self.pressed = None;
                    self.drag = Some((e.abs.y, self.shown_form_height));
                    return true;
                }
                // The tree takes the keyboard for arrow/space navigation.
                if !self.compact && self.list.contains(e.abs) {
                    cx.set_key_focus(area);
                }
                return self.pressed.is_some();
            }
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let (anchor, next) = self
                    .hits
                    .find(e.abs)
                    .map(|(anchor, action)| (anchor, Some(action)))
                    .unwrap_or_default();
                cx.set_cursor(if next == Some(Action::Splitter) || self.drag.is_some() {
                    MouseCursor::RowResize
                } else {
                    MouseCursor::Default
                });
                if next != self.hovered {
                    let tip = next.as_ref().and_then(|action| self.tooltip(action));
                    tooltip_at(ui, tip.as_deref(), anchor, TOOLTIP_WAIT);
                    self.rows_dirty |=
                        matches!(
                            self.hovered.as_ref(),
                            Some(Action::Row(_) | Action::Check(_))
                        ) || matches!(next.as_ref(), Some(Action::Row(_) | Action::Check(_)));
                    self.hovered = next;
                    return true;
                }
            }
            Hit::FingerHoverOut(_) => {
                tooltip_at(ui, None, Rect::default(), TOOLTIP_WAIT);
                self.rows_dirty |= matches!(
                    self.hovered.as_ref(),
                    Some(Action::Row(_) | Action::Check(_))
                );
                return self.hovered.take().is_some();
            }
            Hit::FingerUp(e) => {
                let pressed = self.pressed.take();
                self.rows_dirty |=
                    matches!(pressed.as_ref(), Some(Action::Row(_) | Action::Check(_)));
                if e.is_over
                    && !e.has_long_press_occurred
                    && (!e.device.is_touch() || e.was_tap())
                    && let Some((_, action)) = self.hits.find(e.abs)
                    && pressed.as_ref() == Some(&action)
                {
                    self.activate(ui, action);
                    let display = self.display(ui);
                    self.display_changed |= display != self.presentation;
                    self.presentation = display;
                    return true;
                }
                return pressed.is_some();
            }
            Hit::KeyDown(key) => return self.handle_key(&key),
            _ => {}
        }
        false
    }

    pub fn hide_input(&mut self, cx: &mut Cx, input: &mut TextInput) {
        if cx.has_key_focus(input.area()) {
            cx.revert_key_focus();
        }
        self.touch_scroll.reset();
        self.drag = None;
    }

    pub fn draw(
        &mut self,
        cx: &mut Cx2d,
        ui: &UiContext,
        paint: &mut GitPaint,
        input: &mut TextInput,
        area: Rect,
        compact: bool,
    ) {
        self.hits.clear();
        self.compact = compact;
        let display = self.display(ui);
        self.presentation = display.clone();
        self.form_visible = display == Display::Files;
        if !self.form_visible {
            self.icons_pending = false;
            for row in self.rows.values_mut() {
                row.visible = false;
            }
        }
        let colors = palette();
        if self.begin_part(cx, paint, Part::Background, area) {
            paint.fill(cx, area, colors.surface0);
            self.end_part(cx, Part::Background);
        }
        let state = match &display {
            Display::Files | Display::Empty => None,
            Display::Disconnected => Some(StateCopy {
                icon: Some("wifi-off"),
                error: false,
                title: t("workspace.git.commit.states.disconnected.title"),
                body: t("workspace.git.commit.disconnected"),
                detail: String::new(),
                action: None,
            }),
            Display::Unsupported => Some(StateCopy {
                icon: Some("circle-arrow-up"),
                error: false,
                title: t("workspace.git.commit.states.unsupported.title"),
                body: t("workspace.git.commit.unsupported"),
                detail: String::new(),
                action: None,
            }),
            Display::Loading => Some(StateCopy {
                icon: None,
                error: false,
                title: t("workspace.git.commit.loading"),
                body: String::new(),
                detail: String::new(),
                action: None,
            }),
            Display::Failed(LoadFailure::NotRepository) => Some(StateCopy {
                icon: Some("folder-x"),
                error: false,
                title: t("workspace.git.commit.states.notRepository.title"),
                body: t("workspace.git.commit.states.notRepository.body"),
                detail: self.target.cwd.clone(),
                action: Some(t("workspace.git.commit.states.notRepository.action")),
            }),
            Display::Failed(LoadFailure::TooLarge) => Some(StateCopy {
                icon: Some("triangle-alert"),
                error: false,
                title: t("workspace.git.commit.states.tooLarge.title"),
                body: t("workspace.git.commit.states.tooLarge.body"),
                detail: String::new(),
                action: Some(t("workspace.git.commit.refresh")),
            }),
            Display::Failed(LoadFailure::Other(message)) => Some(StateCopy {
                icon: Some("circle-alert"),
                error: true,
                title: t("workspace.git.commit.states.failed.title"),
                body: String::new(),
                detail: message.clone(),
                action: Some(t("workspace.git.commit.states.failed.retry")),
            }),
        };
        if let Some(state) = state {
            self.list = Rect::default();
            self.draw_state(cx, paint, area, &state);
            return;
        }
        let toolbar_h = if compact { 44.0 } else { TOOLBAR_HEIGHT };
        let metrics = tree_row_metrics(cx, paint);
        self.row_height = metrics.height(tree_row_height(compact), 4.0, 16.0);
        let form_h = if self.form_visible {
            self.form_extent(cx, paint, area.size.y - toolbar_h, compact)
        } else {
            0.0
        };
        self.shown_form_height = form_h;
        let body = rect(
            area.pos.x,
            area.pos.y + toolbar_h,
            area.size.x,
            (area.size.y - toolbar_h - form_h).max(0.0),
        );
        if self.form_visible {
            if self.begin_part(cx, paint, Part::Tree, body) {
                self.draw_tree(cx, paint, body, metrics, compact);
                self.end_part(cx, Part::Tree);
            }
        } else {
            self.list = Rect::default();
            let (title, body_copy) = if self.feedback.is_some() {
                (
                    t("workspace.git.commit.success"),
                    t("workspace.git.commit.states.empty.committed"),
                )
            } else {
                (
                    t("workspace.git.commit.empty"),
                    t("workspace.git.commit.states.empty.body"),
                )
            };
            self.draw_state(
                cx,
                paint,
                body,
                &StateCopy {
                    icon: Some("circle-check"),
                    error: false,
                    title,
                    body: body_copy,
                    detail: String::new(),
                    action: None,
                },
            );
        }
        let bar = rect(area.pos.x, area.pos.y, area.size.x, toolbar_h);
        self.draw_toolbar(cx, ui, paint, bar, compact);
        if self.form_visible {
            let form = rect(area.pos.x, body.pos.y + body.size.y, area.size.x, form_h);
            self.draw_form(cx, ui, paint, input, form, compact);
        }
    }

    fn line(cx: &mut Cx2d, paint: &mut GitPaint, size: f64) -> f64 {
        paint.measure(cx, Font::Regular, size, "Ag").y.max(size)
    }

    fn commit_button_height(compact: bool) -> f64 {
        if compact {
            button_height(ButtonSize::Md, true).max(44.0)
        } else {
            button_height(ButtonSize::Md, false)
        }
    }

    fn banner_height(&self, compact: bool) -> f64 {
        if self.commit_failure.is_none() && !self.uncertain {
            0.0
        } else if compact {
            44.0 + 8.0
        } else {
            32.0 + 8.0
        }
    }

    /// The message form's height: at least label + three input lines + button;
    /// desktop follows the splitter, phones keep four lines pinned at the bottom.
    fn form_extent(
        &self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        available: f64,
        compact: bool,
    ) -> f64 {
        let line = Self::line(cx, paint, ui_px(13.0));
        let label = Self::line(cx, paint, ui_px(12.0));
        let minimum = 10.0
            + self.banner_height(compact)
            + label
            + 6.0
            + line * 3.0
            + 12.0
            + 10.0
            + Self::commit_button_height(compact)
            + 12.0;
        let available = available.max(0.0);
        if compact {
            return (minimum + line).min((available - self.row_height).max(minimum));
        }
        let wanted = self.form_height.unwrap_or(available * 0.4);
        wanted.clamp(minimum, (available - self.row_height * 3.0).max(minimum))
    }

    fn draw_state(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, state: &StateCopy) {
        let colors = palette();
        let center_x = area.pos.x + area.size.x / 2.0;
        let inner = (area.size.x - 48.0).clamp(0.0, 440.0);
        if inner <= 0.0 || area.size.y <= 0.0 {
            return;
        }
        let (title_px, body_px, detail_px) = (ui_px(15.0), ui_px(13.0), ui_px(12.0));
        let title_w = paint
            .measure(cx, Font::Medium, title_px, &state.title)
            .x
            .min(inner);
        let body_w = paint
            .measure(cx, Font::Regular, body_px, &state.body)
            .x
            .min(inner);
        let detail_w = paint
            .measure(cx, Font::Regular, detail_px, &state.detail)
            .x
            .min(inner);
        let title_h =
            paint.measure_wrapped(cx, Font::Medium, title_px, &state.title, title_w + 1.0);
        let body_h = paint.measure_wrapped(cx, Font::Regular, body_px, &state.body, body_w + 1.0);
        let detail_h =
            paint.measure_wrapped(cx, Font::Regular, detail_px, &state.detail, detail_w + 1.0);
        let button_h = if self.compact {
            button_height(ButtonSize::Sm, true).max(44.0)
        } else {
            button_height(ButtonSize::Sm, false)
        };
        let total = 28.0
            + 14.0
            + title_h
            + if body_h > 0.0 { 6.0 + body_h } else { 0.0 }
            + if detail_h > 0.0 { 6.0 + detail_h } else { 0.0 }
            + if state.action.is_some() {
                16.0 + button_h
            } else {
                0.0
            };
        let mut y = area.pos.y + ((area.size.y - total) * 0.4).max(16.0);
        let top = y;
        if self.begin_part(cx, paint, Part::State, area) {
            match state.icon {
                Some(icon) => paint.icon(
                    cx,
                    icon,
                    28.0,
                    if state.error {
                        colors.danger
                    } else {
                        colors.muted
                    },
                    dvec2(center_x, y + 14.0),
                ),
                None => {
                    paint.spinner(cx, dvec2(center_x, y + 14.0), 24.0, colors.muted);
                }
            }
            y += 28.0 + 14.0;
            paint.text_wrapped(
                cx,
                Font::Medium,
                title_px,
                colors.foreground,
                dvec2(center_x - title_w / 2.0, y),
                &state.title,
                title_w + 1.0,
            );
            y += title_h;
            if body_h > 0.0 {
                y += 6.0;
                paint.text_wrapped(
                    cx,
                    Font::Regular,
                    body_px,
                    colors.muted,
                    dvec2(center_x - body_w / 2.0, y),
                    &state.body,
                    body_w + 1.0,
                );
                y += body_h;
            }
            if detail_h > 0.0 {
                y += 6.0;
                paint.text_wrapped(
                    cx,
                    Font::Regular,
                    detail_px,
                    if state.error {
                        colors.danger
                    } else {
                        colors.extra_muted
                    },
                    dvec2(center_x - detail_w / 2.0, y),
                    &state.detail,
                    detail_w + 1.0,
                );
            }
            self.end_part(cx, Part::State);
        }
        y = top + total
            - if state.action.is_some() {
                16.0 + button_h
            } else {
                0.0
            };
        if let Some(label) = &state.action {
            y += 16.0;
            let button_w = measure_button(cx, paint, label, true, ButtonSize::Sm, self.compact)
                .min(inner)
                .max(if self.compact { 120.0 } else { 0.0 });
            let frame = rect(center_x - button_w / 2.0, y, button_w, button_h);
            if frame.pos.y + frame.size.y <= area.pos.y + area.size.y
                && self.begin_part(cx, paint, Part::StateAction, frame)
            {
                let enabled = self.commit.is_none();
                button(
                    cx,
                    paint,
                    frame,
                    label,
                    Some("refresh-cw"),
                    ButtonVariant::Secondary,
                    ButtonSize::Sm,
                    self.compact,
                    enabled,
                    self.hovered == Some(Action::Refresh),
                );
                if enabled {
                    self.hits.push(frame, Action::Refresh);
                }
                self.end_part(cx, Part::StateAction);
            }
        }
    }

    fn reloading(&self) -> bool {
        self.observation.is_some() && !self.loaded
    }

    fn draw_toolbar(
        &mut self,
        cx: &mut Cx2d,
        ui: &UiContext,
        paint: &mut GitPaint,
        bar: Rect,
        compact: bool,
    ) {
        let colors = palette();
        if self.begin_part(cx, paint, Part::Toolbar, bar) {
            toolbar_band(cx, paint, bar, false);
            self.end_part(cx, Part::Toolbar);
        }
        let center_y = bar.pos.y + bar.size.y / 2.0;
        let enabled = self.commit.is_none() && self.available(ui);
        let actions = [
            (Action::Refresh, "refresh-cw", false),
            (Action::CollapseAll, "chevrons-down-up", false),
            (Action::ExpandAll, "chevrons-up-down", false),
            (Action::GroupByDirectory, "folder-tree", self.group_by_dir),
        ];
        // Touch hits are 44px squares whose centres sit 44px apart.
        let pitch = if compact { 44.0 } else { 26.0 };
        let mut right = bar.pos.x + bar.size.x - if compact { 6.0 } else { 8.0 };
        for (action, icon, selected) in
            actions
                .into_iter()
                .take(if self.form_visible { 4 } else { 1 })
        {
            let spinner = action == Action::Refresh && self.reloading();
            let center_x = right - pitch / 2.0;
            let button_right = center_x + toolbar_button_size(compact) / 2.0;
            let size = toolbar_button_size(compact);
            let bounds = rect(button_right - size, center_y - size / 2.0, size, size);
            let key = self.toolbar_key(&action);
            if self
                .toolbar_actions
                .entry(action.clone())
                .or_default()
                .begin(cx, bounds, key, &mut self.hits, paint)
            {
                let frame = toolbar_button(
                    cx,
                    paint,
                    compact,
                    button_right,
                    center_y,
                    (!spinner).then_some(icon),
                    selected,
                    enabled,
                    self.hovered.as_ref() == Some(&action),
                );
                if spinner {
                    paint.spinner(cx, frame.center(), 14.0, colors.muted);
                }
                if enabled {
                    self.hits.push(toolbar_hit(frame, compact), action.clone());
                }
                self.toolbar_actions
                    .get_mut(&action)
                    .unwrap()
                    .end(cx, &self.hits);
            }
            right -= pitch;
        }
        if !self.begin_part(cx, paint, Part::Summary, bar) {
            return;
        }
        let mut x = bar.pos.x + 12.0;
        let limit = right - 8.0;
        let size = ui_px(13.0);
        if let Some(branch) = self.branch.clone()
            && limit - x > 40.0
        {
            paint.icon(
                cx,
                "git-branch",
                14.0,
                colors.muted,
                dvec2(x + 7.0, center_y),
            );
            x += 20.0;
            let max = if self.form_visible {
                ((limit - x) * 0.55).max(40.0)
            } else {
                limit - x
            };
            let drawn = paint.text_centered(
                cx,
                Font::Medium,
                size,
                colors.foreground,
                x,
                bar.pos.y,
                bar.size.y,
                &branch,
                max,
            );
            x += drawn + 12.0;
        }
        if self.form_visible && limit - x > 24.0 {
            let summary = tr(
                "workspace.git.commit.selected",
                &[("count", self.selection.selected.len().to_string())],
            );
            paint.text_centered(
                cx,
                Font::Regular,
                size,
                colors.muted,
                x,
                bar.pos.y,
                bar.size.y,
                &summary,
                limit - x,
            );
        }
        self.end_part(cx, Part::Summary);
    }

    fn draw_tree(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        list: Rect,
        metrics: crate::common::widgets::RowMetrics,
        compact: bool,
    ) {
        let colors = palette();
        self.list = list;
        self.icons_pending = false;
        for row in self.rows.values_mut() {
            row.visible = false;
        }
        let row_h = self.row_height;
        let top_pad = 4.0;
        if self.visible_generation != Some(self.tree_generation) {
            self.visible_nodes = self.tree.visible(&self.collapsed);
            self.visible_generation = Some(self.tree_generation);
        }
        let visible = &self.visible_nodes;
        // A collapsed ancestor takes over the cursor of a hidden node.
        if self.focus.is_some() {
            match self.focus.as_ref().and_then(|id| self.tree.find(id)) {
                None => self.focus = None,
                Some(mut index) => {
                    while !visible.contains(&index) {
                        match self.tree.parent(index) {
                            Some(parent) => index = parent,
                            None => break,
                        }
                    }
                    if self.focus.as_ref() != Some(&self.tree.nodes[index].id) {
                        self.focus = Some(self.tree.nodes[index].id.clone());
                    }
                }
            }
        }
        let focus_pos = self.focus.as_ref().and_then(|id| {
            visible
                .iter()
                .position(|&index| &self.tree.nodes[index].id == id)
        });
        self.max_scroll = (visible.len() as f64 * row_h + top_pad * 2.0 - list.size.y).max(0.0);
        if std::mem::take(&mut self.reveal_focus)
            && let Some(pos) = focus_pos
        {
            let top = top_pad + pos as f64 * row_h;
            if top < self.offset {
                self.offset = top;
            } else if top + row_h > self.offset + list.size.y {
                self.offset = top + row_h - list.size.y;
            }
        }
        self.offset = self.offset.clamp(0.0, self.max_scroll);
        let prefix = &self.selection_prefix;
        let interactive = self.commit.is_none();
        let indent = if compact { 16.0 } else { 14.0 };
        let font = ui_px(14.0);
        let first = ((self.offset - top_pad) / row_h).floor().max(0.0) as usize;
        paint.begin_clip(cx, list);
        for (pos, &index) in visible.iter().enumerate().skip(first) {
            let band = rect(
                list.pos.x,
                list.pos.y + top_pad + pos as f64 * row_h - self.offset,
                list.size.x,
                row_h,
            );
            if band.pos.y > list.pos.y + list.size.y {
                break;
            }
            let node = &self.tree.nodes[index];
            let center_y = band.pos.y + band.size.y / 2.0;
            let x0 = band.pos.x + 6.0 + node.depth as f64 * indent;
            let key = self.row_key(index, prefix[node.files.end] - prefix[node.files.start]);
            if !self.rows.contains_key(&node.id) {
                self.rows.insert(node.id.clone(), RowPaint::default());
            }
            let retained = self.rows.get_mut(&node.id).unwrap();
            retained.visible = true;
            if retained.row.begin(cx, band, key, &mut self.hits, paint) {
                let row_action = Action::Row(node.id.clone());
                let check_action = Action::Check(node.id.clone());
                if !compact && focus_pos == Some(pos) {
                    paint.fill(cx, band, colors.selected);
                }
                if self.hovered.as_ref() == Some(&row_action)
                    || self.hovered.as_ref() == Some(&check_action)
                    || self.pressed.as_ref() == Some(&row_action)
                {
                    paint.fill(cx, band, colors.interaction_highlight);
                }
                if !node.is_file() {
                    let chevron = if self.collapsed.contains(&node.id) {
                        "chevron-right"
                    } else {
                        "chevron-down"
                    };
                    paint.icon(cx, chevron, 14.0, colors.muted, dvec2(x0 + 8.0, center_y));
                }
                let check_x = x0 + 16.0 + 4.0 + 8.0;
                let range = &node.files;
                let check = Check::of(prefix[range.end] - prefix[range.start], range.len());
                let (glyph, glyph_color) = match check {
                    Check::Checked => ("square-check", colors.foreground),
                    Check::Partial => ("square-minus", colors.foreground),
                    Check::Unchecked => ("square", colors.muted),
                };
                paint.icon(cx, glyph, 16.0, glyph_color, dvec2(check_x, center_y));
                let mut leading = 16.0 + 4.0 + 16.0;
                let spec = TextSpec::new(Font::Regular, font, colors.foreground);
                let muted = TextSpec::new(Font::Regular, font, colors.muted);
                let (title, title_spec, detail, trailing) = match &node.kind {
                    NodeKind::Bucket(bucket) => (
                        t(match bucket {
                            Bucket::Changes => "workspace.git.commit.changes",
                            Bucket::Unversioned => "workspace.git.commit.unversioned",
                        }),
                        TextSpec::new(Font::Medium, font, colors.foreground),
                        file_count(range.len()),
                        None,
                    ),
                    NodeKind::Dir { label } => {
                        let open = !self.collapsed.contains(&node.id);
                        paint.icon(
                            cx,
                            if open { "folder-open" } else { "folder" },
                            15.0,
                            colors.muted,
                            dvec2(x0 + leading + 6.0 + 8.0, center_y),
                        );
                        leading += 6.0 + 16.0;
                        (label.clone(), spec, file_count(range.len()), None)
                    }
                    NodeKind::File { index: file } => {
                        let file = &self.selection.files[*file];
                        let path = text(file, "path");
                        let status = file_status(file);
                        leading += 6.0 + 16.0;
                        let detail = rename_hint(file).unwrap_or_else(|| {
                            if self.group_by_dir {
                                String::new()
                            } else {
                                parent_dir(path).to_owned()
                            }
                        });
                        (
                            file_name(path).to_owned(),
                            TextSpec::new(Font::Regular, font, status_color(status)),
                            detail,
                            Some(status),
                        )
                    }
                };
                // The trailing column keeps clear of the scrollbar gutter whether or
                // not it shows; names and hints ellipsize before the status letter.
                let content = rect(
                    x0,
                    band.pos.y,
                    (band.pos.x + band.size.x - SCROLLBAR_GUTTER - x0).max(0.0),
                    band.size.y,
                );
                let trailing_w = if trailing.is_some() { 18.0 } else { 0.0 };
                let slots = metrics.layout(content, 0.0, leading, trailing_w, 8.0);
                let drawn = paint
                    .painter
                    .text_in(cx, slots.title, title_spec, &title, 0.0);
                let detail_x = drawn.pos.x + drawn.size.x + 8.0;
                let detail_w = slots.title.pos.x + slots.title.size.x - detail_x;
                if !detail.is_empty() && detail_w > 24.0 {
                    paint.painter.text_in(
                        cx,
                        rect(detail_x, slots.title.pos.y, detail_w, slots.title.size.y),
                        muted,
                        &detail,
                        0.0,
                    );
                }
                if let Some(status) = trailing {
                    paint.painter.text_in(
                        cx,
                        slots.trailing,
                        TextSpec::new(Font::Medium, ui_px(12.0), status_color(status)),
                        status_letter(status),
                        0.5,
                    );
                }
                if interactive {
                    self.hits.push_clipped(band, list, row_action);
                    let half = if compact { 22.0 } else { 12.0 };
                    self.hits.push_clipped(
                        rect(check_x - half, band.pos.y, half * 2.0, band.size.y),
                        list,
                        check_action,
                    );
                }
                retained.row.end(cx, &self.hits);
            }
            if let NodeKind::File { index } = &node.kind {
                let path = text(&self.selection.files[*index], "path");
                let frame = rect(x0 + 16.0 + 4.0 + 16.0 + 6.0, center_y - 8.0, 16.0, 16.0);
                let visible = frame.pos.x < list.pos.x + list.size.x
                    && frame.pos.x + frame.size.x > list.pos.x
                    && frame.pos.y < list.pos.y + list.size.y
                    && frame.pos.y + frame.size.y > list.pos.y;
                let key = inputs((
                    file_name(path),
                    self.offset.to_bits(),
                    list.size.x.to_bits(),
                    list.size.y.to_bits(),
                ));
                if visible && retained.icon.begin(cx, frame, key, &mut self.hits, paint) {
                    retained.pending = !paint.file_icon(cx, frame, file_name(path));
                    retained.icon.end(cx, &self.hits);
                }
                retained.pending &= visible;
                self.icons_pending |= retained.pending;
            }
        }
        if self.begin_part(cx, paint, Part::Scrollbar, list) {
            if self.max_scroll > 0.0 {
                let content = self.max_scroll + list.size.y;
                let thumb_h = (list.size.y * list.size.y / content).max(24.0);
                let thumb_y =
                    list.pos.y + (list.size.y - thumb_h) * (self.offset / self.max_scroll);
                paint.rounded(
                    cx,
                    rect(list.pos.x + list.size.x - 5.0, thumb_y, 3.0, thumb_h),
                    with_alpha(colors.muted, 0.35),
                    1.5,
                    None,
                );
            }
            self.end_part(cx, Part::Scrollbar);
        }
        paint.end_clip(cx);
    }

    fn draw_form(
        &mut self,
        cx: &mut Cx2d,
        ui: &UiContext,
        paint: &mut GitPaint,
        input: &mut TextInput,
        form: Rect,
        compact: bool,
    ) {
        let colors = palette();
        let splitter = rect(form.pos.x, form.pos.y - 3.0, form.size.x, 7.0);
        if self.begin_part(cx, paint, Part::Splitter, splitter) {
            if !compact && (self.drag.is_some() || self.hovered == Some(Action::Splitter)) {
                paint.fill(
                    cx,
                    rect(form.pos.x, form.pos.y - 1.0, form.size.x, 2.0),
                    colors.border_accent,
                );
            } else {
                paint.fill(
                    cx,
                    rect(form.pos.x, form.pos.y, form.size.x, 1.0),
                    colors.border,
                );
            }
            if !compact {
                self.hits.push(splitter, Action::Splitter);
            }
            self.end_part(cx, Part::Splitter);
        }
        let pad = 12.0;
        let left = form.pos.x + pad;
        let width = (form.size.x - pad * 2.0).max(1.0);
        let mut y = form.pos.y + 10.0;
        let banner_h = self.banner_height(compact);
        if banner_h > 0.0 {
            let banner = rect(left, y, width, banner_h - 8.0);
            let center_y = banner.pos.y + banner.size.y / 2.0;
            let label = t("workspace.git.commit.refresh");
            let button_h = button_height(ButtonSize::Xs, compact);
            let button_w = measure_button(cx, paint, &label, false, ButtonSize::Xs, compact);
            let frame = rect(
                banner.pos.x + banner.size.x - button_w - 6.0,
                center_y - button_h / 2.0,
                button_w,
                button_h,
            );
            if self.begin_part(cx, paint, Part::Banner, banner) {
                paint.rounded(cx, banner, with_alpha(colors.danger, 0.12), 6.0, None);
                self.hits.push(banner, Action::Failure);
                paint.icon(
                    cx,
                    "circle-alert",
                    14.0,
                    colors.danger,
                    dvec2(banner.pos.x + 16.0, center_y),
                );
                let copy = if self.uncertain {
                    t("workspace.git.commit.commitUncertain")
                } else {
                    t("workspace.git.commit.commitFailed")
                };
                let text_x = banner.pos.x + 30.0;
                let text_w = (frame.pos.x - 8.0 - text_x).max(0.0);
                let drawn = paint.text_centered(
                    cx,
                    Font::Medium,
                    ui_px(13.0),
                    colors.danger,
                    text_x,
                    banner.pos.y,
                    banner.size.y,
                    &copy,
                    text_w,
                );
                if let Some(error) = self.commit_failure.as_deref()
                    && !self.uncertain
                    && text_w - drawn > 32.0
                {
                    paint.text_centered(
                        cx,
                        Font::Regular,
                        ui_px(13.0),
                        colors.muted,
                        text_x + drawn + 8.0,
                        banner.pos.y,
                        banner.size.y,
                        error,
                        text_w - drawn - 8.0,
                    );
                }
                self.end_part(cx, Part::Banner);
            }
            if self.begin_part(cx, paint, Part::BannerAction, frame) {
                button(
                    cx,
                    paint,
                    frame,
                    &label,
                    None,
                    ButtonVariant::Secondary,
                    ButtonSize::Xs,
                    compact,
                    true,
                    self.hovered == Some(Action::Refresh),
                );
                let hit = if compact {
                    rect(frame.pos.x, banner.pos.y, frame.size.x, banner.size.y)
                } else {
                    frame
                };
                self.hits.push(hit, Action::Refresh);
                self.end_part(cx, Part::BannerAction);
            }
            y += banner_h;
        }
        let label_px = ui_px(12.0);
        let label = t("workspace.git.commit.message");
        let enabled = self.can_commit(ui);
        let (hint, hint_color) = if let Some(feedback) = &self.feedback {
            (feedback.clone(), colors.success)
        } else if self.commit.is_some()
            || enabled
            || self.commit_failure.is_some()
            || self.uncertain
        {
            (String::new(), colors.muted)
        } else if self.selection.selected.is_empty() {
            (t("workspace.git.commit.hints.selectFiles"), colors.muted)
        } else if self.message.trim().is_empty() {
            (t("workspace.git.commit.hints.message"), colors.muted)
        } else {
            (String::new(), colors.muted)
        };
        // Fit the fallback-font heights before positioning the editor.
        let label_h = Self::line(cx, paint, label_px)
            .max(paint.measure(cx, Font::Medium, label_px, &label).y)
            .max(paint.measure(cx, Font::Regular, label_px, &hint).y);
        if self.begin_part(cx, paint, Part::Hint, rect(left, y, width, label_h)) {
            let label_w = paint.text_centered(
                cx,
                Font::Medium,
                label_px,
                colors.muted,
                left,
                y,
                label_h,
                &label,
                width * 0.5,
            );
            let hint_x = left + label_w + 12.0;
            if !hint.is_empty() && left + width - hint_x > 24.0 {
                paint.painter.text_in(
                    cx,
                    rect(hint_x, y, left + width - hint_x, label_h),
                    TextSpec::new(Font::Regular, label_px, hint_color),
                    &hint,
                    1.0,
                );
            }
            self.end_part(cx, Part::Hint);
        }
        y += label_h + 6.0;
        let button_h = Self::commit_button_height(compact);
        let action_y = form.pos.y + form.size.y - 12.0 - button_h;
        let line = Self::line(cx, paint, ui_px(13.0));
        let input_frame = rect(left, y, width, (action_y - 10.0 - y).max(line + 12.0));
        self.editor_focused = cx.has_key_focus(input.area());
        if self.begin_part(cx, paint, Part::Editor, input_frame) {
            paint.rounded(
                cx,
                input_frame,
                colors.surface1,
                6.0,
                Some((
                    if self.editor_focused {
                        colors.border_accent
                    } else {
                        colors.border
                    },
                    1.0,
                )),
            );
            if !self.input_initialized || std::mem::take(&mut self.clear_input) {
                input.set_text(cx, &self.message);
                input.set_is_multiline(cx, true);
                self.input_initialized = true;
            }
            let font_size = crate::theme::Typography::pt(ui_px(13.0));
            let foreground = colors.foreground;
            let muted = colors.muted;
            input.apply_over(cx, live! { draw_text: { color: (foreground), color_empty: (muted), text_style: { font_size: (font_size) } } draw_cursor: { color: (foreground) } });
            input.set_empty_text(cx, t("workspace.git.commit.messagePlaceholder"));
            input.set_is_read_only(cx, self.commit.is_some());
            let _ = input.draw_walk(
                cx,
                &mut Scope::empty(),
                Walk::abs_rect(rect(
                    input_frame.pos.x + 8.0,
                    input_frame.pos.y + 6.0,
                    (input_frame.size.x - 16.0).max(1.0),
                    (input_frame.size.y - 12.0).max(1.0),
                )),
            );
            self.end_part(cx, Part::Editor);
        }
        let count = self.selection.selected.len();
        let label = if self.commit.is_some() {
            t("workspace.git.commit.committing")
        } else if count > 0 {
            tr(
                "workspace.git.commit.submitCount",
                &[("count", count.to_string())],
            )
        } else {
            t("workspace.git.commit.submit")
        };
        let button_w = if compact {
            width
        } else {
            measure_primary(cx, paint, &label).max(120.0).min(width)
        };
        let commit_button = rect(left, action_y, button_w, button_h);
        if self.begin_part(cx, paint, Part::Submit, commit_button) {
            primary_button(
                cx,
                paint,
                commit_button,
                &label,
                "git-commit-horizontal",
                enabled,
                self.hovered == Some(Action::Commit),
                self.pressed == Some(Action::Commit),
            );
            if enabled {
                self.hits.push(commit_button, Action::Commit);
            }
            self.end_part(cx, Part::Submit);
        }
    }
}

/// The app's primary action look (`common::widgets::PButton` Primary, as on New
/// session and the composer): accent pill with `on_tertiary` content, 0.85 while
/// pressed and 0.5 disabled. Git chrome's `Primary` keeps the original pane's
/// foreground fill for the other git surfaces; Commit is the screen's main action.
const PRIMARY_PAD: f64 = 16.0;
const PRIMARY_ICON: f64 = 16.0;

fn measure_primary(cx: &mut Cx2d, paint: &mut GitPaint, label: &str) -> f64 {
    let text = paint.measure(cx, Font::Regular, ui_px(14.0), label).x;
    PRIMARY_PAD * 2.0 + PRIMARY_ICON + 8.0 + text
}

#[allow(clippy::too_many_arguments)]
fn primary_button(
    cx: &mut Cx2d,
    paint: &mut GitPaint,
    area: Rect,
    label: &str,
    icon: &str,
    enabled: bool,
    hovered: bool,
    pressed: bool,
) {
    let colors = palette();
    let opacity = if !enabled {
        0.5
    } else if pressed {
        0.85
    } else {
        1.0
    };
    let radius = (area.size.y * 0.5).min(16.0);
    paint.rounded(cx, area, with_alpha(colors.accent, opacity), radius, None);
    if enabled && hovered {
        paint.rounded(cx, area, colors.interaction_highlight, radius, None);
    }
    let foreground = with_alpha(colors.on_tertiary, opacity);
    let size = ui_px(14.0);
    let text_w = paint.measure(cx, Font::Regular, size, label).x;
    let content = PRIMARY_ICON + 8.0 + text_w;
    let inset = ((area.size.x - content) * 0.5).max(PRIMARY_PAD.min(area.size.x * 0.5));
    let mut x = area.pos.x + inset;
    let center_y = area.pos.y + area.size.y * 0.5;
    let icon_center = dvec2(x + PRIMARY_ICON * 0.5, center_y);
    paint.icon(cx, icon, PRIMARY_ICON, foreground, icon_center);
    x += PRIMARY_ICON + 8.0;
    paint.text_centered(
        cx,
        Font::Regular,
        size,
        foreground,
        x,
        area.pos.y,
        area.size.y,
        label,
        (area.pos.x + area.size.x - PRIMARY_PAD - x).max(0.0),
    );
}

fn file_count(count: usize) -> String {
    tr(
        "workspace.git.commit.fileCount",
        &[("count", count.to_string())],
    )
}

fn status_key(status: Status) -> &'static str {
    match status {
        Status::Modified => "workspace.git.commit.status.modified",
        Status::Added => "workspace.git.commit.status.added",
        Status::Deleted => "workspace.git.commit.status.deleted",
        Status::Renamed => "workspace.git.commit.status.renamed",
        Status::Untracked => "workspace.git.commit.status.untracked",
    }
}

fn status_letter(status: Status) -> &'static str {
    match status {
        Status::Modified => "M",
        Status::Added => "A",
        Status::Deleted => "D",
        Status::Renamed => "R",
        Status::Untracked => "U",
    }
}

/// IntelliJ file-status colours from the palette: modified/renamed blue, added
/// green, deleted grey, unversioned brown-red. Light themes deepen the blue
/// toward the foreground to keep text contrast.
fn status_color(status: Status) -> Vec4 {
    let colors = palette();
    match status {
        Status::Modified | Status::Renamed => {
            if colors.is_dark {
                colors.running
            } else {
                let (a, b, t) = (colors.running, colors.foreground, 0.3);
                vec4(
                    a.x + (b.x - a.x) * t,
                    a.y + (b.y - a.y) * t,
                    a.z + (b.z - a.z) * t,
                    1.0,
                )
            }
        }
        Status::Added => colors.added,
        Status::Deleted => colors.muted,
        Status::Untracked => colors.danger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refreshed_files_preserve_exclusions_without_silently_selecting_new_changes() {
        let mut selection = Selection::default();
        selection.replace(vec![
            json!({"path":"keep.rs"}),
            json!({"path":"exclude.rs"}),
        ]);
        selection.selected.remove("exclude.rs");
        selection.replace(vec![
            json!({"path":"keep.rs"}),
            json!({"path":"exclude.rs"}),
            json!({"path":"new.rs"}),
        ]);
        assert_eq!(selection.paths(), vec!["keep.rs"]);
        selection.replace(vec![json!({"path":"exclude.rs"}), json!({"path":"new.rs"})]);
        assert!(selection.paths().is_empty());
    }

    fn labels(tree: &Tree, files: &[Value]) -> Vec<(usize, String)> {
        tree.nodes
            .iter()
            .map(|node| {
                let label = match &node.kind {
                    NodeKind::Bucket(Bucket::Changes) => "[changes]".to_owned(),
                    NodeKind::Bucket(Bucket::Unversioned) => "[unversioned]".to_owned(),
                    NodeKind::Dir { label } => format!("{label}/"),
                    NodeKind::File { index } => text(&files[*index], "path").to_owned(),
                };
                (node.depth, label)
            })
            .collect()
    }

    fn sample() -> Vec<Value> {
        vec![
            json!({"path":"README.md"}),
            json!({"path":"packages/gui/src/b.rs"}),
            json!({"path":"packages/daemon/x.rs", "isDeleted": true}),
            json!({"path":"packages/gui/src/a.rs", "isNew": true}),
            json!({"path":"notes/todo.txt", "isNew": true, "untracked": true}),
        ]
    }

    #[test]
    fn groups_by_directory_compacts_chains_and_buckets_untracked_files() {
        let files = sample();
        let tree = Tree::build(&files, true);
        assert_eq!(
            labels(&tree, &files),
            vec![
                (0, "[changes]".into()),
                (1, "packages/".into()),
                (2, "daemon/".into()),
                (3, "packages/daemon/x.rs".into()),
                (2, "gui/src/".into()),
                (3, "packages/gui/src/a.rs".into()),
                (3, "packages/gui/src/b.rs".into()),
                (1, "README.md".into()),
                (0, "[unversioned]".into()),
                (1, "notes/".into()),
                (2, "notes/todo.txt".into()),
            ]
        );
        // Each node's files are its subtree; the compacted node keeps the deepest path.
        let gui = tree
            .find(&NodeId::Dir(Bucket::Changes, "packages/gui/src".into()))
            .unwrap();
        assert_eq!(tree.nodes[gui].files.len(), 2);
        assert_eq!(tree.nodes[0].files.len(), 4);
        let unversioned = tree.find(&NodeId::Bucket(Bucket::Unversioned)).unwrap();
        assert_eq!(tree.nodes[unversioned].files.len(), 1);
        assert_eq!(file_status(&files[2]), Status::Deleted);
        assert_eq!(file_status(&files[3]), Status::Added);
        assert_eq!(file_status(&files[4]), Status::Untracked);
        assert_eq!(file_status(&files[0]), Status::Modified);
    }

    #[test]
    fn flat_mode_lists_files_by_path_under_each_root() {
        let files = sample();
        let tree = Tree::build(&files, false);
        assert_eq!(
            labels(&tree, &files),
            vec![
                (0, "[changes]".into()),
                (1, "README.md".into()),
                (1, "packages/daemon/x.rs".into()),
                (1, "packages/gui/src/a.rs".into()),
                (1, "packages/gui/src/b.rs".into()),
                (0, "[unversioned]".into()),
                (1, "notes/todo.txt".into()),
            ]
        );
        assert!(Tree::build(&[], true).nodes.is_empty());
    }

    #[test]
    fn tri_state_toggles_whole_subtrees_and_reports_partial() {
        let files = sample();
        let tree = Tree::build(&files, true);
        let mut selected = HashSet::new();
        let packages = tree
            .find(&NodeId::Dir(Bucket::Changes, "packages".into()))
            .unwrap();
        let gui = tree
            .find(&NodeId::Dir(Bucket::Changes, "packages/gui/src".into()))
            .unwrap();
        tree.toggle(gui, &files, &mut selected);
        assert_eq!(tree.check(gui, &files, &selected), Check::Checked);
        assert_eq!(tree.check(packages, &files, &selected), Check::Partial);
        assert_eq!(tree.check(0, &files, &selected), Check::Partial);
        // A partial node includes every descendant first, then excludes them all.
        tree.toggle(0, &files, &mut selected);
        assert_eq!(tree.check(0, &files, &selected), Check::Checked);
        assert_eq!(selected.len(), 4);
        assert!(!selected.contains("notes/todo.txt"));
        tree.toggle(packages, &files, &mut selected);
        assert_eq!(tree.check(packages, &files, &selected), Check::Unchecked);
        assert_eq!(
            selected.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["README.md"]
        );
        let mut prefix = Vec::new();
        tree.selected_prefix(&files, &selected, &mut prefix);
        let root = &tree.nodes[0].files;
        assert_eq!(prefix[root.end] - prefix[root.start], 1);
    }

    #[test]
    fn collapsed_nodes_hide_their_subtree_and_parents_resolve() {
        let files = sample();
        let tree = Tree::build(&files, true);
        let packages = NodeId::Dir(Bucket::Changes, "packages".into());
        let collapsed = HashSet::from([packages.clone()]);
        let visible: Vec<_> = tree
            .visible(&collapsed)
            .into_iter()
            .map(|index| labels(&tree, &files)[index].1.clone())
            .collect();
        assert_eq!(
            visible,
            vec![
                "[changes]",
                "packages/",
                "README.md",
                "[unversioned]",
                "notes/",
                "notes/todo.txt"
            ]
        );
        let a = tree
            .find(&NodeId::File("packages/gui/src/a.rs".into()))
            .unwrap();
        let gui = tree.parent(a).unwrap();
        assert_eq!(
            tree.nodes[gui].id,
            NodeId::Dir(Bucket::Changes, "packages/gui/src".into())
        );
        assert_eq!(tree.nodes[tree.parent(gui).unwrap()].id, packages);
        assert_eq!(tree.parent(0), None);
    }

    #[test]
    fn renames_group_by_new_path_and_show_old_to_new() {
        let files = vec![
            json!({"path":"src/new.rs", "oldPath":"src/old.rs"}),
            json!({"path":"lib/moved.rs", "oldPath":"src/moved.rs"}),
        ];
        assert_eq!(file_status(&files[0]), Status::Renamed);
        assert_eq!(rename_hint(&files[0]).as_deref(), Some("old.rs → new.rs"));
        assert_eq!(
            rename_hint(&files[1]).as_deref(),
            Some("src/moved.rs → lib/moved.rs")
        );
        let tree = Tree::build(&files, true);
        assert_eq!(
            labels(&tree, &files),
            vec![
                (0, "[changes]".into()),
                (1, "lib/".into()),
                (2, "lib/moved.rs".into()),
                (1, "src/".into()),
                (2, "src/new.rs".into()),
            ]
        );
    }

    #[test]
    fn diff_payload_failures_name_known_conditions() {
        assert_eq!(
            load_payload(&json!({
                "files": [],
                "error": {"code": "NOT_GIT_REPO", "message": "Not a git repository: /home/me"},
            })),
            Err(LoadFailure::NotRepository)
        );
        assert_eq!(
            load_payload(&json!({
                "files": [],
                "diffTooLarge": true,
                "error": {"code": "UNKNOWN", "message": "Diff too large to display"},
            })),
            Err(LoadFailure::TooLarge)
        );
        assert_eq!(
            load_payload(&json!({"files":[], "error":{"code":"UNKNOWN","message":"boom"}})),
            Err(LoadFailure::Other("boom".into()))
        );
        assert_eq!(
            load_payload(&json!({"files":[{"path":"a"}], "error": null})),
            Ok(vec![json!({"path":"a"})])
        );
        // Daemons that predate the typed code send only the message, which still
        // names the condition.
        let rejected = |code: &str, message: &str| -> Result<Value, CoreError> {
            Err(CoreError {
                code: code.into(),
                message: message.into(),
                outcome_unknown: false,
            })
        };
        assert_eq!(
            load_result(&rejected(
                "DAEMON_RPC_ERROR",
                "Not a git repository: /tmp/x"
            )),
            Err(LoadFailure::NotRepository)
        );
        assert_eq!(
            load_result(&rejected("NOT_GIT_REPO", "anything")),
            Err(LoadFailure::NotRepository)
        );
        assert_eq!(
            load_payload(&json!({"files":[], "error": "Not a git repository: /tmp/x"})),
            Err(LoadFailure::NotRepository)
        );
        assert_eq!(
            load_result(&rejected("DAEMON_RPC_ERROR", "fatal: bad object")),
            Err(LoadFailure::Other("fatal: bad object".into()))
        );
    }
}
