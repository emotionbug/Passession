//! `ChatPanel`, the chat tab root (agent_panel.dart AgentPanel minus the
//! composer internals, which ComposerPanel owns, + chrome/stream_chrome.dart):
//! the virtualized stream with sticky bottom and reading hold, older-history
//! pagination, chat find, the outline rail, the track bar, callouts, toasts,
//! conversation actions (rename, fork, reload/unarchive, usage, detach,
//! archive, delete), rewind, permission responses, file links, compact detail
//! sheets, the image lightbox and the subagent sheet. Session state lives in
//! the module-level store (`store.rs`); the panel only presents it.

use super::attachment_export::{Export as LightboxExport, HostRoute, Mode as ExportMode, Original};
use super::attention::{self, AttentionClear};
use super::canvas::*;
use super::chat_row::{ChatRowEvent, ChatRowWidgetRefExt, RowEventKind, RowModel};
use super::chrome::{self, Callout, NonReady, Toast, ToastVariant};
use super::composer_bridge as bridge;
use super::extension_ui::ExtensionDock;
use super::find::{self, FindController, Phase as FindPhase};
use super::i18n::{t, ta};
use super::images::{self, ImageState};
use super::json::*;
use super::layout::*;
use super::links::{
    self, AssistantFileLinkSource, FileLinkSourceType, InlinePathTarget, Resolution,
};
use super::outline::{self, Rail, RailEvent};
use super::paint::{Draw, Look, MOBILE, RowState, now_seconds};
use super::permission;
use super::rows::{self, FindTarget, LiveFooter, RowContext, RowItem, RowKind};
use super::session::{
    ActionKind, ChatSession, Completion, Cursor, ExecutionStatus, Io, SessionKey, SyncStatus,
};
use super::store::with_store;
use super::task_controls;
use super::tool_sheet::SheetMotion;
use super::tracks::{self, DeleteFinished, SubagentRow, TrackModel, TrackPanel};
use crate::common::file_drop::FileDropZone;
use crate::common::overlay::{
    self, AnchoredPopover, DrawLayerScrim, DrawLayerTexture, LayerTexture, MenuEntry, MenuItem,
    MenuOptions, TapTarget, WindowModal,
};
use crate::common::paint::Painter;
use crate::common::provider_icon::ProviderIcon;
use crate::common::transition::{Intent, Transition, Visual};
use crate::common::widgets::{PTextFieldWidgetRefExt, RowMetrics};
use crate::common::{FileLinkIntent, PanelTarget, TabPlacement, UiContext, UiEffect};
use crate::features::composer::ComposerPanelWidgetRefExt;
use makepad_widgets::scroll_bars::TouchScroll;
use makepad_widgets::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::paint::*;
    use crate::common::widgets::*;
    use crate::common::overlay::DrawLayerScrim;
    use crate::common::overlay::DrawLayerTexture;
    use crate::features::timeline::canvas::*;
    use crate::features::timeline::chat_row::*;
    use crate::features::composer::ui::panel::ComposerPanel;
    use crate::common::file_drop::FileDropZone;

    pub ChatPanelBase = {{ChatPanel}} {}

    // The chat panel body. The root below adds the sheet template; a panel
    // shown inside a subagent sheet receives the same template at runtime
    // (`set_sheet_template`), so sheets stack as in Flutter.
    pub ChatPanelSheet = <ChatPanelBase> {
        width: Fill, height: Fill
        kit: <ChatKit> {}
        modal_kit: <ChatKit> {}
        track_composite: <DrawLayerTexture> {}
        track_scrim: <DrawLayerScrim> {}
        painter: <PaseoPainter> {}
        provider_icon: {
            fn get_color(self) -> vec4 {
                return self.color;
            }
        }
        list: <PortalList> {
            width: Fill, height: Fill
            auto_tail: true
            // strategy-native.tsx: a short transcript rests on the composer.
            align_top_when_empty: false
            drag_scrolling: true
            Row = <ChatRow> {}
        }
        composer: <ComposerPanel> {}
        file_drop: <FileDropZone> {}
        find_field: <PTextField> { width: Fill, height: Fit, focus_scope: "editable" }
    }

    // Re-exported as `crate::features::timeline::ChatPanel` (timeline/mod.rs).
    pub ChatPanelRoot = <ChatPanelSheet> {
        sheet_template: <ChatPanelSheet> {}
    }
}

thread_local! {
    static NEXT_UID: Cell<u64> = const { Cell::new(1) };
    /// Conversation menu facts per open chat (Flutter `ConversationMenus`).
    static MENUS: RefCell<HashMap<(String, String), MenuFacts>> = RefCell::new(HashMap::new());
    /// path-open.ts generation: only the latest open finishes its folder probe.
    static OPEN_GENERATION: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, Default)]
struct MenuFacts {
    expanded_tools: bool,
    archived: bool,
    outline_enabled: bool,
    /// Only a Paseo child agent has a parent to detach from.
    detachable: bool,
}

/// agent-panel.tsx conversation actions offered by the agent tab menu.
pub fn conversation_actions(host_id: &str, agent_id: &str) -> Vec<(String, String)> {
    let facts = MENUS
        .with(|m| {
            m.borrow()
                .get(&(host_id.to_owned(), agent_id.to_owned()))
                .copied()
        })
        .unwrap_or_default();
    let item = |value: &str, key: &str| (value.to_owned(), t(key));
    let mut out = Vec::new();
    // Native has no chat find or outline (chat-find/index.tsx, rail.tsx).
    if !MOBILE {
        out.push(item("find", "paneFind.title"));
        if facts.outline_enabled {
            out.push(item("outline", "settings.appearance.chatOutline.title"));
        }
    }
    out.push(item(
        "tools",
        if facts.expanded_tools {
            "agentPanel.menu.collapseTools"
        } else {
            "agentPanel.menu.expandTools"
        },
    ));
    out.push(item("sync", "agentPanel.menu.syncHistory"));
    out.push(item("rename", "workspace.tabs.menu.renameAgent"));
    out.push(item("fork", "agentPanel.menu.fork"));
    out.push(item(
        "refresh",
        if facts.archived {
            "agentPanel.archived.unarchive"
        } else {
            "workspace.tabs.menu.reloadAgent"
        },
    ));
    out.push(item("usage", "sidebar.footer.usage"));
    if facts.detachable {
        out.push(item("detach", "subagents.detachTooltip"));
    }
    out.push(item("archive", "agentList.archiveSheet.archive"));
    out.push(item("delete", "workspace.fileActions.delete"));
    out
}

/// What the shell mounts a chat tab with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatTarget {
    pub host_id: String,
    /// The real agent (the parent for a provider subagent pane).
    pub agent_id: String,
    pub subagent_id: Option<String>,
    pub workspace_id: String,
    pub cwd: Option<String>,
    pub tab_key: String,
    /// pane-context host "sheet": shown without its composer.
    pub sheet: bool,
    pub title: Option<String>,
}

impl ChatTarget {
    fn key(&self) -> SessionKey {
        SessionKey {
            host_id: self.host_id.clone(),
            agent_id: self.agent_id.clone(),
            subagent_id: self.subagent_id.clone(),
        }
    }
    fn is_subagent(&self) -> bool {
        self.subagent_id.is_some()
    }
    fn cwd(&self) -> &str {
        self.cwd.as_deref().unwrap_or("")
    }
    /// The id this pane shows (the subagent for a provider-subagent pane).
    fn shown_id(&self) -> &str {
        self.subagent_id.as_deref().unwrap_or(&self.agent_id)
    }
}

/// Everything the stream model is derived from; an unchanged signature skips
/// regrouping the whole history (agent_panel.dart `_streamCache`).
#[derive(Clone, Debug, PartialEq)]
struct ModelSig {
    timeline: usize,
    outgoing: u64,
    level: DetailLevel,
    open_turns: u64,
    provider: String,
    running: bool,
    permissions: usize,
    version: u64,
    track_clearance: f64,
    loading_older: bool,
}

#[derive(Default)]
struct OlderPaging {
    at_start: bool,
    request: Option<Cursor>,
    continue_short: bool,
}

impl OlderPaging {
    fn loading(&self) -> bool {
        self.request.is_some()
    }

    fn settled(&mut self, start: Option<&Cursor>) {
        if let Some(request) = self.request.take() {
            self.continue_short =
                start.is_some_and(|start| start.epoch == request.epoch && start.seq < request.seq);
        }
    }

    fn observe(&mut self, at_start: bool, underfilled: bool, available: bool) -> bool {
        if !at_start {
            self.at_start = false;
            self.continue_short = false;
            return false;
        }
        if !available || self.loading() {
            return false;
        }
        let wanted = !self.at_start || (self.continue_short && underfilled);
        self.at_start = true;
        self.continue_short = false;
        wanted
    }
}

fn remap_reading_anchor(
    previous: &[RowItem],
    next: &[RowItem],
    anchor: (usize, f64),
) -> Option<(usize, f64)> {
    let row = previous.get(anchor.0)?;
    let index = next.iter().position(|item| item.id == row.id).or_else(|| {
        // A prepended tool run can absorb the first overview group and change
        // its ID. The old projected sequence still belongs to that group.
        let seq = row.seq()?;
        next.iter().position(|item| match &item.kind {
            RowKind::Entry { rows, index, .. } => {
                let row = &rows[*index];
                let contains = |entry: &Value| {
                    get(entry, "seqStart")
                        .as_i64()
                        .is_some_and(|start| start <= seq)
                        && get(entry, "seqEnd").as_i64().is_some_and(|end| seq <= end)
                };
                row.group.as_ref().map_or_else(
                    || contains(&row.entry),
                    |group| group.iter().any(|entry| contains(entry)),
                )
            }
            _ => false,
        })
    })?;
    Some((index, anchor.1))
}

/// The session facts one frame presents.
#[derive(Clone)]
struct Snap {
    version: u64,
    timeline: Rc<Vec<Rc<Value>>>,
    agent: Arc<Value>,
    permissions: Vec<(String, Value)>,
    subagents: Vec<Value>,
    turns: HashMap<String, Value>,
    epoch: Option<String>,
    start_cursor: Option<Cursor>,
    has_older: bool,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    sync: SyncStatus,
    execution: ExecutionStatus,
    preparation_error: Option<String>,
    preparation_retrying: bool,
    child_refreshing: bool,
    child_refresh_error: Option<String>,
    running: bool,
    authorized: bool,
    archived: bool,
    create_handoff: bool,
    hydrated: bool,
    initial_sync_failed: bool,
    older_failures: u32,
}

impl Snap {
    fn has_visible_history(&self) -> bool {
        self.timeline
            .iter()
            .any(|entry| is_visible_stream_entry(entry))
    }

    /// Toasts and sync chrome do not change the transcript's retained list.
    fn rows_changed(&self, next: &Self) -> bool {
        !Rc::ptr_eq(&self.timeline, &next.timeline)
            || self.agent != next.agent
            || self.permissions != next.permissions
            || self.turns != next.turns
            || self.epoch != next.epoch
            || self.running != next.running
            || self.loading != next.loading
            || self.start_cursor != next.start_cursor
            || self.has_older != next.has_older
            || self.older_failures != next.older_failures
            || self.subagents != next.subagents
    }

    fn of(s: &ChatSession, create_handoff: bool) -> Self {
        Self {
            version: s.version,
            timeline: s.timeline.clone(),
            agent: Arc::new(s.agent.clone()),
            permissions: s.permissions.clone(),
            subagents: s.subagents.clone(),
            turns: s.turns.clone(),
            epoch: s.epoch.clone(),
            start_cursor: s.start_cursor.clone(),
            has_older: s.has_older,
            loading: s.loading,
            error: s.error.clone(),
            notice: s.notice.clone(),
            sync: s.sync_status(),
            execution: s.execution(),
            preparation_error: s.preparation_error(),
            preparation_retrying: s.preparation_retrying(),
            child_refreshing: s.child_refreshing(),
            child_refresh_error: s.child_refresh_error(),
            running: s.running(create_handoff),
            authorized: s.execution_authorized(create_handoff),
            archived: s.archived(),
            create_handoff,
            hydrated: s.hydrated,
            initial_sync_failed: s.initial_sync_failed,
            older_failures: s.older_failures,
        }
    }
}

/// Facts from the shell's state (`state.hosts[]`, preferences).
#[derive(Default, PartialEq)]
struct HostFacts {
    state_ptr: usize,
    connected: bool,
    connecting: bool,
    label: String,
    host_error: Option<String>,
    maintenance: Option<String>,
    labels: HashMap<String, String>,
    /// (serverId, agent) for every host agent (subagent tracks, titles).
    agents: Vec<(String, Value)>,
    agent_row: Option<Value>,
    preferences: Value,
    server_info: Value,
}

/// Overlay answers the panel waits for.
enum Ask {
    Rename,
    Confirm(&'static str),
    ArchiveSubagent(SubagentRow),
    DeleteFinished,
    DetachSubagent(SubagentRow),
    Rewind { row: String, entry: Rc<Value> },
    Outline,
}

enum Modal {
    Track(TrackPanel),
    Sheet {
        row: String,
        kind: String,
    },
    Usage,
    Lightbox {
        source: LightboxSource,
        epoch: Option<String>,
        file_route: Option<HostRoute>,
        export_available: bool,
        export: LightboxExport,
        scale: f64,
        offset: DVec2,
    },
    Subagent {
        label: String,
        provider: String,
        tab: PanelTarget,
    },
}

/// The active identity stays separate from its retained outgoing pixels.
#[derive(Default)]
struct TrackMotion {
    panel: Option<TrackPanel>,
    anchor: Option<TrackPanel>,
    compact: bool,
    open: bool,
    presence: Transition,
    peer: Transition,
    pair: Option<(TrackPanel, TrackPanel)>,
}

impl TrackMotion {
    fn intent(&self) -> Intent {
        match (self.compact, self.open) {
            (true, true) => Intent::OpenSheet,
            (true, false) => Intent::CloseSheet,
            (false, true) => Intent::OpenPopover,
            (false, false) => Intent::ClosePopover,
        }
    }

    fn show(&mut self, cx: &mut Cx, panel: TrackPanel, compact: bool) {
        let previous = self.panel.replace(panel);
        if previous.is_none() {
            self.anchor = Some(panel);
        }
        self.compact = compact;
        self.open = true;
        self.presence
            .retarget(cx, 1.0, self.intent().resolve().spec);
        if !compact && let Some(old) = previous {
            if self.pair.is_none() && old != panel {
                self.pair = Some((old, panel));
                self.peer.snap(0.0);
            }
            if let Some((from, _)) = self.pair {
                let target = if panel == from { 0.0 } else { 1.0 };
                self.peer
                    .retarget(cx, target, Intent::SwitchPeer.resolve().spec);
            }
        }
    }

    fn close(&mut self, cx: &mut Cx) {
        self.open = false;
        self.peer.snap(self.peer.value());
        self.presence
            .retarget(cx, 0.0, self.intent().resolve().spec);
        if !self.presence.is_moving() {
            *self = Self::default();
        }
    }

    fn advance(&mut self, cx: &mut Cx, event: &Event) -> bool {
        let presence = self.presence.advance(cx, event);
        let peer = self.peer.advance(cx, event);
        presence || peer
    }

    fn needs_capture(&self) -> bool {
        !self.compact && (self.presence.is_moving() || self.peer.is_moving())
    }

    fn visual(&self, panel: TrackPanel, size: DVec2) -> Visual {
        let mut visual = self
            .intent()
            .resolve()
            .sample(self.presence.value(), size)
            .incoming;
        if let Some((from, _)) = self.pair {
            let peer = Intent::SwitchPeer.resolve().sample(self.peer.value(), size);
            let peer = if panel == from {
                peer.outgoing
            } else {
                peer.incoming
            };
            visual.offset += peer.offset;
            visual.opacity *= peer.opacity;
            visual.scale *= peer.scale;
        }
        visual
    }
}

struct TrackLayer {
    list: DrawList2d,
    capture: Option<LayerTexture>,
    layout: Option<tracks::PanelLayout>,
    quad: Area,
}

impl TrackLayer {
    fn new(cx: &mut Cx) -> Self {
        Self {
            list: DrawList2d::new(cx),
            capture: None,
            layout: None,
            quad: Area::Empty,
        }
    }

    fn paint_size(size: DVec2, compact: bool) -> DVec2 {
        size + dvec2(0.0, if compact { 0.0 } else { 4.0 })
    }

    fn visual_rect(layout: &tracks::PanelLayout, visual: Visual, compact: bool) -> Rect {
        let pivot = if compact || !layout.above { 0.0 } else { 1.0 };
        overlay::layer_rect(layout.card, visual, dvec2(0.5, pivot))
    }

    fn release_capture(&mut self, cx: &mut Cx) {
        overlay::release_layer_quad(cx, std::mem::take(&mut self.quad));
        if let Some(capture) = self.capture.take() {
            capture.detach(cx);
        }
    }
}

const TRACKS_PANELS: [TrackPanel; 2] = [TrackPanel::Tasks, TrackPanel::Subagents];
const TRACK_SCRIM_ALPHA: f64 = 115.0 / 255.0;

enum LightboxSource {
    Assistant { url: String, row: String },
    User { entry: Rc<Value>, index: usize },
}

impl LightboxSource {
    fn user_image(entry: &Value, index: usize) -> Option<&Value> {
        list(get(get(entry, "item"), "images")).get(index)
    }

    fn original(&self, cwd: Option<&str>) -> Option<Original<'_>> {
        match self {
            Self::Assistant { url, .. } => Original::assistant(url, cwd),
            Self::User { entry, index } => Original::user(Self::user_image(entry, *index)?),
        }
    }
}

/// What a session reveal (older pages until a seq is loaded) was for.
enum RevealFor {
    Find(u64),
    Prompt(i64),
}

/// One "Delete finished subagents" run: provider children in one daemon
/// request, managed children one `delete_agent_request` at a time.
#[derive(Default)]
struct DeleteRun {
    /// Managed children still to delete, last first.
    queue: Vec<SubagentRow>,
    /// A managed child's deletion is awaiting its answer.
    in_flight: bool,
    /// The provider batch is awaiting the daemon's answer.
    provider_pending: bool,
    completed: usize,
    failed: usize,
    total: usize,
}

/// One independently retained piece of chrome, including its current hit geometry.
/// Parent composition and sibling paints must not invalidate these lists.
struct ChromeLayer {
    list: DrawList2d,
    canvas: Canvas,
    area: Area,
    visited: bool,
}

impl ChromeLayer {
    fn new(cx: &mut Cx) -> Self {
        Self {
            list: DrawList2d::new(cx),
            canvas: Canvas::default(),
            area: Area::Empty,
            visited: false,
        }
    }

    fn paint<T>(
        &mut self,
        cx: &mut Cx2d,
        kit: &mut ChatKit,
        state: &mut RowState,
        look: Look,
        rect: Rect,
        paint: impl FnOnce(&mut Cx2d, &mut Draw) -> T,
    ) -> Option<T> {
        self.visited = true;
        if self.list.begin(cx, Walk::abs_rect(rect)).is_err() {
            return None;
        }
        kit.begin_layer(cx);
        cx.add_aligned_rect_area(&mut self.area, rect);
        self.canvas.begin(rect.pos);
        let mut d = Draw {
            kit,
            canvas: &mut self.canvas,
            look,
            state,
            owner: None,
            now: now_seconds(),
            area: self.area,
            animating: false,
            indicators: None,
        };
        let result = paint(cx, &mut d);
        self.list.end(cx);
        Some(result)
    }

    fn finish_frame(&mut self, cx: &mut Cx2d) {
        if !std::mem::take(&mut self.visited) {
            self.list.release(cx);
            self.canvas.hits.clear();
        }
    }
}

const NON_READY: usize = 0;
const NON_READY_INDICATOR: usize = 1;
const SUBTITLE: usize = 2;
const EMPTY: usize = 3;
const RAIL: usize = 4;
const TRACKS: usize = 5;
const JUMP: usize = 6;
const FIND: usize = 7;
const CALLOUTS: usize = 8;
const HISTORY: usize = 9;
const TOAST: usize = 10;
const ARCHIVING: usize = 11;
const CHROME_LAYERS: usize = 12;

#[derive(Live, Widget)]
pub struct ChatPanel {
    /// Texture placeholders are owned by the region that actually drew them;
    /// composer placeholders are handled by ComposerPanel itself.
    #[rust]
    timeline_texture_wait: Option<u64>,
    #[rust]
    modal_texture_wait: Option<u64>,
    #[redraw]
    #[rust]
    area: Area,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[live]
    kit: ChatKit,
    #[live]
    modal_kit: ChatKit,
    #[live]
    painter: Painter,
    #[live]
    provider_icon: DrawIcon,
    #[live]
    list: WidgetRef,
    #[live]
    composer: WidgetRef,
    #[live]
    find_field: WidgetRef,
    /// agent-panel.tsx ChatSurface: the whole pane takes dropped files for its composer.
    #[live]
    file_drop: FileDropZone,
    #[live]
    sheet_template: Option<LivePtr>,

    #[rust(DrawList2d::new(cx))]
    timeline_list: DrawList2d,
    #[rust(DrawList2d::new(cx))]
    composer_list: DrawList2d,
    #[rust]
    chrome_layers: Vec<ChromeLayer>,
    #[rust]
    painted_look: Option<Look>,
    #[rust]
    non_ready_indicator: Option<DVec2>,
    #[rust(DrawList2d::new(cx))]
    modal_content_list: DrawList2d,
    #[rust(DrawList2d::new(cx))]
    cover_list: DrawList2d,
    #[rust]
    cover_area: Area,
    #[rust(DrawList2d::new(cx))]
    find_list: DrawList2d,
    #[rust(DrawList2d::new(cx))]
    modal_list: DrawList2d,
    #[rust]
    overlay_canvas: Canvas,
    #[rust]
    modal_canvas: Canvas,
    #[rust]
    modal_area: Area,
    #[rust]
    chrome_state: RowState,
    #[rust]
    modal_state: RowState,
    #[rust]
    uid: u64,
    #[rust]
    target: Option<ChatTarget>,
    #[rust]
    visible: bool,
    #[rust]
    focused: bool,
    #[rust]
    ensured: bool,
    #[rust]
    snap: Option<Rc<Snap>>,
    #[rust]
    snapshot_stale: bool,
    #[rust]
    host: HostFacts,
    #[rust]
    pending_actions: Vec<String>,

    // Stream model.
    #[rust]
    items: Vec<RowItem>,
    #[rust]
    model_sig: Option<ModelSig>,
    /// Finished turns the reader unfolded, by `turn_key`; `open_turns_revision`
    /// counts changes so the stream model rebuilds.
    #[rust]
    open_turns: HashSet<String>,
    #[rust]
    open_turns_revision: u64,
    #[rust]
    stream_rows: Rc<Vec<StreamRow>>,
    #[rust]
    footer_at_end: bool,
    #[rust]
    row_states: HashMap<String, Rc<RefCell<RowState>>>,
    #[rust]
    presentation_clock: u64,
    #[rust]
    ctx: Option<Rc<RowContext>>,
    #[rust(true)]
    sticky: bool,
    #[rust(true)]
    at_bottom: bool,
    #[rust]
    older: OlderPaging,
    #[rust]
    expanded_tools: bool,
    #[rust]
    stream_rect: Rect,
    #[rust]
    content_rect: Rect,
    #[rust]
    composer_h: f64,
    #[rust]
    detail_motion: SheetMotion,
    #[rust]
    detail_area: Area,
    /// A row jump (seqStart, offset below the row top, find match) whose row
    /// is not in the stream model yet: applied after the next model rebuild.
    #[rust]
    pending_jump: Option<(i64, f64, bool)>,

    // Chat find.
    #[rust]
    find: FindController,
    #[rust]
    find_open: bool,
    #[rust]
    find_focus_seen: u64,
    #[rust]
    find_field_rect: Option<Rect>,
    #[rust]
    find_card_h: f64,
    #[rust]
    find_timers: Vec<(Timer, u64, bool)>,
    #[rust]
    find_scroll: Option<(String, u32)>,
    /// A row whose top edge is brought on screen after the next layout if it
    /// lies above the stream (a question card that switched questions).
    #[rust]
    reveal_top: Option<String>,
    /// `reveal_attention` was requested: applied on the next draw with a stream.
    #[rust]
    reveal_attention: bool,
    /// The first pending permission row to bring on screen after the next layout.
    #[rust]
    attention_row: Option<String>,
    #[rust]
    last_shift: bool,
    /// The reader scrolled the stream (wheel or touch drag) since it last
    /// stuck: only then do list scroll actions detach or re-stick it
    /// (bottom-anchor-controller.ts: scrolls the reader is not driving never
    /// detach; PortalList also reports scroll-bar syncs as scrolls).
    #[rust]
    reader_scrolled: bool,

    // Outline.
    #[rust]
    rail: Rail,
    #[rust]
    prompts_epoch: Option<String>,
    #[rust]
    prompts_timeline: usize,
    #[rust]
    prompts_latest: i64,
    #[rust]
    prompts_request: u64,
    #[rust]
    active_prompt: Option<i64>,

    // Tracks.
    #[rust]
    track_anchors: [Area; 2],
    #[rust]
    track_motion: TrackMotion,
    #[rust]
    track_layers: [Option<TrackLayer>; 2],
    #[rust(DrawList2d::new(cx))]
    track_overlay: DrawList2d,
    #[live]
    track_composite: DrawLayerTexture,
    #[live]
    track_scrim: DrawLayerScrim,
    #[rust]
    track_scrim_area: Area,
    #[rust]
    track_backdrop: Area,
    #[rust]
    track_hit_rect: Option<(Rect, f64)>,
    #[rust]
    track_painted_panel: Option<TrackPanel>,
    #[rust]
    track_dirty: Cell<bool>,
    #[rust]
    track_bounds: Rect,
    #[rust]
    track_anchor_rect: Option<Rect>,
    #[rust]
    track_inset: f64,
    #[rust]
    track_dpi: f64,
    #[rust]
    track_popover: Option<AnchoredPopover>,
    #[rust]
    track_popover_area: Area,
    #[rust]
    delete_status: DeleteFinished,
    #[rust]
    delete_run: DeleteRun,
    /// Managed children archived or deleted from the track, hidden until the
    /// host's agent list drops them (`serverId:agentId`).
    #[rust]
    pending_removal: HashSet<String>,
    #[rust]
    requested_task_stops: HashSet<String>,
    #[rust]
    children_requested: bool,
    #[rust]
    providers: Vec<Value>,
    #[rust]
    providers_requested: bool,
    #[rust]
    providers_request: Option<String>,

    // Panel state.
    #[rust]
    archiving: bool,
    #[rust]
    unarchiving: bool,
    #[rust]
    unarchive_error: Option<String>,
    #[rust]
    error: Option<(u64, String)>,
    #[rust]
    error_seq: u64,
    #[rust]
    dismissed_toasts: HashSet<String>,
    #[rust]
    toast_shown: Option<(String, f64)>,
    #[rust]
    toast_timer: Timer,
    #[rust]
    asks: HashMap<u64, Ask>,
    #[rust]
    modal: Option<Modal>,
    #[rust]
    sheet_panel: Option<WidgetRef>,
    #[rust]
    sheet_content: Rect,
    #[rust]
    reveals: HashMap<u64, RevealFor>,
    #[rust]
    next_token: u64,
    #[rust]
    attention: Option<AttentionClear>,
    #[rust]
    attention_focused: bool,
    #[rust]
    composer_target: Option<crate::features::composer::ComposerTarget>,
    #[rust]
    composer_version: Option<u64>,
    #[rust]
    lightbox_drag: Option<DVec2>,
    /// A scroll bar thumb drag on the modal (true) or chrome canvas.
    #[rust]
    bar_drag: Option<(super::scrollbar::BarDrag, bool)>,
    #[rust(crate::common::widgets::touch_scroll())]
    modal_touch_scroll: TouchScroll,
    #[rust(crate::common::widgets::touch_scroll())]
    modal_horizontal_scroll: TouchScroll,
    #[rust]
    pressed: TapTarget<HitTarget>,
    #[rust]
    modal_pressed: TapTarget<HitTarget>,
    /// The owning subagent sheet, assigned by its dispatcher before child input.
    #[rust]
    input_parent: Option<u64>,
    /// The core handle retained when the view attaches (draws without a UiContext).
    #[rust]
    core: Option<paseo_gui_core::CoreHandle>,
    #[rust]
    window_modal: Option<WindowModal>,
    #[rust]
    modal_rect: Rect,
}

impl LiveHook for ChatPanel {
    fn after_new_from_doc(&mut self, _cx: &mut Cx) {
        self.uid = NEXT_UID.with(|n| {
            let id = n.get();
            n.set(id + 1);
            id
        });
        self.sticky = true;
        self.at_bottom = true;
    }
}

fn optional(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// The shell's `state.hosts[]` record of `host_id` (by serverId).
fn host_row<'a>(state: &'a Value, host_id: &str) -> Option<&'a Value> {
    list(get(state, "hosts"))
        .iter()
        .find(|h| get(h, "serverId") == host_id)
}

fn error_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => non_empty_str(get(value, "message")).map(str::to_owned),
        _ => None,
    }
}

impl ChatPanel {
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn transcript_draw_list_id(&self) -> DrawListId {
        self.timeline_list.id()
    }

