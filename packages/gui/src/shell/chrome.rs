//! Shell chrome (Flutter `PaseoShell.build`, `_header`, `_layout`, `_pane`, `_explorer`,
//! `_content`, `_compactExplorerOverlay`, `_welcome`): geometry, drawing and pointer handling
//! of everything that is not a feature root.

use std::collections::HashSet;

use makepad_widgets::*;
use makepad_widgets::event::TouchState;
use serde_json::Value;

use crate::common::paint::{Font, TextSpec, rect};
use crate::common::provider_icon::ProviderIcon;
use crate::common::widgets::{agent_live_status_bucket, paint_status_badge, StatusBucket};
use crate::common::{PanelTarget, Route, UiContext, clock, icons, json as j, tr, tr_args};
use crate::shell::hosts::{self, Host};
use crate::shell::layout::{Axis, CompactDestination, Node};
use crate::shell::ui::{Ui, Variant};
use crate::shell::{PaseoShell, commands, panels, sidebar};
use crate::theme::{self, ControlSize, Palette, with_alpha};

/// Geometry of the last frame, for drags and drops.
#[derive(Default)]
pub struct ChromeState {
    /// (split id, axis, split rect) for divider drags.
    pub splits: Vec<(String, Axis, Rect)>,
    pub pane_rects: Vec<(String, Rect)>,
    pub tab_bars: Vec<(String, Rect)>,
    pub tab_rects: Vec<(String, String, Rect)>,
    /// Tabs of each pane that did not fit its strip this frame (overflow menu).
    pub tab_overflow: Vec<(String, Vec<String>)>,
    pub sidebar_rect: Rect,
    pub explorer_rect: Rect,
    pub content_rect: Rect,
    /// Active drag: (kind, id, origin abs, origin value).
    pub drag: Option<Drag>,
    /// Compact explorer slide position 0..1 and the animation it is heading to.
    pub explorer_position: f64,
    pub explorer_target: f64,
    pub explorer_anim: Option<NextFrame>,
    pub explorer_anim_start: Option<(f64, f64, f64)>,
    pub edge_start: Option<DVec2>,
    pub drop_pane: Option<String>,
    /// A touch that started in a pane: (touch id, pane, start) until it lifts as a tap.
    pub touch_focus: Option<(u64, String, DVec2)>,
}

#[derive(Clone, Debug)]
pub enum Drag {
    Split { id: String, axis: Axis, rect: Rect },
    Sidebar { start_x: f64, start_width: f64 },
    Explorer { start_x: f64, start_width: f64 },
    Tab { key: String, from_pane: String, start: DVec2, moving: bool },
    /// Compact explorer horizontal pan.
    ExplorerPan { start_x: f64, start_position: f64, opening: bool, last_x: f64, last_time: f64, velocity: f64 },
}

// ---------------------------------------------------------------------------------------------
// Titles and icons (Flutter `_title`, `_headerTitle`, `_tabKindLabel`)

fn agent_row<'a>(state: &'a Value, target: &PanelTarget) -> Option<&'a Value> {
    hosts::host(state, &target.server_id)?.agent(target.id.as_deref()?)
}

pub fn title(state: &Value, target: &PanelTarget) -> String {
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
        "draft" | "new_tab" => tr("sessionNavigation.newSession"),
        "files" => tr("workspace.tabs.actions.files"),
        "changes_tree" => tr("workspace.tabs.actions.changes"),
        "file" => {
            let path = j::text_or(&target.data, "path", target.id.as_deref().unwrap_or("File"));
            path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
        }
        "working_diff" | "commit_diff" => "Diff".to_string(),
        "pull_request" => "Pull request".to_string(),
        "git_log" => tr("workspace.git.log.title"),
        "execution_log" => "Execution log".to_string(),
        "terminal" => tr("workspace.tabs.fallback.terminal"),
        "setup" => "Workspace setup".to_string(),
        "project_settings" => "Project settings".to_string(),
        "provider_subagent" => "Subagent".to_string(),
        other => other.to_string(),
    }
}

/// `_headerTitle`: an agent header shows the directory's title or "Untitled session".
pub fn header_title(state: &Value, target: &PanelTarget) -> String {
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
            path.split('/').rfind(|p| !p.is_empty()).unwrap_or(path).to_string()
        }
        "working_diff" | "changes_tree" => tr("panels.diff.changesLabel"),
        "files" => tr("panels.files.label"),
        "pull_request" => tr("panels.pullRequest.label"),
        "git_log" => tr("workspace.git.log.title"),
        "commit_diff" => target.id.as_deref().map(|id| id.chars().take(7).collect()).unwrap_or_default(),
        _ => tr("workspace.tabs.fallback.agent"),
    }
}

// ---------------------------------------------------------------------------------------------
// Visibility

fn in_workspace(tab: Option<&PanelTarget>, workspace: &PanelTarget) -> bool {
    tab.is_some_and(|t| t.server_id == workspace.server_id && t.workspace_id == workspace.workspace_id)
}

/// The explorer tab keys shown (compact: only the workspace on screen).
pub fn explorer_keys(shell: &PaseoShell) -> (Vec<String>, Option<String>) {
    let layout = &shell.data.layout;
    let mut keys = layout.explorer_tabs.clone();
    let mut selected = layout.selected_explorer.clone();
    if shell.data.compact
        && let Some(workspace) = layout.selected()
    {
        keys.retain(|key| in_workspace(layout.target(key), workspace));
        if !selected.as_ref().is_some_and(|s| keys.contains(s)) {
            selected = keys.first().cloned();
        }
    }
    (keys, selected)
}

/// Tabs whose roots are on screen this frame (`_RetainedPanel.active`).
pub fn visible_tabs(shell: &PaseoShell) -> HashSet<String> {
    let data = &shell.data;
    let layout = &data.layout;
    let mut out = HashSet::new();
    if data.page.is_some() && (data.compact || matches!(data.page, Some(crate::shell::Page::Settings { .. }))) {
        return out;
    }
    let page_over_main = data.page.is_some();
    if data.compact {
        if layout.compact_destination != CompactDestination::Sidebar
            && let Some(key) = &layout.focused().selected
        {
            out.insert(key.clone());
        }
        if (layout.compact_destination == CompactDestination::Explorer || data.chrome.explorer_position > 0.0)
            && let (_, Some(key)) = explorer_keys(shell)
        {
            out.insert(key);
        }
        return out;
    }
    if !page_over_main {
        for pane in layout.panes() {
            if layout.maximized_pane.as_deref().is_none_or(|m| m == pane.id)
                && let Some(key) = &pane.selected
            {
                out.insert(key.clone());
            }
        }
        if layout.explorer_visible
            && let Some(key) = &layout.selected_explorer
        {
            out.insert(key.clone());
        }
    }
    out
}

