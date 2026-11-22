//! Shell chrome (Flutter `PaseoShell.build`, `_header`, `_layout`, `_pane`, `_explorer`,
//! `_content`, `_compactExplorerOverlay`, `_welcome`): geometry, drawing and pointer handling
//! of everything that is not a feature root.

use std::collections::HashSet;

use makepad_widgets::event::TouchState;
use makepad_widgets::*;
use serde_json::Value;

use crate::common::paint::{Font, TextSpec, rect};
use crate::common::provider_icon::ProviderIcon;
use crate::common::transition::{Intent, Transition};
use crate::common::widgets::{StatusBucket, agent_live_status_bucket, paint_status_badge};
use crate::common::{PanelTarget, Route, UiContext, icons, json as j, layer, tr, tr_args};
use crate::shell::hosts::{self, Host};
use crate::shell::layout::{Axis, CompactDestination, Layout as WindowLayout, Node};
use crate::shell::ui::{HitScope, Ui, Variant};
use crate::shell::{PaseoShell, commands, panels, sidebar};
use crate::theme::{self, ControlSize, Palette, with_alpha};

/// Geometry of the last frame, for drags and drops.
#[derive(Default)]
pub struct ChromeState {
    splits: Vec<SplitHandle>,
    /// Child minimums of each split, in draw order. Scratch storage reused across layouts.
    split_minima: Vec<[DVec2; 2]>,
    sidebar_stepped_aside: bool,
    pub pane_rects: Vec<(String, Rect)>,
    pub tab_bars: Vec<(String, Rect)>,
    pub tab_rects: Vec<(String, String, Rect)>,
    /// Tabs of each pane that did not fit its strip this frame (overflow menu).
    pub tab_overflow: Vec<(String, Vec<String>)>,
    pub sidebar_rect: Rect,
    pub explorer_rect: Rect,
    pub content_rect: Rect,
    /// Last measured width of the header's Open In + git actions widgets (0 before first draw),
    /// so the session-menu island sits right next to them instead of a fixed 300px slot.
    pub header_git_w: f64,
    /// Active drag: (kind, id, origin abs, origin value).
    pub drag: Option<Drag>,
    pub(crate) explorer: Transition,
    explorer_layer: layer::Layer,
    explorer_list: Option<DrawList2d>,
    explorer_slot: Option<Slot>,
    explorer_scrim: Area,
    explorer_window: Rect,
    pub edge_start: Option<DVec2>,
    pub drop_pane: Option<String>,
    /// A touch that started in a pane: (touch id, pane, start) until it lifts as a tap.
    pub touch_focus: Option<(u64, String, DVec2)>,
    /// Regions the last chrome paint laid out, each painted into its own retained layer.
    pub slots: Vec<Slot>,
    /// Route slots while laying out the Explorer toolbar.
    pub overlay_pass: bool,
}

impl ChromeState {
    pub(crate) fn explorer_visible(&self) -> bool {
        self.explorer.value() > 0.0 || self.explorer.is_moving()
    }

    pub(crate) fn invalidate_explorer(&self, cx: &mut Cx) {
        self.explorer_layer.invalidate(cx);
    }
}

/// A region laid out by the chrome paint that owns a retained layer (`common::layer`).
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    Sidebar(Rect),
    Root { key: String, rect: Rect },
    Page(Rect),
    Sessions(Rect),
}

#[derive(Clone, Debug)]
pub enum Drag {
    Split {
        id: String,
        grab: f64,
        last_first: f64,
    },
    Sidebar {
        start_x: f64,
        start_width: f64,
    },
    Explorer {
        start_x: f64,
        start_width: f64,
    },
    Tab {
        key: String,
        from_pane: String,
        start: DVec2,
        moving: bool,
    },
}

struct SplitHandle {
    id: String,
    axis: Axis,
    start: f64,
    available: f64,
    minima: [f64; 2],
    first: f64,
}

impl SplitHandle {
    fn drag_first(&self, pointer: f64, grab: f64, last_first: f64) -> Option<f64> {
        // A forced split has nowhere to move. Keep the saved ratio for when there is room.
        if self.available <= self.minima[0] + self.minima[1] {
            return None;
        }
        let ratio = (pointer - self.start - grab) / self.available;
        let first = split_first(self.available, ratio, self.minima);
        ((first - last_first).abs() > 0.01).then_some(first)
    }
}

fn along(axis: Axis, v: DVec2) -> f64 {
    match axis {
        Axis::Horizontal => v.x,
        Axis::Vertical => v.y,
    }
}

// ---------------------------------------------------------------------------------------------
// Titles and icons (Flutter `_title`, `_headerTitle`, `_tabKindLabel`)

fn agent_row<'a>(state: &'a paseo_gui_core::CoreState, target: &PanelTarget) -> Option<&'a Value> {
    hosts::host(state, &target.server_id)?.agent(target.id.as_deref()?)
}

pub fn title(state: &paseo_gui_core::CoreState, target: &PanelTarget) -> String {
    if target.kind == "agent"
        && let Some(agent) = agent_row(state, target)
    {
        let title = j::text(agent, "title");
        if !title.is_empty() {
            return title.to_string();
        }
    }
    if let Some(title) = target.title.as_deref().filter(|t| !t.is_empty()) {
        return title.to_string();
    }
    match target.kind.as_str() {
        "agent" => tr("sessionNavigation.untitled"),
        "new_tab" if target.data.get("addProject") == Some(&Value::Bool(true)) => {
            tr("sidebar.actions.addProject")
        }
        "draft" | "new_tab" => tr("sessionNavigation.newSession"),
        "files" => tr("workspace.tabs.actions.files"),
        "file" => {
            let fallback = tr("shell.tabs.file");
            let path = j::text_or(
                &target.data,
                "path",
                target.id.as_deref().unwrap_or(&fallback),
            );
            path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
        }
        "commit_diff" => tr("panels.diff.diffLabel"),
        "pull_request" => tr("panels.pullRequest.label"),
        "git_log" => tr("workspace.git.log.title"),
        "git_commit" => tr("workspace.git.commit.title"),
        "execution_log" => tr("shell.tabs.executionLog"),
        "terminal" => tr("workspace.tabs.fallback.terminal"),
        "setup" => tr("workspace.tabs.fallback.workspaceSetup"),
        "project_settings" => tr("shell.tabs.projectSettings"),
        "provider_subagent" => tr("shell.tabs.subagent"),
        other => other.to_string(),
    }
}

/// `_headerTitle`: an agent header shows the directory's title or "Untitled session".
pub fn header_title(state: &paseo_gui_core::CoreState, target: &PanelTarget) -> String {
    if target.kind != "agent" {
        return title(state, target);
    }
    agent_row(state, target)
        .map(|agent| j::text(agent, "title").trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| tr("sessionNavigation.untitled"))
}

pub fn kind_label(target: &PanelTarget) -> String {
    match target.kind.as_str() {
        "new_tab" => tr("workspace.tabs.actions.newTab"),
        "draft" => tr("workspace.tabs.fallback.newAgent"),
        "setup" => tr("workspace.tabs.fallback.setup"),
        "terminal" => tr("workspace.tabs.fallback.terminal"),
        "file" => {
            let path = j::text_or(&target.data, "path", target.id.as_deref().unwrap_or(""));
            path.split('/')
                .rfind(|p| !p.is_empty())
                .unwrap_or(path)
                .to_string()
        }
        "files" => tr("panels.files.label"),
        "pull_request" => tr("panels.pullRequest.label"),
        "git_log" => tr("workspace.git.log.title"),
        "git_commit" => tr("workspace.git.commit.title"),
        "commit_diff" => target
            .id
            .as_deref()
            .map(|id| id.chars().take(7).collect())
            .unwrap_or_default(),
        _ => tr("workspace.tabs.fallback.agent"),
    }
}

// ---------------------------------------------------------------------------------------------
// Visibility

fn in_workspace(tab: Option<&PanelTarget>, workspace: &PanelTarget) -> bool {
    tab.is_some_and(|t| {
        t.server_id == workspace.server_id
            && t.workspace_id == workspace.workspace_id
            && t.cwd == workspace.cwd
    })
}

/// Supporting views belong to the selected session's host and directory at every width.
pub fn explorer_keys(shell: &PaseoShell) -> (Vec<String>, Option<String>) {
    explorer_keys_for(&shell.data)
}

pub(super) fn explorer_keys_for(data: &super::Data) -> (Vec<String>, Option<String>) {
    let layout = &data.layout;
    let mut keys = layout.explorer_tabs.clone();
    let mut selected = layout.selected_explorer.clone();
    if let Some(workspace) = layout.selected() {
        keys.retain(|key| in_workspace(layout.target(key), workspace));
        if !selected.as_ref().is_some_and(|s| keys.contains(s)) {
            selected = layout
                .explorer_focus_for(workspace)
                .filter(|key| keys.contains(key))
                .or_else(|| keys.first().cloned());
        }
    }
    (keys, selected)
}

/// Tabs whose roots are on screen this frame (`_RetainedPanel.active`).
pub(super) fn visible_tabs(data: &super::Data, explorer: Option<&str>) -> HashSet<String> {
    let layout = &data.layout;
    let mut out = HashSet::new();
    if data.page.is_some()
        && (data.compact || matches!(data.page, Some(crate::shell::Page::Settings { .. })))
    {
        return out;
    }
    let page_over_main = data.page.is_some();
    if data.compact {
        if layout.compact_destination != CompactDestination::Sidebar
            && let Some(key) = &layout.focused().selected
        {
            out.insert(key.clone());
        }
        if (layout.compact_destination == CompactDestination::Explorer
            || data.chrome.explorer_visible())
            && let Some(key) = explorer
        {
            out.insert(key.to_owned());
        }
        return out;
    }
    if !page_over_main {
        for pane in layout.panes() {
            if layout
                .maximized_pane
                .as_deref()
                .is_none_or(|m| m == pane.id)
                && let Some(key) = &pane.selected
            {
                out.insert(key.clone());
            }
        }
        if layout.explorer_visible
            && let Some(key) = explorer
        {
            out.insert(key.to_owned());
        }
    }
    out
}

/// The focused root: compact explorer → explorer tab; else the focused pane's tab.
pub fn focused_tab(shell: &PaseoShell) -> Option<String> {
    let explorer = if shell.data.compact
        && shell.data.layout.compact_destination == CompactDestination::Explorer
    {
        explorer_keys(shell).1
    } else {
        None
    };
    focused_tab_for(&shell.data, explorer.as_deref())
}

pub(super) fn focused_tab_for(data: &super::Data, explorer: Option<&str>) -> Option<String> {
    let layout = &data.layout;
    if data.compact {
        return match layout.compact_destination {
            CompactDestination::Sidebar => None,
            CompactDestination::Explorer => explorer.map(str::to_owned),
            CompactDestination::Agent => layout.focused().selected.clone(),
        };
    }
    layout.focused().selected.clone()
}

pub fn focused_target(shell: &PaseoShell) -> Option<&PanelTarget> {
    focused_tab(shell).and_then(|key| shell.data.layout.target(&key))
}

// ---------------------------------------------------------------------------------------------
// Drawing

/// Scale only an authored text line box, leaving surrounding padding and icons unchanged.
pub(crate) fn text_line_height(height: f64, font_size: f64) -> f64 {
    height * (theme::current_typography().ui_size(font_size) / font_size).max(1.0)
}