    fn redraw_timeline(&self, cx: &mut Cx) {
        self.timeline_list.redraw(cx);
    }

    fn redraw_chrome(&self, cx: &mut Cx) {
        for layer in &self.chrome_layers {
            layer.list.redraw(cx);
        }
        self.area.redraw(cx);
    }

    fn redraw_chrome_part(&self, cx: &mut Cx, part: usize) {
        if let Some(layer) = self.chrome_layers.get(part) {
            layer.list.redraw(cx);
        } else {
            self.area.redraw(cx);
        }
    }

    fn redraw_modal(&self, cx: &mut Cx) {
        self.track_dirty.set(true);
        self.modal_content_list.redraw(cx);
        self.area.redraw(cx);
    }

    /// A changed session/target can change dock and stream geometry as well as rows.
    fn redraw_session(&self, cx: &mut Cx) {
        self.redraw_timeline(cx);
        self.composer_list.redraw(cx);
        self.redraw_chrome(cx);
        self.redraw_modal(cx);
    }

    fn finish_chrome(&mut self, cx: &mut Cx2d) {
        self.overlay_canvas.begin(self.content_rect.pos);
        for layer in &mut self.chrome_layers {
            layer.finish_frame(cx);
            let offset = layer.canvas.origin - self.content_rect.pos;
            self.overlay_canvas.hits.extend(
                layer
                    .canvas
                    .hits
                    .iter()
                    .map(|(rect, hit)| (rect.translate(offset), hit.clone())),
            );
        }
    }

    // ── target and activity ─────────────────────────────────────────────

