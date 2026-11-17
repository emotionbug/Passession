//! The Git Log tab (`git_log`, IntelliJ's Git Log tool window): a branches
//! panel (a drawer below 900px), the commit table with its lane graph and the
//! selected commit's changed files and message on desktop; on phones a log
//! screen and a commit screen, one at a time. State and requests live in
//! [`LogState`]; this file routes input and actions, `log_draw.rs` paints.

use super::log::{
    Availability, Detail, Filters, Goto, LogState, RefFilter, RefKind, is_hash_query,
};
use super::log_draw::{RefRow, local_time};
use super::model::now_ms;
use super::overlay::{MenuEntry, MenuItem, Overlay, OverlayHost, OverlayKind, OverlayResult};
use super::paint::{GitPaint, Hits, Retained, rect_key};
use super::store::GitStore;
use super::text::{t, tr};
use super::wire;
use crate::common::overlay::{SelectOption, tooltip_at};
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use makepad_widgets::Hit as Pointer;
use makepad_widgets::*;
use paseo_gui_core::{CoreEvent, RequestId};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Typing in the search field filters after this pause.
const SEARCH_DEBOUNCE: f64 = 0.3;
/// Keyboard selection loads the commit's details after this pause.
const DETAIL_DEBOUNCE: f64 = 0.15;
/// Preference holding the dragged split sizes.
const LAYOUT_PREF: &str = "gitLogLayout";

/// The panel's text fields the Git Log draws and routes.
pub struct LogInputs<'a> {
    pub search: &'a mut TextInput,
    pub branch: &'a mut TextInput,
}

/// One vertically scrolled region.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Scroller {
    pub offset: f64,
    pub area: Rect,
    pub content: f64,
}

impl Scroller {
    pub fn max(&self) -> f64 {
        (self.content - self.area.size.y).max(0.0)
    }

    /// The region drawn this frame: `content` tall inside `area`.
    pub fn set(&mut self, area: Rect, content: f64) {
        self.area = area;
        self.content = content;
        self.offset = self.offset.clamp(0.0, self.max());
    }

    fn scroll_by(&mut self, delta: f64) -> bool {
        let before = self.offset;
        self.offset = (self.offset + delta).clamp(0.0, self.max());
        before != self.offset
    }

    /// Scrolls the least so `top..top + height` (content coordinates) is visible.
    pub fn reveal(&mut self, top: f64, height: f64) {
        let view = self.area.size.y;
        if view <= 0.0 {
            return;
        }
        if top < self.offset {
            self.offset = top;
        } else if top + height > self.offset + view {
            self.offset = top + height - view;
        }
        self.offset = self.offset.clamp(0.0, self.max());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Region {
    List,
    Branches,
    Files,
    Info,
    Sheet,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Screen {
    Log,
    /// The phone commit screen of a SHA.
    Commit(String),
}

/// The list the keyboard moves through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    List,
    Files,
}

/// A draggable divider of the desktop layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Split {
    /// Branches panel | table.
    Branches,
    /// Table | details beside it.
    Side,
    /// Table above the details.
    Stacked,
}

/// Dragged split sizes (`None` = the layout's default), saved in [`LAYOUT_PREF`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct SplitSizes {
    pub branches: Option<f64>,
    pub side: Option<f64>,
    pub stacked: Option<f64>,
}

impl SplitSizes {
    const KEYS: [(Split, &'static str); 3] = [
        (Split::Branches, "branchesWidth"),
        (Split::Side, "detailsWidth"),
        (Split::Stacked, "detailsHeight"),
    ];

    fn read(value: Option<&Value>) -> Self {
        let mut sizes = Self::default();
        for (split, key) in Self::KEYS {
            let size = value
                .and_then(|v| v.get(key))
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v > 0.0);
            *sizes.slot(split) = size;
        }
        sizes
    }

    pub fn get(&self, split: Split) -> Option<f64> {
        match split {
            Split::Branches => self.branches,
            Split::Side => self.side,
            Split::Stacked => self.stacked,
        }
    }

    pub fn slot(&mut self, split: Split) -> &mut Option<f64> {
        match split {
            Split::Branches => &mut self.branches,
            Split::Side => &mut self.side,
            Split::Stacked => &mut self.stacked,
        }
    }

    /// `previous` with these sizes (unknown keys kept).
    fn merged(&self, previous: Option<&Value>) -> Value {
        let mut map = previous
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (split, key) in Self::KEYS {
            if let Some(size) = self.get(split) {
                map.insert(key.to_owned(), json!(size.round()));
            }
        }
        Value::Object(map)
    }
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    split: Split,
    /// Pointer coordinate (x, or y for [`Split::Stacked`]) at the press.
    origin: f64,
    start: f64,
}

#[derive(Clone, Debug, PartialEq, Hash)]
pub(super) enum LogHit {
    BranchesToggle,
    Refresh,
    BranchChip,
    AuthorChip,
    PathChip(String),
    ClearBranch,
    ClearAuthor,
    ClearPath,
    ClearFilters,
    ShowAllBranches,
    SearchToggle,
    SearchClear,
    BranchSearchClear,
    Section(String),
    Ref {
        kind: RefKind,
        name: String,
    },
    RefMenu {
        kind: RefKind,
        name: String,
    },
    Row(usize),
    /// The relative date of a row (absolute date tooltip).
    Date(usize),
    PageRetry,
    DetailRetry(String),
    File(String),
    CopyHash(String),
    Parent(String),
    Back,
    /// The branches drawer surface (blocks what lies under it).
    Drawer,
    /// Outside the open branches drawer: closes it.
    DrawerBackdrop,
    Splitter(Split),
}

#[derive(Debug)]
pub(super) enum Purpose {
    Branch,
    Author,
    RefMenu {
        kind: RefKind,
        name: String,
    },
    RowMenu(String),
    FileMenu {
        sha: String,
        path: String,
    },
    /// The checkout of this branch failed on uncommitted changes.
    StashAndSwitch(String),
}

/// The Checkout action's requests (the Changes branch switcher's stash flow).
enum Flow {
    Switch(String),
    StashSave(String),
    SwitchAfterStash(String),
}
/// Sibling paint boundaries. Layout still runs so scrollers and hit ordering stay current.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum LogPart {
    Background,
    Toolbar,
    Banner,
    BranchesChrome,
    RefRow(usize),
    TableHeader,
    Skeleton,
    CommitRow(usize),
    Footer,
    FilesHeader,
    FilesEmpty,
    FileRow(usize),
    Info,
    PhoneToolbar,
    Summary,
    PhoneCommitToolbar,
    PhoneInfo,
    PhoneFilesHeader,
    Splitter(Split),
    Divider(u8),
    Drawer,
    Message(u64),
}

#[derive(Default)]
struct LogLayer {
    paint: Retained<LogHit>,
    data: u64,
    interaction: u64,
    geometry: u64,
    visited: bool,
    spinning: bool,
    icons_pending: bool,
    before: (bool, bool),
}

impl LogLayer {
    fn interaction(&self, hovered: Option<&LogHit>, pressed: Option<&LogHit>) -> u64 {
        crate::common::layer::inputs((
            hovered.filter(|hit| self.paint.owns_hit(hit)),
            pressed.filter(|hit| self.paint.owns_hit(hit)),
        ))
    }
}

pub struct LogView {
    pub target: PanelTarget,
    pub state: LogState,
    pub(super) overlay: OverlayHost<Purpose>,
    flows: HashMap<RequestId, Flow>,
    pub(super) hits: Hits<LogHit>,
    pub(super) hovered: Option<LogHit>,
    pub(super) pressed: Option<LogHit>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    pub(super) list: Scroller,
    pub(super) branches: Scroller,
    pub(super) files: Scroller,
    pub(super) info: Scroller,
    pub(super) sheet: Scroller,
    /// Commit row height of the last draw (keyboard paging, re-anchoring).
    pub(super) row_height: f64,
    /// File row height and the files of `listed_for` (keyboard moves).
    pub(super) file_row_height: f64,
    pub(super) listed_files: Vec<String>,
    pub(super) listed_for: Option<String>,
    /// The wide branches panel is shown.
    pub(super) branches_open: bool,
    /// The narrow layout's branches drawer is open.
    pub(super) drawer_open: bool,
    /// The last desktop draw was wide (panel, not drawer).
    pub(super) wide: bool,
    pub(super) collapsed: HashSet<String>,
    pub(super) collapsed_version: u64,
    /// Branches-panel rows and the `(refs, query, collapsed)` versions they were built for.
    pub(super) ref_rows: Vec<RefRow>,
    pub(super) ref_rows_key: Option<(u64, String, u64)>,
    pub(super) branch_query: String,
    pub(super) search_text: String,
    search_timer: Timer,
    detail_timer: Timer,
    /// A keyboard selection waits for [`DETAIL_DEBOUNCE`] before loading details.
    pub(super) detail_waiting: bool,
    /// The phone top bar shows its search field.
    pub(super) search_open: bool,
    pub(super) focus_search: bool,
    /// Key focus moves to the tab at the next pointer/keyboard pass.
    want_focus: bool,
    was_visible: bool,
    pub(super) screen: Screen,
    pub(super) focus: Focus,
    pub(super) selected_file: Option<String>,
    /// The branch a Checkout is switching to (spinner; repeats ignored).
    pub(super) switching: Option<String>,
    /// A hash lookup cleared the text filter; select this once the list reloads.
    reveal_after_load: Option<(String, bool)>,
    /// The tab target changed; its filters apply at the next input pass.
    retargeted: bool,
    pub(super) sizes: SplitSizes,
    /// The split sizes of the last draw (drag origin).
    pub(super) shown_sizes: SplitSizes,
    drag: Option<Drag>,
    /// The last draw used the stacked phone layout.
    pub(super) phone: bool,
    /// Text field style last applied (theme colours, type size, locale).
    pub(super) input_style: String,
    pub spinning: bool,
    /// A material file icon was still decoding at the last draw.
    pub icons_pending: bool,
    layers: HashMap<LogPart, LogLayer>,
    pub(super) draw_detail_key: u64,
    pub(super) phone_info_height: f64,
    pub(super) availability: Availability,
    pub(super) draw_ref_key: u64,
    pub(super) presentation_now: i64,
}

pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(super) fn dirname(path: &str) -> &str {
    path.rfind('/').map(|index| &path[..index]).unwrap_or("")
}

/// Select values of the Branch filter: `all` or `<kind>:<name>`.
fn ref_value(kind: RefKind, name: &str) -> String {
    let prefix = match kind {
        RefKind::Head => "head",
        RefKind::Local => "local",
        RefKind::Remote => "remote",
        RefKind::Tag => "tag",
    };
    format!("{prefix}:{name}")
}

fn parse_ref_value(value: &str) -> Option<RefFilter> {
    let (prefix, name) = value.split_once(':')?;
    let kind = match prefix {
        "head" => RefKind::Head,
        "local" => RefKind::Local,
        "remote" => RefKind::Remote,
        "tag" => RefKind::Tag,
        _ => return None,
    };
    Some(RefFilter {
        name: name.to_owned(),
        kind: Some(kind),
    })
}

pub(super) fn ahead_behind(ahead: Option<i64>, behind: Option<i64>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ahead) = ahead.filter(|n| *n > 0) {
        parts.push(format!("↑{ahead}"));
    }
    if let Some(behind) = behind.filter(|n| *n > 0) {
        parts.push(format!("↓{behind}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

impl LogView {
    pub fn new(cx: &mut Cx, ui: &UiContext, store: &mut GitStore, target: PanelTarget) -> Self {
        // Status pushes for this checkout tell the log when HEAD moved.
        store.acquire_checkout(cx, ui, &target.server_id, &target.cwd);
        let state = LogState::new(&target, store);
        let availability = state.availability(ui);
        let sizes = SplitSizes::read(ui.preference(LAYOUT_PREF));
        Self {
            target,
            state,
            overlay: OverlayHost::default(),
            flows: HashMap::new(),
            hits: Hits::default(),
            hovered: None,
            pressed: None,
            touch_scroll: Default::default(),
            list: Scroller::default(),
            branches: Scroller::default(),
            files: Scroller::default(),
            info: Scroller::default(),
            sheet: Scroller::default(),
            row_height: 26.0,
            file_row_height: 24.0,
            listed_files: Vec::new(),
            listed_for: None,
            branches_open: true,
            drawer_open: false,
            wide: true,
            collapsed: HashSet::new(),
            collapsed_version: 0,
            ref_rows: Vec::new(),
            ref_rows_key: None,
            branch_query: String::new(),
            search_text: String::new(),
            search_timer: Timer::empty(),
            detail_timer: Timer::empty(),
            detail_waiting: false,
            search_open: false,
            focus_search: false,
            want_focus: false,
            was_visible: false,
            screen: Screen::Log,
            focus: Focus::List,
            selected_file: None,
            switching: None,
            reveal_after_load: None,
            retargeted: false,
            sizes,
            shown_sizes: SplitSizes::default(),
            drag: None,
            phone: false,
            input_style: String::new(),
            spinning: false,
            icons_pending: false,
            layers: HashMap::new(),
            draw_detail_key: 0,
            phone_info_height: 0.0,
            availability,
            draw_ref_key: 0,
            presentation_now: now_ms(),
        }
    }

    pub fn dispose(&mut self, store: &mut GitStore) {
        self.state.dispose();
        store.release_checkout(&self.target.server_id, &self.target.cwd);
    }

    pub fn close_overlay(&mut self, cx: &mut Cx) {
        self.overlay.close(cx);
        self.invalidate(cx);
    }

    /// Sends a menu or select opened during this event to the shell.
    pub fn flush_overlay(&mut self, ui: &mut UiContext) {
        self.overlay.flush(ui);
    }

    /// The same tab reopened with another target (Show in Git Log, Show history).
    pub fn retarget(&mut self, target: PanelTarget) {
        self.target = target;
        self.retargeted = true;
    }

    pub(super) fn host(&self) -> &str {
        &self.target.server_id
    }

    /// The split being dragged.
    pub(super) fn dragging(&self) -> Option<Split> {
        self.drag.map(|drag| drag.split)
    }
    fn shown_sha(&self) -> Option<&str> {
        match &self.screen {
            Screen::Commit(sha) if self.phone => Some(sha.as_str()),
            _ => self.state.selected.as_deref(),
        }
    }

    pub(super) fn detail_key(&self) -> u64 {
        let sha = self.shown_sha();
        let detail = sha.and_then(|sha| self.state.details.get(sha));
        let (kind, identity, error) = match detail {
            None => (0_u8, 0, None),
            Some(Detail::Loading) => (1, 0, None),
            Some(Detail::Missing) => (2, 0, None),
            Some(Detail::Failed(error)) => (3, 0, Some(error)),
            // Ready details are immutable boxed responses; moving the cache never changes identity.
            Some(Detail::Ready(commit)) => (4, &**commit as *const _ as usize, None),
        };
        crate::common::layer::inputs((sha, kind, identity, error))
    }

    fn part_key(&self, part: LogPart, detail: u64) -> u64 {
        use crate::common::layer::inputs;
        let filters = (
            self.state
                .filters
                .revision
                .as_ref()
                .map(|r| (r.kind, &r.name)),
            &self.state.filters.text,
            &self.state.filters.author,
            &self.state.filters.path,
        );
        match part {
            LogPart::Background => inputs((
                (
                    self.phone,
                    self.wide,
                    self.branches_open,
                    self.drawer_open,
                    self.sizes.branches.map(f64::to_bits),
                    self.sizes.side.map(f64::to_bits),
                    self.sizes.stacked.map(f64::to_bits),
                    self.availability as u8,
                    self.shown_sha(),
                    matches!(self.screen, Screen::Commit(_)),
                    self.state.refs.is_some(),
                ),
                (
                    self.state.commits.len(),
                    self.state.loaded,
                    self.state.page_error.is_some(),
                    self.state.refs_error.is_some(),
                    self.collapsed_version,
                    &self.branch_query,
                    self.search_open,
                    self.list.offset.to_bits(),
                    self.branches.offset.to_bits(),
                    self.files.offset.to_bits(),
                    self.info.offset.to_bits(),
                    self.sheet.offset.to_bits(),
                ),
                detail,
            )),
            LogPart::Toolbar | LogPart::PhoneToolbar => inputs((
                filters,
                &self.search_text,
                self.search_open,
                self.focus_search,
                self.branches_open,
                self.drawer_open,
                self.wide,
                self.state.page_loading,
                self.state.loaded,
                matches!(self.overlay.purpose(), Some(Purpose::Branch)),
                matches!(self.overlay.purpose(), Some(Purpose::Author)),
            )),
            LogPart::Banner => inputs((
                self.state.loaded,
                self.state.refresh_failed(),
                &self.state.page_error,
                &self.state.refs_error,
                filters,
            )),
            LogPart::BranchesChrome => inputs(&self.branch_query),
            LogPart::RefRow(index) => match (self.state.refs.as_ref(), self.ref_rows.get(index)) {
                (Some(refs), Some(row)) => self.ref_row_key(refs, row),
                _ => self.draw_ref_key,
            },
            LogPart::CommitRow(index) => {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                if let Some(commit) = self.state.commits.get(index) {
                    (&commit.sha, &commit.subject, &commit.author_name).hash(&mut hash);
                    for label in &commit.refs {
                        (&label.name, label.kind, label.is_head).hash(&mut hash);
                    }
                    let age = commit.author_ms.map(|ms| {
                        let minutes = (self.presentation_now - ms).max(0) / 60_000;
                        if minutes < 60 {
                            (0, minutes)
                        } else if minutes < 1440 {
                            (1, minutes / 60)
                        } else if minutes < 10080 {
                            (2, minutes / 1440)
                        } else {
                            (3, ms)
                        }
                    });
                    (
                        age,
                        self.state.selected.as_deref() == Some(commit.sha.as_str()),
                        self.state
                            .refs
                            .as_ref()
                            .and_then(|refs| refs.head_sha.as_deref())
                            == Some(commit.sha.as_str()),
                    )
                        .hash(&mut hash);
                    if let Some(row) = self.state.rows.get(index) {
                        (row.lane, row.color, row.lanes).hash(&mut hash);
                        for edges in [&row.top, &row.bottom] {
                            edges.len().hash(&mut hash);
                            for edge in edges {
                                (edge.from, edge.to, edge.color).hash(&mut hash);
                            }
                        }
                    }
                }
                hash.finish()
            }
            LogPart::Footer => inputs((
                self.state.page_loading,
                &self.state.page_error,
                self.state.page_error_at_end(),
                self.state.has_more,
            )),
            LogPart::FilesHeader | LogPart::PhoneFilesHeader | LogPart::FilesEmpty => detail,
            LogPart::FileRow(index) => {
                let selected = self
                    .listed_files
                    .get(index)
                    .is_some_and(|path| self.selected_file.as_ref() == Some(path));
                inputs((detail, selected, selected && self.focus == Focus::Files))
            }
            LogPart::Info => inputs((detail, self.info.offset.to_bits())),
            LogPart::PhoneInfo => inputs((detail, self.sheet.offset.to_bits())),
            LogPart::PhoneCommitToolbar => inputs(self.shown_sha()),
            LogPart::Summary => inputs((filters, self.search_open)),
            LogPart::Splitter(split) => inputs(self.dragging() == Some(split)),
            LogPart::Message(_) => inputs((
                self.state.loaded,
                self.state.commits.is_empty(),
                filters,
                &self.state.page_error,
                &self.state.refs_error,
                self.state.page_loading,
                detail,
                self.state.refs_version,
                &self.branch_query,
                self.collapsed_version,
            )),
            LogPart::TableHeader | LogPart::Drawer | LogPart::Divider(_) | LogPart::Skeleton => 0,
        }
    }

    pub(super) fn ref_row_key(&self, refs: &super::log::Refs, row: &RefRow) -> u64 {
        use crate::common::layer::inputs;
        match row {
            RefRow::Section { key, count, open } => inputs((0_u8, key, count, open)),
            RefRow::Head => inputs((
                1_u8,
                &refs.head_sha,
                &refs.head_branch,
                refs.detached,
                self.ref_selected(RefKind::Head, "HEAD"),
            )),
            RefRow::Local(index) => refs.local.get(*index).map_or(0, |branch| {
                inputs((
                    2_u8,
                    &branch.name,
                    branch.current,
                    branch.gone,
                    branch.ahead,
                    branch.behind,
                    self.ref_selected(RefKind::Local, &branch.name),
                    self.switching.as_deref() == Some(branch.name.as_str()),
                ))
            }),
            RefRow::Group {
                remote,
                count,
                open,
            } => inputs((3_u8, remote, count, open)),
            RefRow::Remote(index) => refs.remote.get(*index).map_or(0, |branch| {
                inputs((
                    4_u8,
                    &branch.name,
                    &branch.branch,
                    &branch.remote,
                    self.ref_selected(RefKind::Remote, &branch.name),
                    self.switching.as_deref() == Some(branch.branch.as_str())
                        && !refs.local.iter().any(|local| local.name == branch.branch),
                ))
            }),
            RefRow::Tag(index) => refs.tags.get(*index).map_or(0, |tag| {
                inputs((5_u8, tag, self.ref_selected(RefKind::Tag, tag)))
            }),
        }
    }

    /// A panel dirty signal schedules only boundaries whose presentation changed.
    pub fn invalidate(&mut self, cx: &mut Cx) {
        self.presentation_now = now_ms();
        let detail = self.detail_key();
        for (part, layer) in &self.layers {
            if layer.data != self.part_key(*part, detail)
                || layer.interaction
                    != layer.interaction(self.hovered.as_ref(), self.pressed.as_ref())
            {
                layer.paint.invalidate(cx);
            }
        }
    }

    /// Decode completion belongs to the visible file rows that used a pending icon.
    pub fn invalidate_media(&mut self, cx: &mut Cx) {
        for layer in self.layers.values().filter(|layer| layer.icons_pending) {
            layer.paint.invalidate(cx);
        }
    }

    /// Spinners are deferred to the panel's sibling list; only an elapsed date label can age here.
    pub fn animate(&mut self, cx: &mut Cx) {
        self.presentation_now = now_ms();
        for (part, layer) in &self.layers {
            if matches!(part, LogPart::CommitRow(_))
                && layer.data != self.part_key(*part, self.draw_detail_key)
            {
                layer.paint.invalidate(cx);
            }
        }
    }

    pub(super) fn begin_part(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        part: LogPart,
        area: Rect,
        clip: Rect,
        extra: u64,
    ) -> bool {
        let data = self.part_key(part, self.draw_detail_key);
        let layer = self.layers.entry(part).or_default();
        layer.visited = true;
        let interaction = layer.interaction(self.hovered.as_ref(), self.pressed.as_ref());
        if interaction != layer.interaction {
            layer.paint.invalidate(cx);
        }
        layer.data = data;
        layer.geometry =
            crate::common::layer::inputs((rect_key(area), rect_key(clip), extra, self.phone));
        if !layer.paint.begin(
            cx,
            area,
            crate::common::layer::inputs((data, layer.geometry)),
            &mut self.hits,
            paint,
        ) {
            self.spinning |= layer.spinning;
            self.icons_pending |= layer.icons_pending;
            return false;
        }
        layer.before = (self.spinning, self.icons_pending);
        self.spinning = false;
        self.icons_pending = false;
        true
    }

    pub(super) fn end_part(&mut self, cx: &mut Cx2d, paint: &GitPaint, part: LogPart) {
        let layer = self.layers.get_mut(&part).expect("opened Git Log boundary");
        layer.paint.end(cx, &self.hits, paint);
        layer.interaction = layer.interaction(self.hovered.as_ref(), self.pressed.as_ref());
        layer.spinning = self.spinning;
        layer.icons_pending = self.icons_pending;
        self.spinning |= layer.before.0;
        self.icons_pending |= layer.before.1;
    }

    pub(super) fn begin_frame(&mut self) {
        self.presentation_now = now_ms();
        for layer in self.layers.values_mut() {
            layer.visited = false;
        }
        self.draw_detail_key = self.detail_key();
    }

    pub(super) fn finish_frame(&mut self) {
        // Layout and focus intents are consumed while composing. Other boundaries
        // keep the data they actually painted (paging may start after the footer).
        let detail = self.detail_key();
        let mut layers = std::mem::take(&mut self.layers);
        layers.retain(|_, layer| layer.visited);
        for (part, layer) in &mut layers {
            if matches!(
                part,
                LogPart::Background | LogPart::Toolbar | LogPart::PhoneToolbar
            ) {
                layer.data = self.part_key(*part, detail);
                layer
                    .paint
                    .set_inputs(crate::common::layer::inputs((layer.data, layer.geometry)));
            }
        }
        self.layers = layers;
    }

    /// Applies a core batch, store change or timer; returns true when a redraw is due.
    pub fn process(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        ui: &mut UiContext,
        store: &mut GitStore,
        visible: bool,
    ) -> bool {
        let availability = self.state.availability(ui);
        let availability_changed = self.availability != availability;
        self.availability = availability;
        if visible && !self.was_visible {
            self.want_focus = true;
        }
        self.was_visible = visible;
        self.state.set_active(ui, visible);
        let anchor = self.list_anchor();
        let replaced = self.state.replaced;
        let mut dirty = self.state.process(ui, store) || availability_changed;
        if self.state.replaced != replaced
            && let Some((sha, delta)) = anchor
            && let Some(index) = self.state.index_of(&sha)
        {
            // New commits above keep the rows under the pointer in place.
            self.list.offset = index as f64 * self.row_height + delta;
        }
        let responses: Vec<(Flow, Result<serde_json::Value, wire::Failure>)> = ui
            .events
            .iter()
            .filter_map(|event| match event {
                CoreEvent::Response {
                    request_id, result, ..
                } => self
                    .flows
                    .remove(request_id)
                    .map(|flow| (flow, wire::git_result(result))),
                _ => None,
            })
            .collect();
        for (flow, result) in responses {
            self.on_flow(cx, ui, store, flow, result);
            dirty = true;
        }
        if self.search_timer.is_event(event).is_some() {
            self.search_timer = Timer::empty();
            self.apply_search(ui, false);
            dirty = true;
        }
        if self.detail_timer.is_event(event).is_some() {
            self.detail_timer = Timer::empty();
            self.detail_waiting = false;
            if let Some(sha) = self.state.selected.clone() {
                self.state.ensure_detail(ui, &sha);
            }
            dirty = true;
        }
        if let Some(goto) = self.state.take_goto() {
            self.on_goto(cx, ui, goto);
            dirty = true;
        }
        if let Some((sha, explicit)) = self.reveal_after_load.clone()
            && self.state.loaded
            && !self.state.page_loading
        {
            self.reveal_after_load = None;
            self.go_to_commit(cx, ui, &sha, explicit);
            dirty = true;
        }
        dirty |= self.auto_select(cx, ui);
        if dirty {
            self.sync_selection();
        }
        dirty
    }

    /// The first visible row's SHA and how far it is scrolled past (none at the top).
    fn list_anchor(&self) -> Option<(String, f64)> {
        if self.list.offset <= 0.0 || self.row_height <= 0.0 {
            return None;
        }
        let index = (self.list.offset / self.row_height).floor() as usize;
        let commit = self.state.commits.get(index)?;
        Some((
            commit.sha.clone(),
            self.list.offset - index as f64 * self.row_height,
        ))
    }

    /// The first load of a filter selects HEAD (else the first row) so the
    /// details are not empty.
    fn auto_select(&mut self, cx: &mut Cx, ui: &UiContext) -> bool {
        if !self.state.auto_select || !self.state.loaded {
            return false;
        }
        if self.state.selected.is_some()
            || self.state.commits.is_empty()
            || self.reveal_after_load.is_some()
        {
            self.state.auto_select = false;
            return false;
        }
        let head = match (&self.state.refs, &self.state.refs_error) {
            (Some(refs), _) => refs.head_sha.clone(),
            (None, Some(_)) => None,
            // HEAD is not known yet.
            (None, None) => return false,
        };
        self.state.auto_select = false;
        let index = head.and_then(|sha| self.state.index_of(&sha)).unwrap_or(0);
        self.select(cx, ui, index, false);
        true
    }

    /// Drops a file selection whose commit is gone.
    fn sync_selection(&mut self) {
        if self.state.selected.is_none() {
            self.selected_file = None;
            if self.focus == Focus::Files {
                self.focus = Focus::List;
            }
        }
    }

    /// The search field's query: a 4–64 digit hex query is looked up as a
    /// commit first; anything else (or no such commit) filters messages.
    fn apply_search(&mut self, ui: &UiContext, explicit: bool) {
        let query = self.search_text.trim().to_owned();
        self.state.cancel_goto();
        if is_hash_query(&query) && self.state.availability(ui) == Availability::Ready {
            self.state.goto(ui, &query, explicit);
            return;
        }
        let filters = Filters {
            text: query,
            ..self.state.filters.clone()
        };
        self.apply_filters(ui, filters);
    }

    fn on_goto(&mut self, cx: &mut Cx, ui: &mut UiContext, goto: Goto) {
        match goto {
            Goto::Found { sha, explicit } => {
                if self.state.filters.text.is_empty() {
                    self.go_to_commit(cx, ui, &sha, explicit);
                } else {
                    // The message filter hides it: list everything, then select.
                    let filters = Filters {
                        text: String::new(),
                        ..self.state.filters.clone()
                    };
                    self.apply_filters(ui, filters);
                    self.reveal_after_load = Some((sha, explicit));
                }
            }
            Goto::NotFound { query } => {
                if self.search_text.trim() == query {
                    let filters = Filters {
                        text: query,
                        ..self.state.filters.clone()
                    };
                    self.apply_filters(ui, filters);
                }
            }
            Goto::Failed(message) => ui.toast(message, true),
        }
    }

    /// Selects and reveals a listed commit; an unlisted one shows in the
    /// details (and, when asked for with Enter, opens its diff or commit screen).
    fn go_to_commit(&mut self, cx: &mut Cx, ui: &mut UiContext, sha: &str, explicit: bool) {
        if let Some(index) = self.state.index_of(sha) {
            if self.phone && explicit {
                self.open_commit_screen(cx, ui, index);
            } else {
                self.select(cx, ui, index, false);
            }
            return;
        }
        if self.state.selected.as_deref() != Some(sha) {
            self.state.selected = Some(sha.to_owned());
            self.selected_file = None;
            self.files.offset = 0.0;
            self.info.offset = 0.0;
        }
        if self.phone {
            if explicit {
                self.sheet.offset = 0.0;
                self.screen = Screen::Commit(sha.to_owned());
            }
        } else if explicit {
            self.open_diff(ui, sha, None);
        }
    }

    fn apply_filters(&mut self, ui: &UiContext, filters: Filters) {
        if filters != self.state.filters {
            self.reveal_after_load = None;
            self.list.offset = 0.0;
            self.state.set_filters(ui, filters);
        }
    }

    pub(super) fn refresh(&mut self, ui: &UiContext) {
        if self.state.availability(ui) == Availability::Ready && !self.state.page_loading {
            let keep = self.state.loaded;
            self.state.reload(ui, keep);
        }
    }

    // -----------------------------------------------------------------------
    // Text fields.

    /// Routes an event to the search fields; returns true when a redraw is due.
    pub fn handle_inputs(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        ui: &UiContext,
        inputs: &mut LogInputs<'_>,
    ) -> bool {
        let mut redraw = false;
        if std::mem::take(&mut self.retargeted) {
            let filters = Filters::from_target(&self.target.data);
            inputs.search.set_text(cx, "");
            self.search_text.clear();
            cx.stop_timer(self.search_timer);
            self.search_timer = Timer::empty();
            self.state.cancel_goto();
            self.reveal_after_load = None;
            self.screen = Screen::Log;
            self.state.selected = None;
            self.state.auto_select = true;
            self.apply_filters(ui, filters);
            redraw = true;
        }
        // Down in the search field moves into the commit list.
        if let Event::KeyDown(key) = event
            && key.key_code == KeyCode::ArrowDown
            && cx.has_key_focus(inputs.search.area())
        {
            self.enter_list(cx, ui);
            return true;
        }
        let search_uid = inputs.search.widget_uid();
        let branch_uid = inputs.branch.widget_uid();
        let actions = cx.capture_actions(|cx| {
            inputs.search.handle_event(cx, event, &mut Scope::empty());
            inputs.branch.handle_event(cx, event, &mut Scope::empty());
        });
        for action in &actions {
            let Some(action) = action.as_widget_action() else {
                continue;
            };
            let search = action.widget_uid == search_uid;
            if !search && action.widget_uid != branch_uid {
                continue;
            }
            match action.action.downcast_ref::<TextInputAction>() {
                Some(TextInputAction::Changed(text)) => {
                    let text = text.replace(['\n', '\r'], " ");
                    if search {
                        self.search_text = text;
                        self.state.cancel_goto();
                        self.reveal_after_load = None;
                        cx.stop_timer(self.search_timer);
                        self.search_timer = cx.start_timeout(SEARCH_DEBOUNCE);
                    } else {
                        self.branch_query = text;
                        self.branches.offset = 0.0;
                    }
                    redraw = true;
                }
                Some(TextInputAction::Returned(..)) if search => {
                    cx.stop_timer(self.search_timer);
                    self.search_timer = Timer::empty();
                    self.apply_search(ui, true);
                    self.want_focus = true;
                    redraw = true;
                }
                Some(TextInputAction::Escaped) => {
                    if search {
                        self.clear_search(cx, ui, inputs.search);
                        if self.phone {
                            self.search_open = false;
                        }
                    } else {
                        inputs.branch.set_text(cx, "");
                        self.branch_query.clear();
                    }
                    redraw = true;
                }
                Some(TextInputAction::KeyFocus | TextInputAction::KeyFocusLost) => redraw = true,
                _ => {}
            }
        }
        redraw
    }

    /// Key focus to the commit list, selecting the first row when none is.
    fn enter_list(&mut self, cx: &mut Cx, ui: &UiContext) {
        self.want_focus = true;
        self.focus = Focus::List;
        if self.state.selected.is_none() && !self.state.commits.is_empty() {
            self.select(cx, ui, 0, false);
        }
    }

    fn clear_search(&mut self, cx: &mut Cx, ui: &UiContext, input: &mut TextInput) {
        input.set_text(cx, "");
        self.search_text.clear();
        self.reveal_after_load = None;
        cx.stop_timer(self.search_timer);
        self.search_timer = Timer::empty();
        self.apply_search(ui, false);
    }

    // -----------------------------------------------------------------------
    // Actions.

    /// Selects row `index`. Keyboard moves (`debounce`) load details after a
    /// pause so holding an arrow key does not ask for every commit passed.
    pub(super) fn select(&mut self, cx: &mut Cx, ui: &UiContext, index: usize, debounce: bool) {
        let Some(commit) = self.state.commits.get(index) else {
            return;
        };
        let sha = commit.sha.clone();
        if self.state.selected.as_deref() != Some(sha.as_str()) {
            self.state.selected = Some(sha.clone());
            self.selected_file = None;
            self.files.offset = 0.0;
            self.info.offset = 0.0;
        }
        self.list
            .reveal(index as f64 * self.row_height, self.row_height);
        if self.phone {
            return;
        }
        cx.stop_timer(self.detail_timer);
        self.detail_timer = Timer::empty();
        if debounce && !self.state.details.contains_key(&sha) {
            self.detail_timer = cx.start_timeout(DETAIL_DEBOUNCE);
            self.detail_waiting = true;
        } else {
            self.detail_waiting = false;
            self.state.ensure_detail(ui, &sha);
        }
    }

    fn open_commit_screen(&mut self, cx: &mut Cx, ui: &UiContext, index: usize) {
        self.select(cx, ui, index, false);
        if let Some(sha) = self.state.selected.clone() {
            self.state.ensure_detail(ui, &sha);
            self.sheet.offset = 0.0;
            self.screen = Screen::Commit(sha);
        }
    }

    /// The phone commit screen returns to the log (back arrow, Back, Escape).
    pub fn back(&mut self) -> bool {
        if self.phone && matches!(self.screen, Screen::Commit(_)) {
            self.screen = Screen::Log;
            return true;
        }
        false
    }

    pub(super) fn subject_of(&self, sha: &str) -> String {
        if let Some(commit) = self.state.commit(sha) {
            return commit.subject.clone();
        }
        match self.state.details.get(sha) {
            Some(super::log::Detail::Ready(detail)) => detail.subject.clone(),
            _ => String::new(),
        }
    }

    /// Opens the commit's `commit_diff` tab, focused on `path` when given.
    pub(super) fn open_diff(&self, ui: &mut UiContext, sha: &str, path: Option<&str>) {
        let mut data = json!({ "sha": sha });
        if let Some(path) = path {
            data["focusPath"] = json!(path);
            data["focusRequestId"] = json!(now_ms());
        }
        let target = PanelTarget::new(
            "commit_diff",
            self.host(),
            &self.target.workspace_id,
            &self.state.cwd,
        )
        .with_id(sha)
        .with_title(self.subject_of(sha))
        .with_data(data);
        ui.push(UiEffect::OpenTab {
            target,
            placement: TabPlacement::Main,
        });
    }

    fn copy(ui: &mut UiContext, text: String, toast: &str) {
        ui.push(UiEffect::CopyText {
            text,
            toast: Some(t(toast)),
        });
    }

    fn filter_ref(&mut self, ui: &UiContext, filter: Option<RefFilter>) {
        let filters = Filters {
            revision: filter,
            ..self.state.filters.clone()
        };
        self.apply_filters(ui, filters);
    }

    /// Limits the log to the history of `path`.
    fn show_history(&mut self, ui: &UiContext, path: String) {
        self.screen = Screen::Log;
        let filters = Filters {
            path: Some(path),
            ..self.state.filters.clone()
        };
        self.apply_filters(ui, filters);
    }

    fn clear_filters(&mut self, cx: &mut Cx, ui: &UiContext, search: &mut TextInput) {
        search.set_text(cx, "");
        self.search_text.clear();
        cx.stop_timer(self.search_timer);
        self.search_timer = Timer::empty();
        self.state.cancel_goto();
        self.reveal_after_load = None;
        self.apply_filters(ui, Filters::default());
    }

    fn checkout(&mut self, ui: &UiContext, kind: RefKind, name: &str) {
        // One checkout at a time.
        if self.switching.is_some() {
            return;
        }
        // A remote branch checks out (and tracks) its local namesake.
        let branch = match kind {
            RefKind::Remote => self
                .state
                .refs
                .as_ref()
                .and_then(|refs| refs.remote.iter().find(|b| b.name == name))
                .map(|b| b.branch.clone())
                .unwrap_or_else(|| name.to_owned()),
            _ => name.to_owned(),
        };
        self.switching = Some(branch.clone());
        self.send_switch(ui, branch.clone(), Flow::Switch(branch));
    }

    fn send(&mut self, ui: &UiContext, kind: &str, fields: serde_json::Value, flow: Flow) {
        let request = wire::send(&ui.core, &self.target.server_id, kind, fields);
        self.flows.insert(request, flow);
    }

    fn send_switch(&mut self, ui: &UiContext, branch: String, flow: Flow) {
        let fields = json!({ "cwd": self.state.cwd, "branch": branch });
        self.send(ui, "checkout_switch_branch_request", fields, flow);
    }

    /// Original branch switcher: uncommitted changes offer stash-and-switch.
    fn on_flow(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        store: &mut GitStore,
        flow: Flow,
        result: Result<serde_json::Value, wire::Failure>,
    ) {
        let (host, cwd) = (self.target.server_id.clone(), self.state.cwd.clone());
        match flow {
            Flow::Switch(branch) | Flow::SwitchAfterStash(branch) if result.is_ok() => {
                self.switching = None;
                store.invalidate(cx, &host, &cwd);
                ui.toast(
                    tr("workspace.git.log.checkedOut", &[("name", branch)]),
                    false,
                );
            }
            Flow::Switch(branch) => {
                self.switching = None;
                let failure = result.err().unwrap_or_else(|| wire::Failure::message(""));
                if failure.message.to_lowercase().contains("uncommitted") {
                    self.overlay.open(Overlay {
                        purpose: Purpose::StashAndSwitch(branch),
                        kind: OverlayKind::Confirm {
                            title: t("branchSwitcher.uncommittedTitle"),
                            message: t("branchSwitcher.uncommittedMessage"),
                            confirm: t("branchSwitcher.stashAndSwitch"),
                            destructive: false,
                        },
                    });
                } else {
                    ui.toast(
                        wire::message_or(&failure, t("workspace.git.log.checkoutFailed")),
                        true,
                    );
                }
            }
            Flow::SwitchAfterStash(_) => {
                self.switching = None;
                if let Err(failure) = result {
                    ui.toast(
                        wire::message_or(&failure, t("workspace.git.log.checkoutFailed")),
                        true,
                    );
                }
            }
            Flow::StashSave(branch) => match result {
                Err(failure) => {
                    self.switching = None;
                    ui.toast(
                        wire::message_or(&failure, t("branchSwitcher.failedToStash")),
                        true,
                    );
                }
                Ok(_) => {
                    store.invalidate(cx, &host, &cwd);
                    self.send_switch(ui, branch.clone(), Flow::SwitchAfterStash(branch));
                }
            },
        }
    }

    fn current_branch(&self) -> Option<&str> {
        self.state
            .refs
            .as_ref()
            .and_then(|refs| refs.local.iter().find(|b| b.current))
            .map(|b| b.name.as_str())
    }

    fn ref_menu(&mut self, kind: RefKind, name: &str, anchor: Rect) {
        let mut entries = Vec::new();
        let current = kind == RefKind::Local && self.current_branch() == Some(name);
        if matches!(kind, RefKind::Local | RefKind::Remote) && !current {
            entries.push(MenuEntry::Item(
                MenuItem::new("checkout", t("workspace.git.log.menu.checkout"))
                    .icon("git-branch")
                    .enabled(self.switching.is_none()),
            ));
        }
        entries.push(MenuEntry::Item(
            MenuItem::new("show", t("workspace.git.log.menu.showOnly")).icon("list-filter"),
        ));
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Item(
            MenuItem::new("copy", t("workspace.git.log.menu.copyName")).icon("copy"),
        ));
        self.overlay.open(Overlay {
            purpose: Purpose::RefMenu {
                kind,
                name: name.to_owned(),
            },
            kind: OverlayKind::Menu {
                title: Some(name.to_owned()),
                anchor,
                align_end: false,
                width: None,
                entries,
            },
        });
    }

    /// Copy and open actions, then Checkout / Show only / Copy name for each
    /// branch or tag decorating the commit (phones reach these by long-press).
    fn row_menu(&mut self, index: usize, anchor: Rect) {
        let Some(commit) = self.state.commits.get(index) else {
            return;
        };
        let mut entries = vec![
            MenuEntry::Item(
                MenuItem::new("copy-hash", t("workspace.git.log.menu.copyRevision")).icon("copy"),
            ),
            MenuEntry::Item(
                MenuItem::new("copy-subject", t("workspace.git.log.menu.copySubject")).icon("copy"),
            ),
            MenuEntry::Separator,
            MenuEntry::Item(
                MenuItem::new("open-diff", t("workspace.git.log.menu.openDiff")).icon("file-diff"),
            ),
        ];
        let current = self.current_branch();
        for label in commit
            .refs
            .iter()
            .filter(|label| label.kind != RefKind::Head)
        {
            let value = ref_value(label.kind, &label.name);
            let name = vec![("name", label.name.clone())];
            entries.push(MenuEntry::Separator);
            let is_current = label.kind == RefKind::Local && current == Some(label.name.as_str());
            if matches!(label.kind, RefKind::Local | RefKind::Remote) && !is_current {
                entries.push(MenuEntry::Item(
                    MenuItem::new(
                        format!("checkout|{value}"),
                        tr("workspace.git.log.menu.checkoutRef", &name),
                    )
                    .icon("git-branch")
                    .enabled(self.switching.is_none()),
                ));
            }
            entries.push(MenuEntry::Item(
                MenuItem::new(
                    format!("show|{value}"),
                    tr("workspace.git.log.menu.showOnlyRef", &name),
                )
                .icon("list-filter"),
            ));
            entries.push(MenuEntry::Item(
                MenuItem::new(
                    format!("copy|{value}"),
                    tr("workspace.git.log.menu.copyRefName", &name),
                )
                .icon("copy"),
            ));
        }
        let title = super::log_draw::short(&commit.sha);
        self.overlay.open(Overlay {
            purpose: Purpose::RowMenu(commit.sha.clone()),
            kind: OverlayKind::Menu {
                title: Some(title),
                anchor,
                align_end: false,
                width: None,
                entries,
            },
        });
    }

    fn file_menu(&mut self, path: &str, anchor: Rect) {
        let Some(sha) = self.shown_commit() else {
            return;
        };
        let entries = vec![
            MenuEntry::Item(
                MenuItem::new("open", t("workspace.git.log.menu.openDiff")).icon("file-diff"),
            ),
            MenuEntry::Item(
                MenuItem::new("history", t("workspace.git.log.menu.showHistory")).icon("history"),
            ),
            MenuEntry::Separator,
            MenuEntry::Item(
                MenuItem::new("copy", t("workspace.git.log.menu.copyPath")).icon("copy"),
            ),
        ];
        self.overlay.open(Overlay {
            purpose: Purpose::FileMenu {
                sha,
                path: path.to_owned(),
            },
            kind: OverlayKind::Menu {
                title: Some(basename(path).to_owned()),
                anchor,
                align_end: false,
                width: None,
                entries,
            },
        });
    }

    fn branch_select(&mut self, anchor: Rect) {
        let mut options = vec![
            SelectOption::new("all", t("workspace.git.log.filters.allBranches")).icon("git-graph"),
        ];
        if let Some(error) = &self.state.refs_error {
            options.push(
                SelectOption::new(
                    "refs-error",
                    tr(
                        "workspace.git.log.states.refsFailed",
                        &[("error", error.clone())],
                    ),
                )
                .icon("circle-alert")
                .enabled(false)
                .separator_before(true),
            );
        }
        if let Some(refs) = &self.state.refs {
            let head = match (&refs.head_branch, &refs.head_sha) {
                (Some(branch), _) if !refs.detached => format!("HEAD ({branch})"),
                (_, Some(sha)) => format!("HEAD ({})", sha.chars().take(7).collect::<String>()),
                _ => "HEAD".to_owned(),
            };
            options.push(
                SelectOption::new(ref_value(RefKind::Head, "HEAD"), head)
                    .icon("crosshair")
                    .separator_before(true),
            );
            for (index, branch) in refs.local.iter().enumerate() {
                let mut option =
                    SelectOption::new(ref_value(RefKind::Local, &branch.name), branch.name.clone())
                        .icon(if branch.current {
                            "circle-dot"
                        } else {
                            "git-branch"
                        })
                        .separator_before(index == 0);
                let mut trailing = ahead_behind(branch.ahead, branch.behind);
                if branch.gone {
                    trailing = Some(t("workspace.git.log.branches.gone"));
                }
                if let Some(trailing) = trailing {
                    option = option.trailing(trailing);
                }
                options.push(option);
            }
            for (index, branch) in refs.remote.iter().enumerate() {
                options.push(
                    SelectOption::new(
                        ref_value(RefKind::Remote, &branch.name),
                        branch.name.clone(),
                    )
                    .icon("cloud")
                    .separator_before(index == 0),
                );
            }
            for (index, tag) in refs.tags.iter().enumerate() {
                options.push(
                    SelectOption::new(ref_value(RefKind::Tag, tag), tag.clone())
                        .icon("tag")
                        .separator_before(index == 0),
                );
            }
        }
        let selected = match &self.state.filters.revision {
            None => Some("all".to_owned()),
            Some(RefFilter {
                name,
                kind: Some(kind),
            }) => Some(ref_value(*kind, name)),
            Some(_) => None,
        };
        self.overlay.open(Overlay {
            purpose: Purpose::Branch,
            kind: OverlayKind::Select {
                title: t("workspace.git.log.filters.branchTitle"),
                anchor,
                min_width: 280.0,
                options,
                selected,
                placeholder: t("workspace.git.log.branches.search"),
                empty: t("workspace.git.log.branches.empty"),
            },
        });
    }

    fn author_select(&mut self, anchor: Rect) {
        let mut options =
            vec![SelectOption::new("", t("workspace.git.log.filters.allUsers")).icon("users")];
        for (index, author) in self.state.authors.iter().enumerate() {
            options.push(
                SelectOption::new(format!("author:{author}"), author.clone())
                    .icon("user")
                    .separator_before(index == 0),
            );
        }
        let selected = Some(
            self.state
                .filters
                .author
                .as_ref()
                .map(|a| format!("author:{a}"))
                .unwrap_or_default(),
        );
        self.overlay.open(Overlay {
            purpose: Purpose::Author,
            kind: OverlayKind::Select {
                title: t("workspace.git.log.filters.userTitle"),
                anchor,
                min_width: 240.0,
                options,
                selected,
                placeholder: t("workspace.git.log.filters.userSearch"),
                empty: t("workspace.git.log.filters.userEmpty"),
            },
        });
    }

    /// Applies a menu / select answer; returns true when one arrived.
    pub fn handle_overlay(&mut self, ui: &mut UiContext) -> bool {
        let Some(result) = self.overlay.poll() else {
            return false;
        };
        match result {
            OverlayResult::Chose(Purpose::Branch, value) => {
                let filter = if value == "all" {
                    None
                } else {
                    parse_ref_value(&value)
                };
                if value == "all" || filter.is_some() {
                    self.filter_ref(ui, filter);
                }
            }
            OverlayResult::Chose(Purpose::Author, value) => {
                let author = value.strip_prefix("author:").map(str::to_owned);
                let filters = Filters {
                    author,
                    ..self.state.filters.clone()
                };
                self.apply_filters(ui, filters);
            }
            OverlayResult::Chose(Purpose::RefMenu { kind, name }, action) => {
                match action.as_str() {
                    "checkout" => self.checkout(ui, kind, &name),
                    "show" => self.filter_ref(
                        ui,
                        Some(RefFilter {
                            name,
                            kind: Some(kind),
                        }),
                    ),
                    "copy" => Self::copy(ui, name, "workspace.git.log.toasts.nameCopied"),
                    _ => {}
                }
            }
            OverlayResult::Chose(Purpose::RowMenu(sha), action) => match action.as_str() {
                "copy-hash" => Self::copy(ui, sha, "workspace.git.log.toasts.revisionCopied"),
                "copy-subject" => {
                    let subject = self.subject_of(&sha);
                    Self::copy(ui, subject, "workspace.git.log.toasts.subjectCopied");
                }
                "open-diff" => self.open_diff(ui, &sha, None),
                other => {
                    let Some((verb, value)) = other.split_once('|') else {
                        return true;
                    };
                    let Some(RefFilter {
                        name,
                        kind: Some(kind),
                    }) = parse_ref_value(value)
                    else {
                        return true;
                    };
                    match verb {
                        "checkout" => self.checkout(ui, kind, &name),
                        "show" => {
                            self.screen = Screen::Log;
                            self.filter_ref(
                                ui,
                                Some(RefFilter {
                                    name,
                                    kind: Some(kind),
                                }),
                            );
                        }
                        "copy" => Self::copy(ui, name, "workspace.git.log.toasts.nameCopied"),
                        _ => {}
                    }
                }
            },
            OverlayResult::Chose(Purpose::FileMenu { sha, path }, action) => {
                match action.as_str() {
                    "open" => self.open_diff(ui, &sha, Some(&path)),
                    "history" => self.show_history(ui, path),
                    "copy" => Self::copy(ui, path, "workspace.git.log.toasts.pathCopied"),
                    _ => {}
                }
            }
            OverlayResult::Confirmed(Purpose::StashAndSwitch(branch)) => {
                let mut fields = json!({ "cwd": self.state.cwd });
                if let Some(current) = self.current_branch() {
                    fields["branch"] = json!(current);
                }
                self.switching = Some(branch.clone());
                self.send(ui, "stash_save_request", fields, Flow::StashSave(branch));
            }
            _ => {}
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn activate(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        hit: LogHit,
        anchor: Rect,
        secondary: bool,
        inputs: &mut LogInputs<'_>,
    ) {
        match hit {
            LogHit::BranchesToggle => {
                if self.wide {
                    self.branches_open = !self.branches_open;
                } else {
                    self.drawer_open = !self.drawer_open;
                }
            }
            LogHit::Refresh => self.refresh(ui),
            LogHit::BranchChip => self.branch_select(anchor),
            LogHit::AuthorChip => self.author_select(anchor),
            LogHit::PathChip(_) | LogHit::Drawer | LogHit::Splitter(_) => {}
            LogHit::ClearBranch | LogHit::ShowAllBranches => self.filter_ref(ui, None),
            LogHit::ClearAuthor => {
                let filters = Filters {
                    author: None,
                    ..self.state.filters.clone()
                };
                self.apply_filters(ui, filters);
            }
            LogHit::ClearPath => {
                let filters = Filters {
                    path: None,
                    ..self.state.filters.clone()
                };
                self.apply_filters(ui, filters);
            }
            LogHit::ClearFilters => self.clear_filters(cx, ui, inputs.search),
            LogHit::DrawerBackdrop => self.drawer_open = false,
            LogHit::SearchToggle => {
                if self.search_open {
                    self.search_open = false;
                    if !self.search_text.is_empty() {
                        self.clear_search(cx, ui, inputs.search);
                    }
                } else {
                    self.search_open = true;
                    self.focus_search = true;
                }
            }
            LogHit::SearchClear => self.clear_search(cx, ui, inputs.search),
            LogHit::BranchSearchClear => {
                inputs.branch.set_text(cx, "");
                self.branch_query.clear();
                self.branches.offset = 0.0;
            }
            LogHit::Section(key) => {
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.collapsed_version += 1;
            }
            LogHit::Ref { kind, name } => {
                if secondary {
                    if kind != RefKind::Head {
                        self.ref_menu(kind, &name, anchor);
                    }
                } else if self.ref_selected(kind, &name) {
                    // A second click on the shown ref lists every branch again.
                    self.filter_ref(ui, None);
                } else {
                    self.drawer_open = false;
                    self.filter_ref(
                        ui,
                        Some(RefFilter {
                            name,
                            kind: Some(kind),
                        }),
                    );
                }
            }
            LogHit::RefMenu { kind, name } => self.ref_menu(kind, &name, anchor),
            LogHit::Row(index) | LogHit::Date(index) => {
                self.focus = Focus::List;
                if secondary {
                    self.select(cx, ui, index, false);
                    self.row_menu(index, anchor);
                } else if self.phone {
                    self.open_commit_screen(cx, ui, index);
                } else {
                    self.select(cx, ui, index, false);
                }
            }
            LogHit::PageRetry => self.state.retry(ui),
            LogHit::DetailRetry(sha) => self.state.retry_detail(ui, &sha),
            LogHit::File(path) => {
                self.focus = Focus::Files;
                self.selected_file = Some(path.clone());
                if secondary {
                    self.file_menu(&path, anchor);
                } else if self.phone
                    && let Some(sha) = self.shown_commit()
                {
                    // Desktop selects (double-click or Enter opens); a phone tap opens.
                    self.open_diff(ui, &sha, Some(&path));
                }
            }
            LogHit::CopyHash(sha) => Self::copy(ui, sha, "workspace.git.log.toasts.revisionCopied"),
            LogHit::Parent(sha) => {
                if let Some(index) = self.state.index_of(&sha) {
                    if self.phone {
                        self.open_commit_screen(cx, ui, index);
                    } else {
                        self.select(cx, ui, index, false);
                    }
                }
            }
            LogHit::Back => {
                self.back();
            }
        }
    }

    /// The commit whose details are shown (the phone screen's, else the selection).
    pub(super) fn shown_commit(&self) -> Option<String> {
        match &self.screen {
            Screen::Commit(sha) if self.phone => Some(sha.clone()),
            _ => self.state.selected.clone(),
        }
    }

    fn tooltip_for(&self, hit: &LogHit) -> Option<String> {
        Some(match hit {
            LogHit::BranchesToggle => {
                let open = if self.wide {
                    self.branches_open
                } else {
                    self.drawer_open
                };
                t(if open {
                    "workspace.git.log.toolbar.hideBranches"
                } else {
                    "workspace.git.log.toolbar.showBranches"
                })
            }
            LogHit::Refresh => t("workspace.git.log.toolbar.refresh"),
            LogHit::SearchToggle => t(if self.search_open {
                "workspace.git.log.toolbar.closeSearch"
            } else {
                "workspace.git.log.toolbar.search"
            }),
            LogHit::Back => t("workspace.git.log.toolbar.back"),
            LogHit::SearchClear | LogHit::BranchSearchClear => t("workspace.git.log.toolbar.clear"),
            LogHit::ClearBranch | LogHit::ClearAuthor | LogHit::ClearPath => {
                t("workspace.git.log.filters.clear")
            }
            LogHit::ClearFilters => t("workspace.git.log.filters.clearAll"),
            LogHit::CopyHash(_) => t("workspace.git.log.details.copyHash"),
            LogHit::RefMenu { .. } => t("workspace.git.log.menu.more"),
            LogHit::File(path) | LogHit::PathChip(path) => path.clone(),
            LogHit::Date(index) => {
                let ms = self.state.commits.get(*index)?.author_ms?;
                local_time(ms, "%Y-%m-%d %H:%M")
            }
            _ => return None,
        })
    }

    fn cursor_for(hit: Option<&LogHit>) -> MouseCursor {
        match hit {
            None | Some(LogHit::Drawer | LogHit::PathChip(_)) => MouseCursor::Default,
            Some(LogHit::Splitter(Split::Stacked)) => MouseCursor::RowResize,
            Some(LogHit::Splitter(_)) => MouseCursor::ColResize,
            Some(_) => MouseCursor::Hand,
        }
    }

    fn region_at(&self, point: DVec2) -> Option<Region> {
        // The open branches drawer and its scrim cover the other regions.
        if !self.wide && self.drawer_open {
            return self
                .branches
                .area
                .contains(point)
                .then_some(Region::Branches);
        }
        [
            (Region::Branches, self.branches.area),
            (Region::Sheet, self.sheet.area),
            (Region::Info, self.info.area),
            (Region::Files, self.files.area),
            (Region::List, self.list.area),
        ]
        .into_iter()
        .find(|(_, area)| area.contains(point))
        .map(|(region, _)| region)
    }

    fn scroller(&mut self, region: Region) -> &mut Scroller {
        match region {
            Region::List => &mut self.list,
            Region::Branches => &mut self.branches,
            Region::Files => &mut self.files,
            Region::Info => &mut self.info,
            Region::Sheet => &mut self.sheet,
        }
    }

    /// The files of the shown commit are listed (keyboard moves through them).
    fn files_listed(&self) -> bool {
        !self.listed_files.is_empty()
            && self.listed_for.is_some()
            && self.listed_for == self.shown_commit()
    }

    /// Keyboard: find, Escape, area switching, and moves through the commit
    /// table or the changed files.
    fn handle_key(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        key: &KeyEvent,
        inputs: &mut LogInputs<'_>,
    ) -> bool {
        if key.key_code == KeyCode::KeyF && key.modifiers.is_primary() {
            if self.phone {
                self.screen = Screen::Log;
                self.search_open = true;
            }
            self.focus_search = true;
            return true;
        }
        if key.key_code == KeyCode::Escape {
            if self.back() {
                return true;
            }
            if self.drawer_open {
                self.drawer_open = false;
                return true;
            }
            if !self.search_text.is_empty() || !self.state.filters.text.is_empty() {
                self.clear_search(cx, ui, inputs.search);
                self.search_open = false;
                return true;
            }
            return false;
        }
        if self.phone && matches!(self.screen, Screen::Commit(_)) {
            return false;
        }
        if !self.phone
            && matches!(
                key.key_code,
                KeyCode::Tab | KeyCode::ArrowLeft | KeyCode::ArrowRight
            )
        {
            let next = match key.key_code {
                KeyCode::ArrowLeft => Focus::List,
                KeyCode::ArrowRight => Focus::Files,
                _ if self.focus == Focus::Files => Focus::List,
                _ => Focus::Files,
            };
            if next == Focus::Files && !self.files_listed() {
                return false;
            }
            if next == Focus::Files
                && self
                    .selected_file
                    .as_ref()
                    .is_none_or(|p| !self.listed_files.contains(p))
            {
                self.selected_file = self.listed_files.first().cloned();
                self.files.offset = 0.0;
            }
            let changed = self.focus != next;
            self.focus = next;
            return changed;
        }
        if self.focus == Focus::Files && self.files_listed() {
            let current = self
                .selected_file
                .as_ref()
                .and_then(|p| self.listed_files.iter().position(|f| f == p));
            let last = self.listed_files.len() - 1;
            let next = match key.key_code {
                KeyCode::ArrowUp => current.map_or(0, |i| i.saturating_sub(1)),
                KeyCode::ArrowDown => current.map_or(0, |i| (i + 1).min(last)),
                KeyCode::Home => 0,
                KeyCode::End => last,
                KeyCode::ReturnKey | KeyCode::NumpadEnter => {
                    if let (Some(path), Some(sha)) =
                        (self.selected_file.clone(), self.shown_commit())
                    {
                        self.open_diff(ui, &sha, Some(&path));
                    }
                    return false;
                }
                _ => return false,
            };
            self.selected_file = Some(self.listed_files[next].clone());
            self.files
                .reveal(next as f64 * self.file_row_height, self.file_row_height);
            return true;
        }
        let count = self.state.commits.len();
        if count == 0 {
            return false;
        }
        let current = self
            .state
            .selected
            .as_ref()
            .and_then(|sha| self.state.index_of(sha));
        let page = ((self.list.area.size.y / self.row_height).floor() as usize).max(1);
        let next = match key.key_code {
            KeyCode::ArrowUp => current.map_or(0, |i| i.saturating_sub(1)),
            KeyCode::ArrowDown => current.map_or(0, |i| (i + 1).min(count - 1)),
            KeyCode::PageUp => current.map_or(0, |i| i.saturating_sub(page)),
            KeyCode::PageDown => current.map_or(0, |i| (i + page).min(count - 1)),
            KeyCode::Home => 0,
            KeyCode::End => count - 1,
            KeyCode::ReturnKey | KeyCode::NumpadEnter => {
                match current {
                    Some(index) if self.phone => self.open_commit_screen(cx, ui, index),
                    Some(index) => {
                        let sha = self.state.commits[index].sha.clone();
                        self.open_diff(ui, &sha, None);
                    }
                    None => return false,
                }
                return true;
            }
            _ => return false,
        };
        self.select(cx, ui, next, true);
        true
    }

    /// Saves the dragged split sizes.
    fn save_sizes(&self, ui: &mut UiContext) {
        let value = self.shown_sizes.merged(ui.preference(LAYOUT_PREF));
        ui.push(UiEffect::SetPreference {
            key: LAYOUT_PREF.into(),
            value,
        });
    }

    /// Pointer and keyboard input outside overlays; returns true when a redraw is due.
    pub fn handle_pointer(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        area: Area,
        ui: &mut UiContext,
        inputs: &mut LogInputs<'_>,
    ) -> bool {
        if let Event::BackPressed { handled } = event
            && !handled.get()
            && self.back()
        {
            handled.set(true);
            return true;
        }
        let mut redraw = false;
        if self.want_focus && area.is_valid(cx) {
            self.want_focus = false;
            cx.set_key_focus(area);
            redraw = true;
        }
        let hit = event.hits(cx, area);
        if !area.is_valid(cx) {
            self.touch_scroll.reset();
        }
        if let Some(drag) = self.drag {
            match &hit {
                Pointer::FingerMove(e) => {
                    let delta = match drag.split {
                        Split::Branches => e.abs.x - drag.origin,
                        Split::Side => drag.origin - e.abs.x,
                        Split::Stacked => drag.origin - e.abs.y,
                    };
                    *self.sizes.slot(drag.split) = Some(drag.start + delta);
                    return true;
                }
                Pointer::FingerUp(_) => {
                    self.drag = None;
                    self.save_sizes(ui);
                    return true;
                }
                _ => {}
            }
        }
        if let Some(delta) = self.touch_scroll.handle_event(cx, event, &hit) {
            self.pressed = None;
            let origin = self.touch_scroll.origin();
            let Some(region) = self.region_at(origin) else {
                self.touch_scroll.reset();
                return redraw;
            };
            let scroller = self.scroller(region);
            let (mut offset, max) = (scroller.offset, scroller.max());
            let moved = self.touch_scroll.apply_delta(&mut offset, delta, max);
            self.scroller(region).offset = offset;
            return moved || redraw;
        }
        match hit {
            Pointer::FingerHoverIn(e) | Pointer::FingerHoverOver(e) => {
                let (anchor, hovered) = self
                    .hits
                    .find(e.abs)
                    .map(|(anchor, hit)| (anchor, Some(hit)))
                    .unwrap_or_default();
                cx.set_cursor(Self::cursor_for(hovered.as_ref()));
                if hovered != self.hovered {
                    let tip = hovered.as_ref().and_then(|hit| self.tooltip_for(hit));
                    tooltip_at(ui, tip.as_deref(), anchor, 0.3);
                    self.hovered = hovered;
                    return true;
                }
            }
            Pointer::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.3);
                    return true;
                }
            }
            Pointer::FingerScroll(e) => {
                let delta = if e.modifiers.shift {
                    e.scroll.x
                } else {
                    e.scroll.y
                };
                if let Some(region) = self.region_at(e.abs) {
                    return self.scroller(region).scroll_by(delta) || redraw;
                }
            }
            Pointer::FingerDown(e) => {
                tooltip_at(ui, None, Rect::default(), 0.3);
                self.pressed = self.hits.find(e.abs).map(|(_, hit)| hit);
                if let Some(LogHit::Splitter(split)) = self.pressed {
                    self.pressed = None;
                    let origin = if split == Split::Stacked {
                        e.abs.y
                    } else {
                        e.abs.x
                    };
                    let start = self.shown_sizes.get(split).unwrap_or_default();
                    self.drag = Some(Drag {
                        split,
                        origin,
                        start,
                    });
                    return true;
                }
                if self.region_at(e.abs).is_some() {
                    cx.set_key_focus(area);
                }
                // Phone rows show their pressed state.
                return (self.phone && self.pressed.is_some()) || redraw;
            }
            Pointer::FingerLongPress(e) => {
                if let Some((rect, hit)) = self.hits.find(e.abs)
                    && matches!(hit, LogHit::Row(_) | LogHit::Ref { .. } | LogHit::File(_))
                {
                    self.pressed = None;
                    self.activate(cx, ui, hit, rect, true, inputs);
                    return true;
                }
            }
            Pointer::FingerUp(e) => {
                let pressed = self.pressed.take();
                redraw |= self.phone && pressed.is_some();
                if !e.is_over || e.has_long_press_occurred || (e.device.is_touch() && !e.was_tap())
                {
                    return redraw;
                }
                let secondary = e.mouse_button().is_some_and(|b| b.is_secondary());
                let Some((rect, hit)) = self.hits.find(e.abs) else {
                    return redraw;
                };
                if pressed.as_ref() != Some(&hit) {
                    return redraw;
                }
                // A desktop double-click opens the diff of the row or file.
                if e.tap_count >= 2 && !secondary && !self.phone {
                    match &hit {
                        LogHit::File(path) => {
                            if let Some(sha) = self.shown_commit() {
                                self.open_diff(ui, &sha, Some(path));
                            }
                            return true;
                        }
                        LogHit::Row(index) | LogHit::Date(index) => {
                            if let Some(sha) = self.state.commits.get(*index).map(|c| c.sha.clone())
                            {
                                self.open_diff(ui, &sha, None);
                            }
                            return true;
                        }
                        _ => {}
                    }
                }
                self.activate(cx, ui, hit, rect, secondary, inputs);
                return true;
            }
            Pointer::KeyDown(key) => return self.handle_key(cx, ui, &key, inputs) || redraw,
            _ => {}
        }
        redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sizes_round_trip_and_keep_unknown_keys() {
        let stored = json!({ "branchesWidth": 300.4, "detailsHeight": -5, "other": true });
        let sizes = SplitSizes::read(Some(&stored));
        assert_eq!(
            sizes,
            SplitSizes {
                branches: Some(300.4),
                side: None,
                stacked: None
            }
        );
        let next = SplitSizes {
            side: Some(410.6),
            ..sizes
        }
        .merged(Some(&stored));
        assert_eq!(next["branchesWidth"], json!(300.0));
        assert_eq!(next["detailsWidth"], json!(411.0));
        assert_eq!(next["other"], json!(true));
    }

    #[test]
    fn ref_values_round_trip() {
        let value = ref_value(RefKind::Remote, "origin/feature:x");
        assert_eq!(
            parse_ref_value(&value),
            Some(RefFilter {
                name: "origin/feature:x".to_owned(),
                kind: Some(RefKind::Remote)
            })
        );
        assert_eq!(parse_ref_value("bogus:x"), None);
    }
}