/// Composes one window from retained layers: the chrome, then the sidebar, pane-root and
/// page layers it laid out. Only invalidated layers repaint; the others replay their retained
/// draw calls and shell hit regions.
pub(super) fn draw_live(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    r: Rect,
    retire_hidden: bool,
) {
    shell.data.hits.clear();
    shell.data.tooltips.clear();
    // Safe-area and keyboard insets move the chrome without changing the window rect.
    let insets = crate::shell::platform::safe_insets();
    let inputs = layer::inputs([
        insets.top.to_bits(),
        insets.bottom.to_bits(),
        insets.left.to_bits(),
        insets.right.to_bits(),
        insets.keyboard.to_bits(),
        u64::from(insets.keyboard_visible),
    ]);
    if shell.layers.chrome.begin(
        cx,
        r,
        inputs,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        let p = theme::current_palette();
        let sidebar_destination = shell.data.compact
            && shell.data.layout.compact_destination == CompactDestination::Sidebar;
        let background = if sidebar_destination {
            p.sidebar
        } else {
            p.background
        };
        // The window background (and pointer capture) opens the chrome's box batch.
        shell.layers.bg = shell
            .painter
            .capture_box(cx, r, background, Vec4::default(), 0.0, 0.0);
        draw_chrome(shell, cx, scope, r);
        shell
            .layers
            .chrome
            .end(cx, &shell.data.hits, &shell.data.tooltips);
    }
    let slots = shell.data.chrome.slots.clone();
    if retire_hidden && !slots.iter().any(|slot| matches!(slot, Slot::Sidebar(_))) {
        shell.data.sidebar.viewport = Rect::default();
        shell.data.sidebar.header_layer.finish_frame(cx);
        shell.data.sidebar.tree_layer.finish_frame(cx);
        shell.data.sidebar.footer_layer.finish_frame(cx);
    }
    for slot in slots {
        draw_slot(shell, cx, scope, slot);
    }
}

/// Header, banner, tab strips, dividers and empty states; records the layered regions.
fn draw_chrome(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, r: Rect) {
    let p = theme::current_palette();
    let state = scope.data.get::<UiContext>().map(|ui| ui.state.clone());
    let chrome = &mut shell.data.chrome;
    chrome.splits.clear();
    chrome.pane_rects.clear();
    chrome.tab_bars.clear();
    chrome.tab_rects.clear();
    chrome.tab_overflow.clear();
    chrome.sidebar_rect = Rect::default();
    chrome.explorer_rect = Rect::default();
    chrome.sidebar_stepped_aside = false;
    chrome.slots.clear();
    let compact = shell.data.compact;
    let sidebar_destination =
        compact && shell.data.layout.compact_destination == CompactDestination::Sidebar;

    let Some(state) = state else {
        draw_startup_error(shell, cx, r, &p);
        return;
    };

    let insets = crate::shell::platform::safe_insets();
    let body = rect(
        r.pos.x + insets.left,
        r.pos.y + insets.top,
        r.size.x - insets.left - insets.right,
        (r.size.y - insets.top - insets.bottom_occlusion()).max(0.0),
    );
    let host_count = hosts::hosts(&state).len();

    // Settings is a pushed full-window route.
    if let Some(crate::shell::Page::Settings { .. }) = &shell.data.page {
        draw_page(shell, cx, body);
        return;
    }
    let page_shown = shell.data.page.is_some();
    let mut split_minima = std::mem::take(&mut shell.data.chrome.split_minima);
    split_minima.clear();
    let columns = if compact {
        SideWidths::default()
    } else {
        let layout = &shell.data.layout;
        let min_main = if page_shown || layout.maximized_pane.is_some() {
            MIN_MAIN
        } else {
            measure_split_minima(&layout.tree, &mut split_minima).x.max(MIN_MAIN)
        };
        side_widths(layout, body.size.x, min_main, page_shown)
    };
    shell.data.chrome.sidebar_stepped_aside = columns.sidebar_stepped_aside;
    let mut y = body.pos.y;
    let show_header =
        !(page_shown || (compact && sidebar_destination) || (compact && host_count == 0));
    if show_header {
        let h = if compact {
            28.0 + text_line_height(20.0, 14.0) + text_line_height(16.0, 12.0)
        } else {
            12.0 + text_line_height(24.0, 16.0)
        };
        draw_header(
            shell,
            cx,
            scope,
            &state,
            rect(body.pos.x, y, body.size.x, h),
        );
        y += h;
    }
    if let Some(error) = crate::shell::startup_error().or_else(|| {
        j::object(&state, "storage")
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
    }) {
        y += draw_banner(shell, cx, rect(body.pos.x, y, body.size.x, 0.0), &error);
    }
    let content = rect(body.pos.x, y, body.size.x, body.pos.y + body.size.y - y);
    shell.data.chrome.content_rect = content;
    if host_count == 0 {
        shell.data.chrome.split_minima = split_minima;
        crate::shell::sheets::welcome::draw(shell, cx, content);
        return;
    }
    if compact {
        draw_compact(shell, cx, &state, content, page_shown);
    } else {
        draw_wide(shell, cx, &state, content, page_shown, columns, &split_minima);
    }
    shell.data.chrome.split_minima = split_minima;
}

fn draw_slot(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, slot: Slot) {
    match slot {
        Slot::Sidebar(r) => {
            let Some(state) = scope.data.get::<UiContext>().map(|ui| ui.state.clone()) else {
                return;
            };
            if shell.layers.sidebar.begin(
                cx,
                r,
                layer::inputs(shell.data.layout.selected().map(PanelTarget::key)),
                &mut shell.data.hits,
                &mut shell.data.tooltips,
            ) {
                open_batches(shell, cx, r);
                shell.data.sidebar.viewport = Rect::default();
                sidebar::draw(shell, cx, scope, &state, r);
                shell
                    .layers
                    .sidebar
                    .end(cx, &shell.data.hits, &shell.data.tooltips);
                shell.data.sidebar.header_layer.finish_frame(cx);
                shell.data.sidebar.tree_layer.finish_frame(cx);
                shell.data.sidebar.footer_layer.finish_frame(cx);
            }
        }
        Slot::Root { key, rect } => paint_root(shell, cx, scope, &key, rect),
        Slot::Page(r) => paint_page(shell, cx, scope, r),
        Slot::Sessions(r) => {
            if shell
                .layers
                .sessions
                .begin(cx, r, 0, &mut shell.data.hits, &mut shell.data.tooltips)
            {
                open_batches(shell, cx, r);
                crate::shell::sheets::history::draw_page(shell, cx, scope, r);
                shell
                    .layers
                    .sessions
                    .end(cx, &shell.data.hits, &shell.data.tooltips);
            }
        }
    }
}

/// Makepad appends a primitive to its list's earliest compatible batch, so the first batch of
/// each shader fixes their stacking. Opening a layer with a transparent box batch keeps
/// backgrounds under text and icons, as when every region shared the window list.
fn open_batches(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect) {
    shell.painter.draw_box.paint(cx, r, Vec4::default(), 0.0);
}

fn draw_startup_error(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, p: &Palette) {
    let t = theme::current_typography();
    let message = crate::shell::startup_error().unwrap_or_else(|| tr("shell.chrome.connecting"));
    let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.danger);
    let w = (r.size.x - 48.0).min(560.0);
    let h = shell.painter.paragraph_height(cx, w, spec, &message);
    shell.painter.paragraph(
        cx,
        dvec2(
            r.pos.x + (r.size.x - w) * 0.5,
            r.pos.y + (r.size.y - h) * 0.5,
        ),
        w,
        spec,
        &message,
    );
}

/// Flutter `MaterialBanner` with the error and a Settings action, in the shared danger alert
/// styling: tinted danger fill, a `circle-x` glyph and a danger rule.
fn draw_banner(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, message: &str) -> f64 {
    let p = theme::current_palette();
    let t = theme::current_typography();
    let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.foreground);
    let text_x = r.pos.x + 16.0 + 14.0 + 12.0;
    let text_w = (r.pos.x + r.size.x - text_x - 16.0 - 120.0).max(80.0);
    let text_h = shell
        .painter
        .paragraph_height(cx, text_w, spec, message)
        .max(20.0);
    let h = text_h + 24.0;
    shell.painter.fill(
        cx,
        rect(r.pos.x, r.pos.y, r.size.x, h),
        with_alpha(p.danger, 0.12),
        0.0,
    );
    shell.painter.icon(
        cx,
        rect(r.pos.x + 16.0, r.pos.y + 12.0 + 3.0, 14.0, 14.0),
        "circle-x",
        14.0,
        p.danger,
    );
    shell
        .painter
        .paragraph(cx, dvec2(text_x, r.pos.y + 12.0), text_w, spec, message);
    let mut ui = make_ui(shell);
    let label = tr("settings.title");
    let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
    ui.button(
        cx,
        rect(
            r.pos.x + r.size.x - bw - 16.0,
            r.pos.y + (h - 32.0) * 0.5,
            bw,
            32.0,
        ),
        "banner:settings",
        &label,
        None,
        Variant::Ghost,
        ControlSize::Sm,
        true,
    );
    shell.painter.hline(
        cx,
        r.pos.x,
        r.pos.y + h - 1.0,
        r.size.x,
        with_alpha(p.danger, 0.5),
    );
    h
}

pub(crate) fn make_ui(shell: &mut PaseoShell) -> Ui<'_> {
    let compact = shell.data.compact;
    let data = &mut shell.data;
    Ui {
        painter: &mut shell.painter,
        draw_icon: &mut shell.draw_icon,
        hits: &mut data.hits,
        tooltips: &mut data.tooltips,
        hover: data.pointer.hover.as_deref(),
        pressed: data.pointer.pressed.as_deref(),
        p: theme::current_palette(),
        t: theme::current_typography(),
        compact,
        semantics: if data.chrome.overlay_pass {
            HitScope::Overlay(0)
        } else {
            HitScope::Shell
        },
    }
}

/// Original header-island button: a 32px (compact) / 26px circle with highlight.
fn island(
    ui: &mut Ui,
    cx: &mut Cx2d,
    center: DVec2,
    id: &str,
    tooltip: &str,
    compact: bool,
) -> Rect {
    let extent = if compact { 32.0 } else { 26.0 };
    let r = rect(
        center.x - extent * 0.5,
        center.y - extent * 0.5,
        extent,
        extent,
    );
    if ui.is_hover(id) || ui.is_pressed(id) {
        let h = ui.p.interaction_highlight;
        ui.fill(cx, r, h, extent * 0.5);
    }
    // Phone: a 44x44 touch target centred on the 32px visual (callers space centres 44px apart).
    let hit = if compact {
        rect(center.x - 22.0, center.y - 22.0, 44.0, 44.0)
    } else {
        r
    };
    ui.hit_tip(cx, hit, id, tooltip);
    r
}