    pub fn set_target(&mut self, cx: &mut Cx, target: ChatTarget) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        let changed_session = self.target.as_ref().map(ChatTarget::key) != Some(target.key());
        if changed_session {
            with_store(|store| store.release_view(cx, self.uid));
            self.reset_session_state(cx);
            // A new conversation opens at its latest edge.
            self.list.as_portal_list().set_tail_range(true);
        } else if matches!(self.modal, Some(Modal::Lightbox { .. }))
            && self.target.as_ref().is_some_and(|old| {
                old.cwd != target.cwd
                    || old.workspace_id != target.workspace_id
                    || old.tab_key != target.tab_key
            })
        {
            self.dismiss_modal(cx);
        }
        self.target = Some(target);
        self.ensured = false;
        self.redraw_session(cx);
    }

    fn reset_session_state(&mut self, cx: &mut Cx) {
        self.pressed.cancel();
        self.dismiss_modal(cx);
        self.snap = None;
        self.snapshot_stale = true;
        self.providers_request = None;
        self.providers_requested = false;
        self.items.clear();
        self.model_sig = None;
        self.open_turns.clear();
        self.row_states.clear();
        self.sticky = true;
        self.at_bottom = true;
        self.older = OlderPaging::default();
        self.find = FindController::default();
        self.find_open = false;
        self.rail = Rail::default();
        self.prompts_epoch = None;
        self.prompts_timeline = 0;
        self.prompts_latest = -1;
        self.prompts_request += 1;
        self.active_prompt = None;
        self.delete_status = DeleteFinished::Idle;
        self.delete_run = DeleteRun::default();
        self.pending_removal.clear();
        self.requested_task_stops.clear();
        self.children_requested = false;
        self.archiving = false;
        self.unarchiving = false;
        self.unarchive_error = None;
        self.attention = None;
        self.composer_version = None;
        self.reveals.clear();
    }

    pub fn set_visible(&mut self, cx: &mut Cx, visible: bool) {
        if self.visible != visible {
            self.visible = visible;
            with_store(|store| store.set_view_visible(cx, self.uid, visible));
            self.composer
                .as_composer_panel()
                .set_visible(cx, visible && self.composer_shown());
            if visible {
                self.release_reveal_backlog();
            } else {
                self.pressed.cancel();
                self.rail.hide(cx);
                self.dismiss_modal(cx);
            }
            self.redraw_session(cx);
        }
    }

    /// Text that arrived while this conversation was off screen (hidden tab or
    /// pane, app in the background) is shown whole when it returns.
    fn release_reveal_backlog(&self) {
        for state in self.row_states.values() {
            state.borrow_mut().release_reveal_backlog();
        }
    }

    pub fn set_focused(&mut self, cx: &mut Cx, focused: bool) {
        if self.focused != focused {
            self.focused = focused;
            self.composer_list.redraw(cx);
        }
    }

    fn input_is_modal(&self) -> bool {
        match self.modal {
            Some(Modal::Sheet { .. }) => self.detail_motion.open,
            Some(_) => self.track_popover.is_none(),
            None => false,
        }
    }

    fn reconcile_track_popover(&mut self, cx: &mut Cx) {
        if self.track_popover.as_ref().is_some_and(|p| !p.is_open()) {
            self.close_track(cx);
        }
    }

    fn sync_window_modal(&mut self, cx: &mut Cx, ui: &UiContext) {
        let composer_modal = self.composer.as_composer_panel().captures_input();
        if !matches!(self.modal, Some(Modal::Track(_))) {
            self.end_track_input(cx);
        } else if composer_modal && self.track_popover.is_some() {
            self.close_track(cx);
        }
        let was_open = self.window_modal.is_some();
        let open = self.visible && (self.input_is_modal() || composer_modal);
        let owner = self.widget_uid().0;
        WindowModal::sync(&mut self.window_modal, ui, owner, open);
        if was_open != self.window_modal.is_some() {
            self.pressed.cancel();
            self.modal_pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
        }
    }

    fn modal_branch_accepts_input(&self, window: &str) -> bool {
        overlay::window_modal_accepts_owner(window, self.widget_uid().0)
            || self
                .input_parent
                .is_some_and(|parent| overlay::window_modal_accepts_owner(window, parent))
            || self.sheet_panel.as_ref().is_some_and(|panel| {
                panel
                    .as_chat_panel()
                    .borrow()
                    .is_some_and(|child| child.modal_branch_accepts_input(window))
            })
    }

    fn modal_tap_target(&self, abs: DVec2, surface: bool) -> Option<HitTarget> {
        self.chrome_hit(abs, surface).or_else(|| {
            if surface && matches!(self.modal, Some(Modal::Track(_))) {
                return (self.track_motion.compact && self.track_bounds.contains(abs))
                    .then(|| HitTarget::Chrome("track-close".into()));
            }
            (surface
                && matches!(
                    self.modal,
                    Some(Modal::Lightbox { .. } | Modal::Sheet { .. })
                )
                && self.modal_rect.contains(abs))
            .then(|| HitTarget::Chrome("scrim".into()))
        })
    }

    fn end_track_input(&mut self, cx: &mut Cx) {
        if !self.track_motion.open {
            return;
        }
        self.track_motion.close(cx);
        self.track_popover = None;
        self.track_hit_rect = None;
        if self.track_motion.panel.is_none() {
            self.reset_tracks(cx);
        }
        self.modal_pressed.cancel();
        self.modal_touch_scroll.reset();
        self.modal_horizontal_scroll.reset();
        if self.bar_drag.as_ref().is_some_and(|(_, modal)| *modal) {
            self.bar_drag = None;
        }
        self.area.redraw(cx);
        self.redraw_chrome_part(cx, TRACKS);
    }

    fn close_track(&mut self, cx: &mut Cx) {
        self.end_track_input(cx);
        self.modal = None;
        self.window_modal = None;
    }

    fn reset_tracks(&mut self, cx: &mut Cx) {
        self.track_motion = TrackMotion::default();
        self.track_popover = None;
        self.track_hit_rect = None;
        self.track_painted_panel = None;
        self.track_popover_area = Area::Empty;
        self.track_backdrop = Area::Empty;
        self.track_scrim_area = Area::Empty;
        for mut layer in self.track_layers.iter_mut().filter_map(Option::take) {
            layer.release_capture(cx);
        }
    }

    fn dismiss_modal(&mut self, cx: &mut Cx) {
        self.reset_tracks(cx);
        self.detail_motion = SheetMotion::default();
        self.modal_pressed.cancel();
        if self.bar_drag.as_ref().is_some_and(|(_, modal)| *modal) {
            self.bar_drag = None;
        }
        self.modal_touch_scroll.reset();
        self.modal_horizontal_scroll.reset();
        if let Some(panel) = self.sheet_panel.take() {
            panel.as_chat_panel().set_visible(cx, false);
            if let Some(child) = panel.as_chat_panel().borrow() {
                with_store(|store| store.release_view(cx, child.uid));
            }
        }
        self.modal = None;
        self.window_modal = None;
        self.modal_state = RowState::default();
        self.lightbox_drag = None;
        self.redraw_modal(cx);
        self.redraw_chrome_part(cx, TRACKS);
    }

    /// The tab menu picked a conversation action; runs with the next event.
    pub fn run_conversation_action(&mut self, cx: &mut Cx, action: &str) {
        self.pending_actions.push(action.to_owned());
        SignalToUI::set_ui_signal();
        self.area.redraw(cx);
    }

    /// A notification opened this session: on the next draw the first pending permission or
    /// question card is brought on screen (else the newest edge), whatever the reader's
    /// earlier scroll position.
    pub fn reveal_attention(&mut self, cx: &mut Cx) {
        self.reveal_attention = true;
        self.pending_jump = None;
        self.reveal_top = None;
        self.redraw_timeline(cx);
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn with_session<R>(
        &self,
        cx: &mut Cx,
        ui: &mut UiContext,
        f: impl FnOnce(&mut ChatSession, &mut Io) -> R,
    ) -> Option<R> {
        let key = self.target.as_ref()?.key();
        let mut effects = std::mem::take(&mut ui.effects);
        let result = with_store(|store| store.with_session(cx, &mut effects, &key, f));
        effects.append(&mut ui.effects);
        ui.effects = effects;
        result
    }

    fn show_failure(&mut self, cx: &mut Cx, message: impl Into<String>) {
        self.error_seq += 1;
        self.error = Some((self.error_seq, message.into()));
        self.redraw_chrome_part(cx, TOAST);
    }

    fn feature(&self, name: &str) -> bool {
        feature(&self.host.server_info, name)
    }

    fn row_state(&mut self, id: &str) -> Rc<RefCell<RowState>> {
        self.row_states.entry(id.to_owned()).or_default().clone()
    }

    fn trim_row_presentations(&mut self, visible: &HashSet<String>) {
        // Keep nearby recently visited rows warm, but never evict a row in the
        // current measurement/final-paint pass, even in a very tall viewport.
        let mut resident: Vec<_> = self
            .row_states
            .iter()
            .filter_map(|(id, state)| {
                let state = state.borrow();
                (state.presentation_used != 0 && !visible.contains(id))
                    .then(|| (state.presentation_used, id.clone()))
            })
            .collect();
        resident.sort_unstable_by_key(|(used, _)| *used);
        let evict = resident
            .len()
            .saturating_sub(128usize.saturating_sub(visible.len()));
        for (_, id) in resident.into_iter().take(evict) {
            if let Some(state) = self.row_states.get(&id) {
                let mut state = state.borrow_mut();
                state.evict_presentation();
                state.presentation_used = 0;
            }
        }
    }

    // ── snapshots ───────────────────────────────────────────────────────

    /// A root can first be mounted by the draw that selects it. Its initial
    /// snapshot must not wait for the next event to attach the replica.
    fn ensure_session(&mut self, cx: &mut Cx, ui: &mut UiContext, visible: bool) {
        if !self.ensured
            && let Some(target) = &self.target
        {
            with_store(|store| store.ensure(cx, ui, &target.key(), self.uid, visible));
            self.ensured = true;
            self.core = Some(ui.core.clone());
        }
    }

    /// Copies the session's presented facts when its version moved.
    fn refresh_snapshot(&mut self, cx: &mut Cx) -> bool {
        if !self.visible {
            self.snapshot_stale = true;
            return false;
        }
        let Some(target) = &self.target else {
            return false;
        };
        let key = target.key();
        let seen = self.snap.as_ref().map(|s| s.version);
        let create_handoff =
            !key.is_subagent() && bridge::create_handoff_active(&key.host_id, &key.agent_id);
        let next = with_store(|store| {
            let session = store.sessions.get(&key)?;
            if Some(session.version) == seen
                && self
                    .snap
                    .as_ref()
                    .is_some_and(|s| s.create_handoff == create_handoff)
            {
                return None;
            }
            Some(Snap::of(session, create_handoff))
        });
        match next {
            Some(snap) => {
                let rows_changed = self.snap.as_ref().is_none_or(|old| old.rows_changed(&snap));
                if rows_changed {
                    self.redraw_session(cx);
                } else {
                    self.redraw_chrome_part(cx, TOAST);
                    let chrome_changed = self.snap.as_ref().is_none_or(|old| {
                        old.sync != snap.sync
                            || old.error != snap.error
                            || old.execution != snap.execution
                            || old.preparation_error != snap.preparation_error
                            || old.preparation_retrying != snap.preparation_retrying
                            || old.child_refreshing != snap.child_refreshing
                            || old.child_refresh_error != snap.child_refresh_error
                            || old.create_handoff != snap.create_handoff
                            || old.hydrated != snap.hydrated
                            || old.initial_sync_failed != snap.initial_sync_failed
                    });
                    if chrome_changed {
                        for part in [NON_READY, NON_READY_INDICATOR, CALLOUTS, HISTORY, EMPTY] {
                            self.redraw_chrome_part(cx, part);
                        }
                    }
                }
                self.requested_task_stops
                    .retain(|token| task_controls::has_offer(task_controls::offered(&snap.agent), token));
                self.snap = Some(Rc::new(snap));
                true
            }
            None => false,
        }
    }

    fn refresh_host(&mut self, cx: &mut Cx, ui: &UiContext) {
        let ptr = Arc::as_ptr(&ui.state) as usize;
        let Some(target) = &self.target else { return };
        let server_info = ui.server_infos.get(&target.host_id).cloned();
        if ptr == self.host.state_ptr
            && server_info
                .as_ref()
                .is_none_or(|s| *s == self.host.server_info)
        {
            return;
        }
        let state = &*ui.state;
        let mut facts = HostFacts {
            state_ptr: ptr,
            ..Default::default()
        };
        let row = host_row(state, &target.host_id);
        facts.connected = row.is_some_and(|r| get(r, "status") == "connected");
        facts.connecting = row.is_some_and(|r| get(r, "status") == "connecting");
        facts.label = row
            .map(crate::common::host_display_name)
            .unwrap_or(&target.host_id)
            .to_owned();
        facts.host_error = row
            .and_then(|r| error_text(get(r, "error")).or_else(|| error_text(get(r, "lastError"))));
        facts.maintenance = row.and_then(|r| optional(get(r, "maintenanceOperationId")));
        for host in list(get(state, "hosts")) {
            let server_id = str_of(get(host, "serverId")).to_owned();
            facts.labels.insert(
                server_id.clone(),
                crate::common::host_display_name(host).to_owned(),
            );
            for raw in ui
                .directory(host)
                .into_iter()
                .flat_map(|directory| directory.agents.iter())
                .map(Arc::as_ref)
            {
                let agent = if get(raw, "agent").is_object() {
                    get(raw, "agent")
                } else {
                    raw
                };
                facts.agents.push((server_id.clone(), agent.clone()));
            }
        }
        facts.agent_row = facts
            .agents
            .iter()
            .find(|(server, agent)| {
                *server == target.host_id && get(agent, "id") == target.agent_id.as_str()
            })
            .map(|(_, a)| a.clone());
        facts.preferences = ui.preferences().clone();
        facts.server_info = server_info
            .or_else(|| {
                row.map(|r| get(r, "serverInfo").clone())
                    .filter(Value::is_object)
            })
            .unwrap_or_else(|| super::store::server_info(&target.host_id));
        self.host.state_ptr = ptr;
        if self.host != facts {
            self.redraw_session(cx);
        }
        self.host = facts;
    }

    fn preference_flag(&self, key: &str) -> Option<bool> {
        get(&self.host.preferences, key).as_bool()
    }

    fn outline_enabled(&self) -> bool {
        self.preference_flag("chatOutlineEnabled") != Some(false)
    }

    fn look(&self, width: f64) -> Look {
        let palette = crate::theme::current_palette();
        let typo = crate::theme::current_typography();
        let syntax = str_or(get(&self.host.preferences, "syntaxTheme"), "one");
        Look {
            palette,
            typo,
            compact: crate::common::is_compact_width(width),
            mobile: MOBILE,
            list_inset: if width < 720.0 { 12.0 } else { 16.0 },
            syntax: paseo_highlight::SyntaxThemeId::from_name(syntax)
                .unwrap_or(paseo_highlight::SyntaxThemeId::One),
        }
    }

    // ── stream model ────────────────────────────────────────────────────

    /// subagents/select.ts useSubagentsForParent.
    fn subagent_rows(&mut self) -> Vec<SubagentRow> {
        let (Some(target), Some(snap)) = (self.target.clone(), self.snap.clone()) else {
            return Vec::new();
        };
        let nesting = self.feature("providerSubagentNesting");
        if target.is_subagent() && !nesting {
            return Vec::new();
        }
        let mut rows = Vec::new();
        if !target.is_subagent() {
            for (server_id, child) in &self.host.agents {
                let labels = get(child, "labels");
                let id = str_of(get(child, "id")).to_owned();
                let parent_server = str_or(get(labels, "paseo.parent-server-id"), server_id);
                if get(labels, "paseo.parent-agent-id") != target.agent_id.as_str()
                    || parent_server != target.host_id
                    || !get(child, "archivedAt").is_null()
                    || self.pending_removal.contains(&format!("{server_id}:{id}"))
                {
                    continue;
                }
                rows.push(SubagentRow {
                    paseo: true,
                    id,
                    server_id: server_id.clone(),
                    provider: str_of(get(child, "provider")).to_owned(),
                    status: optional(get(child, "status")),
                    title: optional(get(child, "title")),
                    description: None,
                    subtitle: None,
                    turn_open: get(get(child, "turn"), "phase") == "open"
                        || !get(child, "activeTurn").is_null(),
                    created_ms: parse_time(get(child, "createdAt")).unwrap_or(0),
                    workspace_id: optional(get(child, "workspaceId")),
                    cwd: optional(get(child, "cwd")),
                    parent_agent_id: None,
                    agent: child.clone(),
                });
            }
        }
        for child in &snap.subagents {
            let id = str_of(get(child, "id"));
            let parent = get(child, "parentSubagentId");
            let skip = if target.is_subagent() {
                parent.as_str() != target.subagent_id.as_deref()
            } else {
                !parent.is_null() && nesting
            };
            // Hidden by "Delete finished subagents" against a daemon that cannot
            // delete them (app-wide; a child that runs again is un-hidden by the
            // store when its `running` descriptor arrives).
            if skip
                || with_store(|store| store.subagent_hidden(&target.host_id, &target.agent_id, id))
            {
                continue;
            }
            let id = id.to_owned();
            rows.push(SubagentRow {
                paseo: false,
                id,
                server_id: target.host_id.clone(),
                provider: str_or(get(child, "provider"), str_of(get(&snap.agent, "provider")))
                    .to_owned(),
                status: optional(get(child, "status")),
                title: optional(get(child, "title")),
                description: optional(get(child, "description")),
                subtitle: optional(get(child, "subtitle")),
                turn_open: false,
                created_ms: parse_time(get(child, "createdAt")).unwrap_or(0),
                workspace_id: None,
                cwd: optional(get(child, "cwd")),
                parent_agent_id: Some(target.agent_id.clone()),
                agent: Value::Null,
            });
        }
        rows.sort_by_key(|r| r.created_ms);
        rows
    }

    fn latest_tasks(&self) -> Vec<Value> {
        let Some(snap) = &self.snap else {
            return Vec::new();
        };
        snap.timeline
            .iter()
            .rev()
            .map(|e| get(e, "item"))
            .find(|i| get(i, "type") == "todo")
            .map(|i| list(get(i, "items")).to_vec())
            .unwrap_or_default()
    }

    fn offered_provider_tasks<'a>(&self, snap: Option<&'a Snap>) -> &'a [Value] {
        if !self.host.connected
            || !self.feature("providerTaskStop")
            || self.archiving
            || self.target.as_ref().is_none_or(ChatTarget::is_subagent)
        {
            return &[];
        }
        snap
            .filter(|snap| snap.authorized && !snap.archived)
            .map(|snap| task_controls::offered(&snap.agent))
            .unwrap_or_default()
    }

    fn stop_provider_task(&mut self, cx: &mut Cx, ui: &mut UiContext, token: &str) {
        if !task_controls::has_offer(self.offered_provider_tasks(self.snap.as_deref()), token)
            || !self.requested_task_stops.insert(token.to_owned())
        {
            return;
        }
        if self
            .with_session(cx, ui, |s, io| {
                s.action(
                    io,
                    ActionKind::StopProviderTask { token: token.to_owned() },
                    "agent.provider_tasks.stop.request",
                    "agent.provider_tasks.stop.response",
                    json!({"taskToken":token}),
                )
            })
            .is_none()
        {
            self.requested_task_stops.remove(token);
        }
    }

    fn tracks_visible(&mut self) -> bool {
        let Some(snap) = &self.snap else { return false };
        if snap.archived || self.archiving {
            return false;
        }
        let subagent = self.target.as_ref().is_some_and(ChatTarget::is_subagent);
        let tasks = if subagent {
            Vec::new()
        } else {
            self.latest_tasks()
        };
        let rows = self.subagent_rows();
        !rows.is_empty()
            || !tasks.is_empty()
            || !self.offered_provider_tasks(self.snap.as_deref()).is_empty()
            || self.delete_status != DeleteFinished::Idle
    }

    /// agent-panel.dart `_streamRows` + `_timeline` items, oldest first.
    fn rebuild_items(&mut self, look: Look, track_height: f64) {
        let Some(snap) = self.snap.clone() else {
            return;
        };
        let Some(target) = self.target.clone() else {
            return;
        };
        let level = DetailLevel::from_preference(get(&self.host.preferences, "toolCallDetailLevel"));
        let provider = str_of(get(&snap.agent, "provider")).to_owned();
        let outgoing = if target.is_subagent() {
            0
        } else {
            bridge::outgoing_revision()
        };
        let tracks = self.tracks_visible();
        let clearance = if tracks {
            tracks::tail_clearance(look.compact, track_height)
        } else {
            0.0
        };
        let permission_count = if target.is_subagent() {
            0
        } else {
            snap.permissions.len()
        };
        let sig = ModelSig {
            timeline: Rc::as_ptr(&snap.timeline) as usize,
            outgoing,
            level,
            open_turns: self.open_turns_revision,
            provider: provider.clone(),
            running: snap.running,
            permissions: permission_count,
            version: snap.version,
            track_clearance: clearance,
            loading_older: self.older.loading(),
        };
        if self.model_sig.as_ref() == Some(&sig) {
            return;
        }
        self.model_sig = Some(sig);
        let mut entries: Vec<Rc<Value>> = snap.timeline.iter().cloned().collect();
        let mut queued: Vec<Value> = Vec::new();
        if !target.is_subagent() {
            let echoed: HashSet<String> = entries
                .iter()
                .filter_map(|row| {
                    get(get(row, "item"), "clientMessageId")
                        .as_str()
                        .map(str::to_owned)
                })
                .collect();
            queued = queued_messages(&snap.agent, &echoed);
            let held: HashSet<&str> = queued
                .iter()
                .filter_map(|entry| get(entry, "clientMessageId").as_str())
                .collect();
            let mut sending = Vec::new();
            for row in bridge::outgoing_rows(
                &target.host_id,
                &target.workspace_id,
                &target.agent_id,
                &|id| echoed.contains(id) || held.contains(id),
            ) {
                // Sent into a running turn, a message waits below the reply with the queued
                // ones rather than splitting it, and keeps that place once it is queued.
                if snap.running {
                    sending.push(row);
                } else {
                    entries.push(Rc::new(row));
                }
            }
            // The sending composer kept the images and attachments the host does not echo.
            for entry in &mut queued {
                let client = str_of(get(entry, "clientMessageId")).to_owned();
                if let Some(local) =
                    bridge::sent_attachments(&target.host_id, &target.agent_id, &client)
                {
                    entry["item"]["images"] = get(&local, "images").clone();
                    entry["item"]["attachments"] = get(&local, "attachments").clone();
                }
            }
            queued.extend(sending);
        }
        let rows = Rc::new(build_stream_rows(
            &entries,
            level,
            &provider,
            &self.open_turns,
            snap.running,
        ));
        let running = snap.running;
        let auxiliary = auxiliary_footer_host(&rows, running);
        let has_auxiliary = running || auxiliary.is_some();
        let live_from = if running {
            rows.iter()
                .rposition(|r| r.kind() == "user_message")
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            rows.len()
        };
        let timing = derive_turn_timing(&rows, running);
        let mut used = HashSet::new();
        let mut row_id = |i: usize| -> String {
            let row = &rows[i];
            let seq = get(&row.entry, "seqStart");
            let client = get(get(&row.entry, "item"), "clientMessageId");
            let id = if row.group.is_some() {
                format!("group:{}", row.group_id())
            } else if let Some(fold) = &row.fold {
                format!("turn:{}", fold.key)
            } else if let Some(seq) = seq.as_i64() {
                format!("seq:{seq}")
            } else if let Some(client) = client.as_str() {
                format!("pending:{client}")
            } else {
                format!("index:{i}")
            };
            if used.insert(id.clone()) {
                id
            } else {
                format!("{id}#{i}")
            }
        };
        let mut items = vec![RowItem {
            id: "history-start".into(),
            kind: RowKind::HistoryStart {
                loading: self.older.loading(),
            },
        }];
        let mut ids: HashMap<usize, String> = HashMap::new();
        let mut previous_tasks: Option<Rc<Value>> = None;
        for i in 0..rows.len() {
            let item = get(&rows[i].entry, "item");
            let previous = previous_tasks.clone();
            if get(item, "type") == "todo" {
                previous_tasks = Some(Rc::new(item.clone()));
                // stream.ts appendTodoList: an unchanged list adds no card.
                if let Some(previous) = &previous
                    && rows::todo_activities(item, Some(previous)).is_empty()
                {
                    continue;
                }
            }
            let id = row_id(i);
            ids.insert(i, id.clone());
            let host = completed_footer_host(&rows, i, auxiliary);
            let footer_below = host.is_some() || (has_auxiliary && i + 1 == rows.len());
            let below = rows.get(i + 1).map(|r| &*r.entry);
            let gap_below = if footer_below && below.is_some() {
                0.0
            } else {
                gap_between_stream_items(Some(&rows[i].entry), below)
            };
            items.push(RowItem {
                id: format!("stream:{id}"),
                kind: RowKind::Entry {
                    rows: rows.clone(),
                    index: i,
                    previous_tasks: previous,
                    footer_below,
                    streaming: running && i + 1 == rows.len(),
                    live: i >= live_from,
                    gap_below,
                },
            });
            if let Some(host) = host {
                let host_id = ids.get(&host).cloned().unwrap_or_else(|| row_id(host));
                items.push(RowItem {
                    id: format!("footer:{host_id}"),
                    kind: RowKind::Footer {
                        rows: rows.clone(),
                        host: Some(host),
                        timing: timing.get(&host).copied(),
                    },
                });
            }
        }
        if running {
            items.push(RowItem {
                id: "footer:live".into(),
                kind: RowKind::Footer {
                    rows: rows.clone(),
                    host: None,
                    timing: None,
                },
            });
        } else if let Some(host) = auxiliary {
            let host_id = ids.get(&host).cloned().unwrap_or_else(|| row_id(host));
            items.push(RowItem {
                id: format!("footer:{host_id}"),
                kind: RowKind::Footer {
                    rows: rows.clone(),
                    host: Some(host),
                    timing: timing.get(&host).copied(),
                },
            });
        }
        if !target.is_subagent() {
            for (key, request) in &snap.permissions {
                items.push(RowItem {
                    id: format!("perm:{key}"),
                    kind: RowKind::Permission {
                        key: key.clone(),
                        request: Rc::new(request.clone()),
                    },
                });
            }
        }
        // Messages the agent has not read yet wait below everything it is doing, nearest the
        // composer; each moves into the conversation where the agent reads it.
        let queued_shown = !queued.is_empty();
        if queued_shown {
            items.push(RowItem {
                id: "queued-gap".into(),
                kind: RowKind::Inset(16.0),
            });
            let queued: Rc<Vec<StreamRow>> = Rc::new(
                queued
                    .into_iter()
                    .map(|entry| StreamRow {
                        entry: Rc::new(entry),
                        group: None,
                        fold: None,
                    })
                    .collect(),
            );
            for index in 0..queued.len() {
                let entry = &queued[index].entry;
                let below = queued.get(index + 1).map(|row| &*row.entry);
                items.push(RowItem {
                    id: match get(entry, "clientMessageId").as_str() {
                        Some(client) => format!("queued:{client}"),
                        None => format!("queued:#{index}"),
                    },
                    kind: RowKind::Entry {
                        rows: queued.clone(),
                        index,
                        previous_tasks: None,
                        footer_below: false,
                        streaming: false,
                        live: false,
                        gap_below: gap_between_stream_items(Some(entry), below),
                    },
                });
            }
        }
        // The tail inset keeps the newest rows clear of the floating track bar.
        let footer_at_end = has_auxiliary;
        let existing = if footer_at_end && permission_count == 0 && !queued_shown {
            32.0
        } else {
            0.0
        };
        if clearance > existing {
            items.push(RowItem {
                id: "tail-inset".into(),
                kind: RowKind::Inset(clearance - existing),
            });
        }
        // Resolve against the current list state, not the previous frame:
        // touch/flick input may have moved while an older page was in flight.
        // The loading sentinel is not content and must never survive a prepend
        // as the reading anchor.
        let list = self.list.as_portal_list();
        if !self.sticky
            && let Some(anchor) = list.scroll_anchor(1)
            && let Some((index, offset)) = remap_reading_anchor(&self.items, &items, anchor)
        {
            list.set_first_id_and_scroll(index, offset);
        }
        if self.row_states.len() > items.len() + 256 {
            let live: HashSet<&String> = items.iter().map(|i| &i.id).collect();
            self.row_states.retain(|k, _| live.contains(k));
        }
        self.footer_at_end = footer_at_end;
        self.stream_rows = rows;
        self.items = items;
    }

    fn stream_empty(&self) -> bool {
        self.stream_rows.is_empty()
            && self.snap.as_ref().is_none_or(|s| {
                s.permissions.is_empty()
                    || self.target.as_ref().is_some_and(ChatTarget::is_subagent)
            })
            && !self.footer_at_end
    }

    fn row_context(&mut self, look: Look, core: &paseo_gui_core::CoreHandle) -> Rc<RowContext> {
        let target = self.target.clone().unwrap_or_default();
        let snap = self.snap.clone();
        let agent = snap.as_ref().map(|s| s.agent.clone()).unwrap_or_default();
        let active = get(&agent, "activeTurn");
        let started = parse_time(get(active, "startedAt")).or_else(|| {
            snap.as_ref()
                .and_then(|s| s.turns.get(str_of(get(active, "turnId"))))
                .and_then(|turn| parse_time(get(turn, "startedAt")))
        });
        let live = self.host.connected
            && get(&agent, "snapshotAvailability") != "last_persisted"
            && get(&agent, "_live") != &Value::Bool(false);
        let find = self
            .find
            .highlight()
            .map(|(query, row_seq, occurrence)| FindTarget {
                query,
                row_seq,
                occurrence,
            });
        let agent_titles = self
            .host
            .agents
            .iter()
            .filter(|(server, _)| *server == target.host_id)
            .filter_map(|(_, a)| {
                Some((
                    non_empty_str(get(a, "id"))?.to_owned(),
                    str_of(get(a, "title")).to_owned(),
                ))
            })
            .collect();
        Rc::new(RowContext {
            look,
            host_id: target.host_id.clone(),
            agent_id: target.agent_id.clone(),
            workspace_id: target.workspace_id.clone(),
            cwd: target.cwd.clone(),
            is_subagent: target.is_subagent(),
            capabilities: get(&agent, "capabilities").clone(),
            expanded_tools: self.expanded_tools,
            auto_expand_reasoning: self.preference_flag("autoExpandReasoning") == Some(true),
            core: core.clone(),
            find,
            agent_titles,
            footer: LiveFooter {
                live,
                started_ms: started,
                pending_delivery: !target.is_subagent()
                    && bridge::has_pending_delivery(
                        &target.host_id,
                        &target.workspace_id,
                        &target.agent_id,
                    ),
            },
            can_fork_cursor: self.feature("agentForkContextCursor"),
            epoch: snap.as_ref().and_then(|s| s.epoch.clone()),
        })
    }

    fn entry_of(&self, row_id: &str) -> Option<Rc<Value>> {
        self.items
            .iter()
            .find(|i| i.id == row_id)
            .and_then(|item| match &item.kind {
                RowKind::Entry { rows, index, .. } => rows.get(*index).map(|r| r.entry.clone()),
                RowKind::Footer {
                    rows,
                    host: Some(host),
                    ..
                } => rows.get(*host).map(|r| r.entry.clone()),
                _ => None,
            })
    }

    // ── scrolling ───────────────────────────────────────────────────────

    fn detach(&mut self) {
        if !self.sticky {
            return;
        }
        self.sticky = false;
        self.list.as_portal_list().set_tail_range(false);
    }

    /// view.tsx scrollToBottom("jump-to-bottom").
    fn jump_to_latest(&mut self, cx: &mut Cx) {
        self.sticky = true;
        self.at_bottom = true;
        self.reader_scrolled = false;
        let list = self.list.as_portal_list();
        let last = self.items.len().saturating_sub(1);
        list.set_first_id_and_scroll(last, 0.0);
        list.set_tail_range(true);
        self.redraw_timeline(cx);
        self.redraw_chrome_part(cx, JUMP);
    }

    /// Brings `row_id` to the top of the stream (`offset` below its top edge).
    fn scroll_to_row(&mut self, cx: &mut Cx, row_id: &str, offset: f64) {
        let Some(index) = self.items.iter().position(|i| i.id == row_id) else {
            return;
        };
        self.detach();
        self.at_bottom = false;
        self.list
            .as_portal_list()
            .set_first_id_and_scroll(index, offset);
        self.redraw_timeline(cx);
        self.redraw_chrome_part(cx, JUMP);
    }

    fn row_id_of_seq(&self, seq: i64) -> Option<String> {
        self.items
            .iter()
            .find(|i| i.seq() == Some(seq))
            .map(|i| i.id.clone())
    }

    fn load_older(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(snap) = &self.snap else { return };
        if !self.visible || self.older.loading() || !snap.has_older || snap.loading {
            return;
        }
        let Some(start) = snap.start_cursor.clone() else {
            return;
        };
        self.older.request = Some(start);
        self.with_session(cx, ui, |s, io| s.older(io));
        self.redraw_timeline(cx);
    }

    // ── outline ─────────────────────────────────────────────────────────

    /// use-chat-outline.ts: the prompt index for the served epoch, refreshed
    /// whenever a newer prompt arrives.
    fn refresh_prompts(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(snap) = self.snap.clone() else {
            return;
        };
        let subagent = self.target.as_ref().is_some_and(ChatTarget::is_subagent);
        if subagent
            || !outline::supported()
            || !self.feature("agentTimelinePromptIndex")
            || snap.epoch.is_none()
            || !self.outline_enabled()
        {
            return;
        }
        let ptr = Rc::as_ptr(&snap.timeline) as usize;
        if ptr == self.prompts_timeline && self.prompts_epoch == snap.epoch {
            return;
        }
        self.prompts_timeline = ptr;
        let latest = snap
            .timeline
            .iter()
            .filter(|r| get(get(r, "item"), "type") == "user_message")
            .map(|r| int_of(get(r, "seqStart")))
            .max()
            .unwrap_or(-1);
        if self.prompts_epoch == snap.epoch && latest <= self.prompts_latest {
            return;
        }
        self.prompts_latest = latest;
        if self.prompts_epoch != snap.epoch {
            self.rail.prompts.clear();
            self.redraw_chrome_part(cx, RAIL);
        }
        self.prompts_epoch = snap.epoch.clone();
        self.prompts_request += 1;
        let request = self.prompts_request;
        self.with_session(cx, ui, |s, io| {
            s.action(
                io,
                ActionKind::ListPrompts { request },
                "agent.timeline.list_prompts.request",
                "agent.timeline.list_prompts.response",
                json!({}),
            )
        });
    }

    fn jump_to_prompt(&mut self, cx: &mut Cx, ui: &mut UiContext, seq: i64) {
        let Some(snap) = &self.snap else { return };
        let loaded = snap.timeline.iter().any(|r| {
            get(get(r, "item"), "type") == "user_message"
                && int_of(get(r, "seqStart")) <= seq
                && int_or(get(r, "seqEnd"), int_of(get(r, "seqStart"))) >= seq
        });
        if loaded {
            self.finish_prompt_jump(cx, seq);
            return;
        }
        let token = self.token();
        self.reveals.insert(token, RevealFor::Prompt(seq));
        self.with_session(cx, ui, |s, io| s.reveal(io, seq, token));
    }

    fn finish_prompt_jump(&mut self, cx: &mut Cx, seq: i64) {
        let Some(snap) = &self.snap else { return };
        let row = snap
            .timeline
            .iter()
            .find(|r| {
                get(get(r, "item"), "type") == "user_message"
                    && int_of(get(r, "seqStart")) <= seq
                    && int_or(get(r, "seqEnd"), int_of(get(r, "seqStart"))) >= seq
            })
            .map(|r| int_of(get(r, "seqStart")));
        if let Some(row) = row {
            // Row top at the viewport top.
            self.jump_to_seq(cx, row, 0.0, false);
        }
    }

    /// Scrolls the row starting at `seq` to `offset` below the stream top;
    /// `find` then aligns the highlighted match under the find card. A row
    /// the current model lacks (history just paged in) waits for the next
    /// rebuild.
    fn jump_to_seq(&mut self, cx: &mut Cx, seq: i64, offset: f64, find: bool) {
        if !self.visible {
            self.pending_jump = Some((seq, offset, find));
            self.model_sig = None;
            return;
        }
        match self.row_id_of_seq(seq) {
            Some(id) => {
                self.pending_jump = None;
                self.scroll_to_row(cx, &id, offset);
                if find {
                    self.find_scroll = Some((id, 0));
                }
            }
            None => {
                self.pending_jump = Some((seq, offset, find));
                self.reveal_folded(seq);
                self.model_sig = None;
                self.redraw_timeline(cx);
            }
        }
    }

    /// Unfolds the finished turn whose summary hides the entry starting at `seq`.
    fn reveal_folded(&mut self, seq: i64) {
        let key = self.stream_rows.iter().find_map(|row| {
            let fold = row.fold.as_deref().filter(|fold| !fold.open)?;
            fold.entries
                .iter()
                .any(|entry| get(entry, "seqStart").as_i64() == Some(seq))
                .then(|| fold.key.clone())
        });
        if let Some(key) = key {
            self.open_turns.insert(key);
            self.open_turns_revision += 1;
        }
    }

    /// Shows or folds a finished turn's work below its summary row.
    fn toggle_turn(&mut self, cx: &mut Cx, key: String) {
        if !self.open_turns.remove(&key) {
            self.open_turns.insert(key);
        }
        self.open_turns_revision += 1;
        self.model_sig = None;
        self.redraw_timeline(cx);
    }

    // ── find ────────────────────────────────────────────────────────────

    fn open_find(&mut self, cx: &mut Cx) {
        if MOBILE || self.snap.is_none() {
            return;
        }
        self.find.show();
        self.find_field
            .as_ptext_field()
            .set_placeholder(cx, &t("paneFind.placeholder"));
        self.find_open = true;
        self.redraw_chrome_part(cx, FIND);
    }

    fn close_find(&mut self, cx: &mut Cx) {
        self.find.close();
        self.find_open = false;
        self.find_field_rect = None;
        self.redraw_chrome_part(cx, FIND);
        self.redraw_timeline(cx);
    }

    /// Runs the find controller's requests and redraws the current highlight.
    fn pump_find(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        self.pump_find_requests(cx, ui);
        if self.visible && self.find_open {
            self.redraw_chrome_part(cx, FIND);
            self.redraw_timeline(cx);
        }
    }

    fn pump_find_requests(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let requests = std::mem::take(&mut self.find.requests);
        for request in requests {
            match request {
                find::Request::Debounce { token } => {
                    let timer = cx.start_timeout(0.12);
                    self.find_timers.push((timer, token, false));
                }
                find::Request::WaitDeadline { token } => {
                    let timer = cx.start_timeout(5.0);
                    self.find_timers.push((timer, token, true));
                }
                find::Request::Search {
                    token,
                    query,
                    cursor,
                } => {
                    let mut fields = json!({"query": query});
                    if let Some(cursor) = cursor {
                        fields["cursor"] = json!(cursor);
                    }
                    self.with_session(cx, ui, |s, io| {
                        s.action(
                            io,
                            ActionKind::Search { token },
                            "agent.timeline.search.request",
                            "agent.timeline.search.response",
                            fields,
                        )
                    });
                }
                find::Request::Load { token, seq } => {
                    let reveal = self.token();
                    self.reveals.insert(reveal, RevealFor::Find(token));
                    self.with_session(cx, ui, |s, io| s.reveal(io, seq, reveal));
                }
                find::Request::ScrollTo { row } => {
                    // Like an outline jump, a find reveal leaves sticky-bottom.
                    let clearance = self.find_card_h + 8.0;
                    self.jump_to_seq(cx, row, clearance, true);
                }
            }
        }
        if !self.find.requests.is_empty() {
            self.pump_find_requests(cx, ui);
        }
    }

    // ── actions ─────────────────────────────────────────────────────────

    fn conversation_action(&mut self, cx: &mut Cx, ui: &mut UiContext, action: &str) {
        match action {
            "find" => self.open_find(cx),
            "outline" => self.show_outline(cx, ui),
            "tools" => {
                self.expanded_tools = !self.expanded_tools;
                self.model_sig = None;
                self.redraw_timeline(cx);
            }
            "sync" => {
                self.with_session(cx, ui, |s, io| s.reload(io));
            }
            other => self.lifecycle(cx, ui, other),
        }
    }

    fn show_outline(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if MOBILE {
            return;
        }
        self.with_session(cx, ui, |s, io| {
            s.action(
                io,
                ActionKind::OutlineMenu,
                "agent.timeline.list_prompts.request",
                "agent.timeline.list_prompts.response",
                json!({}),
            )
        });
    }

    fn lifecycle(&mut self, cx: &mut Cx, ui: &mut UiContext, action: &str) {
        let Some(snap) = self.snap.clone() else {
            return;
        };
        match action {
            "rename" => {
                let id = overlay::prompt_text(
                    ui,
                    &t("agentPanel.conversation.renameTitle"),
                    None,
                    str_of(get(&snap.agent, "title")),
                    "",
                    &t("common.actions.save"),
                );
                self.asks.insert(id, Ask::Rename);
            }
            "refresh" => {
                if !snap.archived {
                    ui.toast(t("workspace.tabs.toasts.reloadingAgent"), false);
                }
                self.with_session(cx, ui, |s, io| {
                    s.action(
                        io,
                        ActionKind::Refresh,
                        "refresh_agent_request",
                        "status",
                        json!({}),
                    )
                });
            }
            "fork" => {
                self.fork(cx, ui, None);
            }
            "archive" => {
                let id = overlay::confirm(
                    ui,
                    &t("agentPanel.conversation.archiveTitle"),
                    &t("agentPanel.conversation.archiveBody"),
                    &t("agentList.archiveSheet.archive"),
                    true,
                );
                self.asks.insert(id, Ask::Confirm("archive"));
            }
            "delete" => {
                let id = overlay::confirm(
                    ui,
                    &t("agentPanel.conversation.deleteTitle"),
                    &t("agentPanel.conversation.deleteBody"),
                    &t("workspace.fileActions.delete"),
                    true,
                );
                self.asks.insert(id, Ask::Confirm("delete"));
            }
            "detach" => {
                self.with_session(cx, ui, |s, io| {
                    s.action(
                        io,
                        ActionKind::Detach,
                        "agent.detach.request",
                        "agent.detach.response",
                        json!({}),
                    )
                });
            }
            "cancel" => {
                self.with_session(cx, ui, |s, io| {
                    s.action(
                        io,
                        ActionKind::Cancel,
                        "cancel_agent_request",
                        "cancel_agent_response",
                        json!({}),
                    )
                });
            }
            "usage" => {
                self.modal = Some(Modal::Usage);
                self.modal_state = RowState::default();
                self.redraw_modal(cx);
            }
            _ => {}
        }
    }

    /// hooks/use-fork-agent.ts: the fork context pinned to `entry` (or the
    /// whole timeline so far), opened as a new session draft in the same directory.
    /// Whether a fork request was sent; only a sent request completes and
    /// clears the menu's busy glyph.
    fn fork(&mut self, cx: &mut Cx, ui: &mut UiContext, entry: Option<Rc<Value>>) -> bool {
        let Some(snap) = self.snap.clone() else {
            return false;
        };
        if !self.feature("agentForkContext") {
            self.show_failure(cx, t("message.actions.forkUnavailable"));
            return false;
        }
        let mut fields = json!({});
        if let Some(entry) = &entry {
            if self.feature("agentForkContextCursor")
                && let Some(epoch) = &snap.epoch
            {
                fields["boundaryCursor"] = json!({"epoch": epoch, "seq": get(entry, "seqEnd")});
            }
            let message = get(get(entry, "item"), "messageId");
            if !message.is_null() {
                fields["boundaryMessageId"] = message.clone();
            }
        }
        self.with_session(cx, ui, |s, io| {
            s.action(
                io,
                ActionKind::Fork,
                "agent.fork_context.request",
                "agent.fork_context.response",
                fields,
            )
        })
        .is_some()
    }

    fn config(&self) -> Value {
        let target = self.target.clone().unwrap_or_default();
        let agent = self
            .snap
            .as_ref()
            .map(|s| s.agent.clone())
            .unwrap_or_default();
        let mut config = json!({"provider": get(&agent, "provider"), "cwd": target.cwd()});
        if !get(&agent, "model").is_null() {
            config["model"] = get(&agent, "model").clone();
        }
        if !get(&agent, "currentModeId").is_null() {
            config["modeId"] = get(&agent, "currentModeId").clone();
        }
        if !get(&agent, "thinkingOptionId").is_null() {
            config["thinkingOptionId"] = get(&agent, "thinkingOptionId").clone();
        }
        let mut features = serde_json::Map::new();
        for f in list(get(&agent, "features")) {
            features.insert(str_of(get(f, "id")).to_owned(), get(f, "value").clone());
        }
        config["featureValues"] = Value::Object(features);
        config
    }

    fn respond_permission(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        key: &str,
        action: &str,
        response: Value,
    ) {
        let Some(snap) = &self.snap else { return };
        let Some((_, request)) = snap.permissions.iter().find(|(k, _)| k == key) else {
            return;
        };
        let request_id = get(request, "id").clone();
        let state = self.row_state(&format!("perm:{key}"));
        {
            let mut state = state.borrow_mut();
            if let Some(card) = state.permission.as_mut() {
                // One response per card until it resolves or fails.
                if card.pending {
                    return;
                }
                card.pending = true;
                card.failed = false;
                card.responding_action = Some(action.to_owned());
                state.invalidate();
            }
        }
        let sent = self
            .with_session(cx, ui, |s, io| {
                s.action_with_timeout(
                    io,
                    ActionKind::Permission {
                        key: key.to_owned(),
                    },
                    "agent_permission_response",
                    "agent_permission_resolved",
                    json!({"requestId": request_id, "response": response}),
                    Duration::from_secs(15),
                )
            })
            .is_some();
        if !sent {
            // No completion will arrive to clear pending; show the card as failed.
            let mut state = state.borrow_mut();
            if let Some(card) = state.permission.as_mut()
                && card.pending
            {
                card.pending = false;
                card.failed = true;
                state.invalidate();
            }
        }
        self.redraw_timeline(cx);
    }

    /// The panel's toast (agent-panel.tsx `toast ?? syncToast`).
    fn panel_toast(&self) -> Option<Toast> {
        if let Some((seq, message)) = &self.error {
            return Some(Toast {
                id: format!("error:{seq}"),
                message: message.clone(),
                variant: ToastVariant::Error,
                spinner: false,
                duration: Some(3.2),
            });
        }
        let snap = self.snap.as_ref();
        if let Some(notice) = snap.and_then(|s| s.notice.clone()) {
            return Some(Toast {
                id: format!("notice:{notice}"),
                message: notice,
                variant: ToastVariant::Normal,
                spinner: false,
                duration: Some(3.2),
            });
        }
        if let Some(snap) = snap {
            let id = format!("older-history:{}", snap.older_failures);
            if snap.older_failures > 0 && !self.dismissed_toasts.contains(&id) {
                return Some(Toast {
                    id,
                    message: t("loadOlderHistory.failed"),
                    variant: ToastVariant::Normal,
                    spinner: false,
                    duration: Some(2.2),
                });
            }
        }
        if let Some(maintenance) = &self.host.maintenance {
            let id = format!("maintenance:{maintenance}");
            if !self.dismissed_toasts.contains(&id) {
                return Some(Toast {
                    id,
                    message: t("agentPanel.states.daemonMaintenance"),
                    variant: ToastVariant::Normal,
                    spinner: true,
                    duration: None,
                });
            }
        }
        let snap = snap?;
        if snap.archived {
            return None;
        }
        // A dropped host shows the Disconnected callout instead (with Retry).
        if !self.host.connected {
            return None;
        }
        if snap.sync == SyncStatus::Pending && snap.hydrated && !snap.create_handoff {
            return Some(Toast {
                id: "updating".into(),
                message: t("agentPanel.states.updating"),
                variant: ToastVariant::Normal,
                spinner: true,
                duration: None,
            });
        }
        None
    }

    fn dismiss_toast(&mut self, cx: &mut Cx, ui: &mut UiContext, id: &str) {
        if id.starts_with("error:") {
            self.error = None;
        } else if id.starts_with("notice:") {
            self.with_session(cx, ui, |s, _| s.dismiss_notice());
        } else {
            self.dismissed_toasts.insert(id.to_owned());
        }
        self.toast_shown = None;
        self.redraw_chrome_part(cx, TOAST);
    }

    // ── file links ──────────────────────────────────────────────────────

    /// use-file-link.ts openAssistantFileLink.
    fn open_link(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        href: &str,
        text: &str,
        kind: FileLinkSourceType,
        intent: FileLinkIntent,
    ) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let root = target.cwd().to_owned();
        let source = AssistantFileLinkSource {
            href: href.to_owned(),
            text: (!text.is_empty()).then(|| text.to_owned()),
            kind,
        };
        match links::classify_for_resolution(&source, Some(&root)) {
            Resolution::Ignored => {}
            Resolution::External(url) => {
                let lower = url.to_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    ui.push(UiEffect::OpenExternal { url });
                }
            }
            Resolution::File(path) => self.open_inline_path(cx, ui, path, intent),
            Resolution::NeedsLookup {
                query,
                token,
                target: path,
            } => {
                let fields = json!({
                    "query": query,
                    "cwd": root.trim(),
                    "includeFiles": true,
                    "includeDirectories": false,
                    "matchMode": "suffix",
                    "limit": 1,
                });
                self.with_session(cx, ui, |s, io| {
                    s.host_request(
                        io,
                        ActionKind::LookupFile {
                            token,
                            target: path,
                            intent,
                        },
                        "directory_suggestions_request",
                        "directory_suggestions_response",
                        fields,
                    )
                });
            }
        }
    }

    /// path-open.ts createInlinePathOpener.open.
    fn open_inline_path(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        target: InlinePathTarget,
        intent: FileLinkIntent,
    ) {
        let Some(chat) = self.target.clone() else {
            return;
        };
        let generation = OPEN_GENERATION.with(|g| {
            g.set(g.get() + 1);
            g.get()
        });
        let root = chat.cwd().to_owned();
        let Some((directory, file)) =
            links::normalize_inline_path_target(&target.path, Some(&root))
        else {
            return;
        };
        if let Some(file) = &file
            && target.line_start.is_none()
            && !links::is_absolute_file_path(file)
            && !root.trim().is_empty()
        {
            // A workspace folder (even one with a dot) reveals in the explorer.
            let fields = json!({"cwd": root, "path": file, "mode": "list"});
            self.with_session(cx, ui, |s, io| {
                s.host_request(
                    io,
                    ActionKind::ProbeFolder {
                        generation,
                        target: target.with_path(file.clone()),
                        intent,
                    },
                    "file_explorer_request",
                    "file_explorer_response",
                    fields,
                )
            });
            return;
        }
        self.finish_open(
            ui,
            file.as_deref(),
            &directory,
            target.line_start,
            target.line_end,
            intent,
        );
    }

    fn finish_open(
        &mut self,
        ui: &mut UiContext,
        file: Option<&str>,
        directory: &str,
        line_start: Option<i64>,
        line_end: Option<i64>,
        intent: FileLinkIntent,
    ) {
        let Some(chat) = self.target.clone() else {
            return;
        };
        let Some(file) = file else {
            let target = PanelTarget::new("files", &chat.host_id, &chat.workspace_id, chat.cwd())
                .with_data(json!({"path": directory}));
            ui.push(UiEffect::OpenFileLink {
                target,
                placement: TabPlacement::Explorer,
                intent,
            });
            return;
        };
        let mut data = json!({"path": file});
        if let Some(start) = line_start {
            data["line"] = json!(start);
            if let Some(end) = line_end.filter(|e| *e >= start) {
                data["lineEnd"] = json!(end);
            }
        }
        let title = file.rsplit('/').next().unwrap_or(file).to_owned();
        let target = PanelTarget::new("file", &chat.host_id, &chat.workspace_id, chat.cwd())
            .with_id(file)
            .with_title(title)
            .with_data(data);
        let side =
            get(get(&self.host.preferences, "openInSidePane"), "chatFiles").as_bool() == Some(true);
        let compact = crate::common::is_compact_width(self.content_rect.size.x);
        ui.push(UiEffect::OpenFileLink {
            target,
            placement: if !compact && side {
                TabPlacement::Beside
            } else {
                TabPlacement::Main
            },
            intent,
        });
    }

    // ── subagents ───────────────────────────────────────────────────────

    fn subagent_target(&self, row: &SubagentRow) -> PanelTarget {
        let chat = self.target.clone().unwrap_or_default();
        let title = row
            .label()
            .unwrap_or_else(|| t("agentPanel.conversation.subagent"));
        if row.paseo {
            PanelTarget::new(
                "agent",
                &row.server_id,
                row.workspace_id.as_deref().unwrap_or(&chat.workspace_id),
                row.cwd.as_deref().unwrap_or(chat.cwd()),
            )
            .with_id(row.id.clone())
            .with_title(title)
        } else {
            PanelTarget::new(
                "provider_subagent",
                &chat.host_id,
                &chat.workspace_id,
                row.cwd.as_deref().unwrap_or(chat.cwd()),
            )
            .with_id(row.id.clone())
            .with_title(title)
            .with_data(json!({"parentAgentId": row.parent_agent_id}))
        }
    }

    fn cross_workspace(&self, row: &SubagentRow) -> bool {
        let chat = self.target.clone().unwrap_or_default();
        row.paseo
            && (row.server_id != chat.host_id
                || row
                    .workspace_id
                    .as_deref()
                    .is_some_and(|w| w != chat.workspace_id))
    }

    /// subagents/opens-in-sheet.ts: compact, same workspace, not already a tab.
    fn opens_in_sheet(&self, ui: &UiContext, row: &SubagentRow) -> bool {
        self.sheet_template.is_some()
            && crate::common::is_compact_width(self.content_rect.size.x)
            && !self.cross_workspace(row)
            && ui.layout.target(&self.subagent_target(row).key()).is_none()
    }

    fn open_subagent(&mut self, cx: &mut Cx, ui: &mut UiContext, row: &SubagentRow) {
        if self.cross_workspace(row) {
            ui.push(UiEffect::OpenSession {
                host_id: row.server_id.clone(),
                agent_id: row.id.clone(),
            });
            return;
        }
        let target = self.subagent_target(row);
        if self.opens_in_sheet(ui, row) {
            self.modal = Some(Modal::Subagent {
                label: row.label().unwrap_or_else(|| t("common.states.loading")),
                provider: row.provider.clone(),
                tab: target.clone(),
            });
            if let Some(ptr) = self.sheet_template {
                let panel = WidgetRef::new_from_ptr(cx, Some(ptr));
                panel.as_chat_panel().set_sheet_template(Some(ptr));
                let subagent = target.kind == "provider_subagent";
                let chat = ChatTarget {
                    host_id: target.server_id.clone(),
                    agent_id: if subagent {
                        str_of(get(&target.data, "parentAgentId")).to_owned()
                    } else {
                        target.id.clone().unwrap_or_default()
                    },
                    subagent_id: if subagent { target.id.clone() } else { None },
                    workspace_id: target.workspace_id.clone(),
                    cwd: (!target.cwd.is_empty()).then(|| target.cwd.clone()),
                    tab_key: target.key(),
                    sheet: true,
                    title: target.title.clone(),
                };
                panel.as_chat_panel().set_target(cx, chat);
                panel.as_chat_panel().set_visible(cx, true);
                self.sheet_panel = Some(panel);
            }
            self.redraw_modal(cx);
            return;
        }
        ui.push(UiEffect::OpenTab {
            target,
            placement: TabPlacement::Main,
        });
    }

    fn close_modal(&mut self, cx: &mut Cx, _ui: &mut UiContext) {
        if matches!(self.modal, Some(Modal::Track(_))) {
            self.close_track(cx);
        } else if matches!(self.modal, Some(Modal::Sheet { .. })) {
            self.detail_motion.set_open(cx, false);
            self.window_modal = None;
            self.modal_pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            self.bar_drag = None;
            if !self.detail_motion.is_moving() {
                self.dismiss_modal(cx);
            }
        } else {
            self.dismiss_modal(cx);
        }
    }

    /// subagents/archive-subagent.ts requestArchiveSubagent.
    fn ask_archive_subagent(&mut self, ui: &mut UiContext, row: SubagentRow) {
        let label =
            tracks::row_label(row.title.as_deref()).unwrap_or_else(|| "this subagent".into());
        let running = row.status.as_deref() == Some("running");
        let (title, message) = if running {
            (
                t("agentPanel.conversation.archiveRunningSubagentTitle"),
                ta(
                    "agentPanel.conversation.archiveRunningSubagentBody",
                    &[("label", &label)],
                ),
            )
        } else {
            (
                t("agentPanel.conversation.archiveSubagentTitle"),
                ta(
                    "agentPanel.conversation.archiveSubagentBody",
                    &[("label", &label)],
                ),
            )
        };
        let id = overlay::confirm(
            ui,
            &title,
            &message,
            &t("agentList.archiveSheet.archive"),
            true,
        );
        self.asks.insert(id, Ask::ArchiveSubagent(row));
    }

    fn ask_detach_subagent(&mut self, ui: &mut UiContext, row: SubagentRow) {
        let label = tracks::row_label(row.title.as_deref())
            .unwrap_or_else(|| t("agentPanel.conversation.thisSubagent"));
        let id = overlay::confirm(
            ui,
            &t("agentPanel.conversation.detachSubagentTitle"),
            &ta(
                "agentPanel.conversation.detachSubagentBody",
                &[("label", &label)],
            ),
            &t("agentPanel.conversation.detach"),
            false,
        );
        self.asks.insert(id, Ask::DetachSubagent(row));
    }

    /// "Delete finished subagents" (replaces archive-finished.ts): confirm,
    /// then delete every finished row.
    fn ask_delete_finished(&mut self, ui: &mut UiContext) {
        if matches!(self.delete_status, DeleteFinished::Deleting { .. }) {
            return;
        }
        let count = self
            .subagent_rows()
            .iter()
            .filter(|row| row.is_finished())
            .count();
        if count == 0 {
            return;
        }
        let message = ta(
            "subagents.deleteFinishedConfirm",
            &[("count", &count.to_string())],
        );
        let id = overlay::confirm(
            ui,
            &t("subagents.deleteFinishedAction"),
            &message,
            &t("workspace.fileActions.delete"),
            true,
        );
        self.asks.insert(id, Ask::DeleteFinished);
    }

    /// Finished provider children go to the daemon in one request (it removes
    /// their stored transcripts and tells every client); a daemon without
    /// `providerSubagentDelete` cannot delete them, so they are only hidden for
    /// this app session, as the original archive-finished did. Finished
    /// managed children are deleted one by one.
    fn delete_finished(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if matches!(self.delete_status, DeleteFinished::Deleting { .. }) {
            return;
        }
        let eligible: Vec<SubagentRow> = self
            .subagent_rows()
            .into_iter()
            .filter(SubagentRow::is_finished)
            .collect();
        if eligible.is_empty() {
            return;
        }
        let total = eligible.len();
        let (provider, paseo): (Vec<SubagentRow>, Vec<SubagentRow>) =
            eligible.into_iter().partition(|row| !row.paseo);
        let mut run = DeleteRun {
            total,
            queue: paseo.into_iter().rev().collect(),
            ..DeleteRun::default()
        };
        if !provider.is_empty() {
            let ids: Vec<String> = provider.into_iter().map(|row| row.id).collect();
            if self.feature("providerSubagentDelete") {
                run.provider_pending = self
                    .with_session(cx, ui, |s, io| s.delete_subagents(io, ids.clone()))
                    .is_some();
                if !run.provider_pending {
                    run.failed += ids.len();
                    run.completed += ids.len();
                }
            } else if let Some(target) = &self.target {
                with_store(|store| {
                    store.hide_subagents(
                        &target.host_id,
                        &target.agent_id,
                        ids.iter().map(String::as_str),
                    )
                });
                run.completed += ids.len();
            }
        }
        self.delete_run = run;
        self.delete_next(cx, ui);
    }

    fn delete_next(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        while !self.delete_run.in_flight
            && let Some(row) = self.delete_run.queue.pop()
        {
            let key = format!("{}:{}", row.server_id, row.id);
            let fields = json!({"agentId": row.id});
            let kind = ActionKind::DeleteFinished { key };
            // A child on another host is deleted there; its outcome still
            // counts toward this run (failures become the retry count).
            let sent = self.with_session(cx, ui, |s, io| {
                s.request_on_host(
                    io,
                    &row.server_id,
                    kind,
                    "delete_agent_request",
                    "agent_deleted",
                    fields,
                )
            });
            if sent.is_some() {
                self.delete_run.in_flight = true;
            } else {
                self.delete_run.failed += 1;
                self.delete_run.completed += 1;
            }
        }
        let run = &self.delete_run;
        self.delete_status = if run.in_flight || run.provider_pending {
            DeleteFinished::Deleting {
                completed: run.completed,
                total: run.total,
            }
        } else if run.failed > 0 {
            DeleteFinished::Failed {
                failed: run.failed,
                total: run.failed,
            }
        } else {
            DeleteFinished::Idle
        };
        self.model_sig = None;
        self.redraw_timeline(cx);
        self.redraw_chrome_part(cx, TRACKS);
        self.redraw_modal(cx);
    }

    // ── completions ─────────────────────────────────────────────────────

    fn on_completion(&mut self, cx: &mut Cx, ui: &mut UiContext, completion: Completion) {
        let Completion { kind, result } = completion;
        match &kind {
            ActionKind::Permission { .. } | ActionKind::Fork | ActionKind::Rewind { .. } => {
                self.redraw_timeline(cx);
            }
            ActionKind::ArchiveSubagent { .. } => {
                self.redraw_timeline(cx);
                self.redraw_chrome_part(cx, TRACKS);
                self.redraw_modal(cx);
            }
            ActionKind::ListPrompts { .. } => self.redraw_chrome_part(cx, RAIL),
            ActionKind::Unarchive => self.redraw_chrome_part(cx, CALLOUTS),
            ActionKind::Archive | ActionKind::Delete => self.redraw_chrome_part(cx, ARCHIVING),
            _ => {}
        }
        match kind {
            ActionKind::StopProviderTask { token } => {
                let failure = match result {
                    Ok(body) => error_text(get(&body, "error")),
                    Err(error) => Some(error.message),
                };
                if let Some(message) = failure {
                    self.requested_task_stops.remove(&token);
                    self.show_failure(cx, message);
                }
                self.redraw_modal(cx);
            }
            ActionKind::Permission { key } => {
                let state = self.row_state(&format!("perm:{key}"));
                let mut state = state.borrow_mut();
                if let Some(card) = state.permission.as_mut()
                    && (card.pending || (result.is_err() && !card.failed))
                {
                    card.pending = false;
                    card.failed |= result.is_err();
                    state.invalidate();
                }
            }
            ActionKind::Rename | ActionKind::Detach | ActionKind::Cancel => match result {
                Ok(_) => {
                    self.with_session(cx, ui, |s, io| s.reload(io));
                }
                Err(e) => self.show_failure(cx, e.message),
            },
            ActionKind::Refresh => match result {
                Ok(_) => {
                    if !self.snap.as_ref().is_some_and(|s| s.archived) {
                        ui.toast(t("workspace.tabs.toasts.reloadedAgent"), false);
                    }
                    self.with_session(cx, ui, |s, io| s.reload(io));
                }
                Err(e) => self.show_failure(cx, e.message),
            },
            ActionKind::Unarchive => {
                self.unarchiving = false;
                match result {
                    Ok(_) => {
                        self.unarchive_error = None;
                        self.with_session(cx, ui, |s, io| s.reload(io));
                    }
                    Err(e) => self.unarchive_error = Some(e.message),
                }
            }
            ActionKind::Archive | ActionKind::Delete => {
                self.archiving = false;
                match result {
                    Ok(_) => {
                        if let Some(target) = &self.target {
                            ui.push(UiEffect::CloseTab {
                                tab_key: target.tab_key.clone(),
                            });
                        }
                    }
                    Err(e) => self.show_failure(cx, e.message),
                }
            }
            ActionKind::Fork => {
                for state in self.row_states.values() {
                    let mut state = state.borrow_mut();
                    let before = state.menu_busy.len();
                    state.menu_busy.retain(|k| !k.starts_with("fork:"));
                    if state.menu_busy.len() != before {
                        state.invalidate();
                    }
                }
                match result {
                    Ok(body) if !get(&body, "attachment").is_null() => {
                        let chat = self.target.clone().unwrap_or_default();
                        ui.push(UiEffect::OpenDraft {
                            host_id: chat.host_id.clone(),
                            workspace_id: chat.workspace_id.clone(),
                            cwd: chat.cwd().to_owned(),
                            draft_id: uuid::Uuid::new_v4().to_string(),
                            title: t("agentPanel.conversation.forkTitle"),
                            data: json!({
                                "config": self.config(),
                                "attachments": [get(&body, "attachment")],
                                "attachmentIntentId": uuid::Uuid::new_v4().to_string(),
                            }),
                        });
                    }
                    Ok(_) => self.show_failure(cx, t("message.actions.forkFailed")),
                    Err(e) => self.show_failure(cx, e.message),
                }
            }
            ActionKind::Rewind { mode, text } => {
                for state in self.row_states.values() {
                    let mut state = state.borrow_mut();
                    if state.menu_busy.remove("rewind") {
                        state.invalidate();
                    }
                }
                match result {
                    Ok(_) if mode != "files" => {
                        self.with_session(cx, ui, |s, io| s.refresh_tail(io));
                        if let Some(chat) = self.target.clone() {
                            bridge::restore_text_if_empty(
                                cx,
                                &chat.host_id,
                                &chat.workspace_id,
                                &chat.agent_id,
                                &text,
                            );
                        }
                        self.jump_to_latest(cx);
                    }
                    Ok(_) => {}
                    Err(e) => self.show_failure(cx, e.message),
                }
            }
            ActionKind::ListPrompts { request } => {
                if let Ok(body) = result
                    && request == self.prompts_request
                    && optional(get(&body, "epoch"))
                        == self.snap.as_ref().and_then(|s| s.epoch.clone())
                {
                    self.rail.prompts = list(get(&body, "prompts")).to_vec();
                }
            }
            ActionKind::OutlineMenu => match result {
                Ok(body) => {
                    let prompts = list(get(&body, "prompts")).to_vec();
                    if prompts.is_empty() {
                        return;
                    }
                    let entries = prompts
                        .iter()
                        .map(|p| {
                            let seq = int_of(get(p, "seq"));
                            MenuItem::new(seq.to_string(), str_of(get(p, "preview")))
                                .checked(Some(seq) == self.active_prompt)
                                .entry()
                        })
                        .collect();
                    let anchor = Rect {
                        pos: self.stream_rect.pos
                            + dvec2(self.stream_rect.size.x * 0.5 - 160.0, 8.0),
                        size: dvec2(320.0, 1.0),
                    };
                    let id = overlay::open_menu_with(
                        ui,
                        anchor,
                        Some(t("settings.appearance.chatOutline.title")),
                        entries,
                        MenuOptions {
                            width: 320.0,
                            ..Default::default()
                        },
                    );
                    self.asks.insert(id, Ask::Outline);
                }
                Err(e) => self.show_failure(cx, e.message),
            },
            ActionKind::Search { token } => {
                let selected = self.find.selected_row;
                let (epoch, rows) = self
                    .snap
                    .as_ref()
                    .map(|s| (s.epoch.as_deref(), s.timeline.as_slice()))
                    .unwrap_or_default();
                if self
                    .find
                    .on_search(token, result.map_err(|e| e.message), epoch, rows)
                {
                    self.pump_find_requests(cx, ui);
                    if self.visible && self.find_open {
                        self.redraw_chrome_part(cx, FIND);
                        if selected != self.find.selected_row {
                            self.redraw_timeline(cx);
                        }
                    }
                }
            }
            ActionKind::ClearAttention => {}
            ActionKind::ArchiveSubagent { key } => {
                if let Err(e) = result {
                    self.pending_removal.remove(&key);
                    self.show_failure(cx, e.message);
                }
                self.model_sig = None;
            }
            ActionKind::DetachSubagent { host_id, agent_id } => match result {
                Ok(_) => ui.push(UiEffect::OpenSession { host_id, agent_id }),
                Err(e) => self.show_failure(cx, e.message),
            },
            ActionKind::DeleteFinished { key } => {
                match result {
                    Ok(_) => {
                        self.pending_removal.insert(key);
                    }
                    Err(_) => self.delete_run.failed += 1,
                }
                self.delete_run.in_flight = false;
                self.delete_run.completed += 1;
                self.delete_next(cx, ui);
            }
            ActionKind::DeleteProviderSubagents { ids } => {
                let failed = match &result {
                    Ok(body) => {
                        let deleted: Vec<String> = list(get(body, "deleted"))
                            .iter()
                            .map(|id| str_of(id).to_owned())
                            .collect();
                        self.with_session(cx, ui, |s, _| s.drop_subagents(&deleted));
                        // Running (it started again) and unknown (already gone)
                        // children are not failures; unreadable storage is.
                        list(get(body, "skipped"))
                            .iter()
                            .filter(|s| get(s, "reason") == "unavailable")
                            .count()
                    }
                    Err(_) => ids.len(),
                };
                self.delete_run.provider_pending = false;
                self.delete_run.failed += failed;
                self.delete_run.completed += ids.len();
                self.delete_next(cx, ui);
            }
            ActionKind::LookupFile {
                token,
                target,
                intent,
            } => {
                let root = self
                    .target
                    .as_ref()
                    .map(|t| t.cwd().to_owned())
                    .unwrap_or_default();
                let found = result
                    .ok()
                    .filter(|r| get(r, "error").is_null() || str_of(get(r, "error")).is_empty())
                    .and_then(|r| {
                        let entry = list(get(&r, "entries"))
                            .iter()
                            .find(|e| get(e, "kind") == "file")?
                            .clone();
                        let base = root.replace('\\', "/");
                        let base = base.trim_end_matches('/');
                        let child = str_of(get(&entry, "path")).replace('\\', "/");
                        let child = child.trim_start_matches('/');
                        Some(if base.is_empty() {
                            child.to_owned()
                        } else {
                            format!("{base}/{child}")
                        })
                    });
                match found {
                    Some(path) => {
                        self.open_inline_path(cx, ui, target.with_path(path), intent);
                    }
                    None => {
                        self.show_failure(cx, ta("common.errors.noFileFound", &[("token", &token)]))
                    }
                }
            }
            ActionKind::ProbeFolder {
                generation,
                target,
                intent,
            } => {
                if OPEN_GENERATION.with(Cell::get) != generation {
                    return;
                }
                let file = &target.path;
                let directory = matches!(&result, Ok(r) if get(r, "error").is_null() && !get(r, "directory").is_null());
                if directory {
                    self.finish_open(ui, None, file, None, None, intent);
                } else {
                    self.finish_open(
                        ui,
                        Some(file),
                        "",
                        target.line_start,
                        target.line_end,
                        intent,
                    );
                }
            }
        }
    }

    fn on_reveal(&mut self, cx: &mut Cx, ui: &mut UiContext, token: u64, ok: bool) {
        let Some(purpose) = self.reveals.remove(&token) else {
            return;
        };
        match purpose {
            RevealFor::Find(find_token) => {
                let (epoch, rows) = self
                    .target
                    .as_ref()
                    .and_then(|target| {
                        with_store(|store| {
                            store
                                .sessions
                                .get(&target.key())
                                .map(|s| (s.epoch.clone(), s.timeline.clone()))
                        })
                    })
                    .unwrap_or_default();
                self.find.on_load(find_token, ok, epoch.as_deref(), &rows);
                self.pump_find(cx, ui);
            }
            RevealFor::Prompt(seq) => {
                if ok {
                    // The rows arrive with this frame's model rebuild.
                    self.model_sig = None;
                    if self.visible {
                        let look = self.look(self.content_rect.size.x);
                        let track_height = tracks::pill_height(cx, &mut self.kit, look);
                        self.rebuild_items(look, track_height);
                        self.finish_prompt_jump(cx, seq);
                    } else {
                        let row = self.target.as_ref().and_then(|target| {
                            with_store(|store| {
                                store.sessions.get(&target.key()).and_then(|session| {
                                    session
                                        .timeline
                                        .iter()
                                        .find(|r| {
                                            get(get(r, "item"), "type") == "user_message"
                                                && int_of(get(r, "seqStart")) <= seq
                                                && int_or(
                                                    get(r, "seqEnd"),
                                                    int_of(get(r, "seqStart")),
                                                ) >= seq
                                        })
                                        .map(|r| int_of(get(r, "seqStart")))
                                })
                            })
                        });
                        if let Some(row) = row {
                            self.pending_jump = Some((row, 0.0, false));
                        }
                    }
                } else {
                    self.show_failure(cx, t("agentStream.historyLoadFailed"));
                }
            }
        }
    }

    /// Overlay answers (menus, confirms, prompts) for this panel.
    fn on_overlay(&mut self, cx: &mut Cx, ui: &mut UiContext, id: u64, args: &Value) {
        let Some(ask) = self.asks.remove(&id) else {
            return;
        };
        let cancelled = is_true(get(args, "cancelled"));
        match ask {
            Ask::Rename => {
                if cancelled {
                    return;
                }
                let Some(name) = get(args, "text").as_str() else {
                    return;
                };
                let name = name.trim();
                if name.is_empty() || name.chars().count() > 200 {
                    self.show_failure(cx, t("agentPanel.conversation.titleLength"));
                    return;
                }
                let name = name.to_owned();
                self.with_session(cx, ui, |s, io| {
                    s.action(
                        io,
                        ActionKind::Rename,
                        "update_agent_request",
                        "update_agent_response",
                        json!({"name": name}),
                    )
                });
            }
            Ask::Confirm(action) => {
                if !is_true(get(args, "confirmed")) {
                    return;
                }
                if action == "archive" {
                    // The archiving overlay covers the panel until the tab closes.
                    self.archiving = true;
                    self.redraw_chrome_part(cx, ARCHIVING);
                    self.with_session(cx, ui, |s, io| {
                        s.action(
                            io,
                            ActionKind::Archive,
                            "archive_agent_request",
                            "agent_archived",
                            json!({}),
                        )
                    });
                } else {
                    self.with_session(cx, ui, |s, io| {
                        s.action(
                            io,
                            ActionKind::Delete,
                            "delete_agent_request",
                            "agent_deleted",
                            json!({}),
                        )
                    });
                }
            }
            Ask::ArchiveSubagent(row) => {
                if !is_true(get(args, "confirmed")) {
                    return;
                }
                let key = format!("{}:{}", row.server_id, row.id);
                self.pending_removal.insert(key.clone());
                self.model_sig = None;
                self.redraw_timeline(cx);
                self.redraw_chrome_part(cx, TRACKS);
                self.redraw_modal(cx);
                let fields = json!({"agentId": row.id});
                self.with_session(cx, ui, |s, io| {
                    s.request_on_host(
                        io,
                        &row.server_id,
                        ActionKind::ArchiveSubagent { key },
                        "archive_agent_request",
                        "agent_archived",
                        fields,
                    )
                });
            }
            Ask::DeleteFinished => {
                if is_true(get(args, "confirmed")) {
                    self.delete_finished(cx, ui);
                }
            }
            Ask::DetachSubagent(row) => {
                if !is_true(get(args, "confirmed")) {
                    return;
                }
                let child_connected = host_row(&ui.state, &row.server_id)
                    .is_some_and(|h| get(h, "status") == "connected");
                if !child_connected {
                    self.show_failure(cx, t("workspaceSetup.errors.hostDisconnected"));
                    return;
                }
                let agent_id = row.id.clone();
                let kind = ActionKind::DetachSubagent {
                    host_id: row.server_id.clone(),
                    agent_id: agent_id.clone(),
                };
                self.with_session(cx, ui, |s, io| {
                    s.request_on_host(
                        io,
                        &row.server_id,
                        kind,
                        "agent.detach.request",
                        "agent.detach.response",
                        json!({"agentId": agent_id}),
                    )
                });
            }
            Ask::Rewind { row, entry } => {
                self.redraw_timeline(cx);
                if let Some(state) = self.row_states.get(&row) {
                    let mut state = state.borrow_mut();
                    if state.menu_open.remove("rewind") {
                        state.invalidate();
                    }
                }
                if !cancelled && let Some(mode) = get(args, "value").as_str().map(str::to_owned) {
                    let item = get(&entry, "item");
                    let text = str_of(get(item, "text")).to_owned();
                    let message = get(item, "messageId").clone();
                    let sent = self
                        .with_session(cx, ui, |s, io| {
                            s.action(
                                io,
                                ActionKind::Rewind {
                                    mode: mode.clone(),
                                    text,
                                },
                                "agent.rewind.request",
                                "agent.rewind.response",
                                json!({"messageId": message, "mode": mode}),
                            )
                        })
                        .is_some();
                    // Only a sent request completes and clears busy.
                    if sent && let Some(state) = self.row_states.get(&row) {
                        let mut state = state.borrow_mut();
                        if state.menu_busy.insert("rewind".into()) {
                            state.invalidate();
                        }
                    }
                }
            }
            Ask::Outline => {
                if let Some(seq) = get(args, "value")
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                {
                    self.jump_to_prompt(cx, ui, seq);
                }
            }
        }
    }

    // ── row and chrome taps ─────────────────────────────────────────────

    fn on_row_event(&mut self, cx: &mut Cx, ui: &mut UiContext, event: ChatRowEvent) {
        let row = event.row;
        match event.kind {
            RowEventKind::SelectionScroll { delta } => {
                if !self.input_is_modal()
                    && self.items.iter().any(|item| item.id == row)
                    && self
                        .row_states
                        .get(&row)
                        .is_some_and(|state| state.borrow().selection.is_some())
                {
                    if delta != 0.0 {
                        self.reader_scrolled = true;
                        self.at_bottom = false;
                        self.detach();
                    }
                    self.list.as_portal_list().scroll_selection(cx, delta);
                }
            }
            RowEventKind::RevealTop => {
                self.reveal_top = Some(row);
            }
            RowEventKind::Answer { key, dismiss } => {
                let Some(snap) = &self.snap else { return };
                let Some((_, request)) = snap.permissions.iter().find(|(k, _)| *k == key).cloned()
                else {
                    return;
                };
                let Some(qs) = permission::questions(&request) else {
                    return;
                };
                let state = self
                    .row_state(&row)
                    .borrow()
                    .permission
                    .clone()
                    .unwrap_or_default();
                let response = permission::question_response(&request, &qs, &state, dismiss);
                self.respond_permission(
                    cx,
                    ui,
                    &key,
                    if dismiss { "dismiss" } else { "submit" },
                    response,
                );
            }
            RowEventKind::Tap {
                target,
                rect,
                intent,
            } => match target {
                HitTarget::Link { href, kind, text } => {
                    self.open_link(cx, ui, &href, &text, kind, intent)
                }
                HitTarget::OpenFile(path) => {
                    let target = InlinePathTarget {
                        raw: path.clone(),
                        path,
                        line_start: None,
                        line_end: None,
                    };
                    self.open_inline_path(cx, ui, target, intent);
                }
                HitTarget::Image { key } => {
                    self.open_lightbox(ui, LightboxSource::Assistant { url: key, row });
                }
                HitTarget::UserImage { index } => {
                    if let Some(entry) = self.entry_of(&row)
                        && LightboxSource::user_image(&entry, index).is_some()
                    {
                        self.open_lightbox(ui, LightboxSource::User { entry, index });
                    }
                }
                HitTarget::FoldTurn(key) => self.toggle_turn(cx, key),
                HitTarget::OpenSheet(sheet) => {
                    let (kind, _) = sheet.split_once(':').unwrap_or((sheet.as_str(), ""));
                    self.modal = Some(Modal::Sheet {
                        row: row.clone(),
                        kind: kind.to_owned(),
                    });
                    self.detail_motion.set_open(cx, true);
                    self.modal_state = RowState::default();
                }
                HitTarget::RewindMenu => {
                    let Some(entry) = self.entry_of(&row) else {
                        return;
                    };
                    let capabilities = self
                        .snap
                        .as_ref()
                        .map(|s| get(&s.agent, "capabilities").clone())
                        .unwrap_or_default();
                    let mut entries = vec![MenuEntry::Label(t("rewind.warning"))];
                    for mode in rows::rewind_modes(&capabilities) {
                        let icon = match mode {
                            "conversation" => "message-square",
                            "files" => "file-text",
                            _ => "layers",
                        };
                        entries.push(
                            MenuItem::new(mode, t(&format!("rewind.actions.{mode}")))
                                .icon(icon)
                                .entry(),
                        );
                    }
                    if let Some(state) = self.row_states.get(&row) {
                        let mut state = state.borrow_mut();
                        if state.menu_open.insert("rewind".into()) {
                            state.invalidate();
                        }
                    }
                    let id = overlay::open_menu_with(
                        ui,
                        rect,
                        None,
                        entries,
                        MenuOptions {
                            align_end: true,
                            ..Default::default()
                        },
                    );
                    self.asks.insert(id, Ask::Rewind { row, entry });
                }
                HitTarget::ForkMenu { completed } => {
                    let entry = if completed { self.entry_of(&row) } else { None };
                    // A request that was never sent has no completion to clear busy.
                    if self.fork(cx, ui, entry)
                        && let Some(state) = self.row_states.get(&row)
                    {
                        let mut state = state.borrow_mut();
                        if state.menu_busy.insert(if completed {
                            format!("fork:{row}")
                        } else {
                            "fork:live".into()
                        }) {
                            state.invalidate();
                        }
                    }
                }
                HitTarget::PermissionAction { key, action } => {
                    let Some(snap) = &self.snap else { return };
                    let Some((_, request)) =
                        snap.permissions.iter().find(|(k, _)| *k == key).cloned()
                    else {
                        return;
                    };
                    if action == permission::OPEN_ELICITATION_URL && permission::is_elicitation(&request) {
                        if let Some(url) = permission::elicitation_url(&request) {
                            ui.push(UiEffect::OpenExternal { url });
                        }
                        return;
                    }
                    let Some(choice) = permission::actions(&request)
                        .into_iter()
                        .find(|a| str_of(get(a, "id")) == action)
                    else {
                        return;
                    };
                    let response = permission::action_response(&choice);
                    self.respond_permission(cx, ui, &key, &action, response);
                }
                _ => {}
            },
        }
        self.redraw_timeline(cx);
        self.redraw_modal(cx);
    }

    fn on_chrome_tap(&mut self, cx: &mut Cx, ui: &mut UiContext, id: &str) {
        match id {
            "retry-sync" => {
                self.with_session(cx, ui, |s, io| s.retry_sync(io));
            }
            "retry-preparation" => {
                self.with_session(cx, ui, |s, io| s.retry_preparation(io));
            }
            "retry-child" => {
                self.with_session(cx, ui, |s, io| s.retry_child_refresh(io));
            }
            "unarchive" => {
                self.unarchiving = true;
                self.unarchive_error = None;
                self.with_session(cx, ui, |s, io| {
                    s.action(
                        io,
                        ActionKind::Unarchive,
                        "refresh_agent_request",
                        "status",
                        json!({}),
                    )
                });
            }
            "jump-latest" => self.jump_to_latest(cx),
            "retry-connection" => {
                if let Some(target) = &self.target {
                    ui.core.refresh_host(&target.host_id);
                }
            }
            "track:tasks" => self.open_track(cx, ui, TrackPanel::Tasks),
            "track:subagents" => self.open_track(cx, ui, TrackPanel::Subagents),
            "track-close" => self.close_modal(cx, ui),
            "delete-finished" => self.ask_delete_finished(ui),
            "find:next" => {
                let (epoch, rows) = self
                    .snap
                    .as_ref()
                    .map(|s| (s.epoch.clone(), s.timeline.clone()))
                    .unwrap_or_default();
                self.find.next(epoch.as_deref(), &rows);
                self.pump_find(cx, ui);
            }
            "find:previous" => {
                let (epoch, rows) = self
                    .snap
                    .as_ref()
                    .map(|s| (s.epoch.clone(), s.timeline.clone()))
                    .unwrap_or_default();
                self.find.previous(epoch.as_deref(), &rows);
                self.pump_find(cx, ui);
            }
            "find:close" => self.close_find(cx),
            "find:retry" => {
                self.find.retry();
                self.pump_find(cx, ui);
            }
            "sheet:close" | "lightbox:close" | "scrim" => self.close_modal(cx, ui),
            "sheet:open-tab" => {
                if let Some(Modal::Subagent { tab, .. }) = &self.modal {
                    let tab = tab.clone();
                    self.dismiss_modal(cx);
                    ui.push(UiEffect::OpenTab {
                        target: tab,
                        placement: TabPlacement::Main,
                    });
                }
            }
            "lightbox:save" => self.export_lightbox(ui, ExportMode::Save),
            #[cfg(not(target_arch = "wasm32"))]
            "lightbox:share" => self.export_lightbox(ui, ExportMode::Share),
            "lightbox:zoom-in" => self.zoom(1.5),
            "lightbox:zoom-out" => self.zoom(1.0 / 1.5),
            "lightbox:reset" => {
                if let Some(Modal::Lightbox { scale, offset, .. }) = &mut self.modal {
                    *scale = 1.0;
                    *offset = DVec2::default();
                }
            }
            other => {
                if let Some(toast) = other.strip_prefix("toast:") {
                    self.dismiss_toast(cx, ui, toast);
                } else if let Some(token) = other.strip_prefix(task_controls::HIT_PREFIX) {
                    self.stop_provider_task(cx, ui, token);
                } else if let Some(key) = other.strip_prefix("subagent-open:") {
                    if let Some(row) = self.subagent_rows().into_iter().find(|r| r.key() == key) {
                        if !self.opens_in_sheet(ui, &row) {
                            self.close_modal(cx, ui);
                        } else {
                            self.end_track_input(cx);
                            self.modal = None;
                        }
                        self.open_subagent(cx, ui, &row);
                    }
                } else if let Some(key) = other.strip_prefix("subagent-archive:") {
                    if let Some(row) = self.subagent_rows().into_iter().find(|r| r.key() == key) {
                        self.ask_archive_subagent(ui, row);
                    }
                } else if let Some(key) = other.strip_prefix("subagent-detach:")
                    && let Some(row) = self.subagent_rows().into_iter().find(|r| r.key() == key)
                {
                    self.ask_detach_subagent(ui, row);
                }
            }
        }
        self.redraw_modal(cx);
        if id == "unarchive" {
            self.redraw_chrome_part(cx, CALLOUTS);
        }
    }

    fn open_lightbox(&mut self, ui: &UiContext, source: LightboxSource) {
        let Some(target) = &self.target else {
            return;
        };
        let (export_available, file_route) = if MOBILE || cfg!(target_arch = "wasm32") {
            match source.original(target.cwd.as_deref()) {
                Some(Original::File { .. }) => {
                    let route = HostRoute::capture(ui, &target.host_id);
                    (route.is_some(), route)
                }
                Some(_) => (true, None),
                None => (false, None),
            }
        } else {
            (false, None)
        };
        self.modal = Some(Modal::Lightbox {
            source,
            epoch: self.snap.as_ref().and_then(|snap| snap.epoch.clone()),
            file_route,
            export_available,
            export: LightboxExport::default(),
            scale: 1.0,
            offset: DVec2::default(),
        });
        self.modal_state = RowState::default();
    }

    fn export_lightbox(&mut self, ui: &UiContext, mode: ExportMode) {
        let (
            Some(target),
            Some(Modal::Lightbox {
                source,
                export_available: true,
                file_route,
                export,
                ..
            }),
        ) = (&self.target, &mut self.modal)
        else {
            return;
        };
        if let Some(original) = source.original(target.cwd.as_deref()) {
            export.start(ui, file_route.as_ref(), mode, original);
        }
    }

    fn open_track(&mut self, cx: &mut Cx, ui: &mut UiContext, panel: TrackPanel) {
        let compact = self.look(self.content_rect.size.x).compact;
        if !compact {
            match AnchoredPopover::toggle(
                &mut self.track_popover,
                cx,
                self.track_anchors[panel as usize],
            ) {
                Some(true) => {}
                Some(false) => {
                    self.close_track(cx);
                    return;
                }
                None => return,
            }
        }
        let same_layer = self.track_painted_panel == Some(panel);
        self.track_motion.show(cx, panel, compact);
        self.track_hit_rect = None;
        self.modal = Some(Modal::Track(panel));
        if !same_layer {
            self.modal_state = RowState::default();
        }
        self.modal_pressed.cancel();
        self.bar_drag = None;
        self.modal_touch_scroll.reset();
        self.modal_horizontal_scroll.reset();
        self.redraw_modal(cx);
        self.redraw_chrome_part(cx, TRACKS);
        if panel == TrackPanel::Subagents {
            self.ensure_providers(cx, ui);
        }
    }

    fn ensure_providers(&mut self, _cx: &mut Cx, ui: &mut UiContext) {
        if self.providers_requested || self.providers_request.is_some() {
            return;
        }
        let Some(target) = self.target.as_ref() else {
            return;
        };
        self.providers_requested = true;
        self.providers_request = Some(crate::features::composer::service::with(|svc| {
            svc.request_providers(&ui.core, &target.host_id, target.cwd())
        }));
    }

    fn zoom(&mut self, factor: f64) {
        if let Some(Modal::Lightbox { scale, offset, .. }) = &mut self.modal {
            let next = (*scale * factor).clamp(1.0, 8.0);
            *offset *= next / *scale;
            *scale = next;
            if next == 1.0 {
                *offset = DVec2::default();
            }
        }
    }

    // ── attention ───────────────────────────────────────────────────────

    fn attention_state(&self, ui: &UiContext) -> attention::State {
        let target = self.target.clone().unwrap_or_default();
        let id = (!target.is_subagent()).then(|| target.agent_id.clone());
        let agent = self
            .host
            .agent_row
            .clone()
            .or_else(|| self.snap.as_ref().map(|s| (*s.agent).clone()))
            .unwrap_or_default();
        attention::State {
            agent_id: id,
            connected: self.host.connected,
            requires_attention: is_true(get(&agent, "requiresAttention")),
            attention_reason: optional(get(&agent, "attentionReason")),
            screen_focused: self.focused,
            app_visible: ui.activity.app_active,
        }
    }

    fn sync_attention(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if self.target.as_ref().is_none_or(|t| t.sheet) {
            return;
        }
        let next = self.attention_state(ui);
        let focused = next.screen_focused;
        match &mut self.attention {
            None => self.attention = Some(AttentionClear::new(next)),
            Some(a) => a.update(next),
        }
        if self.attention_focused
            && !focused
            && let Some(a) = &mut self.attention
        {
            a.clear_on(attention::Trigger::AgentBlur);
        }
        self.attention_focused = focused;
        self.flush_attention(cx, ui);
    }

    fn flush_attention(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let cleared = self
            .attention
            .as_mut()
            .map(|a| std::mem::take(&mut a.cleared))
            .unwrap_or_default();
        for agent_id in cleared {
            // daemon-client clearAgentAttention; failures are ignored.
            self.with_session(cx, ui, |s, io| {
                s.host_request(
                    io,
                    ActionKind::ClearAttention,
                    "clear_agent_attention",
                    "clear_agent_attention_response",
                    json!({"agentId": agent_id}),
                )
            });
        }
    }

    // ── composer ────────────────────────────────────────────────────────

    fn composer_shown(&self) -> bool {
        let Some(target) = &self.target else {
            return false;
        };
        let archived = self.snap.as_ref().is_some_and(|s| s.archived);
        !target.is_subagent() && !target.sheet && !archived && !self.archiving
    }

    fn sync_composer(&mut self, cx: &mut Cx) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let next = bridge::target(
            &target.host_id,
            &target.workspace_id,
            target.cwd(),
            target.shown_id(),
            target.is_subagent(),
            &target.tab_key,
            if target.sheet {
                json!({"paneHost": "sheet"})
            } else {
                json!({})
            },
        );
        let composer = self.composer.as_composer_panel();
        if self.composer_target.as_ref() != Some(&next) {
            composer.set_target(cx, next.clone());
            self.composer_target = Some(next);
            self.composer_version = None;
        }
        if let Some(snap) = &self.snap
            && self.composer_version != Some(snap.version)
        {
            self.composer_version = Some(snap.version);
            let pending = !snap.permissions.is_empty();
            composer.set_agent(
                cx,
                bridge::snapshot(snap.agent.clone(), snap.running, snap.authorized, pending),
            );
        }
    }
}