/// The focused root: compact explorer → explorer tab; else the focused pane's tab.
pub fn focused_tab(shell: &PaseoShell) -> Option<String> {
    let layout = &shell.data.layout;
    if shell.data.compact {
        return match layout.compact_destination {
            CompactDestination::Sidebar => None,
            CompactDestination::Explorer => explorer_keys(shell).1,
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

pub fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, r: Rect) {
    let p = theme::current_palette();
    let t = theme::current_typography();
    let state = scope.data.get::<UiContext>().map(|ui| ui.state.clone());
    shell.data.hits.clear();
    shell.data.tooltips.clear();
    shell.data.chrome.splits.clear();
    shell.data.chrome.pane_rects.clear();
    shell.data.chrome.tab_bars.clear();
    shell.data.chrome.tab_rects.clear();
    shell.data.chrome.tab_overflow.clear();
    shell.data.chrome.sidebar_rect = Rect::default();
    shell.data.sidebar.viewport = Rect::default();

    let compact = shell.data.compact;
    let sidebar_destination = compact && shell.data.layout.compact_destination == CompactDestination::Sidebar;
    let background = if sidebar_destination { p.sidebar } else { p.background };
    shell.data.bg_area = shell.painter.capture_box(cx, r, background, Vec4::default(), 0.0, 0.0);

    let Some(state) = state else {
        draw_startup_error(shell, cx, r, &p);
        return;
    };

    let insets = crate::shell::platform::safe_insets();
    let body = rect(r.pos.x + insets.left, r.pos.y + insets.top, r.size.x - insets.left - insets.right, (r.size.y - insets.top - insets.bottom_occlusion()).max(0.0));
    let host_count = hosts::hosts(&state).len();

    // Settings is a pushed full-window route.
    if let Some(crate::shell::Page::Settings { .. }) = &shell.data.page {
        draw_page(shell, cx, scope, body);
        return;
    }
    let page_shown = shell.data.page.is_some();
    let mut y = body.pos.y;
    let show_header = !(page_shown || (compact && sidebar_destination) || (compact && host_count == 0));
    if show_header {
        let h = if compact { 28.0 + text_line_height(20.0, 14.0) + text_line_height(16.0, 12.0) } else { 12.0 + text_line_height(24.0, 16.0) };
        draw_header(shell, cx, scope, &state, rect(body.pos.x, y, body.size.x, h));
        y += h;
    }
    if let Some(error) = crate::shell::startup_error().or_else(|| j::object(&state, "storage").get("error").and_then(Value::as_str).map(str::to_string)) {
        y += draw_banner(shell, cx, rect(body.pos.x, y, body.size.x, 0.0), &error);
    }
    let content = rect(body.pos.x, y, body.size.x, body.pos.y + body.size.y - y);
    shell.data.chrome.content_rect = content;
    if host_count == 0 {
        crate::shell::sheets::welcome::draw(shell, cx, content);
        return;
    }
    if compact {
        draw_compact(shell, cx, scope, &state, content, page_shown);
    } else {
        draw_wide(shell, cx, scope, &state, content, page_shown);
    }
    let _ = t;
}

fn draw_startup_error(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, p: &Palette) {
    let t = theme::current_typography();
    let message = crate::shell::startup_error().unwrap_or_else(|| "Connecting to Paseo…".to_string());
    let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.destructive);
    let w = (r.size.x - 48.0).min(560.0);
    let h = shell.painter.paragraph_height(cx, w, spec, &message);
    shell.painter.paragraph(cx, dvec2(r.pos.x + (r.size.x - w) * 0.5, r.pos.y + (r.size.y - h) * 0.5), w, spec, &message);
}

/// Flutter `MaterialBanner` with the error and a Settings action.
fn draw_banner(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, message: &str) -> f64 {
    let p = theme::current_palette();
    let t = theme::current_typography();
    let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.foreground);
    let text_w = (r.size.x - 32.0 - 120.0).max(80.0);
    let h = shell.painter.paragraph_height(cx, text_w, spec, message).max(20.0) + 24.0;
    shell.painter.fill(cx, rect(r.pos.x, r.pos.y, r.size.x, h), p.surface2, 0.0);
    shell.painter.paragraph(cx, dvec2(r.pos.x + 16.0, r.pos.y + 12.0), text_w, spec, message);
    let mut ui = make_ui(shell);
    let label = tr("settings.title");
    let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
    ui.button(cx, rect(r.pos.x + r.size.x - bw - 16.0, r.pos.y + (h - 32.0) * 0.5, bw, 32.0), "banner:settings", &label, None, Variant::Ghost, ControlSize::Sm, true);
    shell.painter.hline(cx, r.pos.x, r.pos.y + h - 1.0, r.size.x, p.border);
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
    }
}

/// Original header-island button: a 32px (compact) / 26px circle with highlight.
fn island(ui: &mut Ui, cx: &mut Cx2d, center: DVec2, id: &str, tooltip: &str, compact: bool) -> Rect {
    let extent = if compact { 32.0 } else { 26.0 };
    let r = rect(center.x - extent * 0.5, center.y - extent * 0.5, extent, extent);
    if ui.is_hover(id) || ui.is_pressed(id) {
        let h = ui.p.interaction_highlight;
        ui.fill(cx, r, h, extent * 0.5);
    }
    let hit = if compact { rect(r.pos.x, r.pos.y - 8.0, extent, extent + 16.0) } else { r };
    ui.hit_tip(hit, id, tooltip);
    r
}

