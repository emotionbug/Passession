//! `ChatPanel`, the chat tab root (agent_panel.dart AgentPanel minus the
//! composer internals, which ComposerPanel owns, + chrome/stream_chrome.dart):
//! the virtualized stream with sticky bottom and reading hold, older-history
//! pagination, chat find, the outline rail, the track bar, callouts, toasts,
//! conversation actions (rename, fork, reload/unarchive, usage, detach,
//! archive, delete), rewind, permission responses, file links, compact detail
//! sheets, the image lightbox and the subagent sheet. Session state lives in
//! the module-level store (`store.rs`); the panel only presents it.

use super::attention::{self, AttentionClear};
use super::canvas::*;
use super::chat_row::{ChatRowEvent, ChatRowWidgetRefExt, RowEventKind, RowModel};
use super::chrome::{self, Callout, NonReady, Toast, ToastVariant};
use super::composer_bridge as bridge;
use super::find::{self, FindController, Phase as FindPhase};
use super::i18n::{t, ta};
use super::images::{self, ImageState};
use super::json::*;
use super::layout::*;
use super::links::{self, AssistantFileLinkSource, FileLinkSourceType, InlinePathTarget, Resolution};
use super::outline::{self, Rail, RailEvent};
use super::paint::{Draw, Look, MOBILE, RowState, now_seconds};
use super::permission;
use super::rows::{self, FindTarget, LiveFooter, RowContext, RowItem, RowKind};
use super::session::{ActionKind, ChatSession, Completion, Cursor, ExecutionStatus, ForkTarget, Io, SessionKey, SyncStatus};
use super::store::with_store;
use super::tracks::{self, ArchiveFinished, SubagentRow, TrackModel, TrackPanel};
use crate::common::overlay::{self, MenuEntry, MenuItem, MenuOptions, TapTarget, WindowModal};
use crate::common::paint::Painter;
use crate::common::provider_icon::ProviderIcon;
use crate::common::widgets::{PTextFieldWidgetRefExt, RowMetrics};
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use crate::features::composer::ComposerPanelWidgetRefExt;
use makepad_widgets::*;
use makepad_widgets::scroll_bars::TouchScroll;
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
    use crate::features::timeline::canvas::*;
    use crate::features::timeline::chat_row::*;
    use crate::features::composer::ui::panel::ComposerPanel;

    pub ChatPanelBase = {{ChatPanel}} {}

    // The chat panel body. The root below adds the sheet template; a panel
    // shown inside a subagent sheet receives the same template at runtime
    // (`set_sheet_template`), so sheets stack as in Flutter.
    pub ChatPanelSheet = <ChatPanelBase> {
        width: Fill, height: Fill
        kit: <ChatKit> {}
        modal_kit: <ChatKit> {}
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
}