impl Widget for ChatPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.rail.advance(cx, event);
        if self.target.is_none() {
            return;
        }
        let composer_visible = self.visible && self.composer_shown();
        self.advance_tracks(cx, event);
        if self.detail_motion.advance(cx, event) && matches!(self.modal, Some(Modal::Sheet { .. }))
        {
            self.detail_motion
                .compose(cx, &self.modal_content_list, self.modal_rect);
            self.detail_motion.finish_open(cx, self.modal_rect);
            if !self.detail_motion.open && !self.detail_motion.is_moving() {
                self.dismiss_modal(cx);
            }
        }
        if self
            .timeline_texture_wait
            .is_some_and(|rev| rev != crate::features::composer::texture_revision())
        {
            self.timeline_texture_wait = None;
            self.redraw_timeline(cx);
        }
        if self
            .modal_texture_wait
            .is_some_and(|rev| rev != crate::features::composer::texture_revision())
        {
            self.modal_texture_wait = None;
            self.redraw_modal(cx);
        }
        self.reconcile_track_popover(cx);
        let modal_open = self.input_is_modal();
        if let Event::KeyFocus(e) = event
            && self
                .track_anchors
                .iter()
                .any(|area| !area.is_empty() && (*area == e.focus || *area == e.prev))
        {
            self.redraw_chrome_part(cx, TRACKS);
        }
        let modal_input = crate::common::is_user_input(event);
        if matches!(event, Event::Foreground | Event::Resume) {
            self.release_reveal_backlog();
        }
        if matches!(
            event,
            Event::TextHandle(_) | Event::Pause | Event::AppLostFocus
        ) || matches!(event, Event::TouchUpdate(update) if update.cancelled)
        {
            self.pressed.cancel();
            self.modal_pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            self.lightbox_drag = None;
        }
        if modal_input
            && scope
                .data
                .get::<UiContext>()
                .is_some_and(|ui| !self.modal_branch_accepts_input(&ui.window_id))
        {
            self.pressed.cancel();
            self.modal_pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            return;
        }
        // Composer sheets are painted above the stream. Dispatch them before
        // any row can take the pointer/key focus, and retain ownership on close.
        if self.visible
            && !modal_open
            && self.composer_shown()
            && self.composer.as_composer_panel().captures_input()
            && modal_input
        {
            self.composer.handle_event(cx, event, scope);
            if let Some(ui) = scope.data.get::<UiContext>() {
                self.sync_window_modal(cx, ui);
            }
            return;
        }
        // Snapshot descendants before dispatch. A child closing itself still owns
        // the event; an ancestor claims dismissal only when no descendant needs it.
        let child = matches!(self.modal, Some(Modal::Subagent { .. }))
            .then(|| self.sheet_panel.clone())
            .flatten();
        let child_owns_input = child.as_ref().is_some_and(|panel| {
            panel.as_chat_panel().borrow().is_some_and(|p| {
                let composer = p.composer.as_composer_panel();
                p.input_is_modal()
                    || composer.captures_input()
                    || composer.consumes_escape(cx, event)
            })
        });
        if modal_open
            && !child_owns_input
            && matches!(
                event,
                Event::BackPressed { .. }
                    | Event::KeyDown(KeyEvent {
                        key_code: KeyCode::Escape,
                        ..
                    })
            )
        {
            if overlay::take_dismissal(event)
                && let Some(ui) = scope.data.get_mut::<UiContext>()
            {
                self.close_modal(cx, ui);
                self.sync_window_modal(cx, ui);
            }
            return;
        }
        if let Some(panel) = child {
            if let Some(mut child) = panel.as_chat_panel().borrow_mut() {
                child.input_parent = Some(self.widget_uid().0);
            }
            panel.handle_event(cx, event, scope);
            if child_owns_input && modal_input {
                return;
            }
        }
        if let Event::KeyDown(ke) = event {
            self.last_shift = ke.modifiers.shift;
        }
        if !modal_open
            && self.painted_look.is_some_and(|look| !look.compact)
            && let Event::KeyDown(key) = event
            && matches!(key.key_code, KeyCode::ReturnKey | KeyCode::Space)
            && key.modifiers == KeyModifiers::default()
            && let Some(panel) = [TrackPanel::Tasks, TrackPanel::Subagents]
                .into_iter()
                .find(|panel| cx.has_key_focus(self.track_anchors[*panel as usize]))
            && let Some(ui) = scope.data.get_mut::<UiContext>()
        {
            if !key.is_repeat {
                self.open_track(cx, ui, panel);
            }
            return;
        }
        // Wheel over a row's inner scroll region (code panes, capped details)
        // moves that region instead of the stream.
        let mut swallow_scroll = false;
        if let Event::Scroll(se) = event {
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            self.pressed.cancel();
            self.modal_pressed.cancel();
            let over_popover = self.track_popover.is_some()
                && self.track_hit_rect.is_some()
                && self.track_popover_area.clipped_rect(cx).contains(se.abs)
                && !cx.pointer_shielded(self.track_popover_area, se.abs);
            if modal_open || over_popover {
                swallow_scroll = true;
                self.modal_scroll(cx, se.abs, se.scroll);
            } else if self.stream_rect.contains(se.abs) && !cx.pointer_shielded(self.area, se.abs) {
                let list = self.list.as_portal_list();
                let first = list.first_id();
                let count = list.visible_items() + 2;
                for id in first..(first + count).min(self.items.len()) {
                    if let Some((_, item)) = list.get_item(id)
                        && item.as_chat_row().scroll_inner(cx, se.abs, se.scroll)
                    {
                        swallow_scroll = true;
                        break;
                    }
                }
                if !swallow_scroll && se.scroll.y != 0.0 {
                    // PortalList's at_end still describes the previous draw.
                    // Release tail mode before the wheel delta reaches it.
                    self.reader_scrolled = true;
                    self.at_bottom = false;
                    self.detach();
                }
            }
        }
        if !modal_open {
            let reader_input = match event {
                Event::MouseDown(e)
                    if self.stream_rect.contains(e.abs)
                        && e.abs.x >= self.stream_rect.pos.x + self.stream_rect.size.x - 16.0
                        && !cx.pointer_shielded(self.area, e.abs) =>
                {
                    // Focus only the scrollbar gutter, not selectable rows or
                    // their inputs, so Home/End navigate the list afterwards.
                    cx.set_key_focus(self.list.area());
                    true
                }
                Event::KeyDown(e) => {
                    cx.has_key_focus(self.list.area())
                        && matches!(
                            e.key_code,
                            KeyCode::Home
                                | KeyCode::End
                                | KeyCode::PageUp
                                | KeyCode::PageDown
                                | KeyCode::ArrowUp
                                | KeyCode::ArrowDown
                        )
                }
                _ => false,
            };
            if reader_input {
                self.reader_scrolled = true;
                self.at_bottom = false;
                self.detach();
            }
        }
        // The find field sits on the find card above the stream: it claims the
        // pointer before the card's chrome hit or any row underneath.
        let block_pointer = modal_open && modal_input;
        if self.find_open && !block_pointer {
            self.find_field.handle_event(cx, event, scope);
        }
        // Modal hit testing covers its backdrop; an anchored popover only hits the card.
        // Releasing an outside click can still activate its original press target.
        let hit = if modal_open {
            let rect = self.modal_rect;
            let child =
                matches!(self.modal, Some(Modal::Subagent { .. })).then_some(self.sheet_content);
            let (area, rect) = if matches!(self.modal, Some(Modal::Track(_))) {
                (self.track_backdrop, self.track_bounds)
            } else if matches!(self.modal, Some(Modal::Sheet { .. })) {
                (self.detail_area, rect)
            } else {
                (self.modal_area, rect)
            };
            event.hits_with_test(cx, area, |abs, _, _| {
                rect.contains(abs) && !child.is_some_and(|r| r.contains(abs))
            })
        } else if self.track_popover.is_some() && self.track_hit_rect.is_some() {
            event.hits(cx, self.track_popover_area)
        } else {
            Hit::Nothing
        };
        if self.modal.is_none()
            || matches!(self.modal, Some(Modal::Lightbox { .. }))
            || self.bar_drag.is_some()
        {
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
        } else {
            let dy = self.modal_touch_scroll.handle_event(cx, event, &hit);
            let dx = self
                .modal_horizontal_scroll
                .handle_axis_event(cx, event, &hit, Vec2Index::X);
            let origin = self.modal_touch_scroll.origin();
            if let Some(delta) = dy {
                self.modal_pressed.cancel();
                if !self.modal_scroll(cx, origin, dvec2(0.0, delta)) {
                    self.modal_touch_scroll.stop_at_edge();
                }
            }
            if let Some(delta) = dx {
                self.modal_pressed.cancel();
                if !self.modal_scroll(cx, origin, dvec2(delta, 0.0)) {
                    self.modal_horizontal_scroll.stop_at_edge();
                }
            }
        }
        let mut chrome_tap = self.handle_chrome_hit(cx, hit, true);
        if !modal_open {
            let find_field = self.find_field_rect.filter(|_| self.find_open);
            let area = if self.cover_area.is_empty() {
                self.area
            } else {
                self.cover_area
            };
            let hit = event.hits_with_test(cx, area, |abs, _, _| {
                !find_field.is_some_and(|r| r.contains(abs))
                    && self
                        .overlay_canvas
                        .hit_at(abs - self.overlay_canvas.origin)
                        .is_some()
            });
            chrome_tap = self.handle_chrome_hit(cx, hit, false).or(chrome_tap);
        }
        // Children: the stream and the composer.
        if !swallow_scroll && !block_pointer {
            let list = self.list.as_portal_list();
            let previous_scroll = list.scroll_position();
            self.list.handle_event(cx, event, scope);
            // Selection and horizontal child drags are not timeline scrolling.
            // Classify only after the common gesture owner has applied a delta.
            if matches!(event, Event::TouchUpdate(_)) && list.scroll_position() != previous_scroll {
                self.reader_scrolled = true;
                self.at_bottom = false;
                self.detach();
            }
        }
        // file-drop-zone.tsx: files dropped anywhere on the pane go to its composer. No
        // composer (archived, archiving, a subagent) or a busy one refuses the drag.
        let drop_ready = !modal_open
            && self.non_ready().is_none()
            && self
                .composer
                .as_composer_panel()
                .accepts_file_drop(composer_visible);
        if let Some(paths) = self.file_drop.handle_event(cx, event, drop_ready) {
            self.composer.as_composer_panel().drop_files(cx, paths);
        }
        let effects_before = scope
            .data
            .get::<UiContext>()
            .map(|ui| ui.effects.len())
            .unwrap_or(0);
        if !block_pointer || !composer_visible {
            let activity = scope.data.get::<UiContext>().map(|ui| ui.activity);
            if let Some(ui) = scope.data.get_mut::<UiContext>() {
                ui.activity.visible = composer_visible && ui.activity.visible;
            }
            self.composer.handle_event(cx, event, scope);
            if let (Some(activity), Some(ui)) = (activity, scope.data.get_mut::<UiContext>()) {
                ui.activity = activity;
            }
        }
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return;
        };
        // composer/index.tsx handleFocusChange → onAttentionInputFocus.
        let input_focused = ui.effects[effects_before.min(ui.effects.len())..]
            .iter()
            .any(|e| matches!(e, UiEffect::SetFocusScope { scope: Some(s), .. } if s == "message-input"));
        self.ui_pass(cx, event, ui, chrome_tap, input_focused);
        self.sync_window_modal(cx, ui);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        let rect = cx.turtle().rect();
        self.content_rect = rect;
        while self.chrome_layers.len() < CHROME_LAYERS {
            self.chrome_layers.push(ChromeLayer::new(cx));
        }
        self.cover_area = Area::Empty;
        if let Some(ui) = scope.data.get_mut::<UiContext>() {
            if self.visible && ui.activity.visible {
                self.ensure_session(cx, ui, true);
            }
            self.refresh_host(cx, ui);
        }
        let changed = self.refresh_snapshot(cx);
        if self.visible
            && (changed || self.snapshot_stale)
            && let Some(ui) = scope.data.get_mut::<UiContext>()
        {
            self.snapshot_stale = false;
            self.on_session_changed(cx, ui);
        }
        let look = self.look(rect.size.x);
        self.reconcile_track_popover(cx);
        if self.track_motion.panel.is_some() && look.compact != self.track_motion.compact {
            if matches!(self.modal, Some(Modal::Track(_))) {
                self.dismiss_modal(cx);
            } else {
                self.reset_tracks(cx);
            }
        }
        if self.painted_look != Some(look) {
            self.painted_look = Some(look);
            self.redraw_session(cx);
        }
        let p = look.palette;
        self.kit.begin_layer(cx);
        self.kit.rect(cx, rect, p.surface0, 0.0);
        let target = self.target.clone().unwrap_or_default();
        let non_ready = self.non_ready();
        if let Some(view) = non_ready {
            if matches!(self.modal, Some(Modal::Track(_))) {
                self.dismiss_modal(cx);
            }
            self.reset_tracks(cx);
            self.track_overlay.release(cx);
            if let Some(center) = self.chrome_layers[NON_READY].paint(
                cx,
                &mut self.kit,
                &mut self.chrome_state,
                look,
                rect,
                |cx, d| chrome::non_ready(cx, d, &view, rect),
            ) {
                self.non_ready_indicator = center;
            }
            if let Some(center) = self.non_ready_indicator {
                self.chrome_layers[NON_READY_INDICATOR].paint(
                    cx,
                    &mut self.kit,
                    &mut self.chrome_state,
                    look,
                    rect,
                    |cx, d| chrome::spinner(cx, d, center, 24.0, p.muted),
                );
            }
            self.timeline_list.release(cx);
            self.composer_list.release(cx);
            self.find_list.release(cx);
            self.modal_content_list.release(cx);
            self.finish_chrome(cx);
            cx.end_turtle_with_area(&mut self.area);
            return DrawStep::done();
        }
        let compact = look.compact;
        let track_height = tracks::pill_height(cx, &mut self.kit, look);
        let snap = self.snap.clone();
        let subagent = target.is_subagent();
        let archived = snap.as_ref().is_some_and(|s| s.archived);
        let connected = self.host.connected;

        // ── geometry ────────────────────────────────────────────────────
        let facts = if target.sheet && !subagent {
            snap.as_ref()
                .and_then(|s| tracks::facts_line(&self.providers, &s.agent))
        } else {
            None
        };
        let mut callouts = Vec::new();
        if let Some(s) = &snap {
            if !connected {
                callouts.push(Callout::Disconnected {
                    host: self.host.label.clone(),
                });
            }
            if connected && !archived && matches!(s.sync, SyncStatus::Error | SyncStatus::Retrying)
            {
                callouts.push(Callout::SyncError {
                    retrying: s.sync == SyncStatus::Retrying,
                });
            }
            if !subagent && !archived && s.execution != ExecutionStatus::Ready {
                callouts.push(Callout::Preparation {
                    subagent: false,
                    failed: s.execution == ExecutionStatus::Failed,
                    error: s.preparation_error.clone(),
                    retrying: s.preparation_retrying,
                });
            }
            if subagent && (s.child_refreshing || s.child_refresh_error.is_some()) {
                callouts.push(Callout::Preparation {
                    subagent: true,
                    failed: s.child_refresh_error.is_some(),
                    error: s.child_refresh_error.clone(),
                    retrying: s.preparation_retrying,
                });
            }
        }
        let show_archived_callout = !subagent && !target.sheet && archived;
        let (facts_h, callouts_h, archived_h) = {
            let mut d = Draw {
                kit: &mut self.kit,
                canvas: &mut self.overlay_canvas,
                look,
                state: &mut self.chrome_state,
                owner: None,
                now: now_seconds(),
                area: self.area,
                animating: false,
                indicators: None,
            };
            d.measure(|d| {
                let facts_h = facts
                    .as_ref()
                    .map(|f| chrome::pane_subtitle(cx, d, f, rect.pos.x, rect.pos.y, rect.size.x))
                    .unwrap_or(0.0);
                let callouts_h: f64 = callouts
                    .iter()
                    .map(|c| chrome::callout(cx, d, c, rect.pos.x, 0.0, rect.size.x))
                    .sum();
                let archived_h = if show_archived_callout {
                    chrome::archived_callout(
                        cx,
                        d,
                        connected,
                        self.unarchiving,
                        self.unarchive_error.as_deref(),
                        rect.pos.x,
                        0.0,
                        rect.size.x,
                    )
                } else {
                    0.0
                };
                (facts_h, callouts_h, archived_h)
            })
        };
        // ── the composer (drawn first so the stream gets its exact height) ─
        // It rests on the panel bottom at last frame's height; a height change
        // shifts what it just drew by the difference in the same frame.
        let composer_shown = self.composer_shown();
        if composer_shown {
            self.sync_composer(cx);
            let extension = snap
                .as_ref()
                .filter(|_| connected && !subagent)
                .and_then(|snap| ExtensionDock::new(&snap.agent, rect.size.y, look));
            // The pane below its header is the composer dock's stationary viewport.
            self.composer
                .as_composer_panel()
                .set_viewport_height(
                    rect.size.y - extension.as_ref().map_or(0.0, ExtensionDock::height),
                );
            let bottom = rect.pos.y + rect.size.y;
            let walk = Walk {
                abs_pos: Some(dvec2(rect.pos.x, bottom - self.composer_h)),
                margin: Margin::default(),
                width: Size::Fixed(rect.size.x),
                height: Size::Fit,
            };
            let dirty = cx.will_redraw(
                &mut self.composer_list,
                Walk::abs_rect(rect_xywh(
                    rect.pos.x,
                    bottom - self.composer_h,
                    rect.size.x,
                    self.composer_h,
                )),
            );
            if self.composer_list.begin_maybe(cx, dirty).is_ok() {
                cx.begin_turtle(walk, Layout::flow_down());
                if let Some(extension) = &extension {
                    self.kit.begin_layer(cx);
                    extension.above(cx, &mut self.kit, look);
                }
                let _ = self.composer.draw_walk(
                    cx,
                    scope,
                    Walk {
                        abs_pos: None,
                        margin: Margin::default(),
                        width: Size::Fill,
                        height: Size::Fit,
                    },
                );
                if let Some(extension) = &extension {
                    extension.below(cx, &mut self.kit, look);
                }
                let range = cx.get_turtle_align_range();
                let drawn = cx.end_turtle().size.y;
                if drawn.is_finite() && (drawn - self.composer_h).abs() > 0.5 {
                    cx.shift_align_range(&range, dvec2(0.0, self.composer_h - drawn));
                    self.composer_h = drawn;
                }
                self.composer_list.end(cx);
            }
        } else {
            self.composer_list.release(cx);
        }
        let composer_h = if self.composer_shown() {
            self.composer_h
        } else {
            0.0
        };
        let stream_h = (rect.size.y - facts_h - callouts_h - archived_h - composer_h).max(0.0);
        let stream_rect = rect_xywh(rect.pos.x, rect.pos.y + facts_h, rect.size.x, stream_h);
        self.stream_rect = stream_rect;
        let timeline_drawn = self
            .timeline_list
            .begin(cx, Walk::abs_rect(stream_rect))
            .is_ok();
        if timeline_drawn {
            let texture_misses = crate::features::composer::texture_misses();
            let texture_revision = crate::features::composer::texture_revision();
            // Even an empty transcript retains a draw item, so composer/chrome
            // paints can replay it without entering ModelSig or row layout.
            self.kit.begin_layer(cx);
            self.kit.rect(cx, stream_rect, p.surface0, 0.0);
            self.rebuild_items(look, track_height);
            if let Some((seq, offset, find)) = self.pending_jump.take() {
                // Once: a row still missing after the rebuild is not in the stream.
                if let Some(id) = self.row_id_of_seq(seq) {
                    self.scroll_to_row(cx, &id, offset);
                    if find {
                        self.find_scroll = Some((id, 0));
                    }
                }
            }
            if self.reveal_attention && self.snap.is_some() {
                self.reveal_attention = false;
                self.find_scroll = None;
                self.attention_row = self
                    .items
                    .iter()
                    .find(|i| matches!(i.kind, RowKind::Permission { .. }))
                    .map(|i| i.id.clone());
                self.jump_to_latest(cx);
            }
            let core = scope
                .data
                .get::<UiContext>()
                .map(|ui| ui.core.clone())
                .or_else(|| self.core.clone());
            if let Some(core) = &core {
                self.ctx = Some(self.row_context(look, core));
            }
            let empty = self.stream_empty();
            let loading = snap.as_ref().is_some_and(|s| s.loading);

            // ── the stream ──────────────────────────────────────────────────
            if !empty || loading {
                let walk = Walk {
                    abs_pos: Some(stream_rect.pos),
                    margin: Margin::default(),
                    width: Size::Fixed(stream_rect.size.x),
                    height: Size::Fixed(stream_rect.size.y.max(1.0)),
                };
                let list_ref = self.list.clone();
                let portal = list_ref.as_portal_list();
                if self.sticky {
                    // view.tsx scrollToBottom: a sticky stream rests on its newest
                    // edge (the last item drawn first, then the list scans up and
                    // aligns its bottom), however its rows grow.
                    portal.set_first_id_and_scroll(self.items.len().saturating_sub(1), 0.0);
                    portal.set_tail_range(true);
                }
                let mut presented = HashSet::new();
                while list_ref.draw_walk(cx, scope, walk).is_step() {
                    let Some(mut list) = portal.borrow_mut() else {
                        break;
                    };
                    list.set_item_range(cx, 0, self.items.len());
                    list.begin_measurement();
                    while let Some(index) = list.next_visible_item(cx) {
                        let Some(item) = self.items.get(index).cloned() else {
                            continue;
                        };
                        let widget = list.item(cx, index, live_id!(Row));
                        if let Some(ctx) = &self.ctx {
                            let state = self.row_states.entry(item.id.clone()).or_default().clone();
                            self.presentation_clock =
                                self.presentation_clock.wrapping_add(1).max(1);
                            state.borrow_mut().presentation_used = self.presentation_clock;
                            presented.insert(item.id.clone());
                            widget.as_chat_row().set_model(
                                cx,
                                RowModel {
                                    panel: self.uid,
                                    item,
                                    ctx: ctx.clone(),
                                    state,
                                },
                            );
                        }
                        let width = cx.turtle().rect().size.x;
                        let height = widget.as_chat_row().measure(cx, width);
                        cx.walk_turtle(Walk {
                            abs_pos: None,
                            margin: Margin::default(),
                            width: Size::Fill,
                            height: Size::Fixed(height),
                        });
                    }
                    while let Some((index, walk, viewport)) = list.next_final_item(cx) {
                        if let Some((_, widget)) = list.get_item(index) {
                            widget.as_chat_row().paint_final(cx, scope, walk, viewport);
                        }
                    }
                }
                self.trim_row_presentations(&presented);
                if self.reader_scrolled {
                    // Only layout can determine whether the reader reached the
                    // newest edge. Scroll actions arrive before that layout.
                    let at_end = portal.is_at_end();
                    self.sticky = at_end;
                    // strategy-native.tsx isNearBottom: the jump button hides
                    // within 32px of the newest edge, where it would only cover it.
                    self.at_bottom = portal
                        .end_overflow()
                        .is_some_and(|overflow| overflow <= 32.0);
                    portal.set_tail_range(at_end);
                }
            }
            self.timeline_texture_wait = (crate::features::composer::texture_misses()
                != texture_misses)
                .then_some(texture_revision);
            self.timeline_list.end(cx);
        }
        let empty = self.stream_empty();
        let loading = snap.as_ref().is_some_and(|s| s.loading);

        let dock_top = stream_rect.pos.y + stream_rect.size.y;

        // ── chrome over the stream ──────────────────────────────────────
        let tracks_visible = self.tracks_visible();
        let rows = if tracks_visible {
            self.subagent_rows()
        } else {
            Vec::new()
        };
        let tasks = if tracks_visible && !subagent {
            self.latest_tasks()
        } else {
            Vec::new()
        };
        let provider_tasks = self.offered_provider_tasks(snap.as_deref());
        let toast = self.panel_toast();
        let prompts_active = self.active_prompt;
        let find_open = self.find_open;
        let find_status = self.find_status();
        let find_failure = self.find.failure;
        let find_can_navigate = self.find.phase == FindPhase::Ready && self.find.count > 0;
        let history_overlay = snap.as_ref().is_some_and(|s| {
            connected
                && !archived
                && s.sync == SyncStatus::Pending
                && !s.hydrated
                && !s.initial_sync_failed
                && !s.create_handoff
                && !s.has_visible_history()
        });
        let outline_shown = !subagent && self.outline_enabled();
        let host_labels: HashMap<String, String> = self.host.labels.clone();
        let host_label_fn = move |id: &str| {
            host_labels
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_owned())
        };
        let can_detach = self.feature("agentDetach");
        let delete_status = self.delete_status;
        if match self.modal {
            Some(Modal::Track(TrackPanel::Tasks)) => tasks.is_empty() && provider_tasks.is_empty(),
            Some(Modal::Track(TrackPanel::Subagents)) => rows.is_empty(),
            _ => false,
        } {
            self.close_track(cx);
        }
        let open_track = match &self.modal {
            Some(Modal::Track(panel)) => Some(*panel),
            _ => None,
        };
        let jump_visible = !self.at_bottom && !empty;
        let approval_pending =
            !subagent && snap.as_ref().is_some_and(|s| !s.permissions.is_empty());
        macro_rules! chrome_part {
            ($part:expr, $bounds:expr, $paint:expr) => {
                self.chrome_layers[$part].paint(
                    cx,
                    &mut self.kit,
                    &mut self.chrome_state,
                    look,
                    $bounds,
                    $paint,
                )
            };
        }
        if let Some(f) = &facts {
            chrome_part!(SUBTITLE, rect, |cx, d| {
                chrome::pane_subtitle(cx, d, f, rect.pos.x, rect.pos.y, rect.size.x);
            });
        }
        if empty && !loading {
            chrome_part!(EMPTY, stream_rect, |cx, d| {
                chrome::empty_state(
                    cx,
                    d,
                    stream_rect.pos.x,
                    stream_rect.pos.y + stream_rect.size.y,
                    stream_rect.size.x,
                );
            });
        }
        if outline_shown {
            chrome_part!(RAIL, stream_rect, |cx, d| {
                self.rail.draw(cx, d, stream_rect, prompts_active);
            });
        } else {
            self.rail.hide(cx);
        }
        if tracks_visible {
            let model = TrackModel {
                tasks: &tasks,
                provider_tasks,
                requested_task_stops: &self.requested_task_stops,
                rows: &rows,
                delete: delete_status,
                can_delete_finished: !subagent,
                can_detach,
                host_id: &target.host_id,
                providers: &self.providers,
                host_label: &host_label_fn,
            };
            chrome_part!(TRACKS, stream_rect, |cx, d| {
                tracks::draw_bar(
                    cx,
                    d,
                    &model,
                    open_track,
                    stream_rect.pos.x,
                    stream_rect.pos.y + stream_rect.size.y,
                    stream_rect.size.x,
                    track_height,
                    &mut self.track_anchors,
                );
            });
        } else {
            self.track_anchors = [Area::Empty; 2];
        }
        if jump_visible {
            let clearance = if tracks_visible {
                tracks::control_clearance(compact, track_height)
            } else {
                0.0
            };
            let bottom = stream_rect.pos.y + stream_rect.size.y - clearance.max(16.0);
            chrome_part!(JUMP, stream_rect, |cx, d| {
                chrome::jump_button(
                    cx,
                    d,
                    stream_rect.center().x,
                    bottom,
                    stream_rect.size.x - 32.0,
                    approval_pending,
                );
            });
        }
        if find_open {
            if let Some((field, card_h)) = chrome_part!(FIND, stream_rect, |cx, d| {
                find_card(
                    cx,
                    d,
                    stream_rect,
                    &find_status,
                    find_can_navigate,
                    find_failure,
                )
            }) {
                self.find_field_rect = Some(field);
                self.find_card_h = card_h;
            }
        } else {
            self.find_field_rect = None;
        }
        let callout_rect = rect_xywh(rect.pos.x, dock_top, rect.size.x, callouts_h + archived_h);
        if !callouts.is_empty() || show_archived_callout {
            let unarchiving = self.unarchiving;
            let unarchive_error = self.unarchive_error.as_deref();
            chrome_part!(CALLOUTS, callout_rect, |cx, d| {
                let mut cy = dock_top;
                for callout in &callouts {
                    cy += chrome::callout(cx, d, callout, rect.pos.x, cy, rect.size.x);
                }
                if show_archived_callout {
                    chrome::archived_callout(
                        cx,
                        d,
                        connected,
                        unarchiving,
                        unarchive_error,
                        rect.pos.x,
                        cy,
                        rect.size.x,
                    );
                }
            });
        }
        let covered = history_overlay || self.archiving;
        if covered {
            // Keep the input cover outside underlying inputs' lists. Its children
            // preserve toast ordering above history sync and below archiving.
            self.cover_list.begin_always(cx);
            cx.add_aligned_rect_area(&mut self.cover_area, rect);
            overlay::float(self.cover_area);
            self.find_field_rect = None;
        }
        if history_overlay {
            chrome_part!(HISTORY, rect, |cx, d| chrome::history_overlay(cx, d, rect));
        }
        if let Some(toast) = &toast {
            chrome_part!(TOAST, rect, |cx, d| {
                chrome::toast(cx, d, toast, rect);
            });
        }
        if self.archiving {
            chrome_part!(ARCHIVING, rect, |cx, d| chrome::archiving_overlay(
                cx, d, rect
            ));
        }
        if covered {
            self.cover_list.end(cx);
        }
        self.finish_chrome(cx);
        let find_field_rect = self.find_field_rect;

        // ── the find field ──────────────────────────────────────────────
        if let Some(field) = find_field_rect {
            if self.find_list.begin(cx, Walk::abs_rect(field)).is_ok() {
                let walk = Walk {
                    abs_pos: Some(field.pos),
                    margin: Margin::default(),
                    width: Size::Fixed(field.size.x),
                    height: Size::Fixed(field.size.y),
                };
                let _ = self.find_field.draw_walk(cx, scope, walk);
                self.find_list.end(cx);
            }
            if self.find_focus_seen != self.find.focus_requests {
                self.find_focus_seen = self.find.focus_requests;
                let field = self.find_field.as_ptext_field();
                field.set_key_focus(cx);
                field.select_all(cx);
            }
        } else {
            self.find_list.release(cx);
        }

        // ── the file drop backdrop, over the stream and the composer ────
        self.file_drop.draw(cx, rect);

        // ── modal surfaces ──────────────────────────────────────────────
        if self.track_motion.panel.is_some() {
            self.paint_tracks(cx, look, rect, &target, &tasks, provider_tasks, &rows, &host_label_fn);
        } else {
            self.track_overlay.release(cx);
        }
        if self.modal.is_some() && !matches!(self.modal, Some(Modal::Track(_))) {
            self.modal_list.begin_overlay_last(cx);
            // Nested sheets escape their parent's content clip, and root
            // subagent sheets cover the shell header/sidebar as Flutter does.
            cx.begin_pass_sized_turtle(Layout::flow_overlay());
            let full_window = matches!(self.modal, Some(Modal::Subagent { .. }))
                || self.target.as_ref().is_some_and(|t| t.sheet);
            let bounds = if full_window {
                Rect {
                    pos: DVec2::default(),
                    size: cx.current_pass_size(),
                }
            } else {
                rect
            };
            // sheet-tabs.tsx: a sheet that reaches the bottom of the screen owns the system
            // navigation inset. A panel-sized modal ends above it, with the shell body.
            let bottom_inset = if full_window {
                crate::shell::platform::safe_insets().bottom
            } else {
                0.0
            };
            self.modal_rect = bounds;
            let detail = matches!(self.modal, Some(Modal::Sheet { .. }));
            let detail_scrim = if detail {
                self.detail_motion
                    .draw_scrim(cx, &mut self.track_scrim, bounds);
                cx.add_aligned_rect_area(&mut self.detail_area, bounds);
                self.detail_motion.scrim
            } else {
                Area::Empty
            };
            if self
                .modal_content_list
                .begin(cx, Walk::abs_rect(bounds))
                .is_ok()
            {
                let texture_misses = crate::features::composer::texture_misses();
                let texture_revision = crate::features::composer::texture_revision();
                cx.begin_turtle(Walk::abs_rect(bounds), Layout::default());
                self.modal_kit.begin_layer(cx);
                self.modal_canvas.begin(bounds.pos);
                self.draw_modal(cx, scope, look, bounds, bottom_inset);
                self.modal_texture_wait = (crate::features::composer::texture_misses()
                    != texture_misses)
                    .then_some(texture_revision);
                cx.end_turtle_with_area(&mut self.modal_area);
                self.modal_content_list.end(cx);
            }
            if detail {
                if matches!(self.modal, Some(Modal::Sheet { .. })) {
                    self.detail_motion
                        .compose(cx, &self.modal_content_list, bounds);
                } else {
                    // The backing tool row vanished while the modal was drawn.
                    overlay::update_scrim(cx, detail_scrim, 0.0);
                    self.area.redraw(cx);
                }
            } else {
                self.modal_content_list
                    .set_view_transform(cx, &Mat4::identity());
            }
            if self.input_is_modal() {
                let area = if detail {
                    self.detail_area
                } else {
                    self.modal_area
                };
                overlay::modal_float(self.widget_uid().0, area);
            }
            cx.end_pass_sized_turtle();
            self.modal_list.end(cx);
        } else {
            if !matches!(self.modal, Some(Modal::Track(_))) {
                self.modal_canvas.begin(rect.pos);
            }
            self.modal_content_list.release(cx);
        }
        cx.end_turtle_with_area(&mut self.area);

        // ── after layout ────────────────────────────────────────────────
        if timeline_drawn {
            self.after_draw(cx);
        }
        // A reply must be rebuilt and laid out before deciding whether the
        // reader is still at history start. Old indices can queue every page.
        if timeline_drawn
            && let Some(ui) = scope.data.get_mut::<UiContext>()
            && self.wants_older()
        {
            self.load_older(cx, ui);
        }
        if let Some(target) = &self.target {
            MENUS.with(|m| {
                m.borrow_mut().insert(
                    (target.host_id.clone(), target.agent_id.clone()),
                    MenuFacts {
                        expanded_tools: self.expanded_tools,
                        archived,
                        outline_enabled: self.outline_enabled(),
                        detachable: self.snap.as_ref().is_some_and(|s| {
                            get(get(&s.agent, "labels"), "paseo.parent-agent-id")
                                .as_str()
                                .is_some_and(|p| !p.trim().is_empty())
                        }),
                    },
                )
            });
        }
        if let Some(toast) = &toast
            && self.toast_shown.as_ref().map(|(id, _)| id) != Some(&toast.id)
        {
            self.toast_shown = Some((toast.id.clone(), now_seconds()));
            if let Some(duration) = toast.duration {
                self.toast_timer = cx.start_timeout(duration);
            }
        }
        DrawStep::done()
    }
}