fn draw_header(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, state: &Value, r: Rect) {
    let compact = shell.data.compact;
    let target = shell.data.layout.selected().cloned();
    let host = target.as_ref().and_then(|t| hosts::host(state, &t.server_id));
    let workspace = target.as_ref().and_then(|t| host.and_then(|h| h.workspace(&t.workspace_id))).cloned();
    let tabs_count = target.as_ref().map(|t| commands::workspace_tabs(shell, state, t).len()).unwrap_or(0);
    let new_workspace = target.as_ref().is_some_and(|t| t.kind == "new_tab" && t.workspace_id.is_empty());
    let row = rect(r.pos.x + 12.0, r.pos.y + if compact { 8.0 } else { 0.0 }, r.size.x - 24.0, r.size.y - if compact { 8.0 } else { 0.0 });
    let mid = row.pos.y + row.size.y * 0.5;
    let has_scripts = workspace.as_ref().is_some_and(|w| !j::array(w, "scripts").is_empty());
    let header_git = !compact && target.as_ref().is_some_and(|t| !t.cwd.is_empty());
    let mut right_widgets: Option<Rect> = None;
    // A compact detail view (file, diff, subagent…) leads with Back to the tab it came from.
    let detail_back = shell.data.layout.compact_back_tab(compact).is_some();
    {
        let mut ui = make_ui(shell);
        let p = ui.p;
        let extent = if compact { 32.0 } else { 26.0 };
        // Sidebar toggle (MobileMenuIcon on compact, PanelLeft on wide).
        let (toggle_id, toggle_tip) = if detail_back { ("hdr:back", tr("common.actions.back")) } else { ("hdr:sidebar", tr("shell.menu.toggleSidebar")) };
        let toggle = island(&mut ui, cx, dvec2(row.pos.x + extent * 0.5, mid), toggle_id, &toggle_tip, compact);
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
        let mut x = toggle.pos.x + extent + 8.0;
        let mut right = row.pos.x + row.size.x;
        if new_workspace {
            if compact {
                let label = tr("importSession.title");
                let w = ui.button_width(cx, &label, Some("import"), ControlSize::Sm);
                let h = theme::control_height(ControlSize::Sm);
                ui.button(cx, rect(right - w, mid - h * 0.5, w, h), "hdr:import", &label, Some("import"), Variant::Ghost, ControlSize::Sm, true);
            }
        } else {
            // Right island: explorer toggle, git widgets (wide), scripts (wide), menu, tabs (compact).
            let explorer = island(&mut ui, cx, dvec2(right - extent * 0.5, mid), "hdr:explorer", &tr("workspace.tabs.explorerSidebar.toggle"), compact);
            ui.icon(cx, explorer, "panel-right", 16.0, if compact { p.muted } else { p.extra_muted });
            right = explorer.pos.x;
            if header_git {
                // Git actions split button + open-in, laid out by their own widgets.
                // Up to 300px: Open In plus a long primary label ("Update from <base>") and
                // its menu caret; the title keeps at least 120px.
                let w = 300.0_f64.min((right - x - 120.0).max(0.0));
                if w > 0.0 {
                    let h = theme::control_height(ControlSize::Xs);
                    right_widgets = Some(rect(right - w, mid - h * 0.5, w, h));
                    right -= w + 4.0;
                }
            }
            if !compact && has_scripts {
                let scripts = island(&mut ui, cx, dvec2(right - extent * 0.5, mid), "hdr:scripts", &tr("workspace.scripts.title"), false);
                ui.icon(cx, scripts, "play", 16.0, p.extra_muted);
                right = scripts.pos.x;
            }
            let menu = island(&mut ui, cx, dvec2(right - extent * 0.5, mid), "hdr:menu", &tr("workspace.header.actions.workspaceActions"), compact);
            ui.icon(cx, menu, "ellipsis", 16.0, p.extra_muted);
            right = menu.pos.x;
            if compact && target.is_some() {
                let count = tabs_count.to_string();
                let tabs = island(&mut ui, cx, dvec2(right - extent * 0.5, mid), "hdr:tabs", &tr_args("workspace.tabs.switcher.trigger", &[("count", &count)]), true);
                let spec = ui.spec(Font::Medium, 10.0, p.muted);
                let w = ui.measure(cx, spec, &count).max(12.0) + 4.0;
                let badge_h = text_line_height(16.0, 10.0);
                let badge = rect(tabs.pos.x + (extent - w) * 0.5, mid - badge_h * 0.5, w, badge_h);
                ui.painter.outline(cx, badge, Vec4::default(), p.muted, 1.0, 2.0);
                ui.text_centered(cx, badge, spec, &count);
                right = tabs.pos.x;
            }
            // Title group.
            right -= 8.0;
            let title_text = target.as_ref().map(|t| header_title(state, t)).unwrap_or_else(|| "Passession".to_string());
            let subtitle = compact && host.is_some();
            let w = (right - x).max(0.0);
            if subtitle {
                let spec = ui.spec(Font::Medium, 14.0, p.foreground);
                let title_h = text_line_height(20.0, 14.0);
                let subtitle_h = text_line_height(16.0, 12.0);
                let title_y = mid - (title_h + subtitle_h) * 0.5;
                ui.text(cx, rect(x, title_y, w, title_h), spec, &title_text);
                if let (Some(host), Some(target)) = (host, target.as_ref()) {
                    let project = workspace.as_ref().map(|w| j::text(w, "projectDisplayName").to_string()).unwrap_or_default();
                    let directory = target.cwd.split('/').rfind(|p| !p.is_empty()).unwrap_or("").to_string();
                    let project = if project.is_empty() { directory } else { project };
                    header_subtitle(&mut ui, cx, rect(x, title_y + title_h, w, subtitle_h), &project, host);
                }
            } else {
                let spec = ui.spec(Font::Medium, if compact { 14.0 } else { 16.0 }, p.foreground);
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

/// `_headerSubtitle`: project · server glyph + host label in the host colour (local hosts hide it).
fn header_subtitle(ui: &mut Ui, cx: &mut Cx2d, r: Rect, project: &str, host: Host<'_>) {
    let p = ui.p;
    let small = ui.spec(Font::Regular, 12.0, p.muted);
    let show_badge = !host.is_local();
    let badge_color = crate::shell::sheets::labels::label_color(&p, host.color());
    let mut x = r.pos.x;
    let label = host.display_label();
    let reserved = if show_badge { 12.0 + 3.0 + ui.measure(cx, small, &label).min(120.0) + if project.is_empty() { 0.0 } else { 18.0 } } else { 0.0 };
    if !project.is_empty() {
        let w = (r.size.x - reserved).max(0.0);
        let drawn = ui.text(cx, rect(x, r.pos.y, w, r.size.y), small, project);
        x = drawn.pos.x + drawn.size.x;
    }
    if show_badge {
        if !project.is_empty() {
            let sep = ui.spec(Font::Regular, 12.0, p.extra_muted);
            ui.text(cx, rect(x + 6.0, r.pos.y, 8.0, r.size.y), sep, "·");
            x += 18.0;
        }
        ui.icon(cx, rect(x, r.pos.y + 2.0, 12.0, 12.0), "server", 12.0, badge_color);
        x += 15.0;
        let spec = ui.spec(Font::Regular, 12.0, badge_color);
        ui.text(cx, rect(x, r.pos.y, (r.pos.x + r.size.x - x).max(0.0), r.size.y), spec, &label);
    }
}

fn draw_header_git(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, slot: Rect, target: &PanelTarget) {
    if shell.data.header_git.is_none() {
        let (git, open_in) = panels::header_widgets(cx, &shell.templates);
        shell.data.header_git = Some(git);
        shell.data.header_open_in = Some(open_in);
    }
    let (Some(git), Some(open_in)) = (shell.data.header_git.clone(), shell.data.header_open_in.clone()) else { return };
    panels::set_header_target(cx, &git, &open_in, target);
    cx.begin_turtle(Walk::abs_rect(slot), Layout { flow: Flow::Right, spacing: 4.0, align: Align { x: 1.0, y: 0.5 }, ..Layout::default() });
    open_in.draw_all(cx, scope);
    git.draw_all(cx, scope);
    cx.end_turtle();
}

fn draw_wide(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, state: &Value, content: Rect, page_shown: bool) {
    let p = theme::current_palette();
    let layout = &shell.data.layout;
    let explorer_visible = layout.explorer_visible && !page_shown;
    // Saved side-panel widths apply when they fit; otherwise the Explorer gives way down to
    // 180px, then the sidebar down to 200px, so the conversation keeps a usable width.
    const MIN_MAIN: f64 = 320.0;
    let mut sidebar_w = if layout.sidebar_visible { layout.sidebar_width } else { 0.0 };
    let mut explorer_w = if explorer_visible { layout.explorer_width } else { 0.0 };
    let dividers = if sidebar_w > 0.0 { 5.0 } else { 0.0 } + if explorer_visible { 5.0 } else { 0.0 };
    let mut over = sidebar_w + explorer_w + dividers + MIN_MAIN - content.size.x;
    if over > 0.0 && explorer_visible {
        let cut = over.min(explorer_w - 180.0).max(0.0);
        explorer_w -= cut;
        over -= cut;
    }
    if over > 0.0 && sidebar_w > 0.0 {
        sidebar_w -= over.min(sidebar_w - 200.0).max(0.0);
    }
    let mut x = content.pos.x;
    if sidebar_w > 0.0 {
        let side = rect(x, content.pos.y, sidebar_w, content.size.y);
        shell.data.chrome.sidebar_rect = side;
        sidebar::draw(shell, cx, scope, state, side);
        x += sidebar_w;
        let handle = rect(x, content.pos.y, 5.0, content.size.y);
        shell.painter.fill(cx, handle, p.border, 0.0);
        shell.data.hits.push(handle, "resize:sidebar".into());
        x += 5.0;
    }
    let main_right = content.pos.x + content.size.x - if explorer_visible { explorer_w + 5.0 } else { 0.0 };
    let main = rect(x, content.pos.y, main_right - x, content.size.y);
    if page_shown {
        draw_page(shell, cx, scope, main);
    } else {
        let tree = shell.data.layout.tree.clone();
        draw_node(shell, cx, scope, state, &tree, main);
    }
    if explorer_visible {
        let handle = rect(main_right, content.pos.y, 5.0, content.size.y);
        shell.painter.fill(cx, handle, p.border, 0.0);
        shell.data.hits.push(handle, "resize:explorer".into());
        let ex = rect(main_right + 5.0, content.pos.y, explorer_w, content.size.y);
        shell.data.chrome.explorer_rect = ex;
        let (keys, selected) = explorer_keys(shell);
        draw_pane(shell, cx, scope, state, "explorer", &keys, selected.as_deref(), ex, true);
    }
}

fn draw_node(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, state: &Value, node: &Node, r: Rect) {
    let p = theme::current_palette();
    match node {
        Node::Pane(pane) => {
            if shell.data.layout.maximized_pane.as_deref().is_some_and(|m| m != pane.id) {
                return;
            }
            let id = pane.id.clone();
            let tabs = pane.tabs.clone();
            let selected = pane.selected.clone();
            draw_pane(shell, cx, scope, state, &id, &tabs, selected.as_deref(), r, false);
        }
        Node::Split { id, axis, ratio, first, second } => {
            if let Some(maximized) = shell.data.layout.maximized_pane.clone() {
                // Only the branch holding the maximized pane is shown, full size.
                if first.contains_pane(&maximized) {
                    draw_node(shell, cx, scope, state, first, r);
                } else {
                    draw_node(shell, cx, scope, state, second, r);
                }
                return;
            }
            let horizontal = *axis == Axis::Horizontal;
            let extent = if horizontal { r.size.x } else { r.size.y };
            let available = (extent - 5.0).max(0.0);
            let a = available * ratio.clamp(0.15, 0.85);
            let (r1, handle, r2) = if horizontal {
                (rect(r.pos.x, r.pos.y, a, r.size.y), rect(r.pos.x + a, r.pos.y, 5.0, r.size.y), rect(r.pos.x + a + 5.0, r.pos.y, r.size.x - a - 5.0, r.size.y))
            } else {
                (rect(r.pos.x, r.pos.y, r.size.x, a), rect(r.pos.x, r.pos.y + a, r.size.x, 5.0), rect(r.pos.x, r.pos.y + a + 5.0, r.size.x, r.size.y - a - 5.0))
            };
            draw_node(shell, cx, scope, state, first, r1);
            shell.painter.fill(cx, handle, p.border, 0.0);
            shell.data.hits.push(handle, format!("split:{id}"));
            shell.data.chrome.splits.push((id.clone(), *axis, r));
            draw_node(shell, cx, scope, state, second, r2);
        }
    }
}

/// One pane (`_pane`): 38px tab strip + divider (wide, or the compact Explorer), then the
/// selected root or the empty state.
#[allow(clippy::too_many_arguments)]
fn draw_pane(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    state: &Value,
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
        let bar_h = if compact { 18.0 + text_line_height(20.0, 14.0) } else { 22.0 + text_line_height(16.0, 12.0) };
        let bar = rect(r.pos.x, r.pos.y, r.size.x, bar_h);
        shell.data.chrome.tab_bars.push((pane_id.to_string(), bar));
        draw_tab_strip(shell, cx, state, pane_id, keys, selected, bar, explorer);
        shell.painter.hline(cx, r.pos.x, r.pos.y + bar_h, r.size.x, p.border);
        content = rect(r.pos.x, r.pos.y + bar_h + 1.0, r.size.x, (r.size.y - bar_h - 1.0).max(0.0));
    }
    if shell.data.chrome.drop_pane.as_deref() == Some(pane_id) && !explorer {
        shell.painter.outline(cx, r, Vec4::default(), p.primary, 1.0, 0.0);
    }
    let Some(key) = selected.map(str::to_string).filter(|_| !keys.is_empty()) else {
        let has_sessions = hosts::hosts(state).iter().any(|host| host.agents().next().is_some());
        draw_empty(shell, cx, content, has_sessions);
        return;
    };
    draw_root(shell, cx, scope, &key, content);
}

/// Mounts (first time) and draws a tab root in `r`.
pub fn draw_root(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, key: &str, r: Rect) {
    if !shell.data.panels.contains_key(key) {
        let Some(target) = shell.data.layout.target(key).cloned() else { return };
        match panels::mount(cx, &shell.templates, &target) {
            Some(panel) => {
                shell.data.panels.insert(key.to_string(), panel);
            }
            None => {
                draw_error(shell, cx, r, &format!("Unrecognized panel: {}", target.kind));
                return;
            }
        }
    }
    let focused = focused_tab(shell).as_deref() == Some(key);
    let Some(panel) = shell.data.panels.get_mut(key) else { return };
    panels::set_activity(cx, panel, true, focused);
    let widget = panel.widget.clone();
    let target = panel.target.clone();
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        ui.route = Route::from_target(&target);
        ui.target = Some(target);
        ui.tab_key = Some(key.to_string());
        ui.activity = crate::common::PanelActivity { visible: true, focused, app_active: !shell.data.app_inactive };
    }
    cx.begin_turtle(Walk::abs_rect(r), Layout::default());
    widget.draw_walk_all(cx, scope, Walk::fill());
    cx.end_turtle();
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        ui.target = None;
        ui.tab_key = None;
    }
}

/// Roots that were visible last frame but are hidden now get `set_visible(false)`.
pub fn sync_hidden(shell: &mut PaseoShell, cx: &mut Cx) {
    let visible = visible_tabs(shell);
    for (key, panel) in shell.data.panels.iter_mut() {
        if !visible.contains(key) {
            panels::set_activity(cx, panel, false, false);
        }
    }
}

fn draw_error(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect, message: &str) {
    let p = theme::current_palette();
    let t = theme::current_typography();
    let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.destructive);
    let w = (r.size.x - 32.0).max(40.0);
    shell.painter.paragraph(cx, dvec2(r.pos.x + 16.0, r.pos.y + 16.0), w, spec, message);
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
    ui.text_centered(cx, rect(r.pos.x, cy - 12.0 - message_h - title_h, r.size.x, title_h), title, "Passession");
    let hint = tr(if has_sessions { "sessionNavigation.pick" } else { "sessionNavigation.empty" });
    ui.text_centered(cx, rect(r.pos.x + 16.0, cy - 8.0 - message_h, r.size.x - 32.0, message_h), message, &hint);
    let label = tr("sessionNavigation.newSession");
    let w = ui.button_width(cx, &label, Some("plus"), ControlSize::Sm);
    ui.button(cx, rect(r.pos.x + (r.size.x - w) * 0.5, cy + 4.0, w, theme::control_height(ControlSize::Sm)), "empty:new", &label, Some("plus"), Variant::Primary, ControlSize::Sm, true);
}