fn draw_header(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    state: &paseo_gui_core::CoreState,
    r: Rect,
) {
    let compact = shell.data.compact;
    let target = shell.data.layout.selected().cloned();
    let host = target
        .as_ref()
        .and_then(|t| hosts::host(state, &t.server_id));
    let workspace = target
        .as_ref()
        .and_then(|t| host.and_then(|h| h.workspace(&t.workspace_id)))
        .cloned();
    let new_session = target.as_ref().is_some_and(|t| t.kind == "new_tab");
    let row = rect(
        r.pos.x + 12.0,
        r.pos.y + if compact { 8.0 } else { 0.0 },
        r.size.x - 24.0,
        r.size.y - if compact { 8.0 } else { 0.0 },
    );
    let mid = row.pos.y + row.size.y * 0.5;
    let has_scripts = workspace
        .as_ref()
        .is_some_and(|w| !j::array(w, "scripts").is_empty());
    let header_git = !compact && target.as_ref().is_some_and(|t| !t.cwd.is_empty());
    let mut right_widgets: Option<Rect> = None;
    let git_natural = shell.data.chrome.header_git_w;
    // A compact detail view (file, diff, subagent…) leads with Back to the tab it came from.
    let detail_back = shell.data.layout.compact_back_tab(compact).is_some();
    let shell_sidebar_visible =
        shell.data.layout.sidebar_visible && !shell.data.chrome.sidebar_stepped_aside;
    {
        let mut ui = make_ui(shell);
        let p = ui.p;
        let extent = if compact { 32.0 } else { 26.0 };
        // Centre-to-centre pitch: phone islands keep 44px hits that never overlap.
        let slot = if compact { 44.0 } else { extent };
        // Sidebar toggle (MobileMenuIcon on compact, PanelLeft on wide).
        // Another session waiting on the user shows a dot on ☰ / the sidebar toggle while the
        // sidebar is out of sight.
        let sidebar_hidden = compact || !shell_sidebar_visible;
        let waiting_elsewhere = !detail_back
            && sidebar_hidden
            && crate::shell::sidebar::others_need_input(state, target.as_ref());
        let (toggle_id, toggle_tip) = if detail_back {
            ("hdr:back", tr("common.actions.back"))
        } else {
            let tip = crate::common::tooltip_with_shortcut(
                &tr("shell.menu.toggleSidebar"),
                "toggle-left-sidebar",
            );
            (
                "hdr:sidebar",
                if waiting_elsewhere {
                    format!("{tip} · {}", tr("shell.chrome.othersNeedInput"))
                } else {
                    tip
                },
            )
        };
        let toggle = island(
            &mut ui,
            cx,
            dvec2(row.pos.x + slot * 0.5, mid),
            toggle_id,
            &toggle_tip,
            compact,
        );
        if detail_back {
            ui.icon(cx, toggle, "arrow-left", 18.0, p.muted);
        } else if compact {
            let x = toggle.pos.x + (extent - 16.0) * 0.5;
            let y0 = toggle.pos.y + (extent - 12.0) * 0.5;
            for (i, w) in [16.0, 16.0, 8.0].iter().enumerate() {
                ui.fill(cx, rect(x, y0 + i as f64 * 5.25, *w, 1.5), p.muted, 1.0);
            }
        } else {
            ui.icon(cx, toggle, "panel-left", 16.0, p.muted);
        }
        if waiting_elsewhere {
            let color =
                crate::common::widgets::status_dot_color(&p, StatusBucket::NeedsInput, false)
                    .unwrap_or(p.warning);
            let center = dvec2(toggle.pos.x + toggle.size.x - 5.0, toggle.pos.y + 5.0);
            ui.dot(cx, center, 9.0, p.background);
            ui.dot(cx, center, 7.0, color);
        }
        let mut x = toggle.pos.x + extent + 8.0;
        let mut right = row.pos.x + row.size.x;
        if new_session {
            if compact {
                let label = tr("importSession.title");
                let w = ui.button_width(cx, &label, Some("import"), ControlSize::Sm);
                let h = theme::control_height(ControlSize::Sm);
                ui.button(
                    cx,
                    rect(right - w, mid - h * 0.5, w, h),
                    "hdr:import",
                    &label,
                    Some("import"),
                    Variant::Ghost,
                    ControlSize::Sm,
                    true,
                );
                right -= w + 8.0;
            }
            // The page's own title ("New session" / "Add project"), like every other tab.
            if let Some(t) = target.as_ref() {
                let spec = ui.spec(
                    Font::Medium,
                    if compact { 14.0 } else { 16.0 },
                    p.foreground,
                );
                let w = (right - x).max(0.0);
                ui.text(
                    cx,
                    rect(x, row.pos.y, w, row.size.y),
                    spec,
                    &header_title(state, t),
                );
            }
        } else {
            // Right island: explorer toggle, git widgets (wide), scripts (wide), menu.
            let explorer_tip = crate::common::tooltip_with_shortcut(
                &tr("workspace.tabs.explorerSidebar.toggle"),
                "toggle-right-sidebar",
            );
            let explorer = island(
                &mut ui,
                cx,
                dvec2(right - slot * 0.5, mid),
                "hdr:explorer",
                &explorer_tip,
                compact,
            );
            ui.icon(cx, explorer, "panel-right", 16.0, p.muted);
            right -= slot;
            if header_git {
                // Git actions split button + open-in, laid out by their own widgets.
                // Sized to their last measured width (300px before the first measure: Open In
                // plus a long primary label and its caret); the title keeps at least 120px.
                let natural = if git_natural > 0.0 {
                    git_natural
                } else {
                    300.0
                };
                let w = natural.min((right - x - 120.0).max(0.0));
                if w > 0.0 {
                    let h = theme::control_height(ControlSize::Xs);
                    right_widgets = Some(rect(right - w, mid - h * 0.5, w, h));
                    right -= w + 4.0;
                }
            }
            if !compact && has_scripts {
                let scripts = island(
                    &mut ui,
                    cx,
                    dvec2(right - slot * 0.5, mid),
                    "hdr:scripts",
                    &tr("workspace.scripts.title"),
                    false,
                );
                ui.icon(cx, scripts, "play", 16.0, p.muted);
                right -= slot;
            }
            let menu = island(
                &mut ui,
                cx,
                dvec2(right - slot * 0.5, mid),
                "hdr:menu",
                &tr("shell.menu.sessionActions"),
                compact,
            );
            ui.icon(cx, menu, "ellipsis", 16.0, p.muted);
            right -= slot;
            // Title group.
            right -= 8.0;
            let title_text = target
                .as_ref()
                .map(|t| header_title(state, t))
                .unwrap_or_else(|| "Passession".to_string());
            let subtitle = compact && host.is_some();
            let w = (right - x).max(0.0);
            if subtitle {
                let spec = ui.spec(Font::Medium, 14.0, p.foreground);
                let title_h = text_line_height(20.0, 14.0);
                let subtitle_h = text_line_height(16.0, 12.0);
                let title_y = mid - (title_h + subtitle_h) * 0.5;
                ui.text(cx, rect(x, title_y, w, title_h), spec, &title_text);
                if let (Some(host), Some(target)) = (host, target.as_ref()) {
                    let project = workspace
                        .as_ref()
                        .map(|w| j::text(w, "projectDisplayName").to_string())
                        .unwrap_or_default();
                    let directory = target
                        .cwd
                        .split('/')
                        .rfind(|p| !p.is_empty())
                        .unwrap_or("")
                        .to_string();
                    let project = if project.is_empty() {
                        directory
                    } else {
                        project
                    };
                    header_subtitle(
                        &mut ui,
                        cx,
                        rect(x, title_y + title_h, w, subtitle_h),
                        &project,
                        host,
                    );
                }
            } else {
                let spec = ui.spec(
                    Font::Medium,
                    if compact { 14.0 } else { 16.0 },
                    p.foreground,
                );
                ui.text(cx, rect(x, row.pos.y, w, row.size.y), spec, &title_text);
            }
            x = right;
        }
        let _ = x;
    }
    if let (Some(slot), Some(target)) = (right_widgets, target.as_ref()) {
        draw_header_git(shell, cx, scope, slot, target);
    }
}

/// `_headerSubtitle`: project · server glyph + host label in the host colour (local hosts hide
/// it). A host that is not connected always shows, tinted, with its connection state.
fn header_subtitle(ui: &mut Ui, cx: &mut Cx2d, r: Rect, project: &str, host: Host<'_>) {
    let p = ui.p;
    let small = ui.spec(Font::Regular, 12.0, p.muted);
    let status = host.status();
    let offline = status != hosts::Status::Connected;
    let show_badge = !host.is_local() || offline;
    let badge_color = match status {
        hosts::Status::Connected => crate::shell::sheets::labels::label_color(&p, host.color()),
        hosts::Status::Connecting => p.warning,
        hosts::Status::Disconnected | hosts::Status::Error => p.danger,
    };
    let mut x = r.pos.x;
    let label = match status {
        hosts::Status::Connected => host.display_label(),
        hosts::Status::Connecting => tr_args(
            "shell.chrome.hostConnecting",
            &[("host", &host.display_label())],
        ),
        hosts::Status::Disconnected | hosts::Status::Error => tr_args(
            "shell.chrome.hostDisconnected",
            &[("host", &host.display_label())],
        ),
    };
    let label_cap = if offline { r.size.x * 0.6 } else { 120.0 };
    let reserved = if show_badge {
        12.0 + 3.0
            + ui.measure(cx, small, &label).min(label_cap)
            + if project.is_empty() { 0.0 } else { 18.0 }
    } else {
        0.0
    };
    if !project.is_empty() {
        let w = (r.size.x - reserved).max(0.0);
        let drawn = ui.text(cx, rect(x, r.pos.y, w, r.size.y), small, project);
        x = drawn.pos.x + drawn.size.x;
    }
    if show_badge {
        if !project.is_empty() {
            let sep = ui.spec(Font::Regular, 12.0, p.muted);
            ui.text(cx, rect(x + 6.0, r.pos.y, 8.0, r.size.y), sep, "·");
            x += 18.0;
        }
        ui.icon(
            cx,
            rect(x, r.pos.y + 2.0, 12.0, 12.0),
            if offline { "cloud-off" } else { "server" },
            12.0,
            badge_color,
        );
        x += 15.0;
        let spec = ui.spec(Font::Regular, 12.0, badge_color);
        ui.text(
            cx,
            rect(x, r.pos.y, (r.pos.x + r.size.x - x).max(0.0), r.size.y),
            spec,
            &label,
        );
    }
}

fn draw_header_git(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    slot: Rect,
    target: &PanelTarget,
) {
    if shell.data.header_git.is_none() {
        let (git, open_in) = panels::header_widgets(cx, &shell.templates);
        shell.data.header_git = Some(git);
        shell.data.header_open_in = Some(open_in);
    }
    let (Some(git), Some(open_in)) = (
        shell.data.header_git.clone(),
        shell.data.header_open_in.clone(),
    ) else {
        return;
    };
    panels::set_header_target(cx, &git, &open_in, target);
    cx.begin_turtle(
        Walk::abs_rect(slot),
        Layout {
            flow: Flow::Right,
            spacing: 4.0,
            align: Align { x: 1.0, y: 0.5 },
            ..Layout::default()
        },
    );
    open_in.draw_all(cx, scope);
    git.draw_all(cx, scope);
    cx.end_turtle();
    // Measure what the widgets actually used; a change relayouts the header next frame.
    let (a, b) = (open_in.area().rect(cx), git.area().rect(cx));
    let used = [a, b]
        .iter()
        .filter(|r| r.size.x > 0.0)
        .map(|r| (r.pos.x, r.pos.x + r.size.x))
        .fold(None, |acc: Option<(f64, f64)>, (l, r)| {
            Some(acc.map_or((l, r), |(al, ar)| (al.min(l), ar.max(r))))
        })
        .map_or(0.0, |(l, r)| (r - l).ceil());
    if used > 0.0 && (used - shell.data.chrome.header_git_w).abs() > 0.5 {
        shell.data.chrome.header_git_w = used;
        shell.invalidate_region(cx, crate::shell::ShellRegion::Chrome);
    }
}

const MIN_MAIN: f64 = 320.0;
const MIN_SIDEBAR: f64 = 200.0;
const MIN_EXPLORER: f64 = 180.0;
const MIN_SPLIT_WIDTH: f64 = 240.0;
const MIN_SPLIT_HEIGHT: f64 = 160.0;
const DIVIDER: f64 = 5.0;

#[derive(Default)]
struct SideWidths {
    sidebar: f64,
    explorer: f64,
    sidebar_stepped_aside: bool,
}

fn side_widths(layout: &WindowLayout, width: f64, min_main: f64, page_shown: bool) -> SideWidths {
    let explorer_visible = layout.explorer_visible && !page_shown;
    let sidebar_stepped_aside = layout.sidebar_visible
        && explorer_visible
        && width < MIN_SIDEBAR + min_main + MIN_EXPLORER + 2.0 * DIVIDER;
    let sidebar_visible = layout.sidebar_visible && !sidebar_stepped_aside;
    let mut sidebar = if sidebar_visible {
        layout.sidebar_width
    } else {
        0.0
    };
    let mut explorer = if explorer_visible {
        layout.explorer_width
    } else {
        0.0
    };
    let dividers = f64::from(u8::from(sidebar_visible) + u8::from(explorer_visible)) * DIVIDER;
    let mut over = sidebar + explorer + dividers + min_main - width;
    // Let the side columns yield before squeezing the split tree. Stepping aside is
    // presentation only; the saved widths and sidebar visibility stay intact.
    if over > 0.0 && explorer_visible {
        let cut = over.min(explorer - MIN_EXPLORER);
        explorer -= cut;
        over -= cut;
    }
    if over > 0.0 && sidebar_visible {
        sidebar -= over.min(sidebar - MIN_SIDEBAR);
    }
    SideWidths {
        sidebar,
        explorer,
        sidebar_stepped_aside,
    }
}