/// pane-find/index.tsx PaneFind: a 340px surface1 card 8px from the top and
/// 12px from the right edge holding the query field (with the match status
/// inside it), previous / next / close, and the failure row under it.
/// Returns the field rect and the card's height.
fn find_card(
    cx: &mut Cx2d,
    d: &mut Draw,
    stream: Rect,
    status: &str,
    can_navigate: bool,
    failure: Option<find::Failure>,
) -> (Rect, f64) {
    let p = d.look.palette;
    let w = 340.0f64.min(stream.size.x - 24.0).max(120.0);
    let x = stream.pos.x + stream.size.x - 12.0 - w;
    let y = stream.pos.y + 8.0;
    let field_h = 32.0;
    let card_h = field_h + 12.0;
    let failure_text = failure.map(|f| {
        t(match f {
            find::Failure::Connection => "paneFind.connectionFailure",
            find::Failure::HistoryChanged => "paneFind.historyChangedFailure",
            find::Failure::Reveal => "paneFind.revealFailure",
        })
    });
    let small = d.look.ui(12.0);
    let failure_h = failure_text
        .as_ref()
        .map(|f| {
            d.kit
                .measure(cx, Font::Regular, small, 1.4, f, Some(w - 16.0 - 60.0))
                .y
                .max(28.0)
                + 16.0
        })
        .unwrap_or(0.0);
    let shadow = if d.look.dark() {
        rgba(0x33000000)
    } else {
        rgba(0x0a000000)
    };
    d.kit
        .rect(cx, rect_xywh(x, y + 4.0, w, card_h), shadow, 8.0);
    d.kit.bordered(
        cx,
        rect_xywh(x, y, w, card_h),
        p.surface1,
        8.0,
        p.border,
        1.0,
    );
    d.canvas.add_hit(
        rect_xywh(x, y, w, card_h + failure_h),
        HitTarget::Chrome("find:card".into()),
    );
    let buttons_w = 3.0 * 28.0 + 2.0 * 4.0;
    let field = rect_xywh(x + 6.0, y + 6.0, w - 12.0 - buttons_w - 4.0, field_h);
    // The match status sits inside the field's right edge.
    if !status.is_empty() {
        let s = d.kit.measure(cx, Font::Regular, small, 1.3, status, None);
        d.kit.label(
            cx,
            Font::Regular,
            small,
            1.3,
            p.muted,
            dvec2(
                field.pos.x + field.size.x - 8.0 - s.x,
                field.pos.y + (field_h - s.y) * 0.5,
            ),
            status,
            None,
        );
    }
    let mut bx = field.pos.x + field.size.x + 4.0;
    for (icon, id, enabled) in [
        ("chevron-up", "find:previous", can_navigate),
        ("chevron-down", "find:next", can_navigate),
        ("x", "find:close", true),
    ] {
        let r = rect_xywh(bx, y + 8.0, 28.0, 28.0);
        let hovered = d.state.hover_hit.as_deref() == Some(id);
        if hovered && enabled {
            d.kit.rect(cx, r, p.surface2, 6.0);
        }
        d.kit.icon(
            cx,
            icon,
            r,
            16.0,
            if enabled {
                if hovered { p.foreground } else { p.muted }
            } else {
                with_alpha(p.muted, 0.5)
            },
        );
        if enabled {
            d.canvas.add_hit(r, HitTarget::Chrome(id.into()));
        }
        bx += 32.0;
    }
    if let Some(text) = failure_text {
        let fy = y + card_h;
        d.kit
            .rect(cx, rect_xywh(x, fy, w, failure_h), p.surface1, 0.0);
        d.kit.label(
            cx,
            Font::Regular,
            small,
            1.4,
            p.foreground,
            dvec2(x + 8.0, fy + 8.0),
            &text,
            Some(w - 16.0 - 60.0),
        );
        let label = t("paneFind.retry");
        let lw = d.kit.measure(cx, Font::Medium, small, 1.3, &label, None).x + 16.0;
        let r = rect_xywh(x + w - 8.0 - lw, fy + 8.0, lw, 24.0);
        let hovered = d.state.hover_hit.as_deref() == Some("find:retry");
        if hovered {
            d.kit.rect(cx, r, p.surface2, 6.0);
        }
        d.kit.label(
            cx,
            Font::Medium,
            small,
            1.3,
            p.foreground,
            dvec2(r.pos.x + 8.0, r.pos.y + 4.0),
            &label,
            None,
        );
        d.canvas.add_hit(r, HitTarget::Chrome("find:retry".into()));
    }
    (field, card_h + failure_h)
}