/// Tab strip (`ReorderableListView` of chips): wide chips with icon, title, modified dot,
/// ellipsis menu and close; the compact Explorer rail uses filled pills.
#[allow(clippy::too_many_arguments)]
fn draw_tab_strip(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &Value,
    pane_id: &str,
    keys: &[String],
    selected: Option<&str>,
    bar: Rect,
    explorer: bool,
) {
    let compact = shell.data.compact;
    let pill = explorer && compact;
    let dragging = match &shell.data.chrome.drag {
        Some(Drag::Tab { key, moving: true, .. }) => Some(key.clone()),
        _ => None,
    };
    let palette = theme::current_palette();
    let typography = theme::current_typography();
    let mut tabs: Vec<_> = keys.iter().filter_map(|key| {
        let target = shell.data.layout.target(key)?;
        let active = selected == Some(key.as_str());
        let spec = if pill {
            TextSpec::new(Font::Regular, typography.ui_size(14.0), if active { palette.foreground } else { palette.muted })
        } else {
            TextSpec::new(Font::Medium, typography.ui_size(12.0), palette.foreground)
        };
        Some((target.clone(), key.clone(), active, title(state, target), spec, shell.data.dirty.contains(key), 0.0))
    }).collect();
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
        let chrome_w = if pill { 10.0 + 14.0 + 6.0 + 10.0 } else { 10.0 + 15.0 + 7.0 + 2.0 + 28.0 * 2.0 };
        let gap = if pill { 2.0 } else { 0.0 };
        for (_, _, _, label, spec, modified, width) in &mut tabs {
            let text_w = ui.measure(cx, *spec, label);
            *width = (chrome_w + text_w + if *modified { 12.0 } else { 0.0 }).min(240.0).min(available);
        }
        let natural: Vec<f64> = tabs.iter().map(|t| t.6).collect();
        let active = tabs.iter().position(|tab| tab.2).unwrap_or(0);
        let fit = fit_tabs(&natural, active, available, gap, (chrome_w + 48.0).min(available), 36.0);
        for (tab, width) in tabs.iter_mut().zip(&fit.widths) {
            tab.6 = *width;
        }
        let overflow_w = 36.0;
        let (first, end) = (fit.first, fit.end);
        let hidden: Vec<String> = tabs.iter().enumerate().filter(|(i, _)| *i < first || *i >= end).map(|(_, t)| t.1.clone()).collect();
        let shown = end.saturating_sub(first);
        for (target, key, active, label, label_spec, modified, w) in tabs.into_iter().skip(first).take(shown) {
            let chip = if pill { rect(x + 2.0, bar.pos.y + 5.0, w, bar.size.y - 10.0) } else { rect(x, bar.pos.y, w, bar.size.y) };
            let tab_id = format!("tab:{pane_id}:{key}");
            let fade = if dragging.as_deref() == Some(key.as_str()) { 0.5 } else { 1.0 };
            if pill {
                if active {
                    ui.fill(cx, chip, p.surface2, 6.0);
                }
            } else if active {
                ui.fill(cx, chip, p.surface4, 0.0);
                ui.fill(cx, rect(chip.pos.x, chip.pos.y + chip.size.y - 2.0, chip.size.x, 2.0), p.primary, 0.0);
            } else if ui.is_hover(&tab_id) {
                ui.fill(cx, chip, p.interaction_highlight, 0.0);
            }
            let fg = theme::fade(if pill && !active { p.muted } else { p.foreground }, fade);
            let mut cx_x = chip.pos.x + 10.0;
            let icon_size = if pill { 14.0 } else { 15.0 };
            let icon_rect = rect(cx_x, chip.pos.y + (chip.size.y - icon_size) * 0.5, icon_size, icon_size);
            tab_icon(&mut ui, cx, state, &target, icon_rect, fg);
            cx_x += icon_size + if pill { 6.0 } else { 7.0 };
            // Tab menu and close need their 58px plus a little title; a narrower chip keeps
            // only icon and title (its menu stays on right-click) so no action lands outside it.
            let actions = !pill && chip.size.x >= 10.0 + 15.0 + 7.0 + 58.0 + 24.0;
            let buttons_w = if actions { 2.0 + 56.0 } else { 10.0 };
            let dot_w = if modified { 12.0 } else { 0.0 };
            let text_rect = rect(cx_x, chip.pos.y, (chip.pos.x + chip.size.x - buttons_w - dot_w - cx_x).max(0.0), chip.size.y);
            let drawn = ui.text(cx, text_rect, crate::common::paint::TextSpec { color: fg, ..label_spec }, &label);
            if modified {
                let dot_x = drawn.pos.x + drawn.size.x + 4.0 + 4.0;
                ui.dot(cx, dvec2(dot_x, chip.pos.y + chip.size.y * 0.5), 8.0, p.muted);
            }
            ui.hit(chip, tab_id);
            if actions {
                let menu = rect(chip.pos.x + chip.size.x - 58.0, chip.pos.y + (chip.size.y - 28.0) * 0.5, 28.0, 28.0);
                ui.icon_button(cx, menu, &format!("tabmenu:{pane_id}:{key}"), "ellipsis", 14.0, 14.0, &tr_args("workspace.tabs.menu.openFor", &[("label", &label)]));
                let close = rect(chip.pos.x + chip.size.x - 30.0, chip.pos.y + (chip.size.y - 28.0) * 0.5, 28.0, 28.0);
                ui.icon_button(cx, close, &format!("tabclose:{pane_id}:{key}"), "x", 14.0, 14.0, &tr("common.actions.close"));
            }
            tab_rects.push((pane_id.to_string(), key.clone(), chip));
            x = chip.pos.x + chip.size.x + gap;
        }
        if !hidden.is_empty() {
            let id = format!("taboverflow:{pane_id}");
            let button = rect(x + 4.0, bar.pos.y + (bar.size.y - 28.0) * 0.5, overflow_w - 8.0, 28.0);
            if ui.is_hover(&id) {
                ui.fill(cx, button, p.interaction_highlight, 6.0);
            }
            let spec = ui.spec(Font::Medium, 12.0, p.muted);
            ui.text_centered(cx, button, spec, &format!("+{}", hidden.len()));
            ui.hit_tip(button, &id, &tr_args("workspace.tabs.overflow", &[("count", &hidden.len().to_string())]));
            x = button.pos.x + button.size.x;
            shell_overflow = Some(hidden);
        }
        // Trailing controls.
        if explorer {
            let plus = rect(x + 4.0, bar.pos.y + 5.0, 28.0, 28.0);
            ui.icon_button(cx, plus, "explorer:new", "plus", 14.0, 6.0, &tr("workspace.tabs.actions.newTab"));
            if compact {
                let close = rect(bar.pos.x + bar.size.x - 36.0, bar.pos.y + (bar.size.y - 32.0) * 0.5, 32.0, 32.0);
                ui.icon_button(cx, close, "explorer:close", "x", 16.0, 16.0, &tr("workspace.tabs.explorerSidebar.close"));
            } else {
                let close = rect(bar.pos.x + bar.size.x - 36.0, bar.pos.y + (bar.size.y - 32.0) * 0.5, 32.0, 32.0);
                ui.icon_button(cx, close, "explorer:close", "x", 18.0, 16.0, &tr("common.actions.close"));
            }
        } else {
            let plus = rect(bar.pos.x + bar.size.x - 38.0, bar.pos.y + 2.0, 34.0, 34.0);
            ui.icon_button(cx, plus, &format!("panenew:{pane_id}"), "plus", 18.0, 17.0, &tr("sessionNavigation.newSession"));
        }
    }
    shell.data.chrome.tab_rects.extend(tab_rects);
    if let Some(hidden) = shell_overflow {
        shell.data.chrome.tab_overflow.push((pane_id.to_string(), hidden));
    }
}