/// Minimums of both child subtrees in the same pre-order as `draw_node`.
fn measure_split_minima(node: &Node, minima: &mut Vec<[DVec2; 2]>) -> DVec2 {
    let Node::Split {
        axis,
        first,
        second,
        ..
    } = node
    else {
        return dvec2(MIN_SPLIT_WIDTH, MIN_SPLIT_HEIGHT);
    };
    let index = minima.len();
    minima.push([DVec2::default(); 2]);
    let a = measure_split_minima(first, minima);
    let b = measure_split_minima(second, minima);
    minima[index] = [a, b];
    match axis {
        Axis::Horizontal => dvec2(a.x + DIVIDER + b.x, a.y.max(b.y)),
        Axis::Vertical => dvec2(a.x.max(b.x), a.y + DIVIDER + b.y),
    }
}

fn draw_wide(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    content: Rect,
    page_shown: bool,
    columns: SideWidths,
    minima: &[[DVec2; 2]],
) {
    let p = theme::current_palette();
    let SideWidths {
        sidebar, explorer, ..
    } = columns;
    let mut x = content.pos.x;
    if sidebar > 0.0 {
        let side = rect(x, content.pos.y, sidebar, content.size.y);
        shell.data.chrome.sidebar_rect = side;
        shell.data.chrome.slots.push(Slot::Sidebar(side));
        x += sidebar;
        let handle = rect(x, content.pos.y, DIVIDER, content.size.y);
        shell.painter.fill(cx, handle, p.border, 0.0);
        shell.data.hits.push(handle, "resize:sidebar".into());
        x += DIVIDER;
    }
    let main_right = content.pos.x + content.size.x
        - if explorer > 0.0 {
            explorer + DIVIDER
        } else {
            0.0
        };
    let main = rect(x, content.pos.y, main_right - x, content.size.y);
    if page_shown {
        draw_page(shell, cx, main);
    } else if let Some(maximized) = shell.data.layout.maximized_pane.as_deref() {
        if let Some(pane) = shell.data.layout.tree.pane(maximized).cloned() {
            draw_pane(
                shell,
                cx,
                state,
                &pane.id,
                &pane.tabs,
                pane.selected.as_deref(),
                main,
                false,
            );
        }
    } else {
        let tree = shell.data.layout.tree.clone();
        draw_node(shell, cx, state, &tree, main, &mut minima.iter());
    }
    if explorer > 0.0 {
        let handle = rect(main_right, content.pos.y, DIVIDER, content.size.y);
        shell.painter.fill(cx, handle, p.border, 0.0);
        shell.data.hits.push(handle, "resize:explorer".into());
        let ex = rect(main_right + DIVIDER, content.pos.y, explorer, content.size.y);
        shell.data.chrome.explorer_rect = ex;
        let (keys, selected) = explorer_keys(shell);
        draw_pane(
            shell,
            cx,
            state,
            "explorer",
            &keys,
            selected.as_deref(),
            ex,
            true,
        );
    }
}

fn draw_node(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    node: &Node,
    r: Rect,
    minima: &mut std::slice::Iter<'_, [DVec2; 2]>,
) {
    let p = theme::current_palette();
    match node {
        Node::Pane(pane) => {
            draw_pane(
                shell,
                cx,
                state,
                &pane.id,
                &pane.tabs,
                pane.selected.as_deref(),
                r,
                false,
            );
        }
        Node::Split {
            id,
            axis,
            ratio,
            first,
            second,
        } => {
            let child_minima = minima
                .next()
                .expect("measured split")
                .map(|v| along(*axis, v));
            let horizontal = *axis == Axis::Horizontal;
            let extent = along(*axis, r.size);
            let divider = extent.min(DIVIDER);
            let available = extent - divider;
            let a = split_first(available, *ratio, child_minima);
            let (r1, handle, r2) = if horizontal {
                (
                    rect(r.pos.x, r.pos.y, a, r.size.y),
                    rect(r.pos.x + a, r.pos.y, divider, r.size.y),
                    rect(r.pos.x + a + divider, r.pos.y, available - a, r.size.y),
                )
            } else {
                (
                    rect(r.pos.x, r.pos.y, r.size.x, a),
                    rect(r.pos.x, r.pos.y + a, r.size.x, divider),
                    rect(r.pos.x, r.pos.y + a + divider, r.size.x, available - a),
                )
            };
            draw_node(shell, cx, state, first, r1, minima);
            shell.painter.fill(cx, handle, p.border, 0.0);
            shell.data.hits.push(handle, format!("split:{id}"));
            shell.data.chrome.splits.push(SplitHandle {
                id: id.clone(),
                axis: *axis,
                start: along(*axis, r.pos),
                available,
                minima: child_minima,
                first: a,
            });
            draw_node(shell, cx, state, second, r2, minima);
        }
    }
}

/// Display the saved ratio when it fits. In a shortage both subtrees lose the same
/// share of their minimums, so a nested split is not squeezed into one leaf's space.
fn split_first(available: f64, ratio: f64, [first, second]: [f64; 2]) -> f64 {
    let total = first + second;
    if available <= total {
        return available * first / total;
    }
    (available * ratio.clamp(0.15, 0.85)).clamp(first, available - second)
}

/// One pane (`_pane`): 38px tab strip + divider (wide, or the compact Explorer), then the
/// selected root or the empty state.
#[allow(clippy::too_many_arguments)]
fn draw_pane(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    pane_id: &str,
    keys: &[String],
    selected: Option<&str>,
    r: Rect,
    explorer: bool,
) {
    let p = theme::current_palette();
    let compact = shell.data.compact;
    shell.data.chrome.pane_rects.push((pane_id.to_string(), r));
    let mut content = r;
    if !compact || explorer {
        // Phone Explorer strip: at least a 44px touch row.
        let bar_h = if compact {
            (18.0 + text_line_height(20.0, 14.0)).max(44.0)
        } else {
            22.0 + text_line_height(16.0, 12.0)
        };
        let bar = rect(r.pos.x, r.pos.y, r.size.x, bar_h);
        shell.data.chrome.tab_bars.push((pane_id.to_string(), bar));
        draw_tab_strip(shell, cx, state, pane_id, keys, selected, bar, explorer);
        shell
            .painter
            .hline(cx, r.pos.x, r.pos.y + bar_h, r.size.x, p.border);
        content = rect(
            r.pos.x,
            r.pos.y + bar_h + 1.0,
            r.size.x,
            (r.size.y - bar_h - 1.0).max(0.0),
        );
    }
    if shell.data.chrome.drop_pane.as_deref() == Some(pane_id) && !explorer {
        shell
            .painter
            .outline(cx, r, Vec4::default(), p.primary, 1.0, 0.0);
    }
    let Some(key) = selected.map(str::to_string).filter(|_| !keys.is_empty()) else {
        if explorer {
            draw_explorer_empty(shell, cx, content);
            return;
        }
        let has_sessions = hosts::hosts(state)
            .iter()
            .any(|host| host.agents().next().is_some());
        draw_empty(shell, cx, content, has_sessions);
        return;
    };
    place_root(shell, cx, &key, content);
}

/// Mounts a tab root on first use and queues its retained layer.
fn place_root(shell: &mut PaseoShell, cx: &mut Cx2d, key: &str, r: Rect) {
    if !shell.data.panels.contains_key(key) {
        let Some(target) = shell.data.layout.target(key).cloned() else {
            return;
        };
        match panels::mount(cx, &shell.templates, &target) {
            Some(panel) => {
                shell.data.panels.insert(key.to_string(), panel);
                shell.data.dispatch_layout = None;
                SignalToUI::set_ui_signal();
            }
            None => {
                draw_error(
                    shell,
                    cx,
                    r,
                    &tr_args("shell.chrome.unrecognizedPanel", &[("kind", &target.kind)]),
                );
                return;
            }
        }
    }
    if shell.data.chrome.overlay_pass {
        shell.data.chrome.explorer_slot = Some(Slot::Root {
            key: key.to_owned(),
            rect: r,
        });
    } else {
        shell.data.chrome.slots.push(Slot::Root {
            key: key.to_string(),
            rect: r,
        });
    }
}

/// Draws a mounted tab root in its retained layer.
fn paint_root(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, key: &str, r: Rect) {
    let focused = focused_tab(shell).as_deref() == Some(key);
    let app_active = !shell.data.app_inactive;
    let compact = shell.data.compact;
    let Some(panel) = shell.data.panels.get_mut(key) else {
        return;
    };
    let previous = cx.set_timer_target(panel.widget.widget_uid().0);
    shell.data.visibility_sync_pending |= !panel.visible;
    panels::set_activity(cx, panel, true, focused);
    // A notification route asked to show what this session waits on.
    if shell.data.reveal_attention.as_deref() == Some(key) {
        shell.data.reveal_attention = None;
        panels::reveal_attention(cx, panel);
    }
    let widget = panel.widget.clone();
    let target = panel.target.clone();
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        ui.route = Route::from_target(&target);
        ui.target = Some(target);
        ui.tab_key = Some(key.to_string());
        ui.activity = crate::common::PanelActivity {
            visible: true,
            focused,
            app_active,
        };
    }
    // Focus, activity and width class reach the root through `UiContext`, not its areas.
    let inputs = layer::inputs((focused, app_active, compact));
    if panel.layer.begin(
        cx,
        r,
        inputs,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        // See `open_batches`; `panel` borrows `shell.data`.
        shell.painter.draw_box.paint(cx, r, Vec4::default(), 0.0);
        cx.begin_turtle(Walk::abs_rect(r), Layout::default());
        widget.draw_walk_all(cx, scope, Walk::fill());
        cx.end_turtle();
        panel.layer.end(cx, &shell.data.hits, &shell.data.tooltips);
    }
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        ui.target = None;
        ui.tab_key = None;
    }
    cx.set_timer_owner(previous);
}

/// Activate retained panels before draw. Draw-list invalidations are ignored during
/// paint, so waiting until then would capture the old transcript for the transition.
pub(super) fn sync_panel_activity(shell: &mut PaseoShell, cx: &mut Cx) {
    let layout = shell.dispatch_layout();
    if !std::mem::take(&mut shell.data.visibility_sync_pending) {
        return;
    }
    for (key, panel) in shell.data.panels.iter_mut() {
        let visible = layout.visible.contains(key);
        let focused = visible && layout.focused.as_ref() == Some(key);
        if panel.visible != visible || panel.focused != focused {
            let previous = cx.set_timer_target(panel.widget.widget_uid().0);
            panels::set_activity(cx, panel, visible, focused);
            cx.set_timer_owner(previous);
        }
    }
}

fn draw_error(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, message: &str) {
    let p = theme::current_palette();
    let t = theme::current_typography();
    let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.danger);
    let w = (r.size.x - 32.0).max(40.0);
    shell
        .painter
        .paragraph(cx, dvec2(r.pos.x + 16.0, r.pos.y + 16.0), w, spec, message);
}

/// `_welcome`: EmptyState "Passession" + new session button. With sessions on a host the
/// message points to them instead of claiming there are none.
fn draw_empty(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, has_sessions: bool) {
    let mut ui = make_ui(shell);
    let p = ui.p;
    let title = ui.spec(Font::Medium, 18.0, p.foreground);
    let message = ui.spec(Font::Regular, 14.0, p.muted);
    let cy = r.pos.y + r.size.y * 0.5;
    let title_h = text_line_height(24.0, 18.0);
    let message_h = text_line_height(20.0, 14.0);
    ui.text_centered(
        cx,
        rect(r.pos.x, cy - 12.0 - message_h - title_h, r.size.x, title_h),
        title,
        "Passession",
    );
    let hint = tr(if has_sessions {
        "sessionNavigation.pick"
    } else {
        "sessionNavigation.empty"
    });
    ui.text_centered(
        cx,
        rect(
            r.pos.x + 16.0,
            cy - 8.0 - message_h,
            r.size.x - 32.0,
            message_h,
        ),
        message,
        &hint,
    );
    let label = tr("sessionNavigation.newSession");
    let w = ui.button_width(cx, &label, Some("plus"), ControlSize::Sm);
    ui.button(
        cx,
        rect(
            r.pos.x + (r.size.x - w) * 0.5,
            cy + 4.0,
            w,
            theme::control_height(ControlSize::Sm),
        ),
        "empty:new",
        &label,
        Some("plus"),
        Variant::Primary,
        ControlSize::Sm,
        true,
    );
}