impl ChatPanel {
    /// agent-panel.tsx renderChatAgentNonReadyView.
    fn non_ready(&self) -> Option<NonReady> {
        let target = self.target.as_ref()?;
        if target.is_subagent() {
            return None;
        }
        let Some(snap) = &self.snap else {
            return Some(NonReady::Loading);
        };
        if !map(&snap.agent).is_empty()
            || snap.hydrated
            || snap.create_handoff
            || snap.has_visible_history()
        {
            return None;
        }
        if !self.host.connected {
            return Some(NonReady::Unavailable {
                label: self.host.label.clone(),
                connecting: self.host.connecting,
                error: self.host.host_error.clone(),
            });
        }
        if let (Some(message), SyncStatus::Error) = (&snap.error, snap.sync) {
            let lower = message.to_lowercase();
            if lower.contains("agent not found") || lower.contains("not found") {
                return Some(NonReady::NotFound);
            }
            return Some(NonReady::LoadError(message.clone()));
        }
        Some(NonReady::Loading)
    }

    fn find_status(&self) -> String {
        match self.find.phase {
            FindPhase::Searching => t("paneFind.searching"),
            FindPhase::Loading => t("paneFind.loading"),
            FindPhase::Error => t("paneFind.failed"),
            _ if !self.find.query.trim().is_empty() => {
                if self.find.count > 0 {
                    ta(
                        "paneFind.position",
                        &[
                            ("current", &(self.find.occurrence + 1).to_string()),
                            ("total", &self.find.count.to_string()),
                        ],
                    )
                } else {
                    t("paneFind.noMatches")
                }
            }
            _ => String::new(),
        }
    }

    fn modal_content_pos(&self, abs: DVec2) -> Option<DVec2> {
        if matches!(self.modal, Some(Modal::Track(_))) {
            let (rect, scale) = self.track_hit_rect?;
            return (self.track_bounds.contains(abs) && rect.contains(abs))
                .then(|| (abs - rect.pos) / scale);
        }
        let shift = if matches!(self.modal, Some(Modal::Sheet { .. })) {
            self.detail_motion.offset
        } else {
            DVec2::default()
        };
        Some(abs - self.modal_canvas.origin - shift)
    }

    fn chrome_hit(&self, abs: DVec2, surface: bool) -> Option<HitTarget> {
        if surface {
            return self
                .modal_canvas
                .hit_at(self.modal_content_pos(abs)?)
                .cloned();
        }
        self.overlay_canvas
            .hit_at(abs - self.overlay_canvas.origin)
            .cloned()
    }