/// `_tabIcon`: agent tabs show the provider glyph with the agent's status badge.
pub fn tab_icon(ui: &mut Ui, cx: &mut Cx2d, state: &Value, target: &PanelTarget, r: Rect, color: Vec4) {
    if target.kind == "agent"
        && let Some(agent) = agent_row(state, target)
    {
        let provider = j::text(agent, "provider");
        if !provider.is_empty() {
            // Flutter `_tabIcon` passes no host snapshot SVG: built-in, then catalog, then bot.
            ProviderIcon::paint(ui.painter, ui.draw_icon, cx, r, provider, None, color);
            let bucket = agent.get("status").and_then(Value::as_str).is_some().then(|| agent_live_status_bucket(agent));
            let p = ui.p;
            if paint_status_badge(ui.painter, cx, r, bucket.filter(|b| *b != StatusBucket::Done), &p, p.surface0) {
                let area = ui.painter.draw_box.area();
                clock::register(cx, area);
            }
            return;
        }
    }
    ui.icon(cx, r, icons::for_tab_kind(&target.kind), r.size.y, color);
}

fn draw_page(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, r: Rect) {
    let Some(page) = shell.data.page.clone() else { return };
    if matches!(page, crate::shell::Page::Sessions { .. }) {
        crate::shell::sheets::history::draw_page(shell, cx, scope, r);
        return;
    }
    if shell.data.page_root.as_ref().is_none_or(|(p, _)| *p != page) {
        let widget = panels::page_widget(cx, &shell.templates, &page);
        shell.data.page_root = Some((page.clone(), widget));
    }
    let Some((_, widget)) = shell.data.page_root.clone() else { return };
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        ui.route = commands::page_route(&page);
        ui.target = None;
        ui.tab_key = None;
        ui.activity = crate::common::PanelActivity::default();
    }
    cx.begin_turtle(Walk::abs_rect(r), Layout::default());
    widget.draw_walk_all(cx, scope, Walk::fill());
    cx.end_turtle();
}