/// The Explorer pane with no directory context (no session focused): a muted, wrapped,
/// pane-specific note with no call to action, so it never echoes the main empty state.
fn draw_explorer_empty(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect) {
    let mut ui = make_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 13.0, p.muted);
    let message = tr("shell.chrome.explorerEmpty");
    let w = (r.size.x - 48.0).clamp(0.0, 280.0);
    let text_h = ui.painter.paragraph_height(cx, w, spec, &message);
    let top = r.pos.y + (r.size.y - (20.0 + 12.0 + text_h)) * 0.5;
    let center = r.pos.x + r.size.x * 0.5;
    ui.icon(
        cx,
        rect(center - 10.0, top, 20.0, 20.0),
        "folder-open",
        20.0,
        p.muted,
    );
    let tw = ui.painter.measure(cx, spec, &message).x.min(w);
    ui.painter
        .paragraph(cx, dvec2(center - tw * 0.5, top + 32.0), tw, spec, &message);
}

/// Tab strip (`ReorderableListView` of chips): wide chips with icon, title, modified dot,
/// ellipsis menu and close; the compact Explorer rail uses filled pills.
#[allow(clippy::too_many_arguments)]
fn draw_tab_strip(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    pane_id: &str,
    keys: &[String],
    selected: Option<&str>,
    bar: Rect,
    explorer: bool,
) {
    let compact = shell.data.compact;
    let pill = explorer && compact;
    let dragging = match &shell.data.chrome.drag {
        Some(Drag::Tab {
            key, moving: true, ..
        }) => Some(key.clone()),
        _ => None,
    };
    let palette = theme::current_palette();
    let typography = theme::current_typography();
    let mut tabs: Vec<_> = keys
        .iter()
        .filter_map(|key| {
            let target = shell.data.layout.target(key)?;
            let active = selected == Some(key.as_str());
            let spec = if pill {
                TextSpec::new(
                    Font::Regular,
                    typography.ui_size(14.0),
                    if active {
                        palette.foreground
                    } else {
                        palette.muted
                    },
                )
            } else {
                TextSpec::new(Font::Medium, typography.ui_size(12.0), palette.foreground)
            };
            Some((
                target.clone(),
                key.clone(),
                active,
                title(state, target),
                spec,
                shell.data.dirty.contains(key),
                0.0,
            ))
        })
        .collect();
    let mut tab_rects = Vec::new();
    let mut shell_overflow: Option<Vec<String>> = None;
    {
        let mut ui = make_ui(shell);
        let p = ui.p;
        // Reserved trailing slots: the pane's new-session button, or the Explorer's "+" and close.
        let trailing = if explorer { 72.0 } else { 40.0 };
        let mut x = bar.pos.x;
        let limit = bar.pos.x + bar.size.x - trailing;
        let available = (limit - bar.pos.x).max(0.0);
        let chrome_w = if pill {
            10.0 + 14.0 + 6.0 + 10.0
        } else {
            10.0 + 15.0 + 7.0 + 2.0 + 28.0 * 2.0
        };
        let gap = if pill { 2.0 } else { 0.0 };
        // Desktop Explorer: only the active tab reserves its menu/close slot; inactive tabs take
        // their label's width and show close on hover, so labels fit before anything ellipsizes.
        let slim = explorer && !pill;
        let slim_chrome_w = 10.0 + 15.0 + 7.0 + 10.0;
        // Without its menu and close (a narrow chip drops them), a tab needs this beside its label.
        let bare_chrome_w = if pill {
            10.0 + 14.0 + 6.0 + 10.0
        } else {
            slim_chrome_w
        };
        let icon_tab_w = 10.0 + if pill { 14.0 } else { 15.0 } + 10.0;
        let mut bare_chrome = Vec::with_capacity(tabs.len());
        let mut icon_widths = Vec::with_capacity(tabs.len());
        for (_, _, active, label, spec, modified, width) in &mut tabs {
            let text_w = ui.measure(cx, *spec, label);
            let dot = if *modified { 12.0 } else { 0.0 };
            let chrome = if slim && !*active {
                slim_chrome_w
            } else {
                chrome_w
            };
            bare_chrome.push(bare_chrome_w + dot);
            icon_widths.push(icon_tab_w + dot);
            *width = (chrome + text_w + dot).min(240.0).min(available);
        }
        let natural: Vec<f64> = tabs.iter().map(|t| t.6).collect();
        let active = tabs.iter().position(|tab| tab.2).unwrap_or(0);
        let fit = fit_tabs(
            &natural,
            active,
            available,
            gap,
            // Tight strips shrink tabs down to icon + ellipsized label (actions drop out first)
            // before any tab collapses into the overflow chip.
            (10.0 + if pill { 14.0 } else { 15.0 } + 10.0 + 24.0).min(available),
            36.0,
        );
        let mut widths = fit.widths;
        // A label cut to a letter or two says less than the icon: such tabs show the icon
        // (full title in the tooltip) and their room goes to the other labels.
        let icon_only = collapse_labels(
            &mut widths,
            &natural,
            &bare_chrome,
            &icon_widths,
            active,
            fit.first..fit.end,
            36.0,
        );
        for (tab, width) in tabs.iter_mut().zip(&widths) {
            tab.6 = *width;
        }
        let overflow_w = 36.0;
        let (first, end) = (fit.first, fit.end);
        let hidden: Vec<String> = tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < first || *i >= end)
            .map(|(_, t)| t.1.clone())
            .collect();
        let shown = end.saturating_sub(first);
        for (index, (target, key, active, label, label_spec, modified, w)) in
            tabs.into_iter().enumerate().skip(first).take(shown)
        {
            let icon_only = icon_only[index];
            let chip = if pill {
                rect(x + 2.0, bar.pos.y + 5.0, w, bar.size.y - 10.0)
            } else {
                rect(x, bar.pos.y, w, bar.size.y)
            };
            let tab_id = format!("tab:{pane_id}:{key}");
            let fade = if dragging.as_deref() == Some(key.as_str()) {
                0.5
            } else {
                1.0
            };
            if pill {
                if active {
                    ui.fill(cx, chip, p.surface2, 6.0);
                }
            } else if active {
                ui.fill(cx, chip, p.surface4, 0.0);
                ui.fill(
                    cx,
                    rect(chip.pos.x, chip.pos.y + chip.size.y - 2.0, chip.size.x, 2.0),
                    p.primary,
                    0.0,
                );
            } else if ui.is_hover(&tab_id) {
                ui.fill(cx, chip, p.interaction_highlight, 0.0);
            }
            let fg = theme::fade(
                if pill && !active {
                    p.muted
                } else {
                    p.foreground
                },
                fade,
            );
            let mut cx_x = chip.pos.x + 10.0;
            let icon_size = if pill { 14.0 } else { 15.0 };
            let icon_rect = rect(
                cx_x,
                chip.pos.y + (chip.size.y - icon_size) * 0.5,
                icon_size,
                icon_size,
            );
            tab_icon(&mut ui, cx, state, &target, icon_rect, fg);
            cx_x += icon_size + if pill { 6.0 } else { 7.0 };
            // Tab menu and close need their 58px plus a little title; a narrower chip keeps
            // only icon and title (its menu stays on right-click) so no action lands outside it.
            let actions =
                !pill && (!slim || active) && chip.size.x >= 10.0 + 15.0 + 7.0 + 58.0 + 24.0;
            // Slim inactive Explorer tabs show a close button only while hovered.
            let close_id = format!("tabclose:{pane_id}:{key}");
            let hover_close = slim
                && !active
                && chip.size.x >= 10.0 + 15.0 + 7.0 + 30.0
                && (ui.is_hover(&tab_id) || ui.is_hover(&close_id));
            let buttons_w = if actions {
                2.0 + 56.0
            } else if hover_close {
                2.0 + 28.0
            } else {
                10.0
            };
            let dot_w = if modified { 12.0 } else { 0.0 };
            let text_rect = rect(
                cx_x,
                chip.pos.y,
                (chip.pos.x + chip.size.x - buttons_w - dot_w - cx_x).max(0.0),
                chip.size.y,
            );
            let drawn = if icon_only {
                rect(icon_rect.pos.x, chip.pos.y, icon_size, chip.size.y)
            } else {
                ui.text(
                    cx,
                    text_rect,
                    crate::common::paint::TextSpec {
                        color: fg,
                        ..label_spec
                    },
                    &label,
                )
            };
            if modified {
                let dot_x = drawn.pos.x + drawn.size.x + 4.0 + 4.0;
                ui.dot(
                    cx,
                    dvec2(dot_x, chip.pos.y + chip.size.y * 0.5),
                    8.0,
                    p.muted,
                );
            }
            // Phone pills keep their inset visual but take the full bar height as the hit.
            let tab_hit = if pill {
                rect(chip.pos.x, bar.pos.y, chip.size.x, bar.size.y)
            } else {
                chip
            };
            // A shrunk tab keeps its full title as a tooltip.
            let truncated =
                icon_only || ui.measure(cx, label_spec, &label) > text_rect.size.x + 0.5;
            ui.control(
                cx,
                tab_hit,
                &tab_id,
                &label,
                AccessibilityRole::ListItem,
                AccessibilityState {
                    selected: active,
                    ..AccessibilityState::default()
                },
            );
            if truncated {
                ui.tooltip(&tab_id, &label);
            }
            if actions {
                let menu = rect(
                    chip.pos.x + chip.size.x - 58.0,
                    chip.pos.y + (chip.size.y - 28.0) * 0.5,
                    28.0,
                    28.0,
                );
                ui.icon_button(
                    cx,
                    menu,
                    &format!("tabmenu:{pane_id}:{key}"),
                    "ellipsis",
                    14.0,
                    14.0,
                    &tr_args("workspace.tabs.menu.openFor", &[("label", &label)]),
                );
                let close = rect(
                    chip.pos.x + chip.size.x - 30.0,
                    chip.pos.y + (chip.size.y - 28.0) * 0.5,
                    28.0,
                    28.0,
                );
                let close_tip = if active {
                    crate::common::tooltip_with_shortcut(
                        &tr("common.actions.close"),
                        "workspace-tab-close-current",
                    )
                } else {
                    tr("common.actions.close")
                };
                ui.icon_button(
                    cx,
                    close,
                    &format!("tabclose:{pane_id}:{key}"),
                    "x",
                    14.0,
                    14.0,
                    &close_tip,
                );
            }
            if hover_close {
                let close = rect(
                    chip.pos.x + chip.size.x - 30.0,
                    chip.pos.y + (chip.size.y - 28.0) * 0.5,
                    28.0,
                    28.0,
                );
                ui.icon_button(
                    cx,
                    close,
                    &close_id,
                    "x",
                    14.0,
                    14.0,
                    &tr("common.actions.close"),
                );
            }
            tab_rects.push((pane_id.to_string(), key.clone(), chip));
            x = chip.pos.x + chip.size.x + gap;
        }
        if !hidden.is_empty() {
            let id = format!("taboverflow:{pane_id}");
            let button = rect(
                x + 4.0,
                bar.pos.y + (bar.size.y - 28.0) * 0.5,
                overflow_w - 8.0,
                28.0,
            );
            if ui.is_hover(&id) {
                ui.fill(cx, button, p.interaction_highlight, 6.0);
            }
            let spec = ui.spec(Font::Medium, 12.0, p.muted);
            ui.text_centered(cx, button, spec, &format!("+{}", hidden.len()));
            ui.hit_tip(
                cx,
                button,
                &id,
                &tr_args(
                    "workspace.tabs.overflow",
                    &[("count", &hidden.len().to_string())],
                ),
            );
            x = button.pos.x + button.size.x;
            shell_overflow = Some(hidden);
        }
        // Trailing controls.
        if explorer {
            let plus = rect(x + 4.0, bar.pos.y + (bar.size.y - 28.0) * 0.5, 28.0, 28.0);
            ui.icon_button(
                cx,
                plus,
                "explorer:new",
                "plus",
                14.0,
                6.0,
                &tr("workspace.tabs.actions.newTab"),
            );
            if compact {
                ui.hit(rect(x, bar.pos.y, 36.0, bar.size.y), "explorer:new");
            }
            if compact {
                let close = rect(
                    bar.pos.x + bar.size.x - 36.0,
                    bar.pos.y + (bar.size.y - 32.0) * 0.5,
                    32.0,
                    32.0,
                );
                ui.icon_button(
                    cx,
                    close,
                    "explorer:close",
                    "x",
                    16.0,
                    16.0,
                    &tr("workspace.tabs.explorerSidebar.close"),
                );
                ui.hit(
                    rect(bar.pos.x + bar.size.x - 40.0, bar.pos.y, 40.0, bar.size.y),
                    "explorer:close",
                );
            } else {
                let close = rect(
                    bar.pos.x + bar.size.x - 36.0,
                    bar.pos.y + (bar.size.y - 32.0) * 0.5,
                    32.0,
                    32.0,
                );
                ui.icon_button(
                    cx,
                    close,
                    "explorer:close",
                    "x",
                    18.0,
                    16.0,
                    &tr("common.actions.close"),
                );
            }
        } else {
            let plus = rect(bar.pos.x + bar.size.x - 38.0, bar.pos.y + 2.0, 34.0, 34.0);
            ui.icon_button(
                cx,
                plus,
                &format!("panenew:{pane_id}"),
                "plus",
                18.0,
                17.0,
                &crate::common::tooltip_with_shortcut(
                    &tr("sessionNavigation.newSession"),
                    "workspace-tab-target-agent",
                ),
            );
        }
    }
    shell.data.chrome.tab_rects.extend(tab_rects);
    if let Some(hidden) = shell_overflow {
        shell
            .data
            .chrome
            .tab_overflow
            .push((pane_id.to_string(), hidden));
    }
}