/// agent-panel.tsx conversation actions offered by the agent tab menu.
pub fn conversation_actions(host_id: &str, agent_id: &str) -> Vec<(String, String)> {
    let facts = MENUS.with(|m| m.borrow().get(&(host_id.to_owned(), agent_id.to_owned())).copied()).unwrap_or_default();
    let item = |value: &str, key: &str| (value.to_owned(), t(key));
    let mut out = Vec::new();
    // Native has no chat find or outline (chat-find/index.tsx, rail.tsx).
    if !MOBILE {
        out.push(item("find", "paneFind.title"));
        if facts.outline_enabled {
            out.push(item("outline", "settings.appearance.chatOutline.title"));
        }
    }
    out.push(item("tools", if facts.expanded_tools { "agentPanel.menu.collapseTools" } else { "agentPanel.menu.expandTools" }));
    out.push(item("sync", "agentPanel.menu.syncHistory"));
    out.push(item("rename", "workspace.tabs.menu.renameAgent"));
    out.push(item("fork", "agentPanel.menu.fork"));
    out.push(item("refresh", if facts.archived { "agentPanel.archived.unarchive" } else { "workspace.tabs.menu.reloadAgent" }));
    out.push(item("usage", "sidebar.footer.usage"));
    out.push(item("detach", "subagents.detachTooltip"));
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
        SessionKey { host_id: self.host_id.clone(), agent_id: self.agent_id.clone(), subagent_id: self.subagent_id.clone() }
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
    overview: bool,
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
            self.continue_short = start.is_some_and(|start| start.epoch == request.epoch && start.seq < request.seq);
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

fn remap_reading_anchor(previous: &[RowItem], next: &[RowItem], anchor: (usize, f64)) -> Option<(usize, f64)> {
    let row = previous.get(anchor.0)?;
    let index = next.iter().position(|item| item.id == row.id).or_else(|| {
        // A prepended tool run can absorb the first overview group and change
        // its ID. The old projected sequence still belongs to that group.
        let seq = row.seq()?;
        next.iter().position(|item| match &item.kind {
            RowKind::Entry { rows, index, .. } => {
                let row = &rows[*index];
                let contains = |entry: &Value| {
                    get(entry, "seqStart").as_i64().is_some_and(|start| start <= seq)
                        && get(entry, "seqEnd").as_i64().is_some_and(|end| seq <= end)
                };
                row.group.as_ref().map_or_else(|| contains(&row.entry), |group| group.iter().any(|entry| contains(entry)))
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
#[derive(Default)]
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
    additions: i64,
    deletions: i64,
    preferences: Value,
    server_info: Value,
}

/// Overlay answers the panel waits for.
enum Ask {
    Rename,
    Confirm(&'static str),
    ArchiveSubagent(SubagentRow),
    DetachSubagent(SubagentRow),
    Fork { row: String, entry: Option<Rc<Value>> },
    Rewind { row: String, entry: Rc<Value> },
    Outline,
}

enum Modal {
    Track(TrackPanel),
    Sheet { row: String, kind: String, opened: f64 },
    Usage,
    Lightbox { source: LightboxSource, scale: f64, offset: DVec2 },
    Subagent { label: String, provider: String, tab: PanelTarget },
}

enum LightboxSource {
    Assistant { url: String, row: String },
    User(Value),
}

/// What a session reveal (older pages until a seq is loaded) was for.
enum RevealFor {
    Find(u64),
    Prompt(i64),
}

#[derive(Default)]
struct ArchiveRun {
    queue: Vec<SubagentRow>,
    completed: usize,
    failed: usize,
    total: usize,
}

#[derive(Live, Widget)]
pub struct ChatPanel {
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
    #[live]
    sheet_template: Option<LivePtr>,

    #[rust(DrawList2d::new(cx))]
    overlay_list: DrawList2d,
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
    snap: Option<Snap>,
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
    #[rust]
    stream_rows: Rc<Vec<StreamRow>>,
    #[rust]
    footer_at_end: bool,
    #[rust]
    row_states: HashMap<String, Rc<RefCell<RowState>>>,
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
    next_frame: NextFrame,
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
    track_anchors: Vec<(TrackPanel, Rect)>,
    #[rust]
    archive_status: ArchiveFinished,
    #[rust]
    archive_run: ArchiveRun,
    #[rust]
    pending_archive: HashSet<String>,
    #[rust]
    hidden_provider: HashSet<String>,
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
    #[rust]
    modal_touch_scroll: TouchScroll,
    #[rust]
    modal_horizontal_scroll: TouchScroll,
    #[rust]
    pressed: TapTarget<HitTarget>,
    /// The owning subagent sheet, assigned by its dispatcher before child input.
    #[rust]
    input_parent: Option<u64>,
    /// The core handle from the last event pass (draws without a UiContext).
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
    list(get(state, "hosts")).iter().find(|h| get(h, "serverId") == host_id)
}

fn error_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => non_empty_str(get(value, "message")).map(str::to_owned),
        _ => None,
    }
}

impl ChatPanel {
    // ── target and activity ─────────────────────────────────────────────

    pub fn set_target(&mut self, cx: &mut Cx, target: ChatTarget) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        let changed_session = self.target.as_ref().map(ChatTarget::key) != Some(target.key());
        if changed_session {
            self.reset_session_state(cx);
            // A new conversation opens at its latest edge.
            self.list.as_portal_list().set_tail_range(true);
        }
        self.target = Some(target);
        self.ensured = false;
        self.area.redraw(cx);
    }

    fn reset_session_state(&mut self, cx: &mut Cx) {
        self.dismiss_modal(cx);
        self.snap = None;
        self.snapshot_stale = true;
        self.providers_request = None;
        self.providers_requested = false;
        self.items.clear();
        self.model_sig = None;
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
        self.archive_status = ArchiveFinished::Idle;
        self.archive_run = ArchiveRun::default();
        self.pending_archive.clear();
        self.hidden_provider.clear();
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
            self.composer.as_composer_panel().set_visible(cx, visible && self.composer_shown());
            if !visible {
                self.dismiss_modal(cx);
            }
            self.area.redraw(cx);
        }
    }

    pub fn set_focused(&mut self, cx: &mut Cx, focused: bool) {
        if self.focused != focused {
            self.focused = focused;
            self.area.redraw(cx);
        }
    }

    fn sync_window_modal(&mut self, ui: &UiContext) {
        let was_open = self.window_modal.is_some();
        let open = self.visible && (self.modal.is_some() || self.composer.as_composer_panel().captures_input());
        let owner = self.widget_uid().0;
        WindowModal::sync(&mut self.window_modal, ui, owner, open);
        if was_open != self.window_modal.is_some() {
            self.pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
        }
    }

    fn modal_branch_accepts_input(&self, window: &str) -> bool {
        overlay::window_modal_accepts_owner(window, self.widget_uid().0)
            || self.input_parent.is_some_and(|parent| overlay::window_modal_accepts_owner(window, parent))
            || self.sheet_panel.as_ref().is_some_and(|panel| {
                panel.as_chat_panel().borrow().is_some_and(|child| child.modal_branch_accepts_input(window))
            })
    }

    fn modal_tap_target(&self, abs: DVec2) -> Option<HitTarget> {
        self.chrome_hit(abs).or_else(|| {
            (matches!(self.modal, Some(Modal::Lightbox { .. })) && self.modal_rect.contains(abs))
                .then(|| HitTarget::Chrome("scrim".into()))
        })
    }

    fn dismiss_modal(&mut self, cx: &mut Cx) {
        self.modal_touch_scroll.reset();
        self.modal_horizontal_scroll.reset();
        if let Some(panel) = self.sheet_panel.take() {
            panel.as_chat_panel().set_visible(cx, false);
        }
        self.modal = None;
        self.window_modal = None;
        self.modal_state = RowState::default();
        self.pressed.cancel();
        self.lightbox_drag = None;
        self.area.redraw(cx);
    }

    /// The tab menu picked a conversation action; runs with the next event.
    pub fn run_conversation_action(&mut self, cx: &mut Cx, action: &str) {
        self.pending_actions.push(action.to_owned());
        self.area.redraw(cx);
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn with_session<R>(&self, cx: &mut Cx, ui: &mut UiContext, f: impl FnOnce(&mut ChatSession, &mut Io) -> R) -> Option<R> {
        let key = self.target.as_ref()?.key();
        let mut effects = std::mem::take(&mut ui.effects);
        let result = with_store(|store| store.with_session(cx, &mut effects, &key, f));
        effects.append(&mut ui.effects);
        ui.effects = effects;
        result
    }

    fn show_failure(&mut self, message: impl Into<String>) {
        self.error_seq += 1;
        self.error = Some((self.error_seq, message.into()));
    }

    fn feature(&self, name: &str) -> bool {
        feature(&self.host.server_info, name)
    }

    fn row_state(&mut self, id: &str) -> Rc<RefCell<RowState>> {
        self.row_states.entry(id.to_owned()).or_default().clone()
    }

    // ── snapshots ───────────────────────────────────────────────────────

    /// Copies the session's presented facts when its version moved.
    fn refresh_snapshot(&mut self) -> bool {
        if !self.visible {
            self.snapshot_stale = true;
            return false;
        }
        let Some(target) = &self.target else { return false };
        let key = target.key();
        let seen = self.snap.as_ref().map(|s| s.version);
        let create_handoff = !key.is_subagent() && bridge::create_handoff_active(&key.host_id, &key.agent_id);
        let next = with_store(|store| {
            let session = store.sessions.get(&key)?;
            if Some(session.version) == seen && self.snap.as_ref().is_some_and(|s| s.create_handoff == create_handoff) {
                return None;
            }
            Some(Snap::of(session, create_handoff))
        });
        match next {
            Some(snap) => {
                self.snap = Some(snap);
                true
            }
            None => false,
        }
    }

    fn refresh_host(&mut self, ui: &UiContext) {
        let ptr = Arc::as_ptr(&ui.state) as usize;
        let Some(target) = &self.target else { return };
        let server_info = ui.server_infos.get(&target.host_id).cloned();
        if ptr == self.host.state_ptr && server_info.as_ref().is_none_or(|s| *s == self.host.server_info) {
            return;
        }
        let state = &*ui.state;
        let mut facts = HostFacts { state_ptr: ptr, ..Default::default() };
        let row = host_row(state, &target.host_id);
        facts.connected = row.is_some_and(|r| get(r, "status") == "connected");
        facts.connecting = row.is_some_and(|r| get(r, "status") == "connecting");
        facts.label = row.and_then(|r| non_empty_str(get(r, "label"))).unwrap_or(&target.host_id).to_owned();
        facts.host_error = row.and_then(|r| error_text(get(r, "error")).or_else(|| error_text(get(r, "lastError"))));
        facts.maintenance = row.and_then(|r| optional(get(r, "maintenanceOperationId")));
        for host in list(get(state, "hosts")) {
            let server_id = str_of(get(host, "serverId")).to_owned();
            facts.labels.insert(server_id.clone(), str_or(get(host, "label"), &server_id).to_owned());
            for raw in list(get(host, "agents")) {
                let agent = if get(raw, "agent").is_object() { get(raw, "agent") } else { raw };
                facts.agents.push((server_id.clone(), agent.clone()));
            }
        }
        facts.agent_row = facts
            .agents
            .iter()
            .find(|(server, agent)| *server == target.host_id && get(agent, "id") == target.agent_id.as_str())
            .map(|(_, a)| a.clone());
        if let Some(workspace) = row.and_then(|r| list(get(r, "workspaces")).iter().find(|w| get(w, "id") == target.workspace_id.as_str())) {
            let diff = get(workspace, "diffStat");
            facts.additions = int_of(get(diff, "additions"));
            facts.deletions = int_of(get(diff, "deletions"));
        }
        facts.preferences = ui.preferences().clone();
        facts.server_info = server_info
            .or_else(|| row.map(|r| get(r, "serverInfo").clone()).filter(Value::is_object))
            .unwrap_or_else(|| super::store::server_info(&target.host_id));
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
            syntax: paseo_highlight::SyntaxThemeId::from_name(syntax).unwrap_or(paseo_highlight::SyntaxThemeId::One),
        }
    }

    // ── stream model ────────────────────────────────────────────────────

    /// subagents/select.ts useSubagentsForParent.
    fn subagent_rows(&mut self) -> Vec<SubagentRow> {
        let (Some(target), Some(snap)) = (self.target.clone(), self.snap.clone()) else { return Vec::new() };
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
                    || self.pending_archive.contains(&format!("{server_id}:{id}"))
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
                    turn_open: get(get(child, "turn"), "phase") == "open" || !get(child, "activeTurn").is_null(),
                    created_ms: parse_time(get(child, "createdAt")).unwrap_or(0),
                    workspace_id: optional(get(child, "workspaceId")),
                    cwd: optional(get(child, "cwd")),
                    parent_agent_id: None,
                    agent: child.clone(),
                });
            }
        }
        for child in &snap.subagents {
            let id = str_of(get(child, "id")).to_owned();
            let parent = get(child, "parentSubagentId");
            // A dismissed subagent that runs again returns to the track.
            if get(child, "status") == "running" {
                self.hidden_provider.remove(&id);
            }
            let skip = if target.is_subagent() { parent.as_str() != target.subagent_id.as_deref() } else { !parent.is_null() && nesting };
            if self.hidden_provider.contains(&id) || skip {
                continue;
            }
            rows.push(SubagentRow {
                paseo: false,
                id,
                server_id: target.host_id.clone(),
                provider: str_or(get(child, "provider"), str_of(get(&snap.agent, "provider"))).to_owned(),
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
        let Some(snap) = &self.snap else { return Vec::new() };
        snap.timeline
            .iter()
            .rev()
            .map(|e| get(e, "item"))
            .find(|i| get(i, "type") == "todo")
            .map(|i| list(get(i, "items")).to_vec())
            .unwrap_or_default()
    }

    fn tracks_visible(&mut self) -> bool {
        let Some(snap) = &self.snap else { return false };
        if snap.archived || self.archiving {
            return false;
        }
        let subagent = self.target.as_ref().is_some_and(ChatTarget::is_subagent);
        let tasks = if subagent { Vec::new() } else { self.latest_tasks() };
        let rows = self.subagent_rows();
        let (a, d) = if subagent { (0, 0) } else { (self.host.additions, self.host.deletions) };
        a > 0 || d > 0 || !rows.is_empty() || !tasks.is_empty() || self.archive_status != ArchiveFinished::Idle
    }

    /// agent-panel.dart `_streamRows` + `_timeline` items, oldest first.
    fn rebuild_items(&mut self, look: Look, track_height: f64) {
        let Some(snap) = self.snap.clone() else { return };
        let Some(target) = self.target.clone() else { return };
        let overview = get(&self.host.preferences, "toolCallDetailLevel") == "overview";
        let provider = str_of(get(&snap.agent, "provider")).to_owned();
        let outgoing = if target.is_subagent() { 0 } else { bridge::outgoing_revision() };
        let tracks = self.tracks_visible();
        let clearance = if tracks { tracks::tail_clearance(look.compact, track_height) } else { 0.0 };
        let permission_count = if target.is_subagent() { 0 } else { snap.permissions.len() };
        let sig = ModelSig {
            timeline: Rc::as_ptr(&snap.timeline) as usize,
            outgoing,
            overview,
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
        if !target.is_subagent() {
            let echoed: HashSet<String> = entries
                .iter()
                .filter_map(|row| get(get(row, "item"), "clientMessageId").as_str().map(str::to_owned))
                .collect();
            for row in bridge::outgoing_rows(&target.host_id, &target.workspace_id, &target.agent_id, &|id| echoed.contains(id)) {
                entries.push(Rc::new(row));
            }
        }
        let rows = Rc::new(build_stream_rows(&entries, overview, &provider));
        let running = snap.running;
        let auxiliary = auxiliary_footer_host(&rows, running);
        let has_auxiliary = running || auxiliary.is_some();
        let live_from = if running { rows.iter().rposition(|r| r.kind() == "user_message").map(|i| i + 1).unwrap_or(0) } else { rows.len() };
        let timing = derive_turn_timing(&rows, running);
        let mut used = HashSet::new();
        let mut row_id = |i: usize| -> String {
            let row = &rows[i];
            let seq = get(&row.entry, "seqStart");
            let client = get(get(&row.entry, "item"), "clientMessageId");
            let id = if row.group.is_some() {
                format!("group:{}", row.group_id())
            } else if let Some(seq) = seq.as_i64() {
                format!("seq:{seq}")
            } else if let Some(client) = client.as_str() {
                format!("pending:{client}")
            } else {
                format!("index:{i}")
            };
            if used.insert(id.clone()) { id } else { format!("{id}#{i}") }
        };
        let mut items = vec![RowItem { id: "history-start".into(), kind: RowKind::HistoryStart { loading: self.older.loading() } }];
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
            let gap_below = if footer_below && below.is_some() { 0.0 } else { gap_between_stream_items(Some(&rows[i].entry), below) };
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
                items.push(RowItem { id: format!("footer:{host_id}"), kind: RowKind::Footer { rows: rows.clone(), host: Some(host), timing: timing.get(&host).copied() } });
            }
        }
        if running {
            items.push(RowItem { id: "footer:live".into(), kind: RowKind::Footer { rows: rows.clone(), host: None, timing: None } });
        } else if let Some(host) = auxiliary {
            let host_id = ids.get(&host).cloned().unwrap_or_else(|| row_id(host));
            items.push(RowItem { id: format!("footer:{host_id}"), kind: RowKind::Footer { rows: rows.clone(), host: Some(host), timing: timing.get(&host).copied() } });
        }
        if !target.is_subagent() {
            for (key, request) in &snap.permissions {
                items.push(RowItem { id: format!("perm:{key}"), kind: RowKind::Permission { key: key.clone(), request: Rc::new(request.clone()) } });
            }
        }
        // The tail inset keeps the newest rows clear of the floating track bar.
        let footer_at_end = has_auxiliary;
        let existing = if footer_at_end && permission_count == 0 { 32.0 } else { 0.0 };
        if clearance > existing {
            items.push(RowItem { id: "tail-inset".into(), kind: RowKind::Inset(clearance - existing) });
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
            && self.snap.as_ref().is_none_or(|s| s.permissions.is_empty() || self.target.as_ref().is_some_and(ChatTarget::is_subagent))
            && !self.footer_at_end
    }

    fn row_context(&mut self, look: Look, core: &paseo_gui_core::CoreHandle) -> Rc<RowContext> {
        let target = self.target.clone().unwrap_or_default();
        let snap = self.snap.clone();
        let agent = snap.as_ref().map(|s| s.agent.clone()).unwrap_or_default();
        let active = get(&agent, "activeTurn");
        let started = parse_time(get(active, "startedAt")).or_else(|| {
            snap.as_ref().and_then(|s| s.turns.get(str_of(get(active, "turnId")))).and_then(|turn| parse_time(get(turn, "startedAt")))
        });
        let live = self.host.connected && get(&agent, "snapshotAvailability") != "last_persisted" && get(&agent, "_live") != &Value::Bool(false);
        let find = self.find.highlight().map(|(query, row_seq, occurrence)| FindTarget { query, row_seq, occurrence });
        let agent_titles = self
            .host
            .agents
            .iter()
            .filter(|(server, _)| *server == target.host_id)
            .filter_map(|(_, a)| Some((non_empty_str(get(a, "id"))?.to_owned(), str_of(get(a, "title")).to_owned())))
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
                pending_delivery: !target.is_subagent() && bridge::has_pending_delivery(&target.host_id, &target.workspace_id, &target.agent_id),
            },
            can_fork_cursor: self.feature("agentForkContextCursor"),
            epoch: snap.as_ref().and_then(|s| s.epoch.clone()),
        })
    }

    fn entry_of(&self, row_id: &str) -> Option<Rc<Value>> {
        self.items.iter().find(|i| i.id == row_id).and_then(|item| match &item.kind {
            RowKind::Entry { rows, index, .. } => rows.get(*index).map(|r| r.entry.clone()),
            RowKind::Footer { rows, host: Some(host), .. } => rows.get(*host).map(|r| r.entry.clone()),
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
        self.area.redraw(cx);
    }

    /// Brings `row_id` to the top of the stream (`offset` below its top edge).
    fn scroll_to_row(&mut self, cx: &mut Cx, row_id: &str, offset: f64) {
        let Some(index) = self.items.iter().position(|i| i.id == row_id) else { return };
        self.detach();
        self.at_bottom = false;
        self.list.as_portal_list().set_first_id_and_scroll(index, offset);
        self.area.redraw(cx);
    }

    fn row_id_of_seq(&self, seq: i64) -> Option<String> {
        self.items.iter().find(|i| i.seq() == Some(seq)).map(|i| i.id.clone())
    }

    fn load_older(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(snap) = &self.snap else { return };
        if !self.visible || self.older.loading() || !snap.has_older || snap.loading {
            return;
        }
        let Some(start) = snap.start_cursor.clone() else { return };
        self.older.request = Some(start);
        self.with_session(cx, ui, |s, io| s.older(io));
        self.area.redraw(cx);
    }

    // ── outline ─────────────────────────────────────────────────────────

    /// use-chat-outline.ts: the prompt index for the served epoch, refreshed
    /// whenever a newer prompt arrives.
    fn refresh_prompts(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(snap) = self.snap.clone() else { return };
        let subagent = self.target.as_ref().is_some_and(ChatTarget::is_subagent);
        if subagent || !outline::supported() || !self.feature("agentTimelinePromptIndex") || snap.epoch.is_none() || !self.outline_enabled() {
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
        }
        self.prompts_epoch = snap.epoch.clone();
        self.prompts_request += 1;
        let request = self.prompts_request;
        self.with_session(cx, ui, |s, io| {
            s.action(io, ActionKind::ListPrompts { request }, "agent.timeline.list_prompts.request", "agent.timeline.list_prompts.response", json!({}))
        });
    }

    fn jump_to_prompt(&mut self, cx: &mut Cx, ui: &mut UiContext, seq: i64) {
        let Some(snap) = &self.snap else { return };
        let loaded = snap.timeline.iter().any(|r| {
            get(get(r, "item"), "type") == "user_message" && int_of(get(r, "seqStart")) <= seq && int_or(get(r, "seqEnd"), int_of(get(r, "seqStart"))) >= seq
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
            .find(|r| get(get(r, "item"), "type") == "user_message" && int_of(get(r, "seqStart")) <= seq && int_or(get(r, "seqEnd"), int_of(get(r, "seqStart"))) >= seq)
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
                self.model_sig = None;
                self.area.redraw(cx);
            }
        }
    }

    // ── find ────────────────────────────────────────────────────────────

    fn open_find(&mut self, cx: &mut Cx) {
        if MOBILE || self.snap.is_none() {
            return;
        }
        self.find.show();
        self.find_field.as_ptext_field().set_placeholder(cx, &t("paneFind.placeholder"));
        self.find_open = true;
        self.area.redraw(cx);
    }

    fn close_find(&mut self, cx: &mut Cx) {
        self.find.close();
        self.find_open = false;
        self.find_field_rect = None;
        self.area.redraw(cx);
    }

    /// Runs the find controller's requests.
    fn pump_find(&mut self, cx: &mut Cx, ui: &mut UiContext) {
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
                find::Request::Search { token, query, cursor } => {
                    let mut fields = json!({"query": query});
                    if let Some(cursor) = cursor {
                        fields["cursor"] = json!(cursor);
                    }
                    self.with_session(cx, ui, |s, io| {
                        s.action(io, ActionKind::Search { token }, "agent.timeline.search.request", "agent.timeline.search.response", fields)
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
            self.pump_find(cx, ui);
        }
        if self.visible {
            self.area.redraw(cx);
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
                self.area.redraw(cx);
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
            s.action(io, ActionKind::OutlineMenu, "agent.timeline.list_prompts.request", "agent.timeline.list_prompts.response", json!({}))
        });
    }

    fn lifecycle(&mut self, cx: &mut Cx, ui: &mut UiContext, action: &str) {
        let Some(snap) = self.snap.clone() else { return };
        match action {
            "rename" => {
                let id = overlay::prompt_text(ui, "Rename session", None, str_of(get(&snap.agent, "title")), "", &t("common.actions.save"));
                self.asks.insert(id, Ask::Rename);
            }
            "refresh" => {
                self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Refresh, "refresh_agent_request", "status", json!({})));
            }
            "fork" => self.fork(cx, ui, None, ForkTarget::Tab),
            "archive" => {
                let id = overlay::confirm(ui, "Archive session?", "Archive this session and stop active work?", &t("agentList.archiveSheet.archive"), true);
                self.asks.insert(id, Ask::Confirm("archive"));
            }
            "delete" => {
                let id = overlay::confirm(ui, "Delete session?", "Permanently delete this session?", &t("workspace.fileActions.delete"), true);
                self.asks.insert(id, Ask::Confirm("delete"));
            }
            "detach" => {
                self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Detach, "agent.detach.request", "agent.detach.response", json!({})));
            }
            "cancel" => {
                self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Cancel, "cancel_agent_request", "cancel_agent_response", json!({})));
            }
            "usage" => {
                self.modal = Some(Modal::Usage);
                self.modal_state = RowState::default();
                self.area.redraw(cx);
            }
            _ => {}
        }
    }

    /// hooks/use-fork-agent.ts: the fork context pinned to `entry` (or the
    /// whole timeline so far), opened as a draft here or in a new workspace.
    fn fork(&mut self, cx: &mut Cx, ui: &mut UiContext, entry: Option<Rc<Value>>, target: ForkTarget) {
        let Some(snap) = self.snap.clone() else { return };
        let Some(chat) = self.target.clone() else { return };
        if !self.feature("agentForkContext") {
            self.show_failure(t("message.actions.forkUnavailable"));
            return;
        }
        if target == ForkTarget::Tab && chat.workspace_id.is_empty() {
            self.show_failure(t("message.actions.forkMissingWorkspace"));
            return;
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
        self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Fork { target }, "agent.fork_context.request", "agent.fork_context.response", fields));
    }

    fn config(&self) -> Value {
        let target = self.target.clone().unwrap_or_default();
        let agent = self.snap.as_ref().map(|s| s.agent.clone()).unwrap_or_default();
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

    fn respond_permission(&mut self, cx: &mut Cx, ui: &mut UiContext, key: &str, action: &str, response: Value) {
        let Some(snap) = &self.snap else { return };
        let Some((_, request)) = snap.permissions.iter().find(|(k, _)| k == key) else { return };
        let request_id = get(request, "id").clone();
        let state = self.row_state(&format!("perm:{key}"));
        if let Some(card) = state.borrow_mut().permission.as_mut() {
            card.pending = true;
            card.failed = false;
            card.responding_action = Some(action.to_owned());
        }
        let key = key.to_owned();
        self.with_session(cx, ui, |s, io| {
            s.action_with_timeout(
                io,
                ActionKind::Permission { key },
                "agent_permission_response",
                "agent_permission_resolved",
                json!({"requestId": request_id, "response": response}),
                Duration::from_secs(15),
            )
        });
        self.area.redraw(cx);
    }

    /// The panel's toast (agent-panel.tsx `toast ?? syncToast`).
    fn panel_toast(&self) -> Option<Toast> {
        if let Some((seq, message)) = &self.error {
            return Some(Toast { id: format!("error:{seq}"), message: message.clone(), variant: ToastVariant::Error, spinner: false, duration: Some(3.2) });
        }
        let snap = self.snap.as_ref();
        if let Some(notice) = snap.and_then(|s| s.notice.clone()) {
            return Some(Toast { id: format!("notice:{notice}"), message: notice, variant: ToastVariant::Normal, spinner: false, duration: Some(3.2) });
        }
        if let Some(snap) = snap {
            let id = format!("older-history:{}", snap.older_failures);
            if snap.older_failures > 0 && !self.dismissed_toasts.contains(&id) {
                return Some(Toast { id, message: t("loadOlderHistory.failed"), variant: ToastVariant::Normal, spinner: false, duration: Some(2.2) });
            }
        }
        if let Some(maintenance) = &self.host.maintenance {
            let id = format!("maintenance:{maintenance}");
            if !self.dismissed_toasts.contains(&id) {
                return Some(Toast { id, message: t("agentPanel.states.daemonMaintenance"), variant: ToastVariant::Normal, spinner: true, duration: None });
            }
        }
        let snap = snap?;
        if snap.archived {
            return None;
        }
        if !self.host.connected {
            return Some(Toast { id: "reconnecting".into(), message: t("agentPanel.states.reconnecting"), variant: ToastVariant::Normal, spinner: true, duration: None });
        }
        if snap.sync == SyncStatus::Pending && snap.hydrated && !snap.create_handoff {
            return Some(Toast { id: "updating".into(), message: t("agentPanel.states.updating"), variant: ToastVariant::Normal, spinner: true, duration: None });
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
        self.area.redraw(cx);
    }

    // ── file links ──────────────────────────────────────────────────────

    /// use-file-link.ts openAssistantFileLink.
    fn open_link(&mut self, cx: &mut Cx, ui: &mut UiContext, href: &str, text: &str, kind: FileLinkSourceType) {
        let Some(target) = self.target.clone() else { return };
        let root = target.cwd().to_owned();
        let source = AssistantFileLinkSource { href: href.to_owned(), text: (!text.is_empty()).then(|| text.to_owned()), kind };
        match links::classify_for_resolution(&source, Some(&root)) {
            Resolution::Ignored => {}
            Resolution::External(url) => {
                let lower = url.to_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    ui.push(UiEffect::OpenExternal { url });
                }
            }
            Resolution::File(path) => self.open_inline_path(cx, ui, path),
            Resolution::NeedsLookup { query, token, target: path } => {
                let fields = json!({
                    "query": query,
                    "cwd": root.trim(),
                    "includeFiles": true,
                    "includeDirectories": false,
                    "matchMode": "suffix",
                    "limit": 1,
                });
                let encoded = json!({"raw": path.raw, "path": path.path, "lineStart": path.line_start, "lineEnd": path.line_end});
                self.with_session(cx, ui, |s, io| {
                    s.host_request(io, ActionKind::LookupFile { token, target: encoded }, "directory_suggestions_request", "directory_suggestions_response", fields)
                });
            }
        }
    }

    /// path-open.ts createInlinePathOpener.open.
    fn open_inline_path(&mut self, cx: &mut Cx, ui: &mut UiContext, target: InlinePathTarget) {
        let Some(chat) = self.target.clone() else { return };
        let generation = OPEN_GENERATION.with(|g| {
            g.set(g.get() + 1);
            g.get()
        });
        let root = chat.cwd().to_owned();
        let Some((directory, file)) = links::normalize_inline_path_target(&target.path, Some(&root)) else { return };
        if let Some(file) = &file
            && target.line_start.is_none()
            && !links::is_absolute_file_path(file)
            && !root.trim().is_empty()
        {
            // A workspace folder (even one with a dot) reveals in the explorer.
            let encoded = json!({"file": file, "lineStart": target.line_start, "lineEnd": target.line_end});
            let fields = json!({"cwd": root, "path": file, "mode": "list"});
            self.with_session(cx, ui, |s, io| {
                s.host_request(io, ActionKind::ProbeFolder { generation, target: encoded }, "file_explorer_request", "file_explorer_response", fields)
            });
            return;
        }
        self.finish_open(ui, file.as_deref(), &directory, target.line_start, target.line_end);
    }

    fn finish_open(&mut self, ui: &mut UiContext, file: Option<&str>, directory: &str, line_start: Option<i64>, line_end: Option<i64>) {
        let Some(chat) = self.target.clone() else { return };
        let Some(file) = file else {
            let target = PanelTarget::new("files", &chat.host_id, &chat.workspace_id, chat.cwd()).with_data(json!({"path": directory}));
            ui.push(UiEffect::OpenTab { target, placement: TabPlacement::Explorer });
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
        let target = PanelTarget::new("file", &chat.host_id, &chat.workspace_id, chat.cwd()).with_id(file).with_title(title).with_data(data);
        let side = get(get(&self.host.preferences, "openInSidePane"), "chatFiles").as_bool() == Some(true);
        let compact = crate::common::is_compact_width(self.content_rect.size.x);
        ui.push(UiEffect::OpenTab { target, placement: if !compact && side { TabPlacement::Beside } else { TabPlacement::Main } });
    }

    // ── subagents ───────────────────────────────────────────────────────

    fn subagent_target(&self, row: &SubagentRow) -> PanelTarget {
        let chat = self.target.clone().unwrap_or_default();
        let title = row.label().unwrap_or_else(|| "Subagent".into());
        if row.paseo {
            PanelTarget::new("agent", &row.server_id, row.workspace_id.as_deref().unwrap_or(&chat.workspace_id), row.cwd.as_deref().unwrap_or(chat.cwd()))
                .with_id(row.id.clone())
                .with_title(title)
        } else {
            PanelTarget::new("provider_subagent", &chat.host_id, &chat.workspace_id, row.cwd.as_deref().unwrap_or(chat.cwd()))
                .with_id(row.id.clone())
                .with_title(title)
                .with_data(json!({"parentAgentId": row.parent_agent_id}))
        }
    }

    fn cross_workspace(&self, row: &SubagentRow) -> bool {
        let chat = self.target.clone().unwrap_or_default();
        row.paseo && (row.server_id != chat.host_id || row.workspace_id.as_deref().is_some_and(|w| w != chat.workspace_id))
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
            ui.push(UiEffect::OpenSession { host_id: row.server_id.clone(), agent_id: row.id.clone() });
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
                    agent_id: if subagent { str_of(get(&target.data, "parentAgentId")).to_owned() } else { target.id.clone().unwrap_or_default() },
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
            self.area.redraw(cx);
            return;
        }
        ui.push(UiEffect::OpenTab { target, placement: TabPlacement::Main });
    }

    fn close_modal(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if let (Some(Modal::Subagent { tab, .. }), Some(_)) = (&self.modal, &self.sheet_panel) {
            // The sheet's chat leaves the replica unless a tab shows it.
            if ui.layout.target(&tab.key()).is_none() {
                let id = tab.id.clone().unwrap_or_default();
                if tab.kind == "provider_subagent" {
                    super::store::release_subagent(cx, ui, &tab.server_id, str_of(get(&tab.data, "parentAgentId")), &id);
                } else {
                    super::store::release_session(cx, ui, &tab.server_id, &id);
                }
            }
        }
        self.dismiss_modal(cx);
    }

    /// subagents/archive-subagent.ts requestArchiveSubagent.
    fn ask_archive_subagent(&mut self, ui: &mut UiContext, row: SubagentRow) {
        let label = tracks::row_label(row.title.as_deref()).unwrap_or_else(|| "this subagent".into());
        let running = row.status.as_deref() == Some("running");
        let (title, message) = if running {
            ("Archive running subagent?", format!("{label} is still running. Archiving it will stop the subagent and remove it from the track."))
        } else {
            ("Archive subagent?", format!("Remove {label} from the track. The subagent will be archived."))
        };
        let id = overlay::confirm(ui, title, &message, "Archive", true);
        self.asks.insert(id, Ask::ArchiveSubagent(row));
    }

    fn ask_detach_subagent(&mut self, ui: &mut UiContext, row: SubagentRow) {
        let label = tracks::row_label(row.title.as_deref()).unwrap_or_else(|| "This subagent".into());
        let id = overlay::confirm(ui, "Detach subagent?", &format!("{label} will leave this track and continue as a standalone agent."), "Detach", false);
        self.asks.insert(id, Ask::DetachSubagent(row));
    }

    /// archive-finished.ts: archive idle/errored managed children, dismiss
    /// finished provider rows, n/m progress, a retry count for failures.
    fn archive_finished(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if matches!(self.archive_status, ArchiveFinished::Archiving { .. }) {
            return;
        }
        let eligible: Vec<SubagentRow> = self.subagent_rows().into_iter().filter(SubagentRow::is_finished).collect();
        if eligible.is_empty() {
            return;
        }
        self.archive_run = ArchiveRun { total: eligible.len(), queue: eligible.into_iter().rev().collect(), completed: 0, failed: 0 };
        self.archive_status = ArchiveFinished::Archiving { completed: 0, total: self.archive_run.total };
        self.archive_next(cx, ui);
    }

    fn archive_next(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        while let Some(row) = self.archive_run.queue.pop() {
            if row.paseo {
                let key = format!("{}:{}", row.server_id, row.id);
                let fields = json!({"agentId": row.id});
                let kind = ActionKind::ArchiveFinished { key: Some(key) };
                // A child on another host is archived there; its outcome still
                // counts toward this run (failures become the retry count).
                self.with_session(cx, ui, |s, io| {
                    s.request_on_host(io, &row.server_id, kind, "archive_agent_request", "agent_archived", fields)
                });
                return;
            }
            self.hidden_provider.insert(row.id.clone());
            self.archive_run.completed += 1;
            self.archive_status = ArchiveFinished::Archiving { completed: self.archive_run.completed, total: self.archive_run.total };
        }
        self.archive_status = if self.archive_run.failed > 0 {
            ArchiveFinished::Failed { failed: self.archive_run.failed, total: self.archive_run.failed }
        } else {
            ArchiveFinished::Idle
        };
        self.model_sig = None;
        self.area.redraw(cx);
    }

    // ── completions ─────────────────────────────────────────────────────

    fn on_completion(&mut self, cx: &mut Cx, ui: &mut UiContext, completion: Completion) {
        let Completion { kind, result } = completion;
        match kind {
            ActionKind::Permission { key } => {
                let state = self.row_state(&format!("perm:{key}"));
                if let Some(card) = state.borrow_mut().permission.as_mut() {
                    card.pending = false;
                    if result.is_err() {
                        card.failed = true;
                    }
                }
            }
            ActionKind::Rename | ActionKind::Detach | ActionKind::Cancel | ActionKind::Refresh => match result {
                Ok(_) => {
                    self.with_session(cx, ui, |s, io| s.reload(io));
                }
                Err(e) => self.show_failure(e.message),
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
                            ui.push(UiEffect::CloseTab { tab_key: target.tab_key.clone() });
                        }
                    }
                    Err(e) => self.show_failure(e.message),
                }
            }
            ActionKind::Fork { target } => {
                for state in self.row_states.values() {
                    state.borrow_mut().menu_busy.retain(|k| !k.starts_with("fork:"));
                }
                match result {
                    Ok(body) if !get(&body, "attachment").is_null() => {
                        let chat = self.target.clone().unwrap_or_default();
                        ui.push(UiEffect::OpenDraft {
                            host_id: chat.host_id.clone(),
                            // An empty workspace makes the draft create its own at this checkout.
                            workspace_id: if target == ForkTarget::Workspace { String::new() } else { chat.workspace_id.clone() },
                            cwd: chat.cwd().to_owned(),
                            draft_id: uuid::Uuid::new_v4().to_string(),
                            title: "Fork session".into(),
                            data: json!({
                                "config": self.config(),
                                "attachments": [get(&body, "attachment")],
                                "attachmentIntentId": uuid::Uuid::new_v4().to_string(),
                            }),
                        });
                    }
                    Ok(_) => self.show_failure(t("message.actions.forkFailed")),
                    Err(e) => self.show_failure(e.message),
                }
            }
            ActionKind::Rewind { mode, text } => {
                for state in self.row_states.values() {
                    state.borrow_mut().menu_busy.remove("rewind");
                }
                match result {
                    Ok(_) if mode != "files" => {
                        self.with_session(cx, ui, |s, io| s.refresh_tail(io));
                        if let Some(chat) = self.target.clone() {
                            bridge::restore_text_if_empty(cx, &chat.host_id, &chat.workspace_id, &chat.agent_id, &text);
                        }
                        self.jump_to_latest(cx);
                    }
                    Ok(_) => {}
                    Err(e) => self.show_failure(e.message),
                }
            }
            ActionKind::ListPrompts { request } => {
                if let Ok(body) = result
                    && request == self.prompts_request
                    && optional(get(&body, "epoch")) == self.snap.as_ref().and_then(|s| s.epoch.clone())
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
                            MenuItem::new(seq.to_string(), str_of(get(p, "preview"))).checked(Some(seq) == self.active_prompt).entry()
                        })
                        .collect();
                    let anchor = Rect { pos: self.stream_rect.pos + dvec2(self.stream_rect.size.x * 0.5 - 160.0, 8.0), size: dvec2(320.0, 1.0) };
                    let id = overlay::open_menu_with(ui, anchor, Some(t("settings.appearance.chatOutline.title")), entries, MenuOptions { width: 320.0, ..Default::default() });
                    self.asks.insert(id, Ask::Outline);
                }
                Err(e) => self.show_failure(e.message),
            },
            ActionKind::Search { token } => {
                let (epoch, rows) = self.snap.as_ref().map(|s| (s.epoch.clone(), s.timeline.clone())).unwrap_or_default();
                self.find.on_search(token, result.map_err(|e| e.message), epoch.as_deref(), &rows);
                self.pump_find(cx, ui);
            }
            ActionKind::ClearAttention => {}
            ActionKind::ArchiveSubagent { key } => {
                if let Err(e) = result {
                    self.pending_archive.remove(&key);
                    self.show_failure(e.message);
                }
                self.model_sig = None;
            }
            ActionKind::DetachSubagent { host_id, agent_id } => match result {
                Ok(_) => ui.push(UiEffect::OpenSession { host_id, agent_id }),
                Err(e) => self.show_failure(e.message),
            },
            ActionKind::ArchiveFinished { key } => {
                match result {
                    Ok(_) => {
                        if let Some(key) = key {
                            self.pending_archive.insert(key);
                        }
                    }
                    Err(_) => self.archive_run.failed += 1,
                }
                self.archive_run.completed += 1;
                self.archive_status = ArchiveFinished::Archiving { completed: self.archive_run.completed, total: self.archive_run.total };
                self.archive_next(cx, ui);
            }
            ActionKind::LookupFile { token, target } => {
                let root = self.target.as_ref().map(|t| t.cwd().to_owned()).unwrap_or_default();
                let found = result.ok().filter(|r| get(r, "error").is_null() || str_of(get(r, "error")).is_empty()).and_then(|r| {
                    let entry = list(get(&r, "entries")).iter().find(|e| get(e, "kind") == "file")?.clone();
                    let base = root.replace('\\', "/");
                    let base = base.trim_end_matches('/');
                    let child = str_of(get(&entry, "path")).replace('\\', "/");
                    let child = child.trim_start_matches('/');
                    Some(if base.is_empty() { child.to_owned() } else { format!("{base}/{child}") })
                });
                match found {
                    Some(path) => {
                        let lookup = InlinePathTarget {
                            raw: str_of(get(&target, "raw")).to_owned(),
                            path: str_of(get(&target, "path")).to_owned(),
                            line_start: get(&target, "lineStart").as_i64(),
                            line_end: get(&target, "lineEnd").as_i64(),
                        };
                        self.open_inline_path(cx, ui, lookup.with_path(path));
                    }
                    None => self.show_failure(ta("common.errors.noFileFound", &[("token", &token)])),
                }
            }
            ActionKind::ProbeFolder { generation, target } => {
                if OPEN_GENERATION.with(Cell::get) != generation {
                    return;
                }
                let file = str_of(get(&target, "file")).to_owned();
                let directory = matches!(&result, Ok(r) if get(r, "error").is_null() && !get(r, "directory").is_null());
                if directory {
                    self.finish_open(ui, None, &file, None, None);
                } else {
                    self.finish_open(ui, Some(&file), "", get(&target, "lineStart").as_i64(), get(&target, "lineEnd").as_i64());
                }
            }
        }
        if self.visible {
            self.area.redraw(cx);
        }
    }

    fn on_reveal(&mut self, cx: &mut Cx, ui: &mut UiContext, token: u64, ok: bool) {
        let Some(purpose) = self.reveals.remove(&token) else { return };
        match purpose {
            RevealFor::Find(find_token) => {
                let (epoch, rows) = self.target.as_ref().and_then(|target| with_store(|store| {
                    store.sessions.get(&target.key()).map(|s| (s.epoch.clone(), s.timeline.clone()))
                })).unwrap_or_default();
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
                        let row = self.target.as_ref().and_then(|target| with_store(|store| {
                            store.sessions.get(&target.key()).and_then(|session| session.timeline.iter()
                                .find(|r| get(get(r, "item"), "type") == "user_message"
                                    && int_of(get(r, "seqStart")) <= seq
                                    && int_or(get(r, "seqEnd"), int_of(get(r, "seqStart"))) >= seq)
                                .map(|r| int_of(get(r, "seqStart"))))
                        }));
                        if let Some(row) = row {
                            self.pending_jump = Some((row, 0.0, false));
                        }
                    }
                } else {
                    self.show_failure(t("agentStream.historyLoadFailed"));
                }
            }
        }
    }

    /// Overlay answers (menus, confirms, prompts) for this panel.
    fn on_overlay(&mut self, cx: &mut Cx, ui: &mut UiContext, id: u64, args: &Value) {
        let Some(ask) = self.asks.remove(&id) else { return };
        let cancelled = is_true(get(args, "cancelled"));
        match ask {
            Ask::Rename => {
                if cancelled {
                    return;
                }
                let Some(name) = get(args, "text").as_str() else { return };
                let name = name.trim();
                if name.is_empty() || name.chars().count() > 200 {
                    self.show_failure("Session title must contain 1–200 characters.");
                    return;
                }
                let name = name.to_owned();
                self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Rename, "update_agent_request", "update_agent_response", json!({"name": name})));
            }
            Ask::Confirm(action) => {
                if !is_true(get(args, "confirmed")) {
                    return;
                }
                if action == "archive" {
                    // The archiving overlay covers the panel until the tab closes.
                    self.archiving = true;
                    self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Archive, "archive_agent_request", "agent_archived", json!({})));
                } else {
                    self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Delete, "delete_agent_request", "agent_deleted", json!({})));
                }
            }
            Ask::ArchiveSubagent(row) => {
                if !is_true(get(args, "confirmed")) {
                    return;
                }
                let key = format!("{}:{}", row.server_id, row.id);
                self.pending_archive.insert(key.clone());
                self.model_sig = None;
                let fields = json!({"agentId": row.id});
                self.with_session(cx, ui, |s, io| {
                    s.request_on_host(io, &row.server_id, ActionKind::ArchiveSubagent { key }, "archive_agent_request", "agent_archived", fields)
                });
            }
            Ask::DetachSubagent(row) => {
                if !is_true(get(args, "confirmed")) {
                    return;
                }
                let child_connected = host_row(&ui.state, &row.server_id).is_some_and(|h| get(h, "status") == "connected");
                if !child_connected {
                    self.show_failure(t("workspaceSetup.errors.hostDisconnected"));
                    return;
                }
                let agent_id = row.id.clone();
                let kind = ActionKind::DetachSubagent { host_id: row.server_id.clone(), agent_id: agent_id.clone() };
                self.with_session(cx, ui, |s, io| {
                    s.request_on_host(io, &row.server_id, kind, "agent.detach.request", "agent.detach.response", json!({"agentId": agent_id}))
                });
            }
            Ask::Fork { row, entry } => {
                if let Some(state) = self.row_states.get(&row) {
                    let mut state = state.borrow_mut();
                    state.menu_open.retain(|k| !k.starts_with("fork:"));
                }
                let Some(value) = get(args, "value").as_str() else { return };
                let target = if value == "workspace" { ForkTarget::Workspace } else { ForkTarget::Tab };
                if let Some(state) = self.row_states.get(&row) {
                    let key = if entry.is_some() { format!("fork:{row}") } else { "fork:live".into() };
                    state.borrow_mut().menu_busy.insert(key);
                }
                self.fork(cx, ui, entry, target);
            }
            Ask::Rewind { row, entry } => {
                if let Some(state) = self.row_states.get(&row) {
                    state.borrow_mut().menu_open.remove("rewind");
                }
                let Some(mode) = get(args, "value").as_str().map(str::to_owned) else { return };
                if let Some(state) = self.row_states.get(&row) {
                    state.borrow_mut().menu_busy.insert("rewind".into());
                }
                let item = get(&entry, "item");
                let text = str_of(get(item, "text")).to_owned();
                let message = get(item, "messageId").clone();
                self.with_session(cx, ui, |s, io| {
                    s.action(io, ActionKind::Rewind { mode: mode.clone(), text }, "agent.rewind.request", "agent.rewind.response", json!({"messageId": message, "mode": mode}))
                });
            }
            Ask::Outline => {
                if let Some(seq) = get(args, "value").as_str().and_then(|v| v.parse::<i64>().ok()) {
                    self.jump_to_prompt(cx, ui, seq);
                }
            }
        }
        self.area.redraw(cx);
    }

    // ── row and chrome taps ─────────────────────────────────────────────

    fn on_row_event(&mut self, cx: &mut Cx, ui: &mut UiContext, event: ChatRowEvent) {
        let row = event.row;
        match event.kind {
            RowEventKind::SelectionScroll { delta } => {
                if self.modal.is_none()
                    && self.items.iter().any(|item| item.id == row)
                    && self.row_states.get(&row).is_some_and(|state| state.borrow().selection.is_some())
                {
                    if delta != 0.0 {
                        self.reader_scrolled = true;
                        self.at_bottom = false;
                        self.detach();
                    }
                    self.list.as_portal_list().scroll_selection(cx, delta);
                }
            }
            RowEventKind::Answer { key, dismiss } => {
                let Some(snap) = &self.snap else { return };
                let Some((_, request)) = snap.permissions.iter().find(|(k, _)| *k == key).cloned() else { return };
                let Some(qs) = permission::questions(&request) else { return };
                let state = self.row_state(&row).borrow().permission.clone().unwrap_or_default();
                let response = permission::question_response(&request, &qs, &state, dismiss);
                self.respond_permission(cx, ui, &key, if dismiss { "dismiss" } else { "submit" }, response);
            }
            RowEventKind::Tap { target, rect } => match target {
                HitTarget::Link { href, kind, text } => self.open_link(cx, ui, &href, &text, kind),
                HitTarget::OpenFile(path) => {
                    let target = InlinePathTarget { raw: path.clone(), path, line_start: None, line_end: None };
                    self.open_inline_path(cx, ui, target);
                }
                HitTarget::Image { key } => {
                    self.modal = Some(Modal::Lightbox { source: LightboxSource::Assistant { url: key, row }, scale: 1.0, offset: DVec2::default() });
                    self.modal_state = RowState::default();
                }
                HitTarget::UserImage { index } => {
                    if let Some(entry) = self.entry_of(&row)
                        && let Some(image) = list(get(get(&entry, "item"), "images")).get(index)
                    {
                        self.modal = Some(Modal::Lightbox { source: LightboxSource::User(image.clone()), scale: 1.0, offset: DVec2::default() });
                        self.modal_state = RowState::default();
                    }
                }
                HitTarget::OpenSheet(sheet) => {
                    let (kind, _) = sheet.split_once(':').unwrap_or((sheet.as_str(), ""));
                    self.modal = Some(Modal::Sheet { row: row.clone(), kind: kind.to_owned(), opened: now_seconds() });
                    self.modal_state = RowState::default();
                }
                HitTarget::RewindMenu => {
                    let Some(entry) = self.entry_of(&row) else { return };
                    let capabilities = self.snap.as_ref().map(|s| get(&s.agent, "capabilities").clone()).unwrap_or_default();
                    let mut entries = vec![MenuEntry::Label(t("rewind.warning"))];
                    for mode in rows::rewind_modes(&capabilities) {
                        let icon = match mode {
                            "conversation" => "message-square",
                            "files" => "file-text",
                            _ => "layers",
                        };
                        entries.push(MenuItem::new(mode, t(&format!("rewind.actions.{mode}"))).icon(icon).entry());
                    }
                    if let Some(state) = self.row_states.get(&row) {
                        state.borrow_mut().menu_open.insert("rewind".into());
                    }
                    let id = overlay::open_menu_with(ui, rect, None, entries, MenuOptions { align_end: true, ..Default::default() });
                    self.asks.insert(id, Ask::Rewind { row, entry });
                }
                HitTarget::ForkMenu { completed } => {
                    let entry = if completed { self.entry_of(&row) } else { None };
                    let entries = vec![
                        MenuItem::new("tab", t("message.actions.forkInNewTab")).icon("split").entry(),
                        MenuItem::new("workspace", t("message.actions.forkInNewWorkspace")).icon("split").entry(),
                    ];
                    if let Some(state) = self.row_states.get(&row) {
                        state.borrow_mut().menu_open.insert(if completed { format!("fork:{row}") } else { "fork:live".into() });
                    }
                    let id = overlay::open_menu_with(ui, rect, None, entries, MenuOptions { sheet: false, ..Default::default() });
                    self.asks.insert(id, Ask::Fork { row, entry });
                }
                HitTarget::PermissionAction { key, action } => {
                    let Some(snap) = &self.snap else { return };
                    let Some((_, request)) = snap.permissions.iter().find(|(k, _)| *k == key).cloned() else { return };
                    let Some(choice) = permission::actions(&request).into_iter().find(|a| str_of(get(a, "id")) == action) else { return };
                    let response = permission::action_response(&choice);
                    self.respond_permission(cx, ui, &key, &action, response);
                }
                _ => {}
            },
        }
        self.area.redraw(cx);
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
                self.with_session(cx, ui, |s, io| s.action(io, ActionKind::Unarchive, "refresh_agent_request", "status", json!({})));
            }
            "jump-latest" => self.jump_to_latest(cx),
            "track:tasks" => self.open_track(cx, ui, TrackPanel::Tasks),
            "track:subagents" => self.open_track(cx, ui, TrackPanel::Subagents),
            "track:diff" => {
                if let Some(chat) = self.target.clone() {
                    ui.push(UiEffect::OpenTab { target: PanelTarget::new("changes_tree", &chat.host_id, &chat.workspace_id, chat.cwd()), placement: TabPlacement::Main });
                }
            }
            "track-close" => self.close_modal(cx, ui),
            "archive-finished" => self.archive_finished(cx, ui),
            "find:next" => {
                let (epoch, rows) = self.snap.as_ref().map(|s| (s.epoch.clone(), s.timeline.clone())).unwrap_or_default();
                self.find.next(epoch.as_deref(), &rows);
                self.pump_find(cx, ui);
            }
            "find:previous" => {
                let (epoch, rows) = self.snap.as_ref().map(|s| (s.epoch.clone(), s.timeline.clone())).unwrap_or_default();
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
                    ui.push(UiEffect::OpenTab { target: tab, placement: TabPlacement::Main });
                }
            }
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
                } else if let Some(key) = other.strip_prefix("subagent-open:") {
                    if let Some(row) = self.subagent_rows().into_iter().find(|r| r.key() == key) {
                        if !self.opens_in_sheet(ui, &row) {
                            self.close_modal(cx, ui);
                        } else {
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
        self.area.redraw(cx);
    }

    fn open_track(&mut self, cx: &mut Cx, ui: &mut UiContext, panel: TrackPanel) {
        self.modal = Some(Modal::Track(panel));
        self.modal_state = RowState::default();
        if panel == TrackPanel::Subagents {
            self.ensure_providers(cx, ui);
        }
    }

    fn ensure_providers(&mut self, _cx: &mut Cx, ui: &mut UiContext) {
        if self.providers_requested || self.providers_request.is_some() {
            return;
        }
        let Some(target) = self.target.as_ref() else { return };
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
        let agent = self.host.agent_row.clone().or_else(|| self.snap.as_ref().map(|s| (*s.agent).clone())).unwrap_or_default();
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
        let cleared = self.attention.as_mut().map(|a| std::mem::take(&mut a.cleared)).unwrap_or_default();
        for agent_id in cleared {
            // daemon-client clearAgentAttention; failures are ignored.
            self.with_session(cx, ui, |s, io| s.host_request(io, ActionKind::ClearAttention, "clear_agent_attention", "clear_agent_attention_response", json!({"agentId": agent_id})));
        }
    }

    // ── composer ────────────────────────────────────────────────────────

    fn composer_shown(&self) -> bool {
        let Some(target) = &self.target else { return false };
        let archived = self.snap.as_ref().is_some_and(|s| s.archived);
        !target.is_subagent() && !target.sheet && !archived && !self.archiving
    }

    fn sync_composer(&mut self, cx: &mut Cx) {
        let Some(target) = self.target.clone() else { return };
        let next = bridge::target(
            &target.host_id,
            &target.workspace_id,
            target.cwd(),
            target.shown_id(),
            target.is_subagent(),
            &target.tab_key,
            if target.sheet { json!({"paneHost": "sheet"}) } else { json!({}) },
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
            composer.set_agent(cx, bridge::snapshot(snap.agent.clone(), snap.running, snap.authorized, pending));
        }
    }
}

impl Widget for ChatPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.target.is_none() {
            return;
        }
        let composer_visible = self.visible && self.composer_shown();
        if self.visible && self.next_frame.is_event(event).is_some() {
            self.area.redraw(cx);
        }
        let modal_open = self.modal.is_some();
        let modal_input = crate::common::is_user_input(event);
        if matches!(event, Event::TextHandle(_) | Event::Pause | Event::AppLostFocus)
            || matches!(event, Event::TouchUpdate(update) if update.cancelled)
        {
            self.pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            self.lightbox_drag = None;
        }
        if modal_input && scope.data.get::<UiContext>()
            .is_some_and(|ui| !self.modal_branch_accepts_input(&ui.window_id))
        {
            self.pressed.cancel();
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            return;
        }
        // Composer sheets are painted above the stream. Dispatch them before
        // any row can take the pointer/key focus, and retain ownership on close.
        if self.visible && !modal_open && self.composer_shown() && self.composer.as_composer_panel().captures_input() && modal_input {
            self.composer.handle_event(cx, event, scope);
            if let Some(ui) = scope.data.get::<UiContext>() {
                self.sync_window_modal(ui);
            }
            return;
        }
        // Snapshot descendants before dispatch. A child closing itself still owns
        // the event; an ancestor claims dismissal only when no descendant needs it.
        let child = matches!(self.modal, Some(Modal::Subagent { .. })).then(|| self.sheet_panel.clone()).flatten();
        let child_owns_input = child.as_ref().is_some_and(|panel| {
            panel.as_chat_panel().borrow().is_some_and(|p| {
                let composer = p.composer.as_composer_panel();
                p.modal.is_some() || composer.captures_input() || composer.consumes_escape(cx, event)
            })
        });
        if modal_open && !child_owns_input
            && matches!(event, Event::BackPressed { .. } | Event::KeyDown(KeyEvent { key_code: KeyCode::Escape, .. }))
        {
            if overlay::take_dismissal(event)
                && let Some(ui) = scope.data.get_mut::<UiContext>()
            {
                self.close_modal(cx, ui);
                self.sync_window_modal(ui);
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
        // Wheel over a row's inner scroll region (code panes, capped details)
        // moves that region instead of the stream.
        let mut swallow_scroll = false;
        if let Event::Scroll(se) = event {
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
            self.pressed.cancel();
            if modal_open {
                swallow_scroll = true;
                self.modal_scroll(cx, se.abs, se.scroll);
            } else if self.stream_rect.contains(se.abs) {
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
                Event::MouseDown(e) if self.stream_rect.contains(e.abs)
                    && e.abs.x >= self.stream_rect.pos.x + self.stream_rect.size.x - 16.0 => {
                    // Focus only the scrollbar gutter, not selectable rows or
                    // their inputs, so Home/End navigate the list afterwards.
                    cx.set_key_focus(self.list.area());
                    true
                }
                Event::KeyDown(e) => cx.has_key_focus(self.list.area()) && matches!(
                    e.key_code,
                    KeyCode::Home | KeyCode::End | KeyCode::PageUp | KeyCode::PageDown | KeyCode::ArrowUp | KeyCode::ArrowDown
                ),
                _ => false,
            };
            if reader_input {
                self.reader_scrolled = true;
                self.at_bottom = false;
                self.detach();
            }
        }
        // Chrome over the stream and modal surfaces claim pointer events
        // only where they have a target (or everywhere while a modal shows).
        let overlay_rects: Vec<Rect> = self.overlay_canvas.hits.iter().map(|(r, _)| r.translate(self.overlay_canvas.origin)).collect();
        let modal_block = match &self.modal {
            None => None,
            Some(Modal::Subagent { .. }) => Some(Some(self.sheet_content)),
            Some(_) => Some(None),
        };
        let panel_rect = if modal_open { self.modal_rect } else { self.content_rect };
        let area = if modal_open { self.modal_area } else { self.area };
        let hit = event.hits_with_test(cx, area, move |abs, _, _| match modal_block {
            Some(Some(content)) => panel_rect.contains(abs) && !content.contains(abs),
            Some(None) => panel_rect.contains(abs),
            None => overlay_rects.iter().any(|r| r.contains(abs)),
        });
        if !modal_open || matches!(self.modal, Some(Modal::Lightbox { .. })) {
            self.modal_touch_scroll.reset();
            self.modal_horizontal_scroll.reset();
        } else {
            let dy = self.modal_touch_scroll.handle_event(cx, event, &hit);
            let dx = self.modal_horizontal_scroll.handle_axis_event(cx, event, &hit, Vec2Index::X);
            let origin = self.modal_touch_scroll.origin();
            if let Some(delta) = dy {
                self.pressed.cancel();
                if !self.modal_scroll(cx, origin, dvec2(0.0, delta)) {
                    self.modal_touch_scroll.stop_at_edge();
                }
            }
            if let Some(delta) = dx {
                self.pressed.cancel();
                if !self.modal_scroll(cx, origin, dvec2(delta, 0.0)) {
                    self.modal_horizontal_scroll.stop_at_edge();
                }
            }
        }
        let mut chrome_tap: Option<String> = None;
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => self.chrome_hover(cx, Some(e.abs)),
            Hit::FingerHoverOut(_) => self.chrome_hover(cx, None),
            Hit::FingerDown(e) => {
                let target = self.chrome_hit(e.abs);
                if let Some(Modal::Lightbox { .. }) = &self.modal
                    && (target.is_none() || matches!(&target, Some(HitTarget::Chrome(id)) if id == "lightbox:image"))
                {
                    self.lightbox_drag = Some(e.abs);
                }
                let target = self.modal_tap_target(e.abs);
                self.pressed.press(target);
            }
            Hit::FingerMove(e) => {
                if let (Some(last), Some(Modal::Lightbox { offset, scale, .. })) = (self.lightbox_drag, &mut self.modal)
                    && *scale > 1.0
                {
                    *offset += e.abs - last;
                    self.lightbox_drag = Some(e.abs);
                    self.area.redraw(cx);
                }
            }
            Hit::FingerLongPress(_) => self.pressed.cancel(),
            Hit::FingerUp(e) => {
                let dragged = self.lightbox_drag.take().is_some_and(|start| (e.abs - e.abs_start).length() > 4.0 && start != e.abs_start);
                let target = self.modal_tap_target(e.abs);
                let valid = e.is_over && !dragged && !e.has_long_press_occurred && (!e.device.is_touch() || e.was_tap());
                if let Some(target) = self.pressed.release(target, valid) {
                    if let HitTarget::Chrome(id) = target {
                        if id == "outline"
                            && let Some(RailEvent::Jump(seq)) = self.rail.tap(e.abs)
                        {
                            chrome_tap = Some(format!("outline:{seq}"));
                        }
                        if id != "outline" && id != "track-panel" && id != "lightbox:image" && id != "sheet:body" {
                            chrome_tap = Some(id);
                        }
                    } else if let HitTarget::Copy { key, text } = target {
                        cx.copy_to_clipboard(&text);
                        self.modal_state.copied.insert(key, now_seconds());
                    } else if let HitTarget::Toggle(key) = target {
                        self.modal_state.toggle(&key);
                    }
                    self.area.redraw(cx);
                }
            }
            Hit::FingerScroll(e) => {
                if let Some(Modal::Lightbox { .. }) = &self.modal {
                    self.zoom(if e.scroll.y < 0.0 { 1.1 } else { 1.0 / 1.1 });
                    self.area.redraw(cx);
                }
            }
            _ => {}
        }
        // Children: the stream, the composer and the find field.
        let block_pointer = modal_open && modal_input;
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
        let effects_before = scope.data.get::<UiContext>().map(|ui| ui.effects.len()).unwrap_or(0);
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
        if self.find_open && !block_pointer {
            self.find_field.handle_event(cx, event, scope);
        }
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return };
        // composer/index.tsx handleFocusChange → onAttentionInputFocus.
        let input_focused = ui.effects[effects_before.min(ui.effects.len())..]
            .iter()
            .any(|e| matches!(e, UiEffect::SetFocusScope { scope: Some(s), .. } if s == "message-input"));
        self.ui_pass(cx, event, ui, chrome_tap, input_focused);
        self.sync_window_modal(ui);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        let rect = cx.turtle().rect();
        self.content_rect = rect;
        if let Some(ui) = scope.data.get::<UiContext>() {
            self.refresh_host(ui);
        }
        let changed = self.refresh_snapshot();
        if self.visible && (changed || self.snapshot_stale)
            && let Some(ui) = scope.data.get_mut::<UiContext>()
        {
            self.snapshot_stale = false;
            self.on_session_changed(cx, ui);
        }
        let look = self.look(rect.size.x);
        let p = look.palette;
        self.kit.begin_layer(cx);
        self.kit.rect(cx, rect, p.surface0, 0.0);
        let mut animating = false;
        let target = self.target.clone().unwrap_or_default();
        let non_ready = self.non_ready();
        if let Some(view) = non_ready {
            self.overlay_list.begin_always(cx);
            self.kit.begin_layer(cx);
            self.overlay_canvas.begin(rect.pos);
            let mut d = Draw { kit: &mut self.kit, canvas: &mut self.overlay_canvas, look, state: &mut self.chrome_state, now: now_seconds(), area: self.area, animating: false };
            chrome::non_ready(cx, &mut d, &view, rect);
            animating |= d.animating;
            self.overlay_list.end(cx);
            cx.end_turtle_with_area(&mut self.area);
            if animating {
                self.next_frame = cx.new_next_frame();
            }
            return DrawStep::done();
        }
        let compact = look.compact;
        let track_height = tracks::pill_height(cx, &mut self.kit, look);
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
        let snap = self.snap.clone();
        let core = scope.data.get::<UiContext>().map(|ui| ui.core.clone()).or_else(|| self.core.clone());
        if let Some(core) = &core {
            self.ctx = Some(self.row_context(look, core));
        }
        let subagent = target.is_subagent();
        let archived = snap.as_ref().is_some_and(|s| s.archived);
        let connected = self.host.connected;

        // ── geometry ────────────────────────────────────────────────────
        let facts = if target.sheet && !subagent { snap.as_ref().and_then(|s| tracks::facts_line(&self.providers, &s.agent)) } else { None };
        let mut callouts = Vec::new();
        if let Some(s) = &snap {
            if connected && !archived && matches!(s.sync, SyncStatus::Error | SyncStatus::Retrying) {
                callouts.push(Callout::SyncError { retrying: s.sync == SyncStatus::Retrying });
            }
            if !subagent && !archived && s.execution != ExecutionStatus::Ready {
                callouts.push(Callout::Preparation { subagent: false, failed: s.execution == ExecutionStatus::Failed, error: s.preparation_error.clone(), retrying: s.preparation_retrying });
            }
            if subagent && (s.child_refreshing || s.child_refresh_error.is_some()) {
                callouts.push(Callout::Preparation { subagent: true, failed: s.child_refresh_error.is_some(), error: s.child_refresh_error.clone(), retrying: s.preparation_retrying });
            }
        }
        let show_archived_callout = !subagent && !target.sheet && archived;
        let (facts_h, callouts_h, archived_h) = {
            let mut d = Draw { kit: &mut self.kit, canvas: &mut self.overlay_canvas, look, state: &mut self.chrome_state, now: now_seconds(), area: self.area, animating: false };
            d.measure(|d| {
                let facts_h = facts.as_ref().map(|f| chrome::pane_subtitle(cx, d, f, rect.pos.x, rect.pos.y, rect.size.x)).unwrap_or(0.0);
                let callouts_h: f64 = callouts.iter().map(|c| chrome::callout(cx, d, c, rect.pos.x, 0.0, rect.size.x)).sum();
                let archived_h = if show_archived_callout {
                    chrome::archived_callout(cx, d, connected, self.unarchiving, self.unarchive_error.as_deref(), rect.pos.x, 0.0, rect.size.x)
                } else {
                    0.0
                };
                (facts_h, callouts_h, archived_h)
            })
        };
        // ── the composer (drawn first so the stream gets its exact height) ─
        // It rests on the panel bottom at last frame's height; a height change
        // shifts what it just drew by the difference in the same frame.
        if self.composer_shown() {
            self.sync_composer(cx);
            let bottom = rect.pos.y + rect.size.y;
            let walk = Walk { abs_pos: Some(dvec2(rect.pos.x, bottom - self.composer_h)), margin: Margin::default(), width: Size::Fixed(rect.size.x), height: Size::Fit };
            cx.begin_turtle(walk, Layout::flow_down());
            let _ = self.composer.draw_walk(cx, scope, Walk { abs_pos: None, margin: Margin::default(), width: Size::Fill, height: Size::Fit });
            let range = cx.get_turtle_align_range();
            let drawn = cx.end_turtle().size.y;
            if drawn.is_finite() && (drawn - self.composer_h).abs() > 0.5 {
                cx.shift_align_range(&range, dvec2(0.0, self.composer_h - drawn));
                self.composer_h = drawn;
            }
        }
        let composer_h = if self.composer_shown() { self.composer_h } else { 0.0 };
        let stream_h = (rect.size.y - facts_h - callouts_h - archived_h - composer_h).max(0.0);
        let stream_rect = rect_xywh(rect.pos.x, rect.pos.y + facts_h, rect.size.x, stream_h);
        self.stream_rect = stream_rect;
        let empty = self.stream_empty();
        let loading = snap.as_ref().is_some_and(|s| s.loading);

        // ── the stream ──────────────────────────────────────────────────
        if !empty || loading {
            let walk = Walk { abs_pos: Some(stream_rect.pos), margin: Margin::default(), width: Size::Fixed(stream_rect.size.x), height: Size::Fixed(stream_rect.size.y.max(1.0)) };
            let list_ref = self.list.clone();
            let portal = list_ref.as_portal_list();
            if self.sticky {
                // view.tsx scrollToBottom: a sticky stream rests on its newest
                // edge (the last item drawn first, then the list scans up and
                // aligns its bottom), however its rows grow.
                portal.set_first_id_and_scroll(self.items.len().saturating_sub(1), 0.0);
                portal.set_tail_range(true);
            }
            while list_ref.draw_walk(cx, scope, walk).is_step() {
                let Some(mut list) = portal.borrow_mut() else { break };
                list.set_item_range(cx, 0, self.items.len());
                list.begin_measurement();
                while let Some(index) = list.next_visible_item(cx) {
                    let Some(item) = self.items.get(index).cloned() else { continue };
                    let widget = list.item(cx, index, live_id!(Row));
                    if let Some(ctx) = &self.ctx {
                        let state = self.row_states.entry(item.id.clone()).or_default().clone();
                        widget.as_chat_row().set_model(cx, RowModel { panel: self.uid, item, ctx: ctx.clone(), state });
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
            if self.reader_scrolled {
                // Only layout can determine whether the reader reached the
                // newest edge. Scroll actions arrive before that layout.
                let at_end = portal.is_at_end();
                self.sticky = at_end;
                self.at_bottom = at_end;
                portal.set_tail_range(at_end);
            }
        }

        let dock_top = stream_rect.pos.y + stream_rect.size.y;

        // ── chrome over the stream ──────────────────────────────────────
        self.overlay_list.begin_always(cx);
        self.kit.begin_layer(cx);
        self.overlay_canvas.begin(rect.pos);
        let tracks_visible = self.tracks_visible();
        let rows = if tracks_visible { self.subagent_rows() } else { Vec::new() };
        let tasks = if tracks_visible && !subagent { self.latest_tasks() } else { Vec::new() };
        let (additions, deletions) = if subagent { (0, 0) } else { (self.host.additions, self.host.deletions) };
        let toast = self.panel_toast();
        let prompts_active = self.active_prompt;
        let find_open = self.find_open;
        let find_status = self.find_status();
        let find_failure = self.find.failure;
        let find_can_navigate = self.find.phase == FindPhase::Ready && self.find.count > 0;
        let history_overlay = snap.as_ref().is_some_and(|s| {
            connected && !archived && s.sync == SyncStatus::Pending && !s.hydrated && !s.initial_sync_failed && !s.create_handoff
        });
        let outline_shown = !subagent && self.outline_enabled();
        let host_labels: HashMap<String, String> = self.host.labels.clone();
        let host_label_fn = move |id: &str| host_labels.get(id).cloned().unwrap_or_else(|| id.to_owned());
        let can_detach = self.feature("agentDetach");
        let archive_status = self.archive_status;
        let open_track = match &self.modal {
            Some(Modal::Track(panel)) => Some(*panel),
            _ => None,
        };
        let jump_visible = !self.at_bottom && !empty;
        let mut find_field_rect = None;
        let mut track_anchors = Vec::new();
        {
            let mut d = Draw { kit: &mut self.kit, canvas: &mut self.overlay_canvas, look, state: &mut self.chrome_state, now: now_seconds(), area: self.area, animating: false };
            if let Some(f) = &facts {
                chrome::pane_subtitle(cx, &mut d, f, rect.pos.x, rect.pos.y, rect.size.x);
            }
            if empty && !loading {
                chrome::empty_state(cx, &mut d, stream_rect.pos.x, stream_rect.pos.y + stream_rect.size.y, stream_rect.size.x);
            }
            if outline_shown && self.rail.draw(cx, &mut d, stream_rect, prompts_active) {
                d.animating = true;
            }
            if tracks_visible {
                let model = TrackModel {
                    tasks: &tasks,
                    rows: &rows,
                    archive: archive_status,
                    additions,
                    deletions,
                    can_archive_finished: !subagent,
                    can_detach,
                    host_id: &target.host_id,
                    providers: &self.providers,
                    host_label: &host_label_fn,
                };
                track_anchors = tracks::draw_bar(cx, &mut d, &mut self.painter, &model, open_track, stream_rect.pos.x, stream_rect.pos.y + stream_rect.size.y, stream_rect.size.x, track_height);
            }
            if jump_visible {
                let clearance = if tracks_visible { tracks::control_clearance(compact, track_height) } else { 0.0 };
                let bottom = stream_rect.pos.y + stream_rect.size.y - clearance.max(16.0);
                chrome::jump_button(cx, &mut d, stream_rect.center().x, bottom);
            }
            if find_open {
                let (field, card_h) = find_card(cx, &mut d, stream_rect, &find_status, find_can_navigate, find_failure);
                find_field_rect = Some(field);
                self.find_card_h = card_h;
            }
            let mut cy = dock_top;
            for callout in &callouts {
                cy += chrome::callout(cx, &mut d, callout, rect.pos.x, cy, rect.size.x);
            }
            if show_archived_callout {
                chrome::archived_callout(cx, &mut d, connected, self.unarchiving, self.unarchive_error.as_deref(), rect.pos.x, cy, rect.size.x);
            }
            if history_overlay {
                chrome::history_overlay(cx, &mut d, rect);
            }
            if let Some(toast) = &toast {
                chrome::toast(cx, &mut d, toast, rect);
            }
            if self.archiving {
                chrome::archiving_overlay(cx, &mut d, rect);
            }
            animating |= d.animating;
        }
        self.overlay_list.end(cx);
        self.track_anchors = track_anchors;
        self.find_field_rect = find_field_rect;

        // ── the find field ──────────────────────────────────────────────
        if let Some(field) = find_field_rect {
            self.find_list.begin_always(cx);
            let walk = Walk { abs_pos: Some(field.pos), margin: Margin::default(), width: Size::Fixed(field.size.x), height: Size::Fixed(field.size.y) };
            let _ = self.find_field.draw_walk(cx, scope, walk);
            self.find_list.end(cx);
            if self.find_focus_seen != self.find.focus_requests {
                self.find_focus_seen = self.find.focus_requests;
                let field = self.find_field.as_ptext_field();
                field.set_key_focus(cx);
                field.select_all(cx);
            }
        }

        // ── modal surfaces ──────────────────────────────────────────────
        if self.modal.is_some() {
            self.modal_list.begin_overlay_last(cx);
            // Nested sheets escape their parent's content clip, and root
            // subagent sheets cover the shell header/sidebar as Flutter does.
            cx.begin_pass_sized_turtle(Layout::flow_overlay());
            let full_window = matches!(self.modal, Some(Modal::Subagent { .. }))
                || self.target.as_ref().is_some_and(|t| t.sheet);
            let bounds = if full_window { Rect { pos: DVec2::default(), size: cx.current_pass_size() } } else { rect };
            self.modal_rect = bounds;
            cx.begin_turtle(Walk::abs_rect(bounds), Layout::default());
            self.modal_kit.begin_layer(cx);
            self.modal_canvas.begin(bounds.pos);
            animating |= self.draw_modal(cx, scope, look, bounds, &rows, &tasks, &host_label_fn);
            cx.end_turtle_with_area(&mut self.modal_area);
            cx.end_pass_sized_turtle();
            self.modal_list.end(cx);
        } else {
            self.modal_canvas.begin(rect.pos);
        }
        cx.end_turtle_with_area(&mut self.area);

        // ── after layout ────────────────────────────────────────────────
        self.after_draw(cx);
        // A reply must be rebuilt and laid out before deciding whether the
        // reader is still at history start. Old indices can queue every page.
        if let Some(ui) = scope.data.get_mut::<UiContext>()
            && self.wants_older()
        {
            self.load_older(cx, ui);
        }
        if let Some(target) = &self.target {
            MENUS.with(|m| {
                m.borrow_mut().insert(
                    (target.host_id.clone(), target.agent_id.clone()),
                    MenuFacts { expanded_tools: self.expanded_tools, archived, outline_enabled: self.outline_enabled() },
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
        if animating {
            self.next_frame = cx.new_next_frame();
        }
        DrawStep::done()
    }
}

/// pane-find/index.tsx PaneFind: a 340px surface1 card 8px from the top and
/// 12px from the right edge holding the query field (with the match status
/// inside it), previous / next / close, and the failure row under it.
/// Returns the field rect and the card's height.
fn find_card(cx: &mut Cx2d, d: &mut Draw, stream: Rect, status: &str, can_navigate: bool, failure: Option<find::Failure>) -> (Rect, f64) {
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
    let failure_h = failure_text.as_ref().map(|f| d.kit.measure(cx, Font::Regular, small, 1.4, f, Some(w - 16.0 - 60.0)).y.max(28.0) + 16.0).unwrap_or(0.0);
    let shadow = if d.look.dark() { rgba(0x33000000) } else { rgba(0x0a000000) };
    d.kit.rect(cx, rect_xywh(x, y + 4.0, w, card_h), shadow, 8.0);
    d.kit.bordered(cx, rect_xywh(x, y, w, card_h), p.surface1, 8.0, p.border, 1.0);
    d.canvas.add_hit(rect_xywh(x, y, w, card_h + failure_h), HitTarget::Chrome("find:card".into()));
    let buttons_w = 3.0 * 28.0 + 2.0 * 4.0;
    let field = rect_xywh(x + 6.0, y + 6.0, w - 12.0 - buttons_w - 4.0, field_h);
    // The match status sits inside the field's right edge.
    if !status.is_empty() {
        let s = d.kit.measure(cx, Font::Regular, small, 1.3, status, None);
        d.kit.label(cx, Font::Regular, small, 1.3, p.muted, dvec2(field.pos.x + field.size.x - 8.0 - s.x, field.pos.y + (field_h - s.y) * 0.5), status, None);
    }
    let mut bx = field.pos.x + field.size.x + 4.0;
    for (icon, id, enabled) in [("chevron-up", "find:previous", can_navigate), ("chevron-down", "find:next", can_navigate), ("x", "find:close", true)] {
        let r = rect_xywh(bx, y + 8.0, 28.0, 28.0);
        let hovered = d.state.hover_hit.as_deref() == Some(id);
        if hovered && enabled {
            d.kit.rect(cx, r, p.surface2, 6.0);
        }
        d.kit.icon(cx, icon, r, 16.0, if enabled { if hovered { p.foreground } else { p.muted } } else { with_alpha(p.muted, 0.5) });
        if enabled {
            d.canvas.add_hit(r, HitTarget::Chrome(id.into()));
        }
        bx += 32.0;
    }
    if let Some(text) = failure_text {
        let fy = y + card_h;
        d.kit.rect(cx, rect_xywh(x, fy, w, failure_h), p.surface1, 0.0);
        d.kit.label(cx, Font::Regular, small, 1.4, p.foreground, dvec2(x + 8.0, fy + 8.0), &text, Some(w - 16.0 - 60.0));
        let label = t("paneFind.retry");
        let lw = d.kit.measure(cx, Font::Medium, small, 1.3, &label, None).x + 16.0;
        let r = rect_xywh(x + w - 8.0 - lw, fy + 8.0, lw, 24.0);
        let hovered = d.state.hover_hit.as_deref() == Some("find:retry");
        if hovered {
            d.kit.rect(cx, r, p.surface2, 6.0);
        }
        d.kit.label(cx, Font::Medium, small, 1.3, p.foreground, dvec2(r.pos.x + 8.0, r.pos.y + 4.0), &label, None);
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
        let Some(snap) = &self.snap else { return Some(NonReady::Loading) };
        if !map(&snap.agent).is_empty() || snap.hydrated || snap.create_handoff {
            return None;
        }
        if !self.host.connected {
            return Some(NonReady::Unavailable { label: self.host.label.clone(), connecting: self.host.connecting, error: self.host.host_error.clone() });
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
                    ta("paneFind.position", &[("current", &(self.find.occurrence + 1).to_string()), ("total", &self.find.count.to_string())])
                } else {
                    t("paneFind.noMatches")
                }
            }
            _ => String::new(),
        }
    }

    fn chrome_hit(&self, abs: DVec2) -> Option<HitTarget> {
        if self.modal.is_some() {
            return self.modal_canvas.hit_at(abs - self.modal_canvas.origin).cloned();
        }
        self.overlay_canvas.hit_at(abs - self.overlay_canvas.origin).cloned()
    }

    fn chrome_hover(&mut self, cx: &mut Cx, abs: Option<DVec2>) {
        let now = now_seconds();
        if self.modal.is_none() && self.rail.hover(abs, now) {
            self.area.redraw(cx);
        }
        let key = abs.and_then(|abs| self.chrome_hit(abs)).map(|t| super::chat_row::hover_key(&t));
        let state = if self.modal.is_some() { &mut self.modal_state } else { &mut self.chrome_state };
        if state.hover_hit != key {
            state.hover_hit = key.clone();
            state.hovered = abs.is_some();
            self.area.redraw(cx);
        }
        if abs.is_some() {
            cx.set_cursor(if key.is_some() { MouseCursor::Hand } else { MouseCursor::Default });
        }
    }

    fn modal_scroll(&mut self, cx: &mut Cx, abs: DVec2, delta: DVec2) -> bool {
        if let Some(Modal::Lightbox { .. }) = &self.modal {
            self.zoom(if delta.y < 0.0 { 1.1 } else { 1.0 / 1.1 });
            self.area.redraw(cx);
            return true;
        }
        let rel = abs - self.modal_canvas.origin;
        if let Some(key) = self.modal_canvas.scroll_region_at(rel, delta.x.abs() <= delta.y.abs()).map(str::to_owned) {
            let vertical = delta.x.abs() <= delta.y.abs();
            let (offsets, extents) = if vertical { (&mut self.modal_state.scroll_y, &self.modal_state.extent_y) } else { (&mut self.modal_state.scroll_x, &self.modal_state.extent_x) };
            let max = extents.get(&key).copied().unwrap_or(0.0);
            let current = offsets.get(&key).copied().unwrap_or(0.0);
            let detach = key == "sheet:y";
            let next = (current + if vertical { delta.y } else { delta.x }).clamp(0.0, max);
            if next == current {
                return false;
            }
            offsets.insert(key, next);
            if detach {
                // An overview sheet stays pinned to its newest call until scrolled.
                self.modal_state.expanded.insert("sheet:detached".into(), true);
            }
            self.area.redraw(cx);
            return true;
        }
        false
    }

    /// Everything that needs the UiContext: commands, overlay answers,
    /// session completions, row/composer/find actions, timers, attention.
    fn ui_pass(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext, chrome_tap: Option<String>, input_focused: bool) {
        let Some(target) = self.target.clone() else { return };
        if !self.ensured {
            self.ensured = true;
            with_store(|store| store.ensure(cx, ui, &target.key()));
            self.area.redraw(cx);
        }
        self.core = Some(ui.core.clone());
        self.refresh_host(ui);
        let changed = self.refresh_snapshot();

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
            let action = command.action.strip_prefix("conversation.").or_else(|| command.action.strip_prefix("conversation:"));
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
                .map(|s| (std::mem::take(&mut s.completions), std::mem::take(&mut s.reveals)))
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

        // Provider catalog pushes for this cwd (use-providers-snapshot.ts).
        let scope_of = |cwd: &str| cwd.trim().trim_end_matches('/').to_owned();
        let mut reload_providers = false;
        for core_event in ui.events.iter() {
            match core_event {
                paseo_gui_core::CoreEvent::Response { request_id, result, .. }
                    if self.providers_request.as_ref() == Some(request_id) =>
                {
                    self.providers_request = None;
                    match result {
                        Ok(body) => {
                            if let Some(error) = paseo_gui_core::response_error(body, "get_providers_snapshot_request") {
                                self.show_failure(error.message);
                            } else {
                                self.providers = bridge::provider_entries(body);
                            }
                        }
                        Err(error) => self.show_failure(error.message.clone()),
                    }
                    if self.visible {
                        self.area.redraw(cx);
                    }
                }
                paseo_gui_core::CoreEvent::Connected { host_id, .. }
                | paseo_gui_core::CoreEvent::Disconnected { host_id, .. } if host_id == &target.host_id => {
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
            let rows: Vec<ChatRowEvent> = actions.iter().filter_map(|a| a.downcast_ref::<ChatRowEvent>()).filter(|e| e.panel == self.uid).cloned().collect();
            for row in rows {
                self.on_row_event(cx, ui, row);
            }
            let list = self.list.as_portal_list();
            if list.scrolled(actions) {
                // Never consult is_at_end here: it is from the frame before
                // this scroll. draw_walk reconciles the laid-out position.
                self.area.redraw(cx);
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
                    let (epoch, rows) = self.snap.as_ref().map(|s| (s.epoch.clone(), s.timeline.clone())).unwrap_or_default();
                    if self.last_shift { self.find.previous(epoch.as_deref(), &rows) } else { self.find.next(epoch.as_deref(), &rows) }
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
            if let Some(seq) = tap.strip_prefix("outline:").and_then(|s| s.parse::<i64>().ok()) {
                self.jump_to_prompt(cx, ui, seq);
            } else {
                self.on_chrome_tap(cx, ui, &tap);
            }
        }

        // Attention.
        if input_focused && let Some(a) = &mut self.attention {
            a.clear_on(attention::Trigger::InputFocus);
        }
        self.sync_attention(cx, ui);

        // Providers for the subagent facts line.
        if self.modal.is_none()
            && self.track_anchors.iter().any(|(p, _)| *p == TrackPanel::Subagents)
            && !self.providers_requested
            && self.subagent_rows().iter().any(|r| r.paseo)
        {
            self.ensure_providers(cx, ui);
        }
    }

    fn on_session_changed(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(snap) = self.snap.clone() else { return };
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
        self.area.redraw(cx);
    }

    fn wants_older(&mut self) -> bool {
        let Some(snap) = &self.snap else { return false };
        let available = self.visible && snap.has_older && !snap.loading && snap.start_cursor.is_some() && !self.stream_empty();
        let list = self.list.as_portal_list();
        let at_start = list.scroll_anchor(1).is_some_and(|(index, _)| index == 1);
        self.older.observe(at_start, list.not_filling_viewport(), available)
    }

    /// Post-layout bookkeeping: the active prompt and find match position.
    fn after_draw(&mut self, cx: &mut Cx2d) {
        let list = self.list.as_portal_list();
        let first = list.first_id();
        // resolveActivePromptSeq: the last prompt at or before the first row on screen.
        if self.rail.prompts.len() >= 2 {
            let anchor = self.items.iter().skip(first).find_map(RowItem::seq);
            let active = outline::active_prompt_seq(&self.rail.prompts, anchor);
            if active != self.active_prompt {
                self.active_prompt = active;
                self.area.redraw(cx);
            }
        }
        // chatFindScrollToMatch: the highlighted match sits just under the find card.
        if let Some((row, attempts)) = self.find_scroll.clone() {
            let index = self.items.iter().position(|i| i.id == row);
            let found = index.and_then(|i| list.get_item(i)).and_then(|(_, w)| w.as_chat_row().find_y());
            match (index, found) {
                (Some(index), Some(y)) => {
                    let clearance = self.find_card_h + 8.0;
                    list.set_first_id_and_scroll(index, clearance - y);
                    self.find_scroll = None;
                    self.area.redraw(cx);
                }
                _ if attempts < 3 => {
                    self.find_scroll = Some((row, attempts + 1));
                    self.area.redraw(cx);
                }
                _ => self.find_scroll = None,
            }
        }
    }

    /// Track panels, compact detail sheets, the usage sheet, the image
    /// lightbox and the subagent sheet. Returns whether it animates.
    #[allow(clippy::too_many_arguments)]
    fn draw_modal(&mut self, cx: &mut Cx2d, scope: &mut Scope, look: Look, rect: Rect, rows: &[SubagentRow], tasks: &[Value], host_label: &dyn Fn(&str) -> String) -> bool {
        let p = look.palette;
        let Some(modal) = self.modal.take() else { return false };
        let mut animating = false;
        match &modal {
            Modal::Track(panel) => {
                let anchor = self.track_anchors.iter().find(|(p, _)| p == panel).map(|(_, r)| *r).unwrap_or(rect_xywh(rect.pos.x + 16.0, rect.pos.y + rect.size.y - 48.0, 120.0, 32.0));
                let target = self.target.clone().unwrap_or_default();
                let model = TrackModel {
                    tasks,
                    rows,
                    archive: self.archive_status,
                    additions: self.host.additions,
                    deletions: self.host.deletions,
                    can_archive_finished: !target.is_subagent(),
                    can_detach: self.feature("agentDetach"),
                    host_id: &target.host_id,
                    providers: &self.providers,
                    host_label,
                };
                let mut d = Draw { kit: &mut self.modal_kit, canvas: &mut self.modal_canvas, look, state: &mut self.modal_state, now: now_seconds(), area: self.area, animating: false };
                tracks::draw_panel(cx, &mut d, &mut self.painter, &mut self.provider_icon, &model, *panel, anchor, rect);
                animating |= d.animating;
            }
            Modal::Sheet { row, kind, opened } => {
                let item = self.items.iter().find(|i| i.id == *row).cloned();
                // A row that left the stream closes its sheet.
                let (Some(item), Some(ctx)) = (item, self.ctx.clone()) else {
                    return false;
                };
                let Some(header) = rows::sheet_header(&ctx, &item, kind) else { return false };
                let progress = ((now_seconds() - *opened) / super::tool_sheet::OPEN_SECS).clamp(0.0, 1.0);
                let mut d = Draw { kit: &mut self.modal_kit, canvas: &mut self.modal_canvas, look, state: &mut self.modal_state, now: now_seconds(), area: self.area, animating: false };
                let kind_ref = kind.as_str();
                super::tool_sheet::paint_detail_sheet(cx, &mut d, rect, &header, progress, kind == "group", &|cx, d, x, y, w, fill| {
                    rows::sheet_body(cx, d, &ctx, &item, kind_ref, x, y, w, fill)
                });
                animating |= d.animating;
            }
            Modal::Usage => {
                let agent = self.snap.as_ref().map(|s| s.agent.clone()).unwrap_or_default();
                let mut d = Draw { kit: &mut self.modal_kit, canvas: &mut self.modal_canvas, look, state: &mut self.modal_state, now: now_seconds(), area: self.area, animating: false };
                usage_sheet(cx, &mut d, &agent, rect);
                animating |= d.animating;
            }
            Modal::Lightbox { source, scale, offset } => {
                let texture = match source {
                    LightboxSource::Assistant { url, row } => {
                        let core = scope.data.get::<UiContext>().map(|ui| ui.core.clone()).or_else(|| self.core.clone());
                        let target = self.target.clone().unwrap_or_default();
                        match images::image(cx, core.as_ref(), &target.host_id, target.cwd.as_deref(), url, row) {
                            ImageState::Ready { texture, width, height } => Some((texture, width as f64, height as f64)),
                            _ => None,
                        }
                    }
                    LightboxSource::User(image) => bridge::image_texture(cx, image, false).map(|t| {
                        let size = t.get_format(cx).vec_width_height().unwrap_or((1, 1));
                        (t, size.0 as f64, size.1 as f64)
                    }),
                };
                let mut d = Draw { kit: &mut self.modal_kit, canvas: &mut self.modal_canvas, look, state: &mut self.modal_state, now: now_seconds(), area: self.area, animating: false };
                lightbox(cx, &mut d, rect, texture, *scale, *offset);
                animating |= d.animating;
            }
            Modal::Subagent { label, provider, .. } => {
                let d = Draw { kit: &mut self.modal_kit, canvas: &mut self.modal_canvas, look, state: &mut self.modal_state, now: now_seconds(), area: self.area, animating: false };
                d.kit.rect(cx, rect, rgba(0x73000000), 0.0);
                d.canvas.add_hit(rect, HitTarget::Chrome("scrim".into()));
                let h = rect.size.y * 0.92;
                let sheet = rect_xywh(rect.pos.x, rect.pos.y + rect.size.y - h, rect.size.x, h);
                d.kit.rect_full(cx, sheet, p.surface0, vec4(16.0, 16.0, 0.0, 0.0), TRANSPARENT, 0.0);
                d.canvas.add_hit(sheet, HitTarget::Chrome("sheet:body".into()));
                // SheetContents header: provider icon, label, open-as-tab, close.
                let measurement = RowMetrics::new(0.0, 0.0, 0.0).layout(sheet, 16.0, 16.0, 72.0, 8.0);
                let title = d.kit.layout_clamped(cx, Font::Medium, look.ui(14.0), 1.3, label, measurement.title.size.x, 1);
                let metrics = RowMetrics::new(laidout_size(&title).y, 0.0, 0.0);
                let header_h = metrics.height(52.0, 10.0, 32.0);
                let slots = metrics.layout(rect_xywh(sheet.pos.x, sheet.pos.y, sheet.size.x, header_h), 16.0, 16.0, 72.0, 8.0);
                let icon_rect = rect_xywh(slots.leading.pos.x, sheet.pos.y + (header_h - 16.0) * 0.5, 16.0, 16.0);
                if !d.dry() {
                    let svg = bridge::provider_svg(&self.providers, provider);
                    ProviderIcon::paint(&mut self.painter, &mut self.provider_icon, cx, icon_rect, provider, svg.as_deref(), p.muted);
                }
                d.kit.draw_laidout(cx, Font::Medium, p.foreground, slots.title.pos, &title);
                let open = rect_xywh(slots.trailing.pos.x, sheet.pos.y + (header_h - 32.0) * 0.5, 32.0, 32.0);
                let close = rect_xywh(slots.trailing.pos.x + slots.trailing.size.x - 32.0, open.pos.y, 32.0, 32.0);
                for (r, icon, id) in [(open, "maximize-2", "sheet:open-tab"), (close, "x", "sheet:close")] {
                    let hovered = d.state.hover_hit.as_deref() == Some(id);
                    if hovered {
                        d.kit.rect(cx, r, p.surface2, 8.0);
                    }
                    d.kit.icon(cx, icon, r, 16.0, if hovered { p.foreground } else { p.muted });
                    d.canvas.add_hit(r, HitTarget::Chrome(id.into()));
                }
                animating |= d.animating;
                let content = rect_xywh(sheet.pos.x, sheet.pos.y + header_h, sheet.size.x, (sheet.size.y - header_h).max(1.0));
                self.sheet_content = content;
                if let Some(panel) = self.sheet_panel.clone() {
                    let walk = Walk { abs_pos: Some(content.pos), margin: Margin::default(), width: Size::Fixed(content.size.x), height: Size::Fixed(content.size.y) };
                    let _ = panel.draw_walk(cx, scope, walk);
                }
            }
        }
        self.modal = Some(modal);
        animating
    }
}

/// components/context-window-sheet.tsx: the agent's facts and usage in the
/// adaptive sheet (bottom sheet on compact, centered dialog elsewhere).
fn usage_sheet(cx: &mut Cx2d, d: &mut Draw, agent: &Value, rect: Rect) {
    let p = d.look.palette;
    d.kit.rect(cx, rect, rgba(0x73000000), 0.0);
    d.canvas.add_hit(rect, HitTarget::Chrome("scrim".into()));
    let usage = get(agent, "lastUsage");
    let pairs: Vec<(&str, &Value)> = vec![
        ("Provider", get(agent, "provider")),
        ("Model", get(agent, "model")),
        ("Status", get(agent, "status")),
        ("Created", get(agent, "createdAt")),
        ("Updated", get(agent, "updatedAt")),
        ("Last prompt", get(agent, "lastUserMessageAt")),
        ("Input tokens", get(usage, "inputTokens")),
        ("Cached input", get(usage, "cachedInputTokens")),
        ("Output tokens", get(usage, "outputTokens")),
        ("Context used", get(usage, "contextWindowUsedTokens")),
        ("Context maximum", get(usage, "contextWindowMaxTokens")),
        ("Cost (USD)", get(usage, "totalCostUsd")),
    ];
    let compact = d.look.compact;
    let w = if compact { rect.size.x } else { rect.size.x.min(480.0) - 32.0 };
    let base = d.look.ui(14.0);
    let paint = |cx: &mut Cx2d, d: &mut Draw, x: f64, y: f64| -> f64 {
        let mut cy = y + 16.0;
        d.kit.label(cx, Font::Semibold, d.look.ui(16.0), 1.3, p.foreground, dvec2(x + 16.0, cy), "Session activity and usage", Some(w - 64.0));
        cy += d.look.ui(16.0) * 1.3 + 16.0;
        for (label, value) in &pairs {
            if value.is_null() {
                continue;
            }
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            d.kit.label(cx, Font::Regular, base, 1.4, p.muted, dvec2(x + 16.0, cy), label, Some(w * 0.45));
            let laid = d.kit.layout(cx, Font::Regular, base, 1.4, &text, Some(w * 0.5 - 16.0), 0.0);
            let s = laidout_size(&laid);
            let pos = dvec2(x + w - 16.0 - s.x, cy);
            d.kit.draw_laidout(cx, Font::Regular, p.foreground, pos, &laid);
            d.canvas.add_run(laid, pos, Some('\n'));
            cy += s.y.max(base * 1.4) + 8.0;
        }
        if let Some(error) = non_empty_str(get(agent, "lastError")) {
            let laid = d.kit.layout(cx, Font::Regular, base, 1.4, error, Some(w - 32.0), 0.0);
            let pos = dvec2(x + 16.0, cy);
            d.kit.draw_laidout(cx, Font::Regular, p.danger, pos, &laid);
            cy += laidout_size(&laid).y + 8.0;
            d.canvas.add_run(laid, pos, Some('\n'));
        }
        // Runtime details (ExpansionTile with JsonDetails).
        let expanded = d.state.is_expanded("usage:runtime", false);
        let header = rect_xywh(x + 16.0, cy, w - 32.0, 40.0);
        d.kit.label(cx, Font::Regular, base, 1.4, p.foreground, dvec2(header.pos.x, cy + 10.0), "Runtime details", None);
        d.kit.icon(cx, if expanded { "chevron-up" } else { "chevron-down" }, rect_xywh(header.pos.x + header.size.x - 20.0, cy, 20.0, 40.0), 16.0, p.muted);
        d.canvas.add_hit(header, HitTarget::Toggle("usage:runtime".into()));
        cy += 40.0;
        if expanded {
            let blocks = vec![super::tools::DetailBlock::JsonBox(pretty(get(agent, "runtimeInfo")))];
            cy += super::details::draw_details(cx, d, &blocks, false, false, "usage:runtime:json", x + 16.0, cy, w - 32.0, None, None) + 8.0;
        }
        cy + 16.0 - y
    };
    let h = d.measure(|d| paint(cx, d, 0.0, 0.0)).min(rect.size.y * 0.9);
    let (x, y) = if compact { (rect.pos.x, rect.pos.y + rect.size.y - h) } else { (rect.pos.x + (rect.size.x - w) * 0.5, rect.pos.y + (rect.size.y - h) * 0.5) };
    let radii = if compact { vec4(16.0, 16.0, 0.0, 0.0) } else { vec4(12.0, 12.0, 12.0, 12.0) };
    d.kit.rect_full(cx, rect_xywh(x, y, w, h), p.surface1, radii, p.border, 1.0);
    d.canvas.add_hit(rect_xywh(x, y, w, h), HitTarget::Chrome("sheet:body".into()));
    let close = rect_xywh(x + w - 44.0, y + 8.0, 32.0, 32.0);
    let key = "usage:y".to_owned();
    let content = d.measure(|d| paint(cx, d, 0.0, 0.0));
    let max_y = (content - h).max(0.0);
    d.state.extent_y.insert(key.clone(), max_y);
    let sy = d.state.scroll_y.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_y);
    let view = rect_xywh(x, y, w, h);
    cx.begin_turtle(Walk { abs_pos: Some(view.pos), margin: Margin::default(), width: Size::Fixed(w), height: Size::Fixed(h.max(1.0)) }, Layout::default());
    d.canvas.push_clip(view);
    paint(cx, d, x, y - sy);
    d.canvas.pop_clip();
    cx.end_turtle();
    if max_y > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollY(key));
    }
    d.kit.icon(cx, "x", close, 18.0, p.muted);
    d.canvas.add_hit(close, HitTarget::Chrome("sheet:close".into()));
}

/// components/attachment-lightbox.tsx: a 90% black backdrop over the image
/// fitted inside 16px padding and at most 960×640, zoom in / out / reset and
/// close 8px from the top-right corner; "Couldn't load image" on failure.
fn lightbox(cx: &mut Cx2d, d: &mut Draw, rect: Rect, image: Option<(Texture, f64, f64)>, scale: f64, offset: DVec2) {
    let p = d.look.palette;
    d.kit.rect(cx, rect, rgba(0xe6000000), 0.0);
    let fit_w = (rect.size.x - 32.0).clamp(0.0, 960.0);
    let fit_h = (rect.size.y - 32.0).clamp(0.0, 640.0);
    match image {
        Some((texture, w, h)) => {
            let ratio = (fit_w / w.max(1.0)).min(fit_h / h.max(1.0));
            let size = dvec2(w * ratio * scale, h * ratio * scale);
            let pos = rect.center() - size * 0.5 + offset;
            let r = Rect { pos, size };
            cx.begin_turtle(Walk { abs_pos: Some(rect.pos), margin: Margin::default(), width: Size::Fixed(rect.size.x), height: Size::Fixed(rect.size.y) }, Layout::default());
            d.kit.texture(cx, r, &texture, 0.0);
            cx.end_turtle();
            d.canvas.add_hit(r.clip((rect.pos, rect.pos + rect.size)), HitTarget::Chrome("lightbox:image".into()));
        }
        None => {
            let text = t("message.attachments.imageLoadFailed");
            let s = d.kit.measure(cx, Font::Regular, d.look.ui(14.0), 1.4, &text, None);
            d.kit.label(cx, Font::Regular, d.look.ui(14.0), 1.4, p.muted, rect.center() - s * 0.5, &text, None);
            d.animating = true;
        }
    }
    let mut x = rect.pos.x + rect.size.x - 8.0;
    for (icon, id) in [("x", "lightbox:close"), ("rotate-ccw", "lightbox:reset"), ("zoom-out", "lightbox:zoom-out"), ("zoom-in", "lightbox:zoom-in")] {
        x -= 36.0;
        let r = rect_xywh(x, rect.pos.y + 8.0, 32.0, 32.0);
        let hovered = d.state.hover_hit.as_deref() == Some(id);
        d.kit.rect(cx, r, if hovered { rgba(0x66ffffff) } else { rgba(0x33ffffff) }, 16.0);
        d.kit.icon(cx, icon, r, 16.0, vec4(1.0, 1.0, 1.0, 1.0));
        d.canvas.add_hit(r, HitTarget::Chrome(id.into()));
        if id == "lightbox:close" {
            x -= 8.0;
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
}

#[cfg(test)]
mod reading_hold_tests {
    use super::*;

    fn cursor(epoch: &str, seq: i64) -> Cursor {
        Cursor { epoch: epoch.to_owned(), seq }
    }

    fn rows(ids: &[(&str, f64)]) -> Vec<RowItem> {
        ids.iter().map(|(id, height)| RowItem { id: (*id).to_owned(), kind: RowKind::Inset(*height) }).collect()
    }

    #[test]
    fn repeated_prepends_preserve_current_reader_row_and_pixel_offset() {
        let original = rows(&[("history-start", 48.0), ("a", 170.0), ("b", 35.0), ("c", 420.0)]);
        let first_page = rows(&[("history-start", 48.0), ("older-a", 310.0), ("older-b", 27.0), ("a", 170.0), ("b", 35.0), ("c", 420.0)]);
        assert_eq!(remap_reading_anchor(&original, &first_page, (1, -37.5)), Some((3, -37.5)));

        // The reader moved into older-b while the second request was in flight.
        let second_page = rows(&[("history-start", 48.0), ("oldest", 750.0), ("older-a", 310.0), ("older-b", 27.0), ("a", 170.0), ("b", 35.0), ("c", 420.0)]);
        assert_eq!(remap_reading_anchor(&first_page, &second_page, (2, -12.0)), Some((3, -12.0)));
        assert_eq!(remap_reading_anchor(&second_page, &second_page, (6, -80.0)), Some((6, -80.0)));
    }

    #[test]
    fn prepend_absorbed_overview_group_preserves_its_anchor() {
        let entry = |seq| Rc::new(json!({"seqStart": seq, "seqEnd": seq, "item": {"type": "tool_call"}}));
        let group = |id: &str, entries: Vec<Rc<Value>>| {
            let stream = StreamRow { entry: entries.last().unwrap().clone(), group: Some(entries) };
            RowItem {
                id: id.to_owned(),
                kind: RowKind::Entry {
                    rows: Rc::new(vec![stream]), index: 0, previous_tasks: None,
                    footer_below: false, streaming: false, live: false, gap_below: 0.0,
                },
            }
        };
        let previous = vec![group("group:20", vec![entry(20), entry(21)])];
        let next = vec![group("group:10", vec![entry(10), entry(20), entry(21)])];
        assert_eq!(remap_reading_anchor(&previous, &next, (0, -18.0)), Some((0, -18.0)));
        let replacement = vec![group("group:30", vec![entry(30)])];
        assert_eq!(remap_reading_anchor(&previous, &replacement, (0, -18.0)), None);
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