fn draw_compact(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, state: &Value, content: Rect, page_shown: bool) {
    if page_shown {
        draw_page(shell, cx, scope, content);
        return;
    }
    match shell.data.layout.compact_destination {
        CompactDestination::Sidebar => {
            shell.data.chrome.sidebar_rect = content;
            sidebar::draw(shell, cx, scope, state, content);
        }
        _ => {
            let pane = shell.data.layout.focused().clone();
            draw_pane(shell, cx, scope, state, &pane.id, &pane.tabs, pane.selected.as_deref(), content, false);
        }
    }
}

/// Compact Explorer overlay (`_compactExplorerOverlay`): slides from the right over a 50%
/// black backdrop. Drawn by the overlay layer so it covers the conversation.
pub fn draw_compact_explorer(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, window: Rect) {
    let position = shell.data.chrome.explorer_position;
    if !shell.data.compact || position <= 0.0 {
        return;
    }
    let state = match scope.data.get::<UiContext>() {
        Some(ui) => ui.state.clone(),
        None => return,
    };
    let p = theme::current_palette();
    shell.overlay_painter.fill(cx, window, with_alpha(vec4(0.0, 0.0, 0.0, 1.0), (0.5 * position) as f32), 0.0);
    shell.data.overlays.hits.push(window, "compact:backdrop".to_string());
    let insets = crate::shell::platform::safe_insets();
    let x = window.pos.x + window.size.x * (1.0 - position);
    let panel = rect(x, window.pos.y, window.size.x, window.size.y);
    shell.overlay_painter.fill(cx, panel, p.sidebar, 0.0);
    shell.data.overlays.hits.push(panel, "compact:panel".to_string());
    let inner = rect(panel.pos.x + insets.left, panel.pos.y + insets.top + 8.0, panel.size.x - insets.left - insets.right, panel.size.y - insets.top - 8.0 - insets.bottom);
    shell.data.chrome.explorer_rect = inner;
    let (keys, selected) = explorer_keys(shell);
    // The compact Explorer's tab rail lives in the overlay layer's hit map.
    std::mem::swap(&mut shell.painter, &mut shell.overlay_painter);
    let hits = std::mem::take(&mut shell.data.hits);
    shell.data.hits = std::mem::take(&mut shell.data.overlays.hits);
    draw_pane(shell, cx, scope, &state, "explorer", &keys, selected.as_deref(), inner, true);
    shell.data.overlays.hits = std::mem::replace(&mut shell.data.hits, hits);
    std::mem::swap(&mut shell.painter, &mut shell.overlay_painter);
}