    fn handle_chrome_hit(&mut self, cx: &mut Cx, hit: Hit, modal: bool) -> Option<String> {
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                self.chrome_hover(cx, Some(e.abs), modal);
            }
            Hit::FingerHoverOut(_) => self.chrome_hover(cx, None, modal),
            Hit::FingerDown(e) => {
                let target = self.modal_tap_target(e.abs, modal);
                let pos = if modal {
                    self.modal_content_pos(e.abs)
                } else {
                    Some(e.abs - self.overlay_canvas.origin)
                };
                let (state, pressed) = if modal {
                    (&mut self.modal_state, &mut self.modal_pressed)
                } else {
                    (&mut self.chrome_state, &mut self.pressed)
                };
                let drag = target
                    .as_ref()
                    .and_then(|target| super::scrollbar::BarDrag::press(target, pos?, state));
                if let Some(drag) = drag {
                    if drag.vertical && drag.key == "sheet:y" {
                        state.expanded.insert("sheet:detached".into(), true);
                    }
                    self.bar_drag = Some((drag, modal));
                    pressed.cancel();
                    if modal {
                        self.redraw_modal(cx);
                    } else {
                        self.redraw_chrome(cx);
                    }
                } else {
                    self.bar_drag = None;
                    if modal
                        && matches!(self.modal, Some(Modal::Lightbox { .. }))
                        && matches!(&target, Some(HitTarget::Chrome(id)) if id == "scrim" || id == "lightbox:image")
                    {
                        self.lightbox_drag = Some(e.abs);
                    }
                    pressed.press(target);
                }
            }
            Hit::FingerMove(e) => {
                let pos = if modal {
                    self.modal_content_pos(e.abs)
                } else {
                    Some(e.abs - self.overlay_canvas.origin)
                };
                if let Some((drag, _)) = self
                    .bar_drag
                    .as_mut()
                    .filter(|(_, surface)| *surface == modal)
                {
                    let state = if modal {
                        &mut self.modal_state
                    } else {
                        &mut self.chrome_state
                    };
                    if pos.is_some_and(|pos| drag.drag_to(pos, state)) {
                        if modal {
                            self.redraw_modal(cx);
                        } else {
                            self.redraw_chrome(cx);
                        }
                    }
                }
                if modal
                    && let (Some(last), Some(Modal::Lightbox { offset, scale, .. })) =
                        (self.lightbox_drag, &mut self.modal)
                    && *scale > 1.0
                {
                    *offset += e.abs - last;
                    self.lightbox_drag = Some(e.abs);
                    self.redraw_modal(cx);
                }
            }
            Hit::FingerLongPress(_) => {
                if modal {
                    self.modal_pressed.cancel();
                } else {
                    self.pressed.cancel();
                }
            }
            Hit::FingerUp(e) => {
                if self
                    .bar_drag
                    .as_ref()
                    .is_some_and(|(_, surface)| *surface == modal)
                {
                    self.bar_drag = None;
                    if modal {
                        self.modal_state.dragging_bar = None;
                        self.redraw_modal(cx);
                    } else {
                        self.chrome_state.dragging_bar = None;
                        self.redraw_chrome(cx);
                    }
                }
                let dragged = modal
                    && self.lightbox_drag.take().is_some_and(|start| {
                        (e.abs - e.abs_start).length() > 4.0 && start != e.abs_start
                    });
                let target = self.modal_tap_target(e.abs, modal);
                let valid = e.is_over
                    && !dragged
                    && !e.has_long_press_occurred
                    && (!e.device.is_touch() || e.was_tap());
                let pressed = if modal {
                    &mut self.modal_pressed
                } else {
                    &mut self.pressed
                };
                let target = pressed.release(target, valid)?;
                if modal {
                    self.redraw_modal(cx);
                }
                if super::mermaid::tap(&mut self.modal_state, &target) {
                    return None; // Export owns its native completion, even after the sheet closes.
                }
                match target {
                    HitTarget::Chrome(id) => match id.as_str() {
                        "outline" => {
                            if let Some(RailEvent::Jump(seq)) = self.rail.tap(e.abs) {
                                return Some(format!("outline:{seq}"));
                            }
                        }
                        "track-panel" | "lightbox:image" | "lightbox:footer" | "sheet:body" => {}
                        _ => return Some(id),
                    },
                    HitTarget::Copy { key, text } => {
                        cx.copy_to_clipboard(&text);
                        self.modal_state
                            .copied
                            .insert(key, std::time::Instant::now());
                    }
                    HitTarget::Toggle(key) => self.modal_state.toggle(&key),
                    HitTarget::OpenFile(path) => {
                        // The row tap path opens it once the UiContext is at hand.
                        let row = match &self.modal {
                            Some(Modal::Sheet { row, .. }) => row.clone(),
                            _ => String::new(),
                        };
                        cx.action(ChatRowEvent {
                            panel: self.uid,
                            row,
                            kind: RowEventKind::Tap {
                                target: HitTarget::OpenFile(path),
                                rect: Rect::default(),
                                intent: FileLinkIntent::from_pointer(
                                    e.modifiers,
                                    e.device.is_touch(),
                                ),
                            },
                        });
                    }
                    _ => {}
                }
            }
            Hit::FingerScroll(e) if modal && matches!(self.modal, Some(Modal::Lightbox { .. })) => {
                self.zoom(if e.scroll.y < 0.0 { 1.1 } else { 1.0 / 1.1 });
                self.redraw_modal(cx);
            }
            _ => {}
        }
        None
    }

    fn chrome_hover(&mut self, cx: &mut Cx, abs: Option<DVec2>, surface: bool) {
        if !surface && self.rail.hover(cx, abs) {
            self.redraw_chrome_part(cx, RAIL);
        }
        let key = abs
            .and_then(|abs| self.chrome_hit(abs, surface))
            .map(|t| super::chat_row::hover_key(&t));
        let state = if surface {
            &mut self.modal_state
        } else {
            &mut self.chrome_state
        };
        if state.hover_hit != key {
            let previous = std::mem::replace(&mut state.hover_hit, key.clone());
            state.hovered = abs.is_some();
            if surface {
                self.redraw_modal(cx);
            } else {
                for layer in &self.chrome_layers {
                    if layer.canvas.hits.iter().any(|(_, target)| {
                        let hit = super::chat_row::hover_key(target);
                        previous.as_ref() == Some(&hit) || key.as_ref() == Some(&hit)
                    }) {
                        layer.list.redraw(cx);
                    }
                }
            }
        }
        if abs.is_some() {
            cx.set_cursor(if key.is_some() {
                MouseCursor::Hand
            } else {
                MouseCursor::Default
            });
        }
    }

    fn modal_scroll(&mut self, cx: &mut Cx, abs: DVec2, delta: DVec2) -> bool {
        if let Some(Modal::Lightbox { .. }) = &self.modal {
            self.zoom(if delta.y < 0.0 { 1.1 } else { 1.0 / 1.1 });
            self.redraw_modal(cx);
            return true;
        }
        let Some(rel) = self.modal_content_pos(abs) else {
            return false;
        };
        let vertical = delta.x.abs() <= delta.y.abs();
        let amount = if vertical { delta.y } else { delta.x };
        // The deepest region that can still move takes the delta.
        for key in self.modal_canvas.scroll_regions_at(rel, vertical) {
            let (offsets, extents) = if vertical {
                (&mut self.modal_state.scroll_y, &self.modal_state.extent_y)
            } else {
                (&mut self.modal_state.scroll_x, &self.modal_state.extent_x)
            };
            let max = extents.get(key).copied().unwrap_or(0.0);
            let current = offsets.get(key).copied().unwrap_or(0.0).clamp(0.0, max);
            let next = (current + amount).clamp(0.0, max);
            if next == current {
                continue;
            }
            offsets.insert(key.to_owned(), next);
            if key == "sheet:y" {
                // An overview sheet stays pinned to its newest call until scrolled.
                self.modal_state
                    .expanded
                    .insert("sheet:detached".into(), true);
            }
            self.redraw_modal(cx);
            return true;
        }
        false
    }

    /// Everything that needs the UiContext: commands, overlay answers,
    /// session completions, row/composer/find actions, timers, attention.
    fn ui_pass(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        ui: &mut UiContext,
        chrome_tap: Option<String>,
        input_focused: bool,
    ) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let visible = self.visible && ui.activity.visible;
        self.ensure_session(cx, ui, visible);
        with_store(|store| store.set_view_visible(cx, self.uid, visible));
        self.refresh_host(cx, ui);
        let changed = self.refresh_snapshot(cx);

        // Commands addressed to this tab, and overlay answers by id.
        let mut overlay_answers = Vec::new();
        let mut actions = std::mem::take(&mut self.pending_actions);
        for command in ui.commands.iter_mut() {
            if command.handled {
                continue;
            }
            if let Some((id, args)) = overlay::result_of(command) {
                if self.asks.contains_key(&id) {
                    overlay_answers.push((id, args.clone()));
                    command.handled = true;
                }
                continue;
            }
            if command.tab_key != target.tab_key {
                continue;
            }
            let action = command
                .action
                .strip_prefix("conversation.")
                .or_else(|| command.action.strip_prefix("conversation:"));
            if let Some(action) = action {
                actions.push(action.to_owned());
                command.handled = true;
            }
        }
        for action in actions {
            self.conversation_action(cx, ui, &action);
        }
        for (id, args) in overlay_answers {
            self.on_overlay(cx, ui, id, &args);
        }

        // Session outcomes.
        let key = target.key();
        let (completions, reveals) = with_store(|store| {
            store
                .sessions
                .get_mut(&key)
                .map(|s| {
                    (
                        std::mem::take(&mut s.completions),
                        std::mem::take(&mut s.reveals),
                    )
                })
                .unwrap_or_default()
        });
        for completion in completions {
            self.on_completion(cx, ui, completion);
        }
        for reveal in reveals {
            self.on_reveal(cx, ui, reveal.token, reveal.ok);
        }
        if self.visible && (changed || self.snapshot_stale) {
            self.snapshot_stale = false;
            self.on_session_changed(cx, ui);
        }
        if let Some(Modal::Lightbox { epoch, file_route, .. }) = &self.modal
            && (!visible
                || epoch.as_deref() != self.snap.as_ref().and_then(|snap| snap.epoch.as_deref())
                || file_route
                    .as_ref()
                    .is_some_and(|route| !route.still_current(ui, &target.host_id)))
        {
            self.dismiss_modal(cx);
        }

        // Provider catalog pushes for this cwd (use-providers-snapshot.ts).
        let scope_of = |cwd: &str| cwd.trim().trim_end_matches('/').to_owned();
        let mut reload_providers = false;
        for core_event in ui.events.iter() {
            match core_event {
                paseo_gui_core::CoreEvent::Response {
                    request_id, result, ..
                } if self.providers_request.as_ref() == Some(request_id) => {
                    self.providers_request = None;
                    match result {
                        Ok(body) => {
                            if let Some(error) = paseo_gui_core::response_error(
                                body,
                                "get_providers_snapshot_request",
                            ) {
                                self.show_failure(cx, error.message);
                            } else {
                                self.providers = bridge::provider_entries(body);
                            }
                        }
                        Err(error) => self.show_failure(cx, error.message.clone()),
                    }
                    if self.visible {
                        self.redraw_chrome(cx);
                        self.redraw_modal(cx);
                    }
                }
                paseo_gui_core::CoreEvent::Connected { host_id, .. }
                | paseo_gui_core::CoreEvent::Disconnected { host_id, .. }
                    if host_id == &target.host_id =>
                {
                    self.providers_request = None;
                    self.providers_requested = false;
                }
                _ => {}
            }
        }
        for (host, ev) in ui.host_events() {
            if host != target.host_id || get(ev, "type") != "providers_snapshot_update" {
                continue;
            }
            let payload = get(ev, "payload");
            if scope_of(str_of(get(payload, "cwd"))) != scope_of(target.cwd()) {
                continue;
            }
            if !get(payload, "entries").is_null() || !get(payload, "compactSnapshot").is_null() {
                self.providers = bridge::provider_entries(payload);
            } else if self.providers_requested {
                reload_providers = true;
            }
        }
        if reload_providers {
            self.providers_requested = false;
            self.ensure_providers(cx, ui);
        }
        if target.sheet && !target.is_subagent() {
            self.ensure_providers(cx, ui);
        }

        // Row, composer, find field and list actions.
        if let Event::Actions(actions) = event {
            let rows: Vec<ChatRowEvent> = actions
                .iter()
                .filter_map(|a| a.downcast_ref::<ChatRowEvent>())
                .filter(|e| e.panel == self.uid)
                .cloned()
                .collect();
            for row in rows {
                self.on_row_event(cx, ui, row);
            }
            let list = self.list.as_portal_list();
            if list.scrolled(actions) {
                // Never consult is_at_end here: it is from the frame before
                // this scroll. draw_walk reconciles the laid-out position.
                self.redraw_timeline(cx);
                self.redraw_chrome_part(cx, JUMP);
            }
            let composer = self.composer.as_composer_panel();
            if composer.prompt_submitted(actions).is_some() {
                if let Some(a) = &mut self.attention {
                    a.clear_on(attention::Trigger::PromptSend);
                }
                self.jump_to_latest(cx);
            }
            if composer.reload_agent(actions).is_some() {
                self.with_session(cx, ui, |s, io| s.reload(io));
            }
            if self.find_open {
                let field = self.find_field.as_ptext_field();
                if let Some(text) = field.changed(actions) {
                    self.find.set_query(&text);
                    self.pump_find(cx, ui);
                }
                if field.returned(actions).is_some() {
                    let (epoch, rows) = self
                        .snap
                        .as_ref()
                        .map(|s| (s.epoch.clone(), s.timeline.clone()))
                        .unwrap_or_default();
                    if self.last_shift {
                        self.find.previous(epoch.as_deref(), &rows)
                    } else {
                        self.find.next(epoch.as_deref(), &rows)
                    }
                    self.pump_find(cx, ui);
                    field.set_key_focus(cx);
                }
                if field.escaped(actions) {
                    self.close_find(cx);
                }
            }
        }

        // Timers.
        if let Event::Timer(_) = event {
            let mut due = Vec::new();
            self.find_timers.retain(|(timer, token, wait)| {
                if timer.is_event(event).is_some() {
                    due.push((*token, *wait));
                    false
                } else {
                    true
                }
            });
            for (token, wait) in due {
                if wait {
                    self.find.wait_expired(token);
                } else {
                    self.find.search_due(token);
                }
                self.pump_find(cx, ui);
            }
            if self.toast_timer.is_event(event).is_some()
                && let Some((id, _)) = self.toast_shown.clone()
            {
                self.dismiss_toast(cx, ui, &id);
            }
        }

        if let Some(tap) = chrome_tap {
            if let Some(seq) = tap
                .strip_prefix("outline:")
                .and_then(|s| s.parse::<i64>().ok())
            {
                self.jump_to_prompt(cx, ui, seq);
            } else {
                self.on_chrome_tap(cx, ui, &tap);
            }
        }
        // A valid close tap in this event wins over an already-prepared file.
        let owner = self.widget_uid().0;
        if let Some(Modal::Lightbox { export, .. }) = &mut self.modal
            && export.busy()
            && export.poll(
                ui,
                visible
                    && ui.activity.app_active
                    && overlay::window_modal_accepts_owner(&ui.window_id, owner),
            )
        {
            self.redraw_modal(cx);
        }

        // Attention.
        if input_focused && let Some(a) = &mut self.attention {
            a.clear_on(attention::Trigger::InputFocus);
        }
        self.sync_attention(cx, ui);

        // Providers for the subagent facts line.
        if self.modal.is_none()
            && !self.track_anchors[TrackPanel::Subagents as usize].is_empty()
            && !self.providers_requested
            && self.subagent_rows().iter().any(|r| r.paseo)
        {
            self.ensure_providers(cx, ui);
        }
    }

    fn on_session_changed(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(snap) = self.snap.clone() else {
            return;
        };
        if !snap.loading {
            self.older.settled(snap.start_cursor.as_ref());
        }
        let rows = snap.timeline.clone();
        self.find.history_changed(snap.epoch.as_deref(), &rows);
        self.pump_find(cx, ui);
        let subagent = self.target.as_ref().is_some_and(ChatTarget::is_subagent);
        if subagent && !self.children_requested && self.feature("providerSubagentNesting") {
            self.children_requested = true;
            self.with_session(cx, ui, |s, io| s.load_subagents(io));
        }
        self.refresh_prompts(cx, ui);
    }

    fn wants_older(&mut self) -> bool {
        let Some(snap) = &self.snap else { return false };
        let available = self.visible
            && snap.has_older
            && !snap.loading
            && snap.start_cursor.is_some()
            && !self.stream_empty();
        let list = self.list.as_portal_list();
        let at_start = list.scroll_anchor(1).is_some_and(|(index, _)| index == 1);
        self.older
            .observe(at_start, list.not_filling_viewport(), available)
    }

    /// Post-layout bookkeeping: the active prompt, the find match position
    /// and a pending row-top reveal.
    fn after_draw(&mut self, cx: &mut Cx2d) {
        let list = self.list.as_portal_list();
        let first = list.first_id();
        // resolveActivePromptSeq: the last prompt at or before the first row on screen.
        if self.rail.prompts.len() >= 2 {
            let anchor = self.items.iter().skip(first).find_map(RowItem::seq);
            let active = outline::active_prompt_seq(&self.rail.prompts, anchor);
            if active != self.active_prompt {
                self.active_prompt = active;
                self.redraw_chrome_part(cx, RAIL);
            }
        }
        // chatFindScrollToMatch: the highlighted match sits just under the find card.
        if let Some((row, attempts)) = self.find_scroll.clone() {
            let index = self.items.iter().position(|i| i.id == row);
            let found = index
                .and_then(|i| list.get_item(i))
                .and_then(|(_, w)| w.as_chat_row().find_y());
            match (index, found) {
                (Some(index), Some(y)) => {
                    let clearance = self.find_card_h + 8.0;
                    list.set_first_id_and_scroll(index, clearance - y);
                    self.find_scroll = None;
                    self.redraw_timeline(cx);
                }
                _ if attempts < 3 => {
                    self.find_scroll = Some((row, attempts + 1));
                    self.redraw_timeline(cx);
                }
                _ => self.find_scroll = None,
            }
        }
        if let Some(row) = self.reveal_top.take()
            && let Some(index) = self.items.iter().position(|i| i.id == row)
            && let Some((_, widget)) = list.get_item(index)
            && widget.as_chat_row().rect(cx).pos.y < self.stream_rect.pos.y - 0.5
        {
            self.scroll_to_row(cx, &row, 0.0);
        }
        // reveal_attention: the newest edge is shown; when the pending cards below the first
        // one fill the stream, its top is brought on screen instead.
        if let Some(row) = self.attention_row.take()
            && let Some(index) = self.items.iter().position(|i| i.id == row)
        {
            let first = list.first_id();
            let above = index < first
                || (index == first
                    && list.get_item(index).is_some_and(|(_, w)| {
                        w.as_chat_row().rect(cx).pos.y < self.stream_rect.pos.y - 0.5
                    }));
            if above {
                self.scroll_to_row(cx, &row, 0.0);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_tracks(
        &mut self,
        cx: &mut Cx2d,
        look: Look,
        rect: Rect,
        target: &ChatTarget,
        tasks: &[Value],
        provider_tasks: &[Value],
        rows: &[SubagentRow],
        host_label: &dyn Fn(&str) -> String,
    ) {
        let Some(active) = self.track_motion.panel else {
            return;
        };
        self.track_overlay.begin_overlay_last(cx);
        cx.begin_pass_sized_turtle(Layout::flow_overlay());
        let compact = self.track_motion.compact;
        let (bounds, bottom_inset) = if target.sheet {
            (
                Rect {
                    pos: DVec2::default(),
                    size: cx.current_pass_size(),
                },
                crate::shell::platform::safe_insets().bottom,
            )
        } else {
            (rect, 0.0)
        };
        let anchor = self
            .track_motion
            .anchor
            .map(|panel| self.track_anchors[panel as usize])
            .filter(|area| area.is_valid(cx))
            .unwrap_or(self.track_anchors[active as usize]);
        let anchor = anchor.is_valid(cx).then(|| anchor.rect(cx));
        let dpi = cx.current_dpi_factor();
        let geometry_changed = (bounds, anchor, bottom_inset, dpi)
            != (
                self.track_bounds,
                self.track_anchor_rect,
                self.track_inset,
                self.track_dpi,
            );
        self.track_bounds = bounds;
        self.track_anchor_rect = anchor;
        self.track_inset = bottom_inset;
        self.track_dpi = dpi;
        self.track_scrim_area = Area::Empty;
        if compact {
            self.track_scrim.color = vec4(0.0, 0.0, 0.0, self.track_scrim_alpha());
            self.track_scrim.draw_abs(cx, bounds);
            self.track_scrim_area = self.track_scrim.area();
            if self.track_motion.open {
                cx.add_aligned_rect_area(&mut self.track_backdrop, bounds);
                overlay::modal_float(self.widget_uid().0, self.track_backdrop);
            }
        }

        let capture = self.track_motion.needs_capture();
        let previous = self
            .track_motion
            .pair
            .map(|(a, b)| if active == a { b } else { a });
        let can_detach = self.feature("agentDetach");
        let dirty = self.track_dirty.get();
        let incoming_needs_hits = self.track_painted_panel != Some(active);
        let mut hit = None;
        for panel in previous.into_iter().chain(std::iter::once(active)) {
            let index = panel as usize;
            let above = previous
                .and_then(|other| self.track_layers[other as usize].as_ref())
                .and_then(|layer| layer.layout.as_ref())
                .map(|layout| layout.above);
            let layer = self.track_layers[index].get_or_insert_with(|| TrackLayer::new(cx));
            let repaint = panel == active
                && self.track_motion.open
                && (geometry_changed
                    || layer.layout.is_none()
                    || incoming_needs_hits
                    || (dirty && !capture));
            if repaint {
                self.track_dirty.set(false);
                let texture_misses = crate::features::composer::texture_misses();
                let texture_revision = crate::features::composer::texture_revision();
                let model = TrackModel {
                    tasks,
                    provider_tasks,
                    requested_task_stops: &self.requested_task_stops,
                    rows,
                    delete: self.delete_status,
                    can_delete_finished: !target.is_subagent(),
                    can_detach,
                    host_id: &target.host_id,
                    providers: &self.providers,
                    host_label,
                };
                self.modal_canvas.begin(DVec2::default());
                let mut d = Draw {
                    kit: &mut self.modal_kit,
                    canvas: &mut self.modal_canvas,
                    look,
                    state: &mut self.modal_state,
                    owner: None,
                    now: now_seconds(),
                    area: self.area,
                    animating: false,
                    indicators: None,
                };
                layer.layout = tracks::measure_panel(
                    cx,
                    &mut d,
                    &mut self.painter,
                    &mut self.provider_icon,
                    &model,
                    panel,
                    anchor,
                    bounds,
                    bottom_inset,
                    above,
                );
                if let Some(layout) = &layer.layout {
                    let size = TrackLayer::paint_size(layout.card.size, compact);
                    if capture {
                        layer
                            .capture
                            .get_or_insert_with(|| LayerTexture::new(cx))
                            .begin(cx, size);
                    }
                    layer.list.begin_always(cx);
                    cx.begin_sized_turtle(size, Layout::flow_overlay());
                    d.kit.begin_layer(cx);
                    tracks::draw_panel(
                        cx,
                        &mut d,
                        &mut self.painter,
                        &mut self.provider_icon,
                        &model,
                        panel,
                        layout,
                    );
                    cx.end_pass_sized_turtle();
                    layer.list.end(cx);
                    if capture {
                        layer.list.set_view_transform(cx, &Mat4::identity());
                        layer.capture.as_mut().unwrap().end(cx);
                    }
                    self.track_painted_panel = Some(panel);
                }
                self.modal_texture_wait = (crate::features::composer::texture_misses()
                    != texture_misses)
                    .then_some(texture_revision);
            }
            if !capture {
                layer.release_capture(cx);
            }
            layer.quad = Area::Empty;
            let Some(layout) = &layer.layout else {
                continue;
            };
            let visual = self.track_motion.visual(panel, layout.card.size);
            let body = TrackLayer::visual_rect(layout, visual, compact);
            if capture {
                let texture = layer.capture.get_or_insert_with(|| {
                    layer.list.set_view_transform(cx, &Mat4::identity());
                    let mut texture = LayerTexture::new(cx);
                    texture.capture(
                        cx,
                        TrackLayer::paint_size(layout.card.size, compact),
                        &layer.list,
                    );
                    texture
                });
                let output = Rect {
                    pos: body.pos,
                    size: texture.size * visual.scale,
                };
                layer.quad = self.track_composite.draw_texture(
                    cx,
                    output,
                    &texture.texture,
                    visual.opacity as f32,
                );
            } else {
                if !repaint {
                    // The retained card already exists; reattach without relayout.
                    if layer.list.begin_maybe(cx, false).is_ok() {
                        layer.list.end(cx);
                    }
                }
                overlay::place_layer(cx, &layer.list, body, visual.scale);
            }
            if panel == active && self.track_motion.open {
                hit = Some((body, visual.scale, visual.opacity));
            }
        }
        if !compact && self.track_motion.open {
            // Allocate once per structural draw, even when the first sample is transparent.
            let rect = hit.map_or(Rect::default(), |(rect, _, _)| rect);
            cx.add_aligned_rect_area(&mut self.track_popover_area, rect);
        }
        self.set_track_hit(hit);
        cx.end_pass_sized_turtle();
        self.track_overlay.end(cx);
    }

    fn track_scrim_alpha(&self) -> f32 {
        (TRACK_SCRIM_ALPHA
            * self
                .track_motion
                .intent()
                .resolve()
                .sample(self.track_motion.presence.value(), self.track_bounds.size)
                .scrim) as f32
    }

    fn set_track_hit(&mut self, hit: Option<(Rect, f64, f64)>) {
        self.track_hit_rect = hit
            .filter(|_| self.track_motion.panel == self.track_painted_panel)
            .filter(|(_, scale, alpha)| *alpha > 0.0 && *scale > 0.0)
            .map(|(rect, scale, _)| (rect, scale));
        if let Some(popup) = &self.track_popover
            && let Some(panel) = self.track_motion.panel
        {
            let peer = TRACKS_PANELS[1 - panel as usize];
            popup.drawn_with_peer(
                self.track_anchors[panel as usize],
                self.track_anchors[peer as usize],
                if self.track_hit_rect.is_some() {
                    self.track_popover_area
                } else {
                    Area::Empty
                },
            );
        }
    }

    fn advance_tracks(&mut self, cx: &mut Cx, event: &Event) {
        if !self.track_motion.advance(cx, event) {
            return;
        }
        let Some(active) = self.track_motion.panel else {
            return;
        };
        let mut hit = None;
        for (panel, layer) in TRACKS_PANELS.into_iter().zip(&self.track_layers) {
            let Some(layer) = layer else {
                continue;
            };
            let Some(layout) = &layer.layout else {
                continue;
            };
            let visual = self.track_motion.visual(panel, layout.card.size);
            let body = TrackLayer::visual_rect(layout, visual, self.track_motion.compact);
            if let Some(texture) = &layer.capture {
                let output = Rect {
                    pos: body.pos,
                    size: texture.size * visual.scale,
                };
                DrawLayerTexture::update(cx, layer.quad, output, visual.opacity as f32);
            } else if self.track_motion.compact && panel == active {
                overlay::place_layer(cx, &layer.list, body, visual.scale);
            }
            if panel == active && self.track_motion.open {
                hit = Some((body, visual.scale, visual.opacity));
            }
        }
        if let Some((rect, _, _)) = hit
            && self.track_popover_area.is_valid(cx)
        {
            self.track_popover_area.set_rect(cx, &rect);
        }
        self.set_track_hit(hit);
        if self.track_motion.compact {
            overlay::update_scrim(cx, self.track_scrim_area, self.track_scrim_alpha());
        }
        if !self.track_motion.open && !self.track_motion.presence.is_moving() {
            self.reset_tracks(cx);
            self.area.redraw(cx);
        } else if !self.track_motion.needs_capture()
            && (self.track_motion.pair.take().is_some()
                || self.track_layers[active as usize]
                    .as_ref()
                    .is_some_and(|layer| layer.capture.is_some()))
        {
            if let Some(mut outgoing) = self.track_layers[1 - active as usize].take() {
                outgoing.release_capture(cx);
            }
            if let Some(layer) = &mut self.track_layers[active as usize] {
                layer.release_capture(cx);
            }
            self.area.redraw(cx);
        }
    }

    /// Compact detail sheets, the usage sheet, the lightbox and the subagent sheet.
    fn draw_modal(
        &mut self,
        cx: &mut Cx2d,
        scope: &mut Scope,
        look: Look,
        rect: Rect,
        bottom_inset: f64,
    ) {
        let p = look.palette;
        let Some(modal) = self.modal.take() else {
            return;
        };
        match &modal {
            Modal::Track(_) => unreachable!("track lists paint in their own retained layer"),
            Modal::Sheet { row, kind } => {
                let item = self.items.iter().find(|i| i.id == *row).cloned();
                // A row that left the stream closes its sheet.
                let (Some(item), Some(ctx)) = (item, self.ctx.clone()) else {
                    self.detail_motion = SheetMotion::default();
                    self.window_modal = None;
                    return;
                };
                let Some(header) = rows::sheet_header(&ctx, &item, kind) else {
                    self.detail_motion = SheetMotion::default();
                    self.window_modal = None;
                    return;
                };
                self.detail_motion.mark_cover(cx, rect, p.surface2.w >= 1.0);
                let mut d = Draw {
                    kit: &mut self.modal_kit,
                    canvas: &mut self.modal_canvas,
                    look,
                    state: &mut self.modal_state,
                    owner: None,
                    now: now_seconds(),
                    area: self.area,
                    animating: false,
                    indicators: None,
                };
                let kind_ref = kind.as_str();
                super::tool_sheet::paint_detail_sheet(
                    cx,
                    &mut d,
                    rect,
                    bottom_inset,
                    &header,
                    kind == "group",
                    &|cx, d, x, y, w, fill| {
                        rows::sheet_body(cx, d, &ctx, &item, kind_ref, x, y, w, fill)
                    },
                );
            }
            Modal::Usage => {
                let agent = self
                    .snap
                    .as_ref()
                    .map(|s| s.agent.clone())
                    .unwrap_or_default();
                let mut d = Draw {
                    kit: &mut self.modal_kit,
                    canvas: &mut self.modal_canvas,
                    look,
                    state: &mut self.modal_state,
                    owner: None,
                    now: now_seconds(),
                    area: self.area,
                    animating: false,
                    indicators: None,
                };
                usage_sheet(cx, &mut d, &agent, rect, bottom_inset);
            }
            Modal::Lightbox {
                source,
                export_available,
                export,
                scale,
                offset,
                ..
            } => {
                let texture = match source {
                    LightboxSource::Assistant { url, row } => {
                        let core = scope
                            .data
                            .get::<UiContext>()
                            .map(|ui| ui.core.clone())
                            .or_else(|| self.core.clone());
                        let target = self.target.clone().unwrap_or_default();
                        match images::image(
                            cx,
                            core.as_ref(),
                            &target.host_id,
                            target.cwd.as_deref(),
                            url,
                            row,
                            None,
                        ) {
                            ImageState::Ready {
                                texture,
                                width,
                                height,
                            } => Some((texture, width as f64, height as f64)),
                            _ => None,
                        }
                    }
                    LightboxSource::User { entry, index } => {
                        LightboxSource::user_image(entry, *index)
                            .and_then(|image| bridge::image_texture(cx, image, false))
                            .map(|texture| {
                                let size = texture
                                    .get_format(cx)
                                    .vec_width_height()
                                    .unwrap_or((1, 1));
                                (texture, size.0 as f64, size.1 as f64)
                            })
                    }
                };
                let mut d = Draw {
                    kit: &mut self.modal_kit,
                    canvas: &mut self.modal_canvas,
                    look,
                    state: &mut self.modal_state,
                    owner: None,
                    now: now_seconds(),
                    area: self.area,
                    animating: false,
                    indicators: None,
                };
                let footer = MOBILE.then_some(LightboxFooter {
                    bottom_inset,
                    export: export_available.then_some(export),
                });
                lightbox(cx, &mut d, rect, texture, *scale, *offset, footer);
            }
            Modal::Subagent {
                label, provider, ..
            } => {
                let d = Draw {
                    kit: &mut self.modal_kit,
                    canvas: &mut self.modal_canvas,
                    look,
                    state: &mut self.modal_state,
                    owner: None,
                    now: now_seconds(),
                    area: self.area,
                    animating: false,
                    indicators: None,
                };
                d.kit.rect(cx, rect, rgba(0x73000000), 0.0);
                d.canvas.add_hit(rect, HitTarget::Chrome("scrim".into()));
                let h = rect.size.y * 0.92;
                let sheet = rect_xywh(rect.pos.x, rect.pos.y + rect.size.y - h, rect.size.x, h);
                d.kit.rect_full(
                    cx,
                    sheet,
                    p.surface0,
                    vec4(16.0, 16.0, 0.0, 0.0),
                    TRANSPARENT,
                    0.0,
                );
                if p.surface0.w >= 1.0 {
                    // Rounded corners and the translucent top scrim do not occlude.
                    crate::common::clock::occlude(
                        cx,
                        rect_xywh(
                            sheet.pos.x,
                            sheet.pos.y + 16.0,
                            sheet.size.x,
                            sheet.size.y - 16.0,
                        ),
                    );
                }
                d.canvas
                    .add_hit(sheet, HitTarget::Chrome("sheet:body".into()));
                // SheetContents header: provider icon, label, open-as-tab, close.
                let measurement =
                    RowMetrics::new(0.0, 0.0, 0.0).layout(sheet, 16.0, 16.0, 72.0, 8.0);
                let title = d.kit.layout_clamped(
                    cx,
                    Font::Medium,
                    look.ui(14.0),
                    1.3,
                    label,
                    measurement.title.size.x,
                    1,
                );
                let metrics = RowMetrics::new(laidout_size(&title).y, 0.0, 0.0);
                let header_h = metrics.height(52.0, 10.0, 32.0);
                let slots = metrics.layout(
                    rect_xywh(sheet.pos.x, sheet.pos.y, sheet.size.x, header_h),
                    16.0,
                    16.0,
                    72.0,
                    8.0,
                );
                let icon_rect = rect_xywh(
                    slots.leading.pos.x,
                    sheet.pos.y + (header_h - 16.0) * 0.5,
                    16.0,
                    16.0,
                );
                if !d.dry() {
                    let svg = bridge::provider_svg(&self.providers, provider);
                    ProviderIcon::paint(
                        &mut self.painter,
                        &mut self.provider_icon,
                        cx,
                        icon_rect,
                        provider,
                        svg.as_deref(),
                        p.muted,
                    );
                }
                d.kit
                    .draw_laidout(cx, Font::Medium, p.foreground, slots.title.pos, &title);
                let open = rect_xywh(
                    slots.trailing.pos.x,
                    sheet.pos.y + (header_h - 32.0) * 0.5,
                    32.0,
                    32.0,
                );
                let close = rect_xywh(
                    slots.trailing.pos.x + slots.trailing.size.x - 32.0,
                    open.pos.y,
                    32.0,
                    32.0,
                );
                for (r, icon, id) in [
                    (open, "maximize-2", "sheet:open-tab"),
                    (close, "x", "sheet:close"),
                ] {
                    let hovered = d.state.hover_hit.as_deref() == Some(id);
                    if hovered {
                        d.kit.rect(cx, r, p.surface2, 8.0);
                    }
                    d.kit.icon(
                        cx,
                        icon,
                        r,
                        16.0,
                        if hovered { p.foreground } else { p.muted },
                    );
                    d.canvas.add_hit(r, HitTarget::Chrome(id.into()));
                }
                let content = rect_xywh(
                    sheet.pos.x,
                    sheet.pos.y + header_h,
                    sheet.size.x,
                    (sheet.size.y - header_h - bottom_inset).max(1.0),
                );
                self.sheet_content = content;
                if let Some(panel) = self.sheet_panel.clone() {
                    let walk = Walk {
                        abs_pos: Some(content.pos),
                        margin: Margin::default(),
                        width: Size::Fixed(content.size.x),
                        height: Size::Fixed(content.size.y),
                    };
                    let _ = panel.draw_walk(cx, scope, walk);
                }
            }
        }
        self.modal = Some(modal);
    }
}

/// components/context-window-sheet.tsx: the agent's facts and usage in the
/// adaptive sheet (bottom sheet on compact, centered dialog elsewhere).
fn usage_sheet(cx: &mut Cx2d, d: &mut Draw, agent: &Value, rect: Rect, bottom_inset: f64) {
    let p = d.look.palette;
    d.kit.rect(cx, rect, rgba(0x73000000), 0.0);
    d.canvas.add_hit(rect, HitTarget::Chrome("scrim".into()));
    let usage = get(agent, "lastUsage");
    let pairs: Vec<(String, &Value)> = vec![
        (t("agentPanel.usage.provider"), get(agent, "provider")),
        (t("agentPanel.usage.model"), get(agent, "model")),
        (t("agentPanel.usage.status"), get(agent, "status")),
        (t("agentPanel.usage.created"), get(agent, "createdAt")),
        (t("agentPanel.usage.updated"), get(agent, "updatedAt")),
        (
            t("agentPanel.usage.lastPrompt"),
            get(agent, "lastUserMessageAt"),
        ),
        (t("agentPanel.usage.inputTokens"), get(usage, "inputTokens")),
        (
            t("agentPanel.usage.cachedInput"),
            get(usage, "cachedInputTokens"),
        ),
        (
            t("agentPanel.usage.outputTokens"),
            get(usage, "outputTokens"),
        ),
        (
            t("agentPanel.usage.contextUsed"),
            get(usage, "contextWindowUsedTokens"),
        ),
        (
            t("agentPanel.usage.contextMaximum"),
            get(usage, "contextWindowMaxTokens"),
        ),
        (t("agentPanel.usage.cost"), get(usage, "totalCostUsd")),
    ];
    let title = t("agentPanel.usage.title");
    let runtime_label = t("agentPanel.usage.runtimeDetails");
    let compact = d.look.compact;
    let w = if compact {
        rect.size.x
    } else {
        rect.size.x.min(480.0) - 32.0
    };
    let base = d.look.ui(14.0);
    let paint = |cx: &mut Cx2d, d: &mut Draw, x: f64, y: f64| -> f64 {
        let mut cy = y + 16.0;
        d.kit.label(
            cx,
            Font::Semibold,
            d.look.ui(16.0),
            1.3,
            p.foreground,
            dvec2(x + 16.0, cy),
            &title,
            Some(w - 64.0),
        );
        cy += d.look.ui(16.0) * 1.3 + 16.0;
        for (label, value) in &pairs {
            if value.is_null() {
                continue;
            }
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            d.kit.label(
                cx,
                Font::Regular,
                base,
                1.4,
                p.muted,
                dvec2(x + 16.0, cy),
                label,
                Some(w * 0.45),
            );
            let laid = d.kit.layout(
                cx,
                Font::Regular,
                base,
                1.4,
                &text,
                Some(w * 0.5 - 16.0),
                0.0,
            );
            let s = laidout_size(&laid);
            let pos = dvec2(x + w - 16.0 - s.x, cy);
            d.kit
                .draw_laidout(cx, Font::Regular, p.foreground, pos, &laid);
            d.canvas.add_run(laid, pos, Some('\n'));
            cy += s.y.max(base * 1.4) + 8.0;
        }
        if let Some(error) = non_empty_str(get(agent, "lastError")) {
            let laid = d
                .kit
                .layout(cx, Font::Regular, base, 1.4, error, Some(w - 32.0), 0.0);
            let pos = dvec2(x + 16.0, cy);
            d.kit.draw_laidout(cx, Font::Regular, p.danger, pos, &laid);
            cy += laidout_size(&laid).y + 8.0;
            d.canvas.add_run(laid, pos, Some('\n'));
        }
        // Runtime details (ExpansionTile with JsonDetails).
        let expanded = d.state.is_expanded("usage:runtime", false);
        let header = rect_xywh(x + 16.0, cy, w - 32.0, 40.0);
        d.kit.label(
            cx,
            Font::Regular,
            base,
            1.4,
            p.foreground,
            dvec2(header.pos.x, cy + 10.0),
            &runtime_label,
            None,
        );
        d.kit.icon(
            cx,
            if expanded {
                "chevron-up"
            } else {
                "chevron-down"
            },
            rect_xywh(header.pos.x + header.size.x - 20.0, cy, 20.0, 40.0),
            16.0,
            p.muted,
        );
        d.canvas
            .add_hit(header, HitTarget::Toggle("usage:runtime".into()));
        cy += 40.0;
        if expanded {
            let blocks = vec![super::tools::DetailBlock::JsonBox(pretty(get(
                agent,
                "runtimeInfo",
            )))];
            cy += super::details::draw_details(
                cx,
                d,
                &blocks,
                false,
                false,
                "usage:runtime:json",
                x + 16.0,
                cy,
                w - 32.0,
                None,
                None,
            ) + 8.0;
        }
        cy + 16.0 - y
    };
    // The compact sheet's background runs to the bottom edge; its content ends above the inset.
    let content = d.measure(|d| paint(cx, d, 0.0, 0.0));
    let inset = if compact { bottom_inset } else { 0.0 };
    let h = (content + inset).min(rect.size.y * 0.9);
    let (x, y) = if compact {
        (rect.pos.x, rect.pos.y + rect.size.y - h)
    } else {
        (
            rect.pos.x + (rect.size.x - w) * 0.5,
            rect.pos.y + (rect.size.y - h) * 0.5,
        )
    };
    let radii = if compact {
        vec4(16.0, 16.0, 0.0, 0.0)
    } else {
        vec4(12.0, 12.0, 12.0, 12.0)
    };
    d.kit
        .rect_full(cx, rect_xywh(x, y, w, h), p.surface1, radii, p.border, 1.0);
    d.canvas.add_hit(
        rect_xywh(x, y, w, h),
        HitTarget::Chrome("sheet:body".into()),
    );
    let close = rect_xywh(x + w - 44.0, y + 8.0, 32.0, 32.0);
    let key = "usage:y".to_owned();
    let view = rect_xywh(x, y, w, (h - inset).max(0.0));
    let max_y = (content - view.size.y).max(0.0);
    d.state.extent_y.insert(key.clone(), max_y);
    let sy = d
        .state
        .scroll_y
        .get(&key)
        .copied()
        .unwrap_or(0.0)
        .clamp(0.0, max_y);
    if max_y > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollY(key.clone()));
    }
    cx.begin_turtle(
        Walk {
            abs_pos: Some(view.pos),
            margin: Margin::default(),
            width: Size::Fixed(w),
            height: Size::Fixed(view.size.y.max(1.0)),
        },
        Layout::default(),
    );
    d.canvas.push_clip(view);
    paint(cx, d, x, y - sy);
    d.canvas.pop_clip();
    cx.end_turtle();
    super::scrollbar::draw(
        cx,
        d,
        view,
        None,
        None,
        Some(super::scrollbar::Axis {
            key: &key,
            offset: sy,
            max: max_y,
        }),
    );
    d.kit.icon(cx, "x", close, 18.0, p.muted);
    d.canvas
        .add_hit(close, HitTarget::Chrome("sheet:close".into()));
}