/// `_tabIcon`: agent tabs show the provider glyph with the agent's status badge.
pub fn tab_icon(
    ui: &mut Ui,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    target: &PanelTarget,
    r: Rect,
    color: Vec4,
) {
    if target.kind == "agent"
        && let Some(agent) = agent_row(state, target)
    {
        let provider = j::text(agent, "provider");
        if !provider.is_empty() {
            // Flutter `_tabIcon` passes no host snapshot SVG: built-in, then catalog, then bot.
            ProviderIcon::paint(ui.painter, ui.draw_icon, cx, r, provider, None, color);
            let bucket = agent
                .get("status")
                .and_then(Value::as_str)
                .is_some()
                .then(|| agent_live_status_bucket(agent));
            let p = ui.p;
            paint_status_badge(
                ui.painter,
                cx,
                r,
                bucket.filter(|b| *b != StatusBucket::Done),
                &p,
                p.surface0,
            );
            return;
        }
    }
    ui.icon(cx, r, icons::for_tab_kind(&target.kind), r.size.y, color);
}

/// Lays out the shown page: the sessions page (shell-drawn) or a mounted page root.
fn draw_page(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect) {
    let Some(page) = shell.data.page.clone() else {
        return;
    };
    if matches!(page, crate::shell::Page::Sessions { .. }) {
        shell.data.chrome.slots.push(Slot::Sessions(r));
        return;
    }
    if shell
        .data
        .page_root
        .as_ref()
        .is_none_or(|(p, _)| *p != page)
    {
        let widget = panels::page_widget(cx, &shell.templates, &page);
        shell.data.page_root = Some((page.clone(), widget));
    }
    shell.data.chrome.slots.push(Slot::Page(r));
}

fn paint_page(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, r: Rect) {
    let Some((page, widget)) = shell.data.page_root.clone() else {
        return;
    };
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        ui.route = commands::page_route(&page);
        ui.target = None;
        ui.tab_key = None;
        ui.activity = crate::common::PanelActivity::default();
    }
    // A different page root replaces the retained paint.
    let inputs = layer::inputs(widget.widget_uid().0);
    if shell.layers.page.begin(
        cx,
        r,
        inputs,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        open_batches(shell, cx, r);
        cx.begin_turtle(Walk::abs_rect(r), Layout::default());
        let previous = cx.set_timer_target(widget.widget_uid().0);
        widget.draw_walk_all(cx, scope, Walk::fill());
        cx.set_timer_owner(previous);
        cx.end_turtle();
        shell
            .layers
            .page
            .end(cx, &shell.data.hits, &shell.data.tooltips);
    }
}

fn draw_compact(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    content: Rect,
    page_shown: bool,
) {
    if page_shown {
        draw_page(shell, cx, content);
        return;
    }
    match shell.data.layout.compact_destination {
        CompactDestination::Sidebar => {
            shell.data.chrome.sidebar_rect = content;
            shell.data.chrome.slots.push(Slot::Sidebar(content));
        }
        _ => {
            let pane = shell.data.layout.focused().clone();
            draw_pane(
                shell,
                cx,
                state,
                &pane.id,
                &pane.tabs,
                pane.selected.as_deref(),
                content,
                false,
            );
        }
    }
}

/// Compact Explorer overlay (`_compactExplorerOverlay`): slides from the right over a 50%
/// black backdrop. Drawn by the overlay layer so it covers the conversation.
pub fn draw_compact_explorer(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
) {
    if !shell.data.compact || !shell.data.chrome.explorer_visible() {
        shell.data.chrome.explorer_layer.finish_frame(cx);
        if let Some(list) = &mut shell.data.chrome.explorer_list {
            list.release(cx);
        }
        shell.data.chrome.explorer_scrim = Area::Empty;
        return;
    }
    let Some(state) = scope.data.get::<UiContext>().map(|ui| ui.state.clone()) else {
        return;
    };
    shell.data.chrome.explorer_window = window;
    let visual = Intent::OpenExplorer
        .resolve()
        .sample(shell.data.chrome.explorer.value(), window.size);
    shell.motion_scrim.new_draw_call(cx);
    shell.motion_scrim.color = vec4(0.0, 0.0, 0.0, (visual.scrim * 0.5) as f32);
    shell.motion_scrim.draw_abs(cx, window);
    shell.data.chrome.explorer_scrim = shell.motion_scrim.area();
    let mut list = shell
        .data
        .chrome
        .explorer_list
        .take()
        .unwrap_or_else(|| DrawList2d::new(cx));
    list.begin_always(cx);
    let insets = crate::shell::platform::safe_insets();
    let inputs = layer::inputs((
        shell.data.layout.revision,
        insets.left.to_bits(),
        insets.right.to_bits(),
        insets.top.to_bits(),
        insets.bottom.to_bits(),
    ));
    std::mem::swap(&mut shell.painter, &mut shell.overlay_painter);
    std::mem::swap(&mut shell.data.hits, &mut shell.data.overlays.hits);
    std::mem::swap(&mut shell.data.tooltips, &mut shell.data.overlays.tooltips);
    shell.data.hits.push(window, "compact:backdrop".to_owned());
    if shell.data.chrome.explorer_layer.begin(
        cx,
        window,
        inputs,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        let p = theme::current_palette();
        shell.painter.fill(cx, window, p.sidebar, 0.0);
        shell.data.hits.push(window, "compact:panel".to_owned());
        let inner = rect(
            window.pos.x + insets.left,
            window.pos.y + insets.top + 8.0,
            window.size.x - insets.left - insets.right,
            window.size.y - insets.top - 8.0 - insets.bottom,
        );
        let (keys, selected) = explorer_keys(shell);
        let chrome = &mut shell.data.chrome;
        chrome.explorer_rect = inner;
        chrome.pane_rects.retain(|(pane, _)| pane != "explorer");
        chrome.tab_bars.retain(|(pane, _)| pane != "explorer");
        chrome.tab_rects.retain(|(pane, _, _)| pane != "explorer");
        chrome.tab_overflow.retain(|(pane, _)| pane != "explorer");
        chrome.explorer_slot = None;
        chrome.overlay_pass = true;
        draw_pane(
            shell,
            cx,
            &state,
            "explorer",
            &keys,
            selected.as_deref(),
            inner,
            true,
        );
        shell.data.chrome.overlay_pass = false;
        shell
            .data
            .chrome
            .explorer_layer
            .end(cx, &shell.data.hits, &shell.data.tooltips);
    }
    if let Some(slot) = shell.data.chrome.explorer_slot.clone() {
        draw_slot(shell, cx, scope, slot);
    }
    // During motion the unshifted hit geometry is deliberately never dispatched.
    if shell.data.chrome.explorer.is_moving() {
        shell.data.hits.clear();
        shell.data.tooltips.clear();
    }
    std::mem::swap(&mut shell.data.tooltips, &mut shell.data.overlays.tooltips);
    std::mem::swap(&mut shell.data.hits, &mut shell.data.overlays.hits);
    std::mem::swap(&mut shell.painter, &mut shell.overlay_painter);
    shell.data.chrome.explorer_layer.finish_frame(cx);
    list.end(cx);
    shell.data.chrome.explorer_list = Some(list);
    place_explorer(shell, cx);
}

pub(super) fn accessibility_explorer_root(shell: &PaseoShell) -> Option<DrawListId> {
    (!shell.data.chrome.explorer.is_moving())
        .then(|| {
            shell
                .data
                .chrome
                .explorer_list
                .as_ref()
                .map(|list| list.id())
        })
        .flatten()
}

// ---------------------------------------------------------------------------------------------
// Events

pub(crate) fn sidebar_visible(shell: &PaseoShell) -> bool {
    if matches!(
        shell.data.page.as_ref(),
        Some(crate::shell::Page::Settings { .. })
    ) {
        return false;
    }
    if shell.data.compact {
        shell.data.page.is_none()
            && shell.data.layout.compact_destination == CompactDestination::Sidebar
    } else {
        shell.data.layout.sidebar_visible && !shell.data.chrome.sidebar_stepped_aside
    }
}

/// The wide sidebar toggle. A sidebar that stepped aside for the Explorer comes back by closing
/// the Explorer, rather than the toggle silently "hiding" what is already out of sight.
pub(crate) fn toggle_wide_sidebar(shell: &mut PaseoShell) {
    if shell.data.chrome.sidebar_stepped_aside && shell.data.layout.explorer_visible {
        shell.data.layout.show_explorer(false);
    } else {
        let visible = shell.data.layout.sidebar_visible;
        shell.data.layout.show_sidebar(false, !visible);
    }
}