// ---------------------------------------------------------------------------------------------
// Events

pub(crate) fn sidebar_visible(shell: &PaseoShell) -> bool {
    if matches!(shell.data.page.as_ref(), Some(crate::shell::Page::Settings { .. })) {
        return false;
    }
    if shell.data.compact {
        shell.data.page.is_none() && shell.data.layout.compact_destination == CompactDestination::Sidebar
    } else {
        shell.data.layout.sidebar_visible
    }
}

/// Pointer handling for chrome ids recorded in `shell.data.hits`.
pub fn handle_event(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, scope: &mut Scope) {
    sync_hidden(shell, cx);
    let sidebar_visible = sidebar_visible(shell);
    if !sidebar_visible {
        shell.data.sidebar.stop_motion();
    }
    let area = shell.data.bg_area;
    match event.hits(cx, area) {
        Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
            let hover = shell.data.hits.at(e.abs);
            let cursor = match hover.as_deref() {
                Some(id) if id.starts_with("split:") => {
                    let horizontal = shell.data.chrome.splits.iter().any(|(sid, axis, _)| format!("split:{sid}") == id && *axis == Axis::Horizontal);
                    if horizontal { MouseCursor::ColResize } else { MouseCursor::RowResize }
                }
                Some("resize:sidebar") | Some("resize:explorer") => MouseCursor::ColResize,
                Some(_) => MouseCursor::Hand,
                None => MouseCursor::Default,
            };
            cx.set_cursor(cursor);
            if hover != shell.data.pointer.hover {
                shell.data.pointer.hover = hover.clone();
                crate::shell::overlays::hover_tooltip(shell, cx, hover, e.abs);
                shell.redraw(cx);
            }
        }
        Hit::FingerHoverOut(_) => {
            if shell.data.pointer.hover.take().is_some() {
                crate::shell::overlays::hover_tooltip(shell, cx, None, DVec2::default());
                shell.redraw(cx);
            }
        }
        Hit::FingerDown(fe) => {
            shell.data.sidebar.stop_motion();
            let id = shell.data.hits.at(fe.abs);
            if fe.mouse_button().is_some_and(|b| b.is_secondary()) {
                if let Some(id) = id
                    && let Some(ui) = scope.data.get_mut::<UiContext>()
                {
                    commands::secondary(shell, cx, ui, &id, fe.abs);
                }
                return;
            }
            if shell.data.compact && (fe.abs.x < 24.0 || shell.data.layout.compact_destination == CompactDestination::Sidebar) {
                shell.data.chrome.edge_start = Some(fe.abs);
            }
            if sidebar_visible && fe.device.is_touch() && shell.data.sidebar.viewport.contains(fe.abs) {
                shell.data.sidebar.touch_down(fe.abs, fe.time);
            }
            start_drag(shell, id.as_deref(), fe.abs);
            shell.data.pointer.pressed = id;
            shell.redraw(cx);
        }
        Hit::FingerMove(fe) => {
            if fe.device.is_touch() && shell.data.sidebar.touch_move(fe.abs, fe.time) {
                shell.data.pointer.pressed = None;
                shell.data.chrome.edge_start = None;
                shell.redraw(cx);
                return;
            }
            if update_drag(shell, cx, fe.abs, fe.time) {
                shell.redraw(cx);
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
                        layout.show_sidebar(true, true);
                    } else if delta.x < 0.0 && layout.compact_destination == CompactDestination::Sidebar {
                        layout.show_sidebar(true, false);
                    }
                    shell.redraw(cx);
                    return;
                }
            }
            if dragged {
                shell.redraw(cx);
                return;
            }
            if let Some(id) = pressed
                && fe.is_over
                && !fe.has_long_press_occurred
                && (!fe.device.is_touch() || fe.was_tap())
                && shell.data.hits.at(fe.abs).as_deref() == Some(id.as_str())
                && let Some(ui) = scope.data.get_mut::<UiContext>()
            {
                commands::click(shell, cx, ui, &id, fe.modifiers);
            }
            if !fe.device.has_hovers() {
                shell.data.pointer.hover = None;
            }
            shell.redraw(cx);
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
            chrome.splits.iter().find(|(sid, _, _)| sid == split).map(|(sid, axis, r)| Drag::Split { id: sid.clone(), axis: *axis, rect: *r })
        }
        // Drags start from the width on screen (the saved width may be clamped to fit), so
        // the divider follows the pointer from the first pixel.
        Some("resize:sidebar") => Some(Drag::Sidebar { start_x: abs.x, start_width: chrome.sidebar_rect.size.x.max(1.0) }),
        Some("resize:explorer") => Some(Drag::Explorer { start_x: abs.x, start_width: chrome.explorer_rect.size.x.max(1.0) }),
        Some(id) if id.starts_with("tab:") && !id.starts_with("tab:explorer:") => {
            let rest = &id["tab:".len()..];
            rest.split_once(':').map(|(pane, key)| Drag::Tab { key: key.to_string(), from_pane: pane.to_string(), start: abs, moving: false })
        }
        _ => None,
    };
}

fn update_drag(shell: &mut PaseoShell, _cx: &mut Cx, abs: DVec2, _time: f64) -> bool {
    let Some(drag) = shell.data.chrome.drag.clone() else { return false };
    match drag {
        Drag::Split { id, axis, rect } => {
            let (start, extent) = match axis {
                Axis::Horizontal => (rect.pos.x, rect.size.x),
                Axis::Vertical => (rect.pos.y, rect.size.y),
            };
            let value = match axis {
                Axis::Horizontal => abs.x,
                Axis::Vertical => abs.y,
            };
            let available = (extent - 5.0).max(1.0);
            shell.data.layout.resize_split(&id, ((value - start) / available).clamp(0.15, 0.85));
            true
        }
        Drag::Sidebar { start_x, start_width } => {
            shell.data.layout.set_sidebar_width(start_width + abs.x - start_x);
            true
        }
        Drag::Explorer { start_x, start_width } => {
            shell.data.layout.set_explorer_width(start_width - (abs.x - start_x));
            true
        }
        Drag::Tab { key, from_pane, start, moving } => {
            let moving = moving || (abs - start).length() > 6.0;
            let drop = shell
                .data
                .chrome
                .pane_rects
                .iter()
                .find(|(id, r)| id != "explorer" && r.contains(abs))
                .map(|(id, _)| id.clone());
            shell.data.chrome.drop_pane = if moving { drop } else { None };
            shell.data.chrome.drag = Some(Drag::Tab { key, from_pane, start, moving });
            moving
        }
        Drag::ExplorerPan { .. } => false,
    }
}