struct LightboxFooter<'a> {
    bottom_inset: f64,
    export: Option<&'a LightboxExport>,
}

/// components/attachment-lightbox.tsx: a 90% black backdrop over the image
/// fitted inside 16px padding and at most 960×640, zoom in / out / reset and
/// close 8px from the top-right corner; "Couldn't load image" on failure.
fn lightbox(
    cx: &mut Cx2d,
    d: &mut Draw,
    rect: Rect,
    image: Option<(Texture, f64, f64)>,
    scale: f64,
    offset: DVec2,
    footer: Option<LightboxFooter<'_>>,
) {
    let p = d.look.palette;
    d.kit.rect(cx, rect, rgba(0xe6000000), 0.0);
    let mut content = rect;
    let (status, footer_height) = if let Some(footer) = &footer {
        let status = match footer.export {
            Some(export) => export.status(),
            None => Some((t("message.attachments.lightbox.originalUnavailable"), false)),
        };
        let height = footer.bottom_inset + if footer.export.is_some() { 90.0 } else { 30.0 };
        content.size.y = (content.size.y - height).max(0.0);
        (status, height.min(rect.size.y))
    } else {
        (None, 0.0)
    };
    let fit_w = (content.size.x - 32.0).clamp(0.0, 960.0);
    let fit_h = (content.size.y - 32.0).clamp(0.0, 640.0);
    match image {
        Some((texture, w, h)) => {
            let ratio = (fit_w / w.max(1.0)).min(fit_h / h.max(1.0));
            let size = dvec2(w * ratio * scale, h * ratio * scale);
            let pos = content.center() - size * 0.5 + offset;
            let r = Rect { pos, size };
            cx.begin_turtle(
                Walk {
                    abs_pos: Some(content.pos),
                    margin: Margin::default(),
                    width: Size::Fixed(content.size.x),
                    height: Size::Fixed(content.size.y),
                },
                Layout::default(),
            );
            d.kit.texture(cx, r, &texture, 0.0);
            cx.end_turtle();
            d.canvas.add_hit(
                r.clip((content.pos, content.pos + content.size)),
                HitTarget::Chrome("lightbox:image".into()),
            );
        }
        None => {
            let text = t("message.attachments.imageLoadFailed");
            let s = d
                .kit
                .measure(cx, Font::Regular, d.look.ui(14.0), 1.4, &text, None);
            d.kit.label(
                cx,
                Font::Regular,
                d.look.ui(14.0),
                1.4,
                p.muted,
                content.center() - s * 0.5,
                &text,
                None,
            );
        }
    }
    let mut x = rect.pos.x + rect.size.x - 8.0;
    for (icon, id) in [
        ("x", "lightbox:close"),
        ("rotate-ccw", "lightbox:reset"),
        ("zoom-out", "lightbox:zoom-out"),
        ("zoom-in", "lightbox:zoom-in"),
    ] {
        x -= 36.0;
        let r = rect_xywh(x, rect.pos.y + 8.0, 32.0, 32.0);
        let hovered = d.state.hover_hit.as_deref() == Some(id);
        d.kit.rect(
            cx,
            r,
            if hovered {
                rgba(0x66ffffff)
            } else {
                rgba(0x33ffffff)
            },
            16.0,
        );
        d.kit.icon(cx, icon, r, 16.0, vec4(1.0, 1.0, 1.0, 1.0));
        d.canvas.add_hit(r, HitTarget::Chrome(id.into()));
        if id == "lightbox:close" {
            x -= 8.0;
        }
    }
    if let Some(footer) = footer {
        let top = content.pos.y + content.size.y;
        let area = rect_xywh(rect.pos.x, top, rect.size.x, footer_height);
        d.kit.rect(cx, area, rgba(0xe6000000), 0.0);
        d.canvas.add_hit(area, HitTarget::Chrome("lightbox:footer".into()));
        if let Some((status, error)) = &status {
            let size = d.look.ui(13.0);
            let width = (area.size.x - 24.0).max(0.0);
            let measured = d.kit.measure(cx, Font::Regular, size, 1.3, status, None).x;
            d.kit.label_line(
                cx,
                Font::Regular,
                size,
                1.3,
                if *error { p.danger } else { rgba(0xffffffff) },
                dvec2(area.center().x - measured.min(width) * 0.5, top + 8.0),
                status,
                width,
            );
        }
        if let Some(export) = footer.export {
            let y = top + 38.0;
            let buttons = [
                ("lightbox:save", "message.attachments.lightbox.saveImage"),
                #[cfg(not(target_arch = "wasm32"))]
                ("lightbox:share", "message.attachments.lightbox.share"),
            ];
            let count = buttons.len() as f64;
            let w = ((area.size.x - 32.0) / count).clamp(0.0, 156.0);
            let left = area.center().x - (w * count + 8.0 * (count - 1.0)) * 0.5;
            for (i, (id, key)) in buttons.into_iter().enumerate() {
                let button = rect_xywh(left + i as f64 * (w + 8.0), y, w, 44.0);
                let busy = export.busy();
                let hovered = d.state.hover_hit.as_deref() == Some(id);
                d.kit.rect(
                    cx,
                    button,
                    rgba(if hovered && !busy { 0x66ffffff } else { 0x33ffffff }),
                    8.0,
                );
                let label = t(key);
                let size = d.look.ui(14.0);
                let width = (w - 12.0).max(0.0);
                let measured = d.kit.measure(cx, Font::Medium, size, 1.3, &label, None);
                d.kit.label_line(
                    cx,
                    Font::Medium,
                    size,
                    1.3,
                    rgba(if busy { 0x99ffffff } else { 0xffffffff }),
                    button.center() - dvec2(measured.x.min(width), measured.y) * 0.5,
                    &label,
                    width,
                );
                if !busy {
                    d.canvas.add_hit(button, HitTarget::Chrome(id.into()));
                }
            }
        }
    }
}

impl ChatPanelRef {
    pub fn set_target(&self, cx: &mut Cx, target: ChatTarget) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_target(cx, target);
        }
    }

    pub fn set_visible(&self, cx: &mut Cx, visible: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_visible(cx, visible);
        }
    }

    pub fn set_focused(&self, cx: &mut Cx, focused: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_focused(cx, focused);
        }
    }

    /// The template nested subagent sheets are built from (a sheet's own
    /// panel gets its parent's, so a subagent opened inside it stacks).
    pub fn set_sheet_template(&self, ptr: Option<LivePtr>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.sheet_template = ptr;
        }
    }

    pub fn run_conversation_action(&self, cx: &mut Cx, action: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.run_conversation_action(cx, action);
        }
    }

    pub fn reveal_attention(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.reveal_attention(cx);
        }
    }
}

#[cfg(test)]
mod modal_layer_tests {
    use super::*;

    fn track_tick(motion: &mut TrackMotion, cx: &mut Cx, time: f64) -> bool {
        let set = [
            motion.presence.scheduled_frame(),
            motion.peer.scheduled_frame(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let event = Event::NextFrame(NextFrameEvent {
            frame: 0,
            time,
            set,
        });
        crate::common::transition::begin_event(&event);
        motion.advance(cx, &event)
    }

    #[test]
    fn track_popover_opens_switches_in_place_and_closes() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut motion = TrackMotion::default();
        let size = dvec2(420.0, 100.0);
        use TrackPanel::{Subagents, Tasks};

        motion.show(&mut cx, Tasks, false);
        assert_eq!(motion.visual(Tasks, size).opacity, 0.0);
        assert!(motion.needs_capture());
        track_tick(&mut motion, &mut cx, 0.0);
        track_tick(&mut motion, &mut cx, 1.0);
        assert_eq!(motion.visual(Tasks, size).opacity, 1.0);
        assert!(!motion.needs_capture());

        motion.show(&mut cx, Subagents, false);
        assert_eq!(motion.panel, Some(Subagents));
        assert_eq!(motion.anchor, Some(Tasks));
        assert_eq!(motion.pair, Some((Tasks, Subagents)));
        assert_eq!(motion.visual(Tasks, size).opacity, 1.0);
        assert_eq!(motion.visual(Subagents, size).opacity, 0.0);
        track_tick(&mut motion, &mut cx, 2.0);
        track_tick(&mut motion, &mut cx, 2.05);
        assert!(motion.visual(Tasks, size).opacity < 1.0);
        let elapsed = motion.peer.value();
        let outgoing = motion.visual(Tasks, size);
        let incoming = motion.visual(Subagents, size);
        motion.show(&mut cx, Tasks, false);
        assert_eq!(motion.panel, Some(Tasks));
        assert_eq!(motion.peer.value(), elapsed);
        assert_eq!(motion.visual(Tasks, size), outgoing);
        assert_eq!(motion.visual(Subagents, size), incoming);
        track_tick(&mut motion, &mut cx, 2.05);
        track_tick(&mut motion, &mut cx, 3.0);
        assert_eq!(motion.visual(Tasks, size).opacity, 1.0);
        assert_eq!(motion.visual(Subagents, size).opacity, 0.0);

        motion.close(&mut cx);
        assert!(!motion.open);
        assert!(motion.needs_capture());
        track_tick(&mut motion, &mut cx, 4.0);
        track_tick(&mut motion, &mut cx, 5.0);
        assert_eq!(motion.visual(Tasks, size).opacity, 0.0);
        assert!(!motion.needs_capture());
        assert!(motion.presence.scheduled_frame().is_none());
        assert!(motion.peer.scheduled_frame().is_none());
        assert!(!track_tick(&mut motion, &mut cx, 6.0));
    }

    #[test]
    fn track_bottom_sheet_reopens_from_mid_close_without_jump_or_idle_frames() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut motion = TrackMotion::default();
        let panel = TrackPanel::Tasks;
        let size = dvec2(390.0, 200.0);

        motion.show(&mut cx, panel, true);
        assert_eq!(motion.visual(panel, size).offset.y, size.y);
        assert!(!motion.needs_capture());
        track_tick(&mut motion, &mut cx, 0.0);
        track_tick(&mut motion, &mut cx, 1.0);
        assert_eq!(motion.visual(panel, size).offset.y, 0.0);
        assert!(motion.presence.scheduled_frame().is_none());

        motion.close(&mut cx);
        track_tick(&mut motion, &mut cx, 2.0);
        track_tick(&mut motion, &mut cx, 2.08);
        let closing = motion.visual(panel, size);
        assert!((0.0..size.y).contains(&closing.offset.y));
        assert!(!motion.open);
        motion.show(&mut cx, panel, true);
        assert!(motion.open);
        assert_eq!(motion.visual(panel, size), closing);
        track_tick(&mut motion, &mut cx, 2.08);
        track_tick(&mut motion, &mut cx, 3.0);
        assert_eq!(motion.visual(panel, size).offset.y, 0.0);

        motion.close(&mut cx);
        track_tick(&mut motion, &mut cx, 4.0);
        track_tick(&mut motion, &mut cx, 5.0);
        assert_eq!(motion.visual(panel, size).offset.y, size.y);
        assert!(motion.presence.scheduled_frame().is_none());
        assert!(!track_tick(&mut motion, &mut cx, 6.0));
    }

    #[test]
    fn cached_tool_sheet_stays_above_streaming_subagent() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_draw::live_design(&mut cx);
        makepad_draw::shader::draw_trapezoid::DrawTrapezoidVector::register_main_module(&mut cx);
        cx.live_expand();
        let pass = Pass::new(&mut cx);
        let size = dvec2(100.0, 100.0);
        pass.set_size(&mut cx, size);
        let bounds = rect_xywh(0.0, 0.0, size.x, size.y);
        let viewport = rect_xywh(0.0, 20.0, size.x, 80.0);
        let overlay = Overlay::new(&mut cx);
        let mut window = DrawList2d::new(&mut cx);
        let mut subagent = DrawList2d::new(&mut cx);
        let mut subagent_content = DrawList2d::new(&mut cx);
        let mut stream = DrawList2d::new(&mut cx);
        let mut tool = DrawList2d::new(&mut cx);
        let mut tool_content = DrawList2d::new(&mut cx);
        // A retained list with a draw item can replay rather than repaint.
        let mut detail = DrawList2d::new(&mut cx);
        let mut detail_area = Area::Empty;
        let window_id = window.draw_list_id();
        let stream_id = stream.draw_list_id();
        let detail_id = detail.draw_list_id();

        fn paint_order(cx: &Cx, id: DrawListId, order: &mut Vec<DrawListId>) {
            order.push(id);
            let items = &cx.draw_lists[id].draw_items;
            for i in 0..items.len() {
                if let Some(child) = items[i].sub_list() {
                    paint_order(cx, child, order);
                }
            }
        }

        // Open both sheets, repaint just the parent window, then repaint the child's stream.
        // Neither subsequent repaint should change sheet order or repaint the tool itself.
        for (dirty, draws_stream) in [(detail_id, true), (window_id, false), (stream_id, true)] {
            {
                let event = DrawEvent {
                    draw_lists: vec![dirty],
                    ..Default::default()
                };
                let mut draw = CxDraw::new(&mut cx, &event);
                draw.begin_pass(&pass, Some(1.0));
                window.begin_always(&mut draw);
                let mut cx = Cx2d::new(&mut draw);
                overlay.begin(&mut cx);
                subagent.begin_overlay_last(&mut cx);
                cx.begin_pass_sized_turtle(Layout::flow_overlay());
                let repaint = subagent_content
                    .begin(&mut cx, Walk::abs_rect(bounds))
                    .is_ok();
                assert_eq!(repaint, draws_stream);
                if repaint {
                    cx.begin_turtle(Walk::abs_rect(viewport), Layout::flow_overlay());
                    stream.begin_always(&mut cx);
                    stream.end(&mut cx);
                    tool.begin_overlay_last(&mut cx);
                    cx.begin_pass_sized_turtle(Layout::flow_overlay());
                    if tool_content.begin(&mut cx, Walk::abs_rect(bounds)).is_ok() {
                        assert_eq!(dirty, detail_id);
                        detail.begin_always(&mut cx);
                        cx.add_aligned_rect_area(&mut detail_area, bounds);
                        detail.end(&mut cx);
                        tool_content.end(&mut cx);
                    }
                    cx.end_pass_sized_turtle();
                    tool.end(&mut cx);
                    cx.end_turtle();
                    subagent_content.end(&mut cx);
                }
                cx.end_pass_sized_turtle();
                subagent.end(&mut cx);
                overlay.end(&mut cx);
                window.end(&mut cx);
                cx.end_pass(&pass);
            }

            let mut order = Vec::new();
            paint_order(&cx, window_id, &mut order);
            order.retain(|id| *id == stream_id || *id == detail_id);
            assert_eq!(order, [stream_id, detail_id], "dirty list: {dirty:?}");
            assert_eq!(detail_area.clipped_rect(&cx), bounds);
        }
    }
}

#[cfg(test)]
mod reading_hold_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn timeline_handoff_first_draw_attaches_and_reentry_paints_available_rows() {
        use paseo_gui_core::{CoreConfig, CoreEvent, CoreState, GuiCore};

        let dir = std::env::temp_dir().join(format!("paseo-chat-handoff-{}", uuid::Uuid::new_v4()));
        let core = GuiCore::start(
            CoreConfig {
                state_dir: dir.join("state"),
                daemon_home: Some(dir.join("daemon")),
                default_endpoint: None,
                tailnet_key_store: None,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let mut ui = UiContext::new(core.handle());
        let host_state = |status| {
            Arc::new(CoreState::new(
                json!({"hosts": [{"serverId": "host", "status": status}]}),
            ))
        };
        ui.state = host_state("connecting");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        ChatPanel::register_main_module(&mut cx);
        crate::app::live_design(&mut cx);
        cx.live_expand();
        cx.live_scan_dependencies();
        cx.native_load_dependencies();
        let module = ChatPanel::live_type_info(&mut cx).module_id;
        let mut panel = ChatPanel::new_from_module(&mut cx, module, live_id!(ChatPanelRoot)).unwrap();
        let target = ChatTarget {
            host_id: "host".into(),
            agent_id: "agent".into(),
            ..Default::default()
        };
        let key = target.key();
        panel.set_target(&mut cx, target);
        panel.set_visible(&mut cx, true);

        let pass = Pass::new(&mut cx);
        pass.set_size(&mut cx, dvec2(420.0, 760.0));
        let mut window = DrawList2d::new(&mut cx);
        let overlay = Overlay::new(&mut cx);
        let mut paint = |cx: &mut Cx, panel: &mut ChatPanel, ui: &mut UiContext| {
            let event = DrawEvent {
                redraw_all: true,
                ..DrawEvent::default()
            };
            let mut draw = CxDraw::new(cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            window.begin_always(&mut draw);
            let mut draw = Cx2d::new(&mut draw);
            overlay.begin(&mut draw);
            draw.begin_pass_sized_turtle(Layout::flow_overlay());
            let _ = panel.draw_walk(&mut draw, &mut Scope::with_data(ui), Walk::fill());
            draw.end_pass_sized_turtle();
            overlay.end(&mut draw);
            window.end(&mut draw);
            draw.end_pass(&pass);
        };

        // Host hello precedes the debounced State. The newly mounted root
        // has not received any widget event yet.
        ui.events.push(CoreEvent::Connected {
            host_id: "host".into(),
            server_info: json!({}),
        });
        super::super::store::handle_event(&mut cx, &Event::Signal, &mut ui);
        ui.events.clear();
        paint(&mut cx, &mut panel, &mut ui);
        assert!(with_store(|store| store.sessions[&key].connected));
        assert!(panel.snap.is_some());

        panel.set_visible(&mut cx, false);
        ui.state = host_state("connected");
        ui.events.push(CoreEvent::HostEvent {
            host_id: "host".into(),
            event: json!({"type": "agent_stream", "payload": {
                "agentId": "agent", "epoch": "epoch", "seq": 1,
                "event": {"type": "timeline", "provider": "codex",
                    "item": {"type": "assistant_message", "messageId": "past", "text": "History"}}
            }}),
        });
        super::super::store::handle_event(&mut cx, &Event::Signal, &mut ui);
        ui.events.clear();
        assert!(panel.snap.as_ref().unwrap().timeline.is_empty());

        // Reentry reads the existing replica in the first paint. Neither the
        // agent metadata nor an authoritative tail has arrived yet.
        panel.set_visible(&mut cx, true);
        paint(&mut cx, &mut panel, &mut ui);
        let snap = panel.snap.as_ref().unwrap();
        assert!(!snap.hydrated && !snap.authorized);
        assert!(panel.non_ready().is_none());
        assert!(panel.cover_area.is_empty());
        assert!(panel.items.iter().any(|item| item.id == "stream:seq:1"));

        with_store(|store| store.release_view(&mut cx, panel.uid));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn snapshot() -> Snap {
        Snap::of(
            &ChatSession::new(
                SessionKey {
                    host_id: "host".into(),
                    agent_id: "agent".into(),
                    subagent_id: None,
                },
                true,
            ),
            false,
        )
    }

    #[test]
    fn toast_and_sync_chrome_leave_timeline_retained() {
        let before = snapshot();
        let mut after = before.clone();
        after.version += 1;
        after.notice = Some("updated".into());
        after.error = Some("retry failed".into());
        after.sync = SyncStatus::Retrying;
        assert!(!before.rows_changed(&after));
    }

    #[test]
    fn transcript_and_permission_changes_invalidate_timeline() {
        let before = snapshot();
        let mut after = before.clone();
        after.timeline = Rc::new(vec![Rc::new(json!({"seqStart": 1}))]);
        assert!(before.rows_changed(&after));
        after = before.clone();
        after
            .permissions
            .push(("permission".into(), json!({"id": "permission"})));
        assert!(before.rows_changed(&after));
    }

    fn cursor(epoch: &str, seq: i64) -> Cursor {
        Cursor {
            epoch: epoch.to_owned(),
            seq,
        }
    }

    fn rows(ids: &[(&str, f64)]) -> Vec<RowItem> {
        ids.iter()
            .map(|(id, height)| RowItem {
                id: (*id).to_owned(),
                kind: RowKind::Inset(*height),
            })
            .collect()
    }

    #[test]
    fn repeated_prepends_preserve_current_reader_row_and_pixel_offset() {
        let original = rows(&[
            ("history-start", 48.0),
            ("a", 170.0),
            ("b", 35.0),
            ("c", 420.0),
        ]);
        let first_page = rows(&[
            ("history-start", 48.0),
            ("older-a", 310.0),
            ("older-b", 27.0),
            ("a", 170.0),
            ("b", 35.0),
            ("c", 420.0),
        ]);
        assert_eq!(
            remap_reading_anchor(&original, &first_page, (1, -37.5)),
            Some((3, -37.5))
        );

        // The reader moved into older-b while the second request was in flight.
        let second_page = rows(&[
            ("history-start", 48.0),
            ("oldest", 750.0),
            ("older-a", 310.0),
            ("older-b", 27.0),
            ("a", 170.0),
            ("b", 35.0),
            ("c", 420.0),
        ]);
        assert_eq!(
            remap_reading_anchor(&first_page, &second_page, (2, -12.0)),
            Some((3, -12.0))
        );
        assert_eq!(
            remap_reading_anchor(&second_page, &second_page, (6, -80.0)),
            Some((6, -80.0))
        );
    }

    #[test]
    fn prepend_absorbed_overview_group_preserves_its_anchor() {
        let entry =
            |seq| Rc::new(json!({"seqStart": seq, "seqEnd": seq, "item": {"type": "tool_call"}}));
        let group = |id: &str, entries: Vec<Rc<Value>>| {
            let stream = StreamRow {
                entry: entries.last().unwrap().clone(),
                group: Some(entries),
                fold: None,
            };
            RowItem {
                id: id.to_owned(),
                kind: RowKind::Entry {
                    rows: Rc::new(vec![stream]),
                    index: 0,
                    previous_tasks: None,
                    footer_below: false,
                    streaming: false,
                    live: false,
                    gap_below: 0.0,
                },
            }
        };
        let previous = vec![group("group:20", vec![entry(20), entry(21)])];
        let next = vec![group("group:10", vec![entry(10), entry(20), entry(21)])];
        assert_eq!(
            remap_reading_anchor(&previous, &next, (0, -18.0)),
            Some((0, -18.0))
        );
        let replacement = vec![group("group:30", vec![entry(30)])];
        assert_eq!(
            remap_reading_anchor(&previous, &replacement, (0, -18.0)),
            None
        );
    }

    #[test]
    fn older_pages_require_threshold_reentry_after_filling_viewport() {
        let mut paging = OlderPaging::default();
        assert!(paging.observe(true, false, true));
        paging.request = Some(cursor("epoch", 100));
        assert!(!paging.observe(true, false, true));
        paging.settled(Some(&cursor("epoch", 80)));
        assert!(!paging.observe(true, false, true));
        assert!(!paging.observe(true, false, true));
        assert!(!paging.observe(false, false, true));
        assert!(paging.observe(true, false, true));
    }

    #[test]
    fn compacted_pages_continue_only_while_short_and_making_progress() {
        let mut paging = OlderPaging::default();
        assert!(paging.observe(true, true, true));
        paging.request = Some(cursor("epoch", 100));
        paging.settled(Some(&cursor("epoch", 80)));
        assert!(paging.observe(true, true, true));
        paging.request = Some(cursor("epoch", 80));
        paging.settled(Some(&cursor("epoch", 60)));
        assert!(!paging.observe(true, false, true));

        assert!(!paging.observe(false, false, true));
        assert!(paging.observe(true, true, true));
        paging.request = Some(cursor("epoch", 60));
        paging.settled(Some(&cursor("epoch", 60)));
        assert!(!paging.observe(true, true, true));
    }

    #[test]
    fn older_reply_does_not_pull_reader_back_after_scrolling_away() {
        let mut paging = OlderPaging::default();
        assert!(paging.observe(true, false, true));
        paging.request = Some(cursor("epoch", 100));
        assert!(!paging.observe(false, false, false));
        paging.settled(Some(&cursor("epoch", 80)));
        assert!(!paging.observe(false, false, true));
        assert!(paging.observe(true, false, true));
    }

    #[test]
    fn epoch_replacement_is_not_progress_for_short_page_continuation() {
        let mut paging = OlderPaging::default();
        assert!(!paging.observe(true, true, false));
        assert!(paging.observe(true, true, true));
        paging.request = Some(cursor("old", 100));
        paging.settled(Some(&cursor("new", 20)));
        assert!(!paging.observe(true, true, true));
    }
}