/// Pointer handling for chrome ids recorded in `shell.data.hits`.
pub fn handle_event(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, scope: &mut Scope) {
    let sidebar_visible = sidebar_visible(shell);
    if !sidebar_visible {
        shell.data.sidebar.stop_motion();
    }
    if let Event::AccessibilityAction(action) = event {
        use super::accessibility;
        if shell.data.app_inactive
            || (shell.data.compact && shell.data.chrome.explorer_visible())
            || crate::common::overlay::window_modal_open(&shell.data.window_id)
        {
            return;
        }
        if action.id == accessibility::sidebar_id() && sidebar_visible {
            if let Some(delta) =
                accessibility::page_delta(action.action, shell.data.sidebar.viewport)
            {
                sidebar::scroll(shell, cx, delta);
            }
        } else if action.action == AccessibilityAction::Activate
            && let Some(id) = accessibility::hit(&shell.data.hits, HitScope::Shell, action.id)
            && let Some(ui) = scope.data.get_mut::<UiContext>()
        {
            commands::click(shell, cx, ui, &id, KeyModifiers::default());
        }
        return;
    }
    let area = shell.layers.bg;
    match event.hits(cx, area) {
        Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
            let hover = shell.data.hits.at(e.abs);
            let cursor = match hover.as_deref() {
                Some(id) if id.starts_with("split:") => {
                    let horizontal = shell.data.chrome.splits.iter().any(|handle| {
                        handle.id == id["split:".len()..] && handle.axis == Axis::Horizontal
                    });
                    if horizontal {
                        MouseCursor::ColResize
                    } else {
                        MouseCursor::RowResize
                    }
                }
                Some("resize:sidebar") | Some("resize:explorer") => MouseCursor::ColResize,
                Some(_) => MouseCursor::Hand,
                None => MouseCursor::Default,
            };
            cx.set_cursor(cursor);
            if hover != shell.data.pointer.hover {
                let previous = std::mem::replace(&mut shell.data.pointer.hover, hover.clone());
                crate::shell::overlays::hover_tooltip(shell, cx, hover.clone(), e.abs);
                shell.redraw_hit_owners(cx, &[previous.as_deref(), hover.as_deref()]);
            }
        }
        Hit::FingerHoverOut(_) => {
            if let Some(previous) = shell.data.pointer.hover.take() {
                crate::shell::overlays::hover_tooltip(shell, cx, None, DVec2::default());
                shell.redraw_hit_owners(cx, &[Some(previous.as_str())]);
            }
        }
        Hit::FingerDown(fe) => {
            shell.data.sidebar.stop_motion();
            // A press dismisses the hover tooltip; it returns only when hover moves on.
            crate::shell::overlays::hover_tooltip(shell, cx, None, DVec2::default());
            let id = shell.data.hits.at(fe.abs);
            if fe.mouse_button().is_some_and(|b| b.is_secondary()) {
                if let Some(id) = id
                    && let Some(ui) = scope.data.get_mut::<UiContext>()
                {
                    commands::secondary(shell, cx, ui, &id, fe.abs);
                }
                return;
            }
            if shell.data.compact
                && (fe.abs.x < 24.0
                    || shell.data.layout.compact_destination == CompactDestination::Sidebar)
            {
                shell.data.chrome.edge_start = Some(fe.abs);
            }
            if sidebar_visible
                && fe.device.is_touch()
                && shell.data.sidebar.viewport.contains(fe.abs)
            {
                shell.data.sidebar.touch_down(fe.abs, fe.time);
            }
            start_drag(shell, id.as_deref(), fe.abs);
            let previous = std::mem::replace(&mut shell.data.pointer.pressed, id.clone());
            shell.redraw_hit_owners(cx, &[previous.as_deref(), id.as_deref()]);
        }
        Hit::FingerMove(fe) => {
            if fe.device.is_touch() && shell.data.sidebar.touch_move(cx, fe.abs, fe.time) {
                let previous = shell.data.pointer.pressed.take();
                shell.data.chrome.edge_start = None;
                shell.redraw_hit_owners(cx, &[previous.as_deref()]);
                return;
            }
            if update_drag(shell, fe.abs) {
                shell.invalidate_layout(cx);
            }
        }
        Hit::FingerLongPress(lp) => {
            shell.data.sidebar.stop_motion();
            if let Some(id) = shell.data.pointer.pressed.take()
                && !matches!(shell.data.chrome.drag, Some(Drag::Tab { moving: true, .. }))
            {
                shell.data.chrome.drag = None;
                if let Some(ui) = scope.data.get_mut::<UiContext>() {
                    commands::secondary(shell, cx, ui, &id, lp.abs);
                }
            }
        }
        Hit::FingerUp(fe) => {
            if sidebar::touch_up(shell, cx, fe.time) {
                shell.data.pointer.pressed = None;
                shell.data.chrome.edge_start = None;
            }
            let pressed = shell.data.pointer.pressed.take();
            let dragged = finish_drag(shell, cx, fe.abs);
            let edge = shell.data.chrome.edge_start.take();
            if let (Some(start), false) = (edge, dragged) {
                let delta = fe.abs - start;
                if delta.x.abs() >= 80.0 && delta.x.abs() >= delta.y.abs() * 2.0 {
                    let layout = &mut shell.data.layout;
                    if delta.x > 0.0 && layout.compact_destination == CompactDestination::Agent {
                        if let Some(ui) = scope.data.get_mut::<UiContext>() {
                            commands::back(shell, cx, ui);
                        }
                    } else if delta.x < 0.0
                        && layout.compact_destination == CompactDestination::Sidebar
                    {
                        layout.show_sidebar(true, false);
                    }
                    shell.invalidate_layout(cx);
                    return;
                }
            }
            if dragged {
                shell.invalidate_layout(cx);
                return;
            }
            if let Some(id) = pressed.as_deref()
                && fe.is_over
                && !fe.has_long_press_occurred
                && (!fe.device.is_touch() || fe.was_tap())
                && shell.data.hits.at(fe.abs).as_deref() == Some(id)
                && let Some(ui) = scope.data.get_mut::<UiContext>()
            {
                commands::click(shell, cx, ui, id, fe.modifiers);
            }
            let hover = if fe.device.has_hovers() {
                None
            } else {
                shell.data.pointer.hover.take()
            };
            shell.redraw_hit_owners(cx, &[pressed.as_deref(), hover.as_deref()]);
        }
        Hit::FingerScroll(e) => {
            if sidebar_visible && shell.data.chrome.sidebar_rect.contains(e.abs) {
                sidebar::scroll(shell, cx, e.scroll.y);
            }
        }
        _ => {}
    }
}

fn start_drag(shell: &mut PaseoShell, id: Option<&str>, abs: DVec2) {
    let chrome = &mut shell.data.chrome;
    chrome.drag = match id {
        Some(id) if id.starts_with("split:") => {
            let split = &id["split:".len()..];
            chrome
                .splits
                .iter()
                .find(|handle| handle.id == split)
                .map(|handle| Drag::Split {
                    id: handle.id.clone(),
                    grab: along(handle.axis, abs) - handle.start - handle.first,
                    last_first: handle.first,
                })
        }
        // Drags start from the width on screen (the saved width may be clamped to fit), so
        // the divider follows the pointer from the first pixel.
        Some("resize:sidebar") => Some(Drag::Sidebar {
            start_x: abs.x,
            start_width: chrome.sidebar_rect.size.x.max(1.0),
        }),
        Some("resize:explorer") => Some(Drag::Explorer {
            start_x: abs.x,
            start_width: chrome.explorer_rect.size.x.max(1.0),
        }),
        Some(id) if id.starts_with("tab:") && !id.starts_with("tab:explorer:") => {
            let rest = &id["tab:".len()..];
            rest.split_once(':').map(|(pane, key)| Drag::Tab {
                key: key.to_string(),
                from_pane: pane.to_string(),
                start: abs,
                moving: false,
            })
        }
        _ => None,
    };
}

fn update_drag(shell: &mut PaseoShell, abs: DVec2) -> bool {
    let chrome = &mut shell.data.chrome;
    let Some(drag) = chrome.drag.as_mut() else {
        return false;
    };
    match drag {
        Drag::Split {
            id,
            grab,
            last_first,
        } => {
            let Some(handle) = chrome.splits.iter().find(|handle| handle.id == *id) else {
                return false;
            };
            let Some(first) = handle.drag_first(along(handle.axis, abs), *grab, *last_first) else {
                return false;
            };
            *last_first = first;
            shell.data.layout.resize_split(id, first / handle.available);
            true
        }
        Drag::Sidebar {
            start_x,
            start_width,
        } => {
            shell
                .data
                .layout
                .set_sidebar_width(*start_width + abs.x - *start_x);
            true
        }
        Drag::Explorer {
            start_x,
            start_width,
        } => {
            shell
                .data
                .layout
                .set_explorer_width(*start_width - (abs.x - *start_x));
            true
        }
        Drag::Tab { start, moving, .. } => {
            *moving |= (abs - *start).length() > 6.0;
            chrome.drop_pane = if *moving {
                chrome
                    .pane_rects
                    .iter()
                    .find(|(id, r)| id != "explorer" && r.contains(abs))
                    .map(|(id, _)| id.clone())
            } else {
                None
            };
            *moving
        }
    }
}

/// Ends a drag: tab drops move the tab (reorder within the strip, or `moveTab`).
fn finish_drag(shell: &mut PaseoShell, cx: &mut Cx, abs: DVec2) -> bool {
    let Some(drag) = shell.data.chrome.drag.take() else {
        return false;
    };
    shell.data.chrome.drop_pane = None;
    match drag {
        Drag::Tab {
            key,
            from_pane,
            moving: true,
            ..
        } => {
            let target_pane = shell
                .data
                .chrome
                .pane_rects
                .iter()
                .find(|(id, r)| id != "explorer" && r.contains(abs))
                .map(|(id, _)| id.clone());
            if let Some(pane) = target_pane {
                // Drop index: before the first visible tab whose centre is right of the pointer,
                // else after the last visible one. Strip positions map back to the pane's full
                // tab order, so tabs hidden behind the overflow button keep their places.
                let visible: Vec<(String, bool)> = shell
                    .data
                    .chrome
                    .tab_rects
                    .iter()
                    .filter(|(p, k, _)| *p == pane && *k != key)
                    .map(|(_, k, r)| (k.clone(), abs.x < r.pos.x + r.size.x * 0.5))
                    .collect();
                let others: Vec<String> = shell
                    .data
                    .layout
                    .tree
                    .pane(&pane)
                    .map(|p| p.tabs.iter().filter(|k| **k != key).cloned().collect())
                    .unwrap_or_default();
                let position = |k: &str| others.iter().position(|t| t == k);
                let index = match visible.iter().find(|(_, before)| *before) {
                    Some((k, _)) => position(k),
                    None => visible.last().and_then(|(k, _)| position(k)).map(|i| i + 1),
                };
                if pane == from_pane {
                    if let Some(old) = shell
                        .data
                        .layout
                        .pane_of(&key)
                        .and_then(|id| {
                            shell
                                .data
                                .layout
                                .tree
                                .pane(&id)
                                .map(|p| p.tabs.iter().position(|k| *k == key))
                        })
                        .flatten()
                    {
                        let count = shell
                            .data
                            .layout
                            .tree
                            .pane(&pane)
                            .map_or(0, |p| p.tabs.len());
                        let new = index.unwrap_or(count.saturating_sub(1));
                        shell.data.layout.reorder_tab(&pane, old, new);
                    }
                } else {
                    shell.data.layout.move_tab(&key, &pane, index);
                }
            }
            shell.invalidate_layout(cx);
            true
        }
        Drag::Tab { .. } => false,
        Drag::Split { .. } | Drag::Sidebar { .. } | Drag::Explorer { .. } => true,
    }
}

fn place_explorer(shell: &PaseoShell, cx: &mut Cx) {
    let chrome = &shell.data.chrome;
    let visual = Intent::OpenExplorer
        .resolve()
        .sample(chrome.explorer.value(), chrome.explorer_window.size);
    if let Some(list) = &chrome.explorer_list {
        crate::common::overlay::place_layer(
            cx,
            list,
            Rect {
                pos: visual.incoming.offset,
                size: chrome.explorer_window.size,
            },
            1.0,
        );
    }
    crate::common::overlay::update_scrim(cx, chrome.explorer_scrim, (visual.scrim * 0.5) as f32);
}

pub(crate) fn animate_explorer(shell: &mut PaseoShell, cx: &mut Cx, event: &Event) {
    let open = shell.data.compact
        && shell.data.page.is_none()
        && shell.data.layout.compact_destination == CompactDestination::Explorer;
    let intent = if open {
        Intent::OpenExplorer
    } else {
        Intent::CloseExplorer
    };
    let position = if open { 1.0 } else { 0.0 };
    if !shell.data.compact || shell.data.page.is_some() {
        if shell.data.chrome.explorer.snap(position) {
            shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
        }
        return;
    }
    let explorer = &mut shell.data.chrome.explorer;
    let retargeted = explorer.retarget(cx, position, intent.resolve().spec);
    if explorer.advance(cx, event) {
        if explorer.is_moving() {
            place_explorer(shell, cx);
        } else {
            shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
        }
    } else if retargeted {
        shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
    }
}

fn pane_at(shell: &PaseoShell, abs: DVec2) -> Option<String> {
    shell
        .data
        .chrome
        .pane_rects
        .iter()
        .find(|(id, r)| id != "explorer" && r.contains(abs))
        .map(|(id, _)| id.clone())
}