/// Ends a drag: tab drops move the tab (reorder within the strip, or `moveTab`).
fn finish_drag(shell: &mut PaseoShell, cx: &mut Cx, abs: DVec2) -> bool {
    let Some(drag) = shell.data.chrome.drag.take() else { return false };
    shell.data.chrome.drop_pane = None;
    match drag {
        Drag::Tab { key, from_pane, moving: true, .. } => {
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
                let others: Vec<String> = shell.data.layout.tree.pane(&pane).map(|p| p.tabs.iter().filter(|k| **k != key).cloned().collect()).unwrap_or_default();
                let position = |k: &str| others.iter().position(|t| t == k);
                let index = match visible.iter().find(|(_, before)| *before) {
                    Some((k, _)) => position(k),
                    None => visible.last().and_then(|(k, _)| position(k)).map(|i| i + 1),
                };
                if pane == from_pane {
                    if let Some(old) = shell.data.layout.pane_of(&key).and_then(|id| shell.data.layout.tree.pane(&id).map(|p| p.tabs.iter().position(|k| *k == key))).flatten() {
                        let count = shell.data.layout.tree.pane(&pane).map_or(0, |p| p.tabs.len());
                        let new = index.unwrap_or(count.saturating_sub(1));
                        shell.data.layout.reorder_tab(&pane, old, new);
                    }
                } else {
                    shell.data.layout.move_tab(&key, &pane, index);
                }
            }
            shell.redraw(cx);
            true
        }
        Drag::Tab { .. } => false,
        Drag::Split { .. } | Drag::Sidebar { .. } | Drag::Explorer { .. } => true,
        Drag::ExplorerPan { .. } => true,
    }
}

/// 220ms `Cubic(0.25, 0.1, 0.25, 1)` slide of the compact Explorer towards its destination.
pub(crate) fn animate_explorer(shell: &mut PaseoShell, cx: &mut Cx, event: &Event) {
    let open = shell.data.compact && shell.data.layout.compact_destination == CompactDestination::Explorer;
    let target = if open { 1.0 } else { 0.0 };
    let chrome = &mut shell.data.chrome;
    if matches!(chrome.drag, Some(Drag::ExplorerPan { .. })) {
        chrome.explorer_anim = None;
        chrome.explorer_anim_start = None;
        return;
    }
    if !shell.data.compact {
        chrome.explorer_position = 0.0;
        chrome.explorer_anim = None;
        chrome.explorer_anim_start = None;
        return;
    }
    // Keep the pending frame until it arrives; replacing its token here prevents
    // the first animation frame from ever matching.
    if (chrome.explorer_target - target).abs() > f64::EPSILON
        || (chrome.explorer_anim.is_none() && chrome.explorer_anim_start.is_none() && (chrome.explorer_position - target).abs() > 1e-3)
    {
        chrome.explorer_target = target;
        chrome.explorer_anim_start = None;
        chrome.explorer_anim = Some(cx.new_next_frame());
    }
    if let Some(frame) = chrome.explorer_anim.and_then(|next| next.is_event(event)) {
        chrome.explorer_anim = None;
        let (start_time, from, to) = *chrome.explorer_anim_start.get_or_insert((frame.time, chrome.explorer_position, chrome.explorer_target));
        let t = ((frame.time - start_time) / 0.22).clamp(0.0, 1.0);
        chrome.explorer_position = from + (to - from) * cubic_ease(t);
        if t < 1.0 {
            chrome.explorer_anim = Some(cx.new_next_frame());
        } else {
            chrome.explorer_anim_start = None;
            chrome.explorer_position = to;
        }
        shell.redraw(cx);
    }
}

/// `Cubic(0.25, 0.1, 0.25, 1)` evaluated at `t` by bisection on x.
fn cubic_ease(t: f64) -> f64 {
    let (x1, y1, x2, y2) = (0.25, 0.1, 0.25, 1.0);
    let bezier = |a: f64, b: f64, s: f64| 3.0 * a * s * (1.0 - s).powi(2) + 3.0 * b * s * s * (1.0 - s) + s * s * s;
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..24 {
        let mid = (lo + hi) * 0.5;
        if bezier(x1, x2, mid) < t { lo = mid } else { hi = mid }
    }
    bezier(y1, y2, (lo + hi) * 0.5)
}

fn pane_at(shell: &PaseoShell, abs: DVec2) -> Option<String> {
    shell.data.chrome.pane_rects.iter().find(|(id, r)| id != "explorer" && r.contains(abs)).map(|(id, _)| id.clone())
}

/// Pane focus follows a press anywhere in a pane, including content that captures the press
/// itself (terminal, editor, chat). Runs before panels see the event. A mouse focuses on press;
/// a touch only on a tap released in the pane it started in, so drags never retarget the pane.
pub(crate) fn observe_pane_focus(shell: &mut PaseoShell, event: &Event) {
    const TAP_SLOP: f64 = 12.0;
    let window = shell.data.os_window;
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
                        shell.data.chrome.touch_focus = pane_at(shell, touch.abs).map(|pane| (touch.uid, pane, touch.abs));
                    }
                    TouchState::Move => {
                        if shell.data.chrome.touch_focus.as_ref().is_some_and(|(uid, _, start)| *uid == touch.uid && (touch.abs - *start).length() > TAP_SLOP) {
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
pub(crate) fn fit_tabs(natural: &[f64], active: usize, available: f64, gap: f64, min_w: f64, overflow_w: f64) -> TabFit {
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
    let room = if total(&widths) > available { (available - overflow_w).max(0.0) } else { available };
    let Some(selected) = widths.get_mut(active) else {
        return TabFit { widths, first: 0, end: 0 };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_that_fit_keep_their_widths() {
        let fit = fit_tabs(&[120.0, 150.0], 1, 400.0, 0.0, 138.0, 36.0);
        assert_eq!(fit, TabFit { widths: vec![120.0, 150.0], first: 0, end: 2 });
    }

    #[test]
    fn other_tabs_shrink_before_any_tab_is_hidden() {
        let fit = fit_tabs(&[240.0, 240.0, 200.0], 2, 500.0, 0.0, 138.0, 36.0);
        assert_eq!(fit, TabFit { widths: vec![150.0, 150.0, 200.0], first: 0, end: 3 });
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
}