/// Pane focus follows a press anywhere in a pane, including content that captures the press
/// itself (terminal, editor, chat). Runs before panels see the event. A mouse focuses on press;
/// a touch only on a tap released in the pane it started in, so drags never retarget the pane.
/// A press on a floating surface belongs to that surface, whichever pane lies under it.
pub(crate) fn observe_pane_focus(shell: &mut PaseoShell, cx: &Cx, event: &Event) {
    const TAP_SLOP: f64 = 12.0;
    let window = shell.data.os_window;
    let area = shell.data.overlays.area;
    let pane_at = |shell: &PaseoShell, abs: DVec2| {
        pane_at(shell, abs).filter(|_| !cx.pointer_shielded(area, abs))
    };
    match event {
        Event::MouseDown(e) if Some(e.window_id) == window && !e.button.is_secondary() => {
            if let Some(pane) = pane_at(shell, e.abs) {
                shell.data.layout.focus_pane(&pane);
            }
        }
        Event::TouchUpdate(e) if Some(e.window_id) == window => {
            if e.cancelled {
                shell.data.chrome.touch_focus = None;
                return;
            }
            for touch in &e.touches {
                match touch.state {
                    TouchState::Start => {
                        shell.data.chrome.touch_focus =
                            pane_at(shell, touch.abs).map(|pane| (touch.uid, pane, touch.abs));
                    }
                    TouchState::Move => {
                        if shell
                            .data
                            .chrome
                            .touch_focus
                            .as_ref()
                            .is_some_and(|(uid, _, start)| {
                                *uid == touch.uid && (touch.abs - *start).length() > TAP_SLOP
                            })
                        {
                            shell.data.chrome.touch_focus = None;
                        }
                    }
                    TouchState::Stop => {
                        if let Some((uid, pane, start)) = shell.data.chrome.touch_focus.take()
                            && uid == touch.uid
                            && (touch.abs - start).length() <= TAP_SLOP
                            && pane_at(shell, touch.abs).as_deref() == Some(pane.as_str())
                        {
                            shell.data.layout.focus_pane(&pane);
                        }
                    }
                    TouchState::Stable => {}
                }
            }
        }
        _ => {}
    }
}

/// Tab strip fit: chip widths and the visible run `[first, end)`.
#[derive(Debug, PartialEq)]
pub(crate) struct TabFit {
    pub widths: Vec<f64>,
    pub first: usize,
    pub end: usize,
}

/// Fits chips of `natural` widths (each already ≤ `available`) into a strip: when they do not
/// fit, the other tabs shrink toward `min_w`; tabs that still do not fit stay out of the
/// visible run and `overflow_w` is reserved for the `+N` button. The selected tab is always
/// visible (clamped to the remaining room), with as many tabs before it, then after it, as fit.
pub(crate) fn fit_tabs(
    natural: &[f64],
    active: usize,
    available: f64,
    gap: f64,
    min_w: f64,
    overflow_w: f64,
) -> TabFit {
    let mut widths = natural.to_vec();
    let total = |widths: &[f64]| widths.iter().map(|w| w + gap).sum::<f64>();
    if total(&widths) > available && widths.len() > 1 {
        let active_w = widths.get(active).map_or(0.0, |w| w + gap);
        let others = (widths.len() - 1).max(1) as f64;
        let share = ((available - active_w) / others - gap).max(min_w);
        for (i, width) in widths.iter_mut().enumerate() {
            if i != active {
                *width = width.min(share);
            }
        }
    }
    let room = if total(&widths) > available {
        (available - overflow_w).max(0.0)
    } else {
        available
    };
    let Some(selected) = widths.get_mut(active) else {
        return TabFit {
            widths,
            first: 0,
            end: 0,
        };
    };
    *selected = selected.min(room);
    let (mut first, mut end) = (active, active + 1);
    let mut occupied = widths[active] + gap;
    while first > 0 && occupied + widths[first - 1] + gap <= room {
        occupied += widths[first - 1] + gap;
        first -= 1;
    }
    while end < widths.len() && occupied + widths[end] + gap <= room {
        occupied += widths[end] + gap;
        end += 1;
    }
    TabFit { widths, first, end }
}

/// After `fit_tabs`, collapses other visible tabs (`shown`) whose label would show fewer than
/// `min_label` px (a letter or two and an ellipsis) to their `icon_w`, one at a time from the
/// most cut, giving the freed room back to cut visible labels in strip order. `bare_chrome` is
/// each chip's width without its label. Returns which tabs show only their icon.
pub(crate) fn collapse_labels(
    widths: &mut [f64],
    natural: &[f64],
    bare_chrome: &[f64],
    icon_w: &[f64],
    active: usize,
    shown: std::ops::Range<usize>,
    min_label: f64,
) -> Vec<bool> {
    let mut icon_only = vec![false; widths.len()];
    loop {
        let cut = shown
            .clone()
            .filter(|&i| {
                i != active
                    && !icon_only[i]
                    && widths[i] + 0.5 < natural[i]
                    && widths[i] - bare_chrome[i] < min_label
                    && icon_w[i] < widths[i]
            })
            .min_by(|&a, &b| (widths[a] - bare_chrome[a]).total_cmp(&(widths[b] - bare_chrome[b])));
        let Some(cut) = cut else {
            return icon_only;
        };
        let mut freed = widths[cut] - icon_w[cut];
        widths[cut] = icon_w[cut];
        icon_only[cut] = true;
        for i in shown.clone() {
            if freed <= 0.0 {
                break;
            }
            if !icon_only[i] && widths[i] < natural[i] {
                let grow = (natural[i] - widths[i]).min(freed);
                widths[i] += grow;
                freed -= grow;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_that_fit_keep_their_widths() {
        let fit = fit_tabs(&[120.0, 150.0], 1, 400.0, 0.0, 138.0, 36.0);
        assert_eq!(
            fit,
            TabFit {
                widths: vec![120.0, 150.0],
                first: 0,
                end: 2
            }
        );
    }

    #[test]
    fn other_tabs_shrink_before_any_tab_is_hidden() {
        let fit = fit_tabs(&[240.0, 240.0, 200.0], 2, 500.0, 0.0, 138.0, 36.0);
        assert_eq!(
            fit,
            TabFit {
                widths: vec![150.0, 150.0, 200.0],
                first: 0,
                end: 3
            }
        );
    }

    #[test]
    fn tabs_beyond_the_minimum_overflow_while_the_selected_one_stays_visible() {
        // 275px strip: the selected 240px tab is clamped so the +N button has its slot.
        let fit = fit_tabs(&[240.0, 240.0, 240.0, 240.0], 2, 275.0, 0.0, 138.0, 36.0);
        assert_eq!((fit.first, fit.end), (2, 3));
        assert_eq!(fit.widths[2], 239.0);
        // Room for the selected tab plus one more: earlier tabs come first.
        let fit = fit_tabs(&[200.0, 200.0, 200.0, 200.0], 2, 400.0, 0.0, 138.0, 36.0);
        assert_eq!((fit.first, fit.end), (1, 3));
        assert!(fit.widths[1] + fit.widths[2] <= 400.0 - 36.0);
    }

    #[test]
    fn split_sides_keep_their_minimum_or_share_evenly() {
        // Wide: the saved ratio applies as long as both sides keep 240px.
        let minima = [240.0; 2];
        assert_eq!(split_first(1000.0, 0.5, minima), 500.0);
        assert_eq!(split_first(1000.0, 0.15, minima), 240.0);
        assert_eq!(split_first(1000.0, 0.85, minima), 760.0);
        // Too narrow for two 240px sides: an even split instead of a 15% sliver.
        assert_eq!(split_first(400.0, 0.15, minima), 200.0);
        assert_eq!(split_first(0.0, 0.5, minima), 0.0);
    }

    #[test]
    fn narrow_wide_columns_yield_room_without_changing_tabs_or_saved_ratios() {
        let mut layout = WindowLayout::default();
        layout.open(
            PanelTarget::new("agent", "a", "ws", "/work").with_id("chat"),
            false,
        );
        layout.open(
            PanelTarget::new("agent", "b", "ws", "/work").with_id("chat"),
            true,
        );
        let Node::Split { id, .. } = &layout.tree else {
            panic!("expected two panes");
        };
        let split = id.clone();
        layout.resize_split(&split, 0.625);
        let min = measure_split_minima(&layout.tree, &mut Vec::new()).x;
        assert_eq!(min, 485.0);
        assert_eq!(side_widths(&layout, 600.0, min, false).sidebar, 200.0);
        assert_eq!(side_widths(&layout, 710.0, min, false).sidebar, 220.0);

        layout.open_explorer(PanelTarget::new("files", "b", "ws", "/work"), true);
        let saved = layout.to_json();
        let columns = side_widths(&layout, 600.0, min, false);
        assert!(columns.sidebar_stepped_aside);
        assert_eq!((columns.sidebar, columns.explorer), (0.0, 180.0));
        let columns = side_widths(&layout, 710.0, min, false);
        assert_eq!((columns.sidebar, columns.explorer), (0.0, 220.0));
        let columns = side_widths(&layout, 1600.0, min, false);
        assert!(!columns.sidebar_stepped_aside);
        assert_eq!((columns.sidebar, columns.explorer), (320.0, 340.0));
        let Node::Split { ratio, .. } = &layout.tree else {
            panic!("expected retained split");
        };
        assert_eq!(split_first(925.0, *ratio, [240.0; 2]), 578.125);
        assert_eq!(layout.to_json(), saved);
    }

    #[test]
    fn nested_splits_share_a_shortage_by_subtree_minimum() {
        let mut layout = WindowLayout::default();
        let right = layout
            .split_pane(crate::shell::layout::MAIN_PANE, Axis::Horizontal)
            .unwrap();
        layout
            .split_pane(crate::shell::layout::MAIN_PANE, Axis::Horizontal)
            .unwrap();
        layout.split_pane(&right, Axis::Vertical).unwrap();
        let mut minima = Vec::new();
        assert_eq!(measure_split_minima(&layout.tree, &mut minima), dvec2(730.0, 325.0));
        assert_eq!(minima[0], [dvec2(485.0, 160.0), dvec2(240.0, 325.0)]);
        let a = split_first(362.5, 0.2, minima[0].map(|v| v.x));
        assert_eq!(a, 242.5);
        assert_eq!(362.5 - a, 120.0);
    }

    #[test]
    fn a_split_drag_only_persists_visible_movement() {
        let mut handle = SplitHandle {
            id: "split".into(),
            axis: Axis::Horizontal,
            start: 100.0,
            available: 400.0,
            minima: [240.0; 2],
            first: 200.0,
        };
        assert_eq!(handle.drag_first(340.0, 3.0, 200.0), None);
        handle.available = 600.0;
        // The saved split is clamped to 240. Grabbing it 3px in and dragging further
        // into the clamp must not turn that displayed position into the saved ratio.
        assert_eq!(handle.drag_first(343.0, 3.0, 240.0), None);
        assert_eq!(handle.drag_first(330.0, 3.0, 240.0), None);
        assert_eq!(handle.drag_first(403.0, 3.0, 240.0), Some(300.0));
    }

    #[test]
    fn labels_cut_to_a_letter_become_icons_and_give_their_room_to_the_others() {
        // The default Explorer: Files and Git Log were squeezed to "Fi…" and "Gi…" beside the
        // active Commit tab. Files becomes an icon and Git Log gets its whole label back.
        let natural = [72.0, 87.0, 140.0];
        let mut widths = vec![64.0, 64.0, 140.0];
        let icons = collapse_labels(&mut widths, &natural, &[42.0; 3], &[35.0; 3], 2, 0..3, 36.0);
        assert_eq!(icons, [true, false, false]);
        assert_eq!(widths, [35.0, 87.0, 140.0]);
        // Tabs that keep a readable part of their label are left alone, and so is the active one.
        let mut widths = vec![90.0, 50.0];
        let natural = [120.0, 140.0];
        let icons = collapse_labels(&mut widths, &natural, &[42.0; 2], &[35.0; 2], 1, 0..2, 36.0);
        assert_eq!(icons, [false, false]);
        assert_eq!(widths, [90.0, 50.0]);
        // A tab already in the +N overflow frees no room: the clamped active tab must not grow
        // into the overflow chip and the new-tab button.
        let mut widths = vec![115.0, 59.0];
        let natural = [151.0, 151.0];
        let icons = collapse_labels(&mut widths, &natural, &[42.0; 2], &[35.0; 2], 0, 0..1, 36.0);
        assert_eq!(icons, [false, false]);
        assert_eq!(widths, [115.0, 59.0]);
    }
}
