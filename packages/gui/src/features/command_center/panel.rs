//! `CommandCenterPanel` (Flutter `ui/command_center.dart` `_CommandCenter`): the query
//! input with the Files scope chip, the file search, the action / workspace / agent / file
//! sections, keyboard selection with reveal-once scrolling and row activation. The DSL
//! lives in `command_center/mod.rs`.

use makepad_widgets::*;
use crate::features::workspace::kit::{WaModalWidgetExt, WaModalWidgetRefExt};
use serde_json::{json, Value};

use super::matching::highlight_ranges;
use super::results::{self, Contribution, Icon, Kind, ListRow, Projection, ResultRow, Run, Section};
use super::{parse_ms, time_ago};
use crate::common::provider_icon::ProviderIconWidgetRefExt;
use crate::common::paint::{Font, Painter, TextSpec, rect};
use crate::common::widgets::{agent_status_bucket, StatusBucket, StatusMarkWidgetRefExt, SurfaceRole, SurfaceWidgetRefExt};
use crate::features::files::kit::DrawFilesImage;
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use crate::features::workspace::host::{self, agent_of};
use crate::features::workspace::i18n::{l, t, ta};
use crate::features::workspace::identity::project_view_key;
use crate::features::workspace::kit::{set_optional, set_project_icon, WaButtonWidgetRefExt, WaIconWidgetRefExt, WaInputWidgetExt, WaInputWidgetRefExt};
use crate::features::workspace::rpc::{array_of, new_id, object_of, str_of, str_or, Pending};

#[derive(Clone, Debug, DefaultNone)]
pub enum CommandCenterAction {
    None,
    /// A shell command (`onCommand: _execute`), handled by the shell after the palette closed.
    Command { action: String, args: Value },
}

/// A workspace file row: (path, name, directory).
type FileEntry = (String, String, String);

/// A debounced file search: (generation, host, cwd, query, workspace).
type PendingSearch = (u64, String, String, String, String);

#[derive(Clone, Debug)]
enum Req {
    Files { generation: u64 },
}

#[derive(Live, LiveHook, Widget)]
pub struct CommandCenterPanel {
    #[deref]
    view: View,
    #[rust]
    open: bool,
    #[rust]
    files_scope: bool,
    #[rust]
    query: String,
    #[rust]
    focus_query: bool,
    // `workspace-file-search.ts`
    #[rust]
    file_timer: Timer,
    #[rust]
    file_generation: u64,
    #[rust]
    file_source: Option<String>,
    #[rust]
    file_request: Option<String>,
    #[rust]
    file_entries: Vec<FileEntry>,
    #[rust]
    file_loading: bool,
    #[rust]
    file_error: Option<String>,
    #[rust]
    pending_search: Option<PendingSearch>,
    #[rust]
    pending: Pending<Req>,
    // `interaction.ts`: the selection persists; permission to move the viewport lasts for
    // one user command.
    #[rust]
    active_id: Option<String>,
    #[rust]
    reveal_pending: bool,
    #[rust]
    projection: Option<Projection>,
    #[rust]
    compact: bool,
    #[rust]
    pending_effects: Vec<UiEffect>,
    /// Relative times refresh every minute while the palette is open.
    #[rust]
    clock: Timer,
    /// Files state revision (material file icons decode off the GUI thread).
    #[rust]
    files_revision: u64,
    #[rust]
    panel_bounds: Option<(bool, DVec2)>,
}

// ---------------------------------------------------------------------------------------------
// Row primitives

/// One run of a [`WaMatchText`]: its text, the char ranges the query marked, and whether its
/// unmarked parts are muted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchSpan {
    pub text: String,
    pub marks: Vec<(usize, usize)>,
    pub muted: bool,
}

impl MatchSpan {
    fn new(text: &str, query: &str, muted: bool) -> Self {
        let marks = if query.trim().is_empty() { Vec::new() } else { highlight_ranges(query, text, false) };
        Self { text: text.to_owned(), marks, muted }
    }
}

/// Original `HighlightedText`: one line of spans with the query's marked runs on a surface3
/// tint in the foreground colour, ellipsized; `tail` (the timestamp) follows the text, is never
/// marked and never truncated.
#[derive(Live, LiveHook, Widget)]
pub struct WaMatchText {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    /// Authored px.
    #[live(14.0)]
    size: f64,
    #[live(20.0)]
    line_height: f64,
    #[rust]
    spans: Vec<MatchSpan>,
    #[rust]
    tail: String,
}

impl Widget for WaMatchText {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let p = crate::theme::current_palette();
        let t = crate::theme::current_typography();
        let walk = Walk { height: Size::Fixed(self.line_height), ..walk };
        let r = cx.walk_turtle(walk);
        // Opens the box batch before any text so marks paint under their glyphs.
        self.painter.capture_box(cx, r, Vec4::default(), Vec4::default(), 0.0, 0.0);
        let spec = TextSpec::new(Font::Regular, t.ui_size(self.size), p.foreground);
        let tail_w = if self.tail.is_empty() { 0.0 } else { self.painter.measure(cx, spec, &self.tail).x };
        let limit = r.pos.x + (r.size.x - tail_w).max(0.0);
        let mut x = r.pos.x;
        let mut clipped = false;
        let spans = std::mem::take(&mut self.spans);
        'spans: for span in &spans {
            let chars: Vec<char> = span.text.chars().collect();
            let mut chunks: Vec<(String, bool)> = Vec::new();
            let mut cursor = 0usize;
            for &(start, length) in &span.marks {
                let start = start.max(cursor).min(chars.len());
                let end = (start + length).min(chars.len());
                if end <= start {
                    continue;
                }
                if start > cursor {
                    chunks.push((chars[cursor..start].iter().collect(), false));
                }
                chunks.push((chars[start..end].iter().collect(), true));
                cursor = end;
            }
            if cursor < chars.len() {
                chunks.push((chars[cursor..].iter().collect(), false));
            }
            for (text, marked) in chunks {
                let color = if marked || !span.muted { p.foreground } else { p.muted };
                let spec = TextSpec { color, ..spec };
                let room = limit - x;
                let width = self.painter.measure(cx, spec, &text).x;
                let shown = if width <= room + 0.5 { text } else { self.painter.fit(cx, spec, &text, room.max(0.0)) };
                let shown_w = self.painter.measure(cx, spec, &shown).x;
                if marked {
                    let height = self.painter.line_height(cx, spec);
                    self.painter.fill(cx, rect(x, r.pos.y + (r.size.y - height) * 0.5, shown_w, height), p.surface3, 0.0);
                }
                self.painter.text_in(cx, rect(x, r.pos.y, shown_w + 1.0, r.size.y), spec, &shown, 0.0);
                x += shown_w;
                if width > room + 0.5 {
                    clipped = true;
                    break 'spans;
                }
            }
        }
        self.spans = spans;
        let _ = clipped;
        if !self.tail.is_empty() {
            let tail = self.tail.clone();
            self.painter.text_in(cx, rect(x, r.pos.y, tail_w + 1.0, r.size.y), TextSpec { color: p.muted, ..spec }, &tail, 0.0);
        }
        DrawStep::done()
    }
}

impl WaMatchTextRef {
    pub fn set(&self, cx: &mut Cx, spans: Vec<MatchSpan>, tail: &str) {
        if let Some(mut inner) = self.borrow_mut()
            && (inner.spans != spans || inner.tail != tail)
        {
            inner.spans = spans;
            inner.tail = tail.to_owned();
            inner.redraw(cx);
        }
    }
}

/// The material file icon (Files feature textures) for a file row.
#[derive(Live, LiveHook, Widget)]
pub struct WaFileIcon {
    #[redraw]
    #[live]
    image: DrawFilesImage,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[rust]
    name: String,
}

impl Widget for WaFileIcon {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let r = cx.walk_turtle(walk);
        let name = self.name.clone();
        crate::features::files::draw_file_icon(cx, &mut self.image, r, &name);
        DrawStep::done()
    }
}

impl WaFileIconRef {
    pub fn set_name(&self, cx: &mut Cx, name: &str) {
        if let Some(mut inner) = self.borrow_mut()
            && inner.name != name
        {
            inner.name = name.to_owned();
            inner.redraw(cx);
        }
    }
}

/// Original `ShortcutBadge`: the OS-formatted shortcut as small muted text on a half-opacity
/// surface3, 4/2 insets, 6px radius.
#[derive(Live, LiveHook, Widget)]
pub struct WaShortcutBadge {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[rust]
    label: String,
}

impl Widget for WaShortcutBadge {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = crate::theme::current_palette();
        let t = crate::theme::current_typography();
        let spec = TextSpec::new(Font::Regular, t.ui_size(12.0), p.muted);
        // A multi-step chord is a row of chips 4px apart.
        let steps: Vec<String> = self.label.split_whitespace().map(str::to_owned).collect();
        let sizes: Vec<DVec2> = steps.iter().map(|step| self.painter.measure(cx, spec, step)).collect();
        let height = sizes.iter().map(|s| s.y).fold(0.0, f64::max) + 4.0;
        let width = sizes.iter().map(|s| s.x + 8.0).sum::<f64>() + 4.0 * steps.len().saturating_sub(1) as f64;
        let walk = Walk { width: Size::Fixed(width), height: Size::Fixed(height), ..walk };
        let r = cx.walk_turtle(walk);
        let fill = crate::theme::with_alpha(p.surface3, p.surface3.w * 0.5);
        let mut x = r.pos.x;
        for (step, size) in steps.iter().zip(&sizes) {
            let chip = rect(x, r.pos.y, size.x + 8.0, height);
            self.painter.fill(cx, chip, fill, 6.0);
            self.painter.text_in(cx, chip, spec, step, 0.5);
            x += size.x + 12.0;
        }
        DrawStep::done()
    }
}

impl WaShortcutBadgeRef {
    pub fn set_label(&self, cx: &mut Cx, label: &str) {
        if let Some(mut inner) = self.borrow_mut()
            && inner.label != label
        {
            inner.label = label.to_owned();
            inner.redraw(cx);
        }
    }
}

fn workspace_bucket(status: &str) -> Option<StatusBucket> {
    match status {
        "needs_input" => Some(StatusBucket::NeedsInput),
        "failed" => Some(StatusBucket::Failed),
        "running" => Some(StatusBucket::Running),
        "attention" => Some(StatusBucket::Attention),
        "done" => Some(StatusBucket::Done),
        _ => None,
    }
}

/// Listed agents: not archived, top level, in a workspace.
fn listed_agent(agent: &Value) -> bool {
    agent.get("archivedAt").is_none_or(Value::is_null)
        && agent.get("parentAgentId").is_none_or(Value::is_null)
        && !str_of(agent, "workspaceId").is_empty()
}

impl CommandCenterPanel {
    /// The content root as a handle (the deref `View` is not `Clone`).
    fn root(&self) -> WidgetRef {
        self.view.widget(id!(wa_root))
    }

    fn target(ctx: &UiContext) -> Option<PanelTarget> {
        ctx.layout.selected_target().cloned()
    }

    /// Files are searched in the active workspace when the Files scope is on or anything has
    /// been typed; the previous entries stay up while the next query loads.
    fn sync_file_search(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let target = Self::target(ctx);
        let enabled = self.files_scope || !self.query.trim().is_empty();
        let source = target
            .as_ref()
            .filter(|t| !t.server_id.is_empty() && !t.cwd.is_empty())
            .map(|t| format!("{}\u{0}{}\u{0}{}", t.server_id, t.workspace_id, t.cwd));
        let request = source.as_ref().filter(|_| enabled).map(|s| format!("{s}\u{0}{}", self.query));
        if request == self.file_request {
            return;
        }
        cx.stop_timer(self.file_timer);
        self.file_timer = Timer::default();
        self.file_generation += 1;
        self.file_request = request.clone();
        let Some(_) = request else {
            self.file_source = None;
            self.file_entries.clear();
            self.file_loading = false;
            self.file_error = None;
            return;
        };
        if self.file_source != source {
            self.file_entries.clear();
        }
        self.file_source = source;
        self.file_loading = true;
        self.file_error = None;
        let target = target.expect("source requires a target");
        self.pending_search = Some((self.file_generation, target.server_id, target.cwd, self.query.clone(), target.workspace_id));
        self.file_timer = cx.start_timeout(if self.query.trim().is_empty() { 0.0 } else { 0.1 });
    }

    fn run_file_search(&mut self, ctx: &UiContext) {
        let Some((generation, host_id, cwd, query, _)) = self.pending_search.take() else { return };
        if generation != self.file_generation {
            return;
        }
        self.pending.send(
            &ctx.core,
            &host_id,
            "directory_suggestions_request",
            json!({ "query": query, "cwd": cwd, "includeFiles": true, "includeDirectories": false, "matchMode": "fuzzy", "limit": 100 }),
            Req::Files { generation },
        );
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        host::learn(&ctx.events);
        for effect in self.pending_effects.drain(..) {
            ctx.effects.push(effect);
        }
        for (req, result) in self.pending.take_raw(&ctx.events) {
            let Req::Files { generation } = req;
            if !self.open || generation != self.file_generation {
                continue;
            }
            self.file_loading = false;
            match result {
                Ok(body) => {
                    let error = str_of(&body, "error");
                    if error.is_empty() {
                        self.file_error = None;
                        self.file_entries = array_of(&body, "entries")
                            .iter()
                            .filter(|e| e.get("kind").is_none_or(|k| k.is_null() || k.as_str() == Some("file")))
                            .map(|e| results::describe_file_path(str_of(e, "path")))
                            .collect();
                    } else {
                        self.file_entries.clear();
                        self.file_error = Some(error.to_owned());
                    }
                }
                Err(error) => {
                    self.file_entries.clear();
                    self.file_error = Some(error.to_string());
                }
            }
            self.view.redraw(cx);
        }
        if self.open && !ctx.events.is_empty() {
            self.sync_file_search(cx, ctx);
            self.view.redraw(cx);
        }
    }

    fn shortcut(ctx: &UiContext, help_id: &str) -> String {
        if host::is_mobile() {
            return String::new();
        }
        crate::common::shortcut_label(ctx, help_id).unwrap_or_default()
    }

    /// Root and workspace actions in registry order.
    fn contributions(&self, ctx: &UiContext) -> Vec<Contribution> {
        let compact = self.compact;
        let shortcuts = !host::is_mobile() && !compact;
        let shortcut = |id: &str| if shortcuts { Self::shortcut(ctx, id) } else { String::new() };
        let selected = Self::target(ctx);
        let hosts = host::hosts(&ctx.state);
        let host_id = selected.as_ref().map(|t| t.server_id.clone()).or_else(|| hosts.first().map(|h| h.id().to_owned()));
        let section = t("shell.commandCenter.actions");
        let root = |id: &str, rank: i64, title: String, icon: &'static str, keywords: &[&str], run: Run, query_only: bool, shortcut_id: &str| Contribution {
            id: format!("root:{id}"),
            group: "actions".into(),
            group_rank: 0,
            rank,
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            query_only,
            title,
            section_title: section.clone(),
            icon,
            shortcut: if shortcut_id.is_empty() { String::new() } else { shortcut(shortcut_id) },
            run,
        };
        let command = |action: &str, args: Value| Run::Command(action.to_owned(), args);
        let mut out = Vec::new();
        if let Some(host_id) = &host_id {
            let target = PanelTarget::new("new_tab", host_id, "", "").with_id(new_id()).with_title(l("Add project")).with_data(json!({ "addProject": true }));
            out.push(root(
                "add-project",
                0,
                t("shell.commandCenter.addProject"),
                "folder-plus",
                &["open", "project", "folder", "workspace", "repo"],
                Run::Effect(UiEffect::OpenTab { target, placement: TabPlacement::Main }),
                true,
                "new-agent",
            ));
        }
        out.push(root("new-workspace", 1, t("sidebar.actions.newWorkspace"), "plus", &["new", "workspace", "worktree", "branch"], command("workspace.new", Value::Null), false, "new-workspace"));
        if let Some(host_id) = &host_id {
            out.push(root(
                "import-session",
                2,
                t("importSession.title"),
                "import",
                &["import", "session", "terminal"],
                command("session.import", json!({ "serverId": host_id })),
                false,
                "",
            ));
            out.push(root(
                "history",
                4,
                t("sidebar.sections.sessions"),
                "history",
                &["history", "sessions", "recent"],
                Run::Effect(UiEffect::Navigate(crate::common::Route::Sessions { host_id: None })),
                false,
                "",
            ));
        }
        out.push(root(
            "schedules",
            5,
            t("sidebar.sections.schedules"),
            "calendar-clock",
            &["schedules", "scheduled", "automation", "recurring"],
            Run::Effect(UiEffect::Navigate(crate::common::Route::Schedules { host_id: host_id.clone().unwrap_or_default() })),
            false,
            "",
        ));
        out.push(root("settings", 6, t("sidebar.actions.settings"), "settings", &["settings", "preferences", "config", "configuration"], command("settings.toggle", Value::Null), false, "toggle-settings"));
        if shortcuts {
            out.push(root("keyboard-shortcuts", 7, t("sidebar.help.shortcuts"), "keyboard", &["keyboard", "shortcuts", "keys", "hotkeys"], command("shortcuts.dialog.toggle", Value::Null), false, "show-shortcuts"));
        }
        out.push(root(
            "toggle-left-sidebar",
            8,
            t("settings.shortcuts.help.toggleLeftSidebar"),
            "panel-left",
            &["toggle", "sidebar", "left", "panel", "workspaces"],
            command("sidebar.toggle.left", Value::Null),
            true,
            "toggle-left-sidebar",
        ));
        out.extend(self.workspace_contributions(ctx, &shortcut));
        out.sort_by(results::compare_contributions);
        out
    }

    /// `workspace-contributions.ts` for the focused workspace.
    fn workspace_contributions(&self, ctx: &UiContext, shortcut: &dyn Fn(&str) -> String) -> Vec<Contribution> {
        let Some(target) = Self::target(ctx).filter(|t| !t.server_id.is_empty() && !t.cwd.is_empty()) else { return Vec::new() };
        let host = host::host(&ctx.state, &target.server_id);
        let workspace = if target.workspace_id.is_empty() { None } else { host.and_then(|h| h.workspace(&target.workspace_id)).cloned() };
        let section = t("workspace.header.actions.workspaceActions");
        let can_split = !host::is_mobile() && !self.compact;
        let is_git = workspace.as_ref().is_some_and(|w| w.get("gitRuntime").is_some_and(|g| !g.is_null()) || str_of(w, "projectKind") == "git");
        let branch = workspace.as_ref().map(|w| str_of(object_of(w, "gitRuntime"), "currentBranch").trim().to_owned()).unwrap_or_default();
        let pinned = workspace.as_ref().is_some_and(|w| w.get("pinnedAt").is_some_and(|p| !p.is_null()));
        let can_pin = workspace.is_some() && host.is_some_and(|h| h.supports("workspacePinning"));
        let pane = ctx.layout.panes.iter().find(|p| p.id == ctx.layout.focused_pane);
        let tab_keys: Vec<String> = pane.map(|p| p.tabs.clone()).unwrap_or_default();
        let tab_index = pane.and_then(|p| p.selected.as_ref()).and_then(|s| tab_keys.iter().position(|k| k == s));
        let item = |id: &str, rank: i64, title: String, icon: &'static str, keywords: Vec<String>, run: Run, query_only: bool, shortcut_id: &str| Contribution {
            id: format!("workspace:{id}"),
            group: "workspace".into(),
            group_rank: -1,
            rank,
            keywords,
            query_only,
            title,
            section_title: section.clone(),
            icon,
            shortcut: if shortcut_id.is_empty() { String::new() } else { shortcut(shortcut_id) },
            run,
        };
        let kw = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let command = |action: &str, args: Value| Run::Command(action.to_owned(), args);
        let open = |key: &str| ta("shell.commandCenter.open", &[("name", &t(key))]);
        let copied = t("common.states.copied");
        let path = workspace.as_ref().map(|w| str_or(w, "workspaceDirectory", &target.cwd).to_owned()).unwrap_or_else(|| target.cwd.clone());
        let mut out = vec![item("tab:new-agent", 0, t("workspace.tabs.actions.newAgent"), "square-pen", kw(&["tab", "new", "agent", "chat"]), command("workspace.tab.target.agent", Value::Null), false, "workspace-tab-target-agent")];
        if !target.workspace_id.is_empty() {
            out.push(item("tab:new-terminal", 2, t("workspace.tabs.actions.newTerminal"), "square-terminal", kw(&["terminal", "shell", "console"]), command("workspace.terminal.new", Value::Null), true, "workspace-terminal-new"));
        }
        if is_git {
            out.push(item("tab:open:changes", 4, open("workspace.tabs.actions.changes"), "git-compare-arrows", kw(&["open", "changes", "tab", "supporting"]), command("workspace.tab.target.changes", Value::Null), true, ""));
        }
        out.push(item("tab:open:files", 5, open("workspace.tabs.actions.files"), "files", kw(&["open", "files", "tab", "supporting"]), command("workspace.tab.target.files", Value::Null), true, ""));
        if !tab_keys.is_empty() {
            out.push(item("tab:previous", 30, t("settings.shortcuts.help.previousTab"), "arrow-left", kw(&["tab", "previous", "switch", "navigate"]), command("workspace.tab.navigate.relative", json!({ "delta": -1 })), true, "workspace-tab-prev"));
            out.push(item("tab:next", 31, t("settings.shortcuts.help.nextTab"), "arrow-right", kw(&["tab", "next", "switch", "navigate"]), command("workspace.tab.navigate.relative", json!({ "delta": 1 })), true, "workspace-tab-next"));
            out.push(item("tab:close-current", 32, t("settings.shortcuts.help.closeCurrentTab"), "x", kw(&["tab", "close", "current"]), command("workspace.tab.close.current", Value::Null), true, "workspace-tab-close-current"));
            if (target.kind == "agent" || target.kind == "terminal") && target.id.as_deref().is_some_and(|id| !id.is_empty()) {
                let key = if target.kind == "agent" { "workspace.tabs.menu.copyAgentId" } else { "workspace.tabs.menu.copyTerminalId" };
                let effect = UiEffect::CopyText { text: target.id.clone().unwrap_or_default(), toast: Some(copied.clone()) };
                out.push(item("tab:copy-id", 34, t(key), "copy", kw(&["tab", "copy", "id"]), Run::Effect(effect), true, ""));
            }
            if target.kind == "file" {
                let file = str_or(&target.data, "path", target.id.as_deref().unwrap_or("")).to_owned();
                let effect = UiEffect::CopyText { text: file, toast: Some(copied.clone()) };
                out.push(item("tab:copy-file-path", 37, t("workspace.tabs.menu.copyFilePath"), "copy", kw(&["tab", "file", "copy", "path"]), Run::Effect(effect), true, ""));
            }
            if tab_index.is_some_and(|i| i > 0) {
                out.push(item("tab:close-left", 38, t("workspace.tabs.menu.closeLeft"), "x", kw(&["tab", "close", "left"]), command("workspace.tab.close-left", Value::Null), true, ""));
            }
            if tab_index.is_some_and(|i| i + 1 < tab_keys.len()) {
                out.push(item("tab:close-right", 39, t("workspace.tabs.menu.closeRight"), "x", kw(&["tab", "close", "right"]), command("workspace.tab.close-right", Value::Null), true, ""));
            }
            if tab_keys.len() > 1 {
                out.push(item("tab:close-others", 40, t("workspace.tabs.menu.closeOthers"), "x", kw(&["tab", "close", "others"]), command("workspace.tab.close-others", Value::Null), true, ""));
            }
        }
        if can_split {
            out.push(item("pane:split-right", 50, t("workspace.tabs.actions.splitRight"), "columns-2", kw(&["split", "pane", "vertical"]), command("workspace.pane.split.right", Value::Null), true, "workspace-pane-split-right"));
            out.push(item("pane:split-down", 51, t("workspace.tabs.actions.splitDown"), "rows-2", kw(&["split", "pane", "horizontal"]), command("workspace.pane.split.down", Value::Null), true, "workspace-pane-split-down"));
            for (index, direction) in ["left", "right", "up", "down"].iter().enumerate() {
                let capital = format!("{}{}", direction[..1].to_uppercase(), &direction[1..]);
                out.push(item(
                    &format!("pane:focus-{direction}"),
                    52 + index as i64,
                    t(&format!("settings.shortcuts.help.focusPane{capital}")),
                    "focus",
                    kw(&["pane", "focus", direction]),
                    command(&format!("workspace.pane.focus.{direction}"), Value::Null),
                    true,
                    "",
                ));
            }
            for (index, direction) in ["left", "right", "up", "down"].iter().enumerate() {
                let capital = format!("{}{}", direction[..1].to_uppercase(), &direction[1..]);
                out.push(item(
                    &format!("pane:move-tab-{direction}"),
                    56 + index as i64,
                    t(&format!("settings.shortcuts.help.moveTab{capital}")),
                    "move",
                    kw(&["pane", "tab", "move", direction]),
                    command(&format!("workspace.pane.move-tab.{direction}"), Value::Null),
                    true,
                    "",
                ));
            }
            out.push(item("pane:close", 60, t("settings.shortcuts.help.closePane"), "x", kw(&["pane", "close"]), command("workspace.pane.close", Value::Null), true, "workspace-pane-close"));
            out.push(item("pane:focus-mode-toggle", 62, t("settings.shortcuts.help.toggleFocusMode"), "arrow-down-to-line", kw(&["pane", "focus", "mode", "toggle"]), command("view.toggle.focus", Value::Null), true, "toggle-focus"));
        }
        if can_pin {
            let key = if pinned { "sidebar.workspace.actions.unpin" } else { "sidebar.workspace.actions.pin" };
            out.push(item("workspace:pin", 20, t(key), if pinned { "pin-off" } else { "pin" }, kw(&["pin", "unpin", "favorite", "sticky"]), command("workspace.pin", Value::Null), false, "pin-workspace"));
        }
        let copy_path = if path.is_empty() {
            Run::Error(t("sidebar.workspace.toasts.workspacePathUnavailable"))
        } else {
            Run::Effect(UiEffect::CopyText { text: path.clone(), toast: Some(t("sidebar.workspace.toasts.pathCopied")) })
        };
        out.push(item("workspace:copy-path", 22, t("workspace.header.actions.copyPath"), "copy", kw(&["copy", "path", "directory", "folder", "cwd"]), copy_path, true, ""));
        if !branch.is_empty() {
            let mut words = kw(&["copy", "branch", "git"]);
            words.push(branch.clone());
            let effect = UiEffect::CopyText { text: branch.clone(), toast: Some(t("sidebar.workspace.toasts.branchNameCopied")) };
            out.push(item("workspace:copy-branch-name", 23, t("workspace.header.actions.copyBranchName"), "git-branch", words, Run::Effect(effect), true, ""));
        }
        out.push(item("workspace:toggle-explorer-sidebar", 25, t("workspace.tabs.explorerSidebar.toggle"), "panel-right", kw(&["toggle", "sidebar", "explorer", "files", "changes"]), command("sidebar.toggle.right", Value::Null), true, "toggle-right-sidebar"));
        if !can_split {
            out.push(item("workspace:toggle-focus-mode", 26, t("settings.shortcuts.help.toggleFocusMode"), "focus", kw(&["toggle", "focus", "zen", "distraction", "fullscreen"]), command("view.toggle.focus", Value::Null), true, "toggle-focus"));
        }
        out
    }

    /// Workspace (rank 2) and agent (rank 3) rows, pinned below every action section.
    fn built_in_sections(&self, ctx: &UiContext) -> Vec<Section> {
        let hosts = host::hosts(&ctx.state);
        let show_host = hosts.len() > 1;
        let mut workspaces = Vec::new();
        let mut titles: Vec<(String, String)> = Vec::new();
        for host in &hosts {
            let label = host.label();
            for workspace in host.workspaces() {
                if workspace.get("archivingAt").is_some_and(|v| !v.is_null()) {
                    continue;
                }
                let id = str_of(workspace, "id");
                let title = if str_of(workspace, "title").trim().is_empty() { str_of(workspace, "name") } else { str_of(workspace, "title") }.to_owned();
                titles.push((format!("{}:{id}", host.id()), title.clone()));
                let project = object_of(workspace, "project");
                let project_name = str_or(workspace, "projectDisplayName", str_of(project, "projectName")).to_owned();
                let mut placement = project.clone();
                if !placement.is_object() {
                    placement = json!({});
                }
                placement["projectId"] = str_or(workspace, "projectId", str_of(project, "projectId")).into();
                let mut row = ResultRow::new(
                    format!("workspace:{}:{id}", host.id()),
                    Kind::Workspace,
                    title,
                    Run::OpenWorkspace { host_id: host.id().to_owned(), workspace: workspace.clone() },
                );
                row.subtitle = results::join_subtitle(&[show_host.then_some(label.as_str()), Some(&project_name), Some(str_of(object_of(workspace, "gitRuntime"), "currentBranch"))]);
                row.activity_at = parse_ms(workspace.get("statusEnteredAt"));
                row.change_request_number = object_of(object_of(workspace, "githubRuntime"), "pullRequest").get("number").and_then(Value::as_i64);
                row.icon = Icon::Project {
                    name: project_name,
                    view_key: project_view_key(host.id(), &placement),
                    bucket: workspace_bucket(str_of(workspace, "status")),
                };
                workspaces.push(row);
            }
        }
        let mut agents = Vec::new();
        for host in &hosts {
            let label = host.label();
            for entry in host.agents() {
                let agent = agent_of(entry);
                if !listed_agent(agent) {
                    continue;
                }
                let key = format!("{}:{}", host.id(), str_of(agent, "workspaceId"));
                let workspace_title = titles.iter().find(|(k, _)| *k == key).map(|(_, t)| t.clone()).unwrap_or_else(|| super::shorten(str_of(agent, "cwd")));
                let target = crate::shell::commands::navigation_agent_target(host.id(), agent, None);
                let mut row = ResultRow::new(
                    format!("agent:{}:{}", host.id(), str_of(agent, "id")),
                    Kind::Agent,
                    results::agent_label(str_of(agent, "title")).unwrap_or_else(|| t("workspace.tabs.loading")),
                    Run::Effect(UiEffect::OpenTab { target, placement: TabPlacement::Main }),
                );
                row.subtitle = results::join_subtitle(&[show_host.then_some(label.as_str()), Some(&workspace_title)]);
                row.hidden = vec![str_of(agent, "cwd").to_owned()];
                row.activity_at = parse_ms(agent.get("lastActivityAt").filter(|v| !v.is_null()).or_else(|| agent.get("updatedAt")));
                row.icon = Icon::Provider { provider: str_of(agent, "provider").to_owned(), bucket: Some(agent_status_bucket(agent)) };
                agents.push(row);
            }
        }
        vec![
            Section { id: "workspaces".into(), band: 1, rank: 2, title: Some(t("shell.commandCenter.workspaces")), results: results::rank_workspaces(workspaces, &self.query), score: None },
            Section { id: "agents".into(), band: 1, rank: 3, title: Some(t("shell.commandCenter.agents")), results: results::rank_built_in(agents, &self.query), score: None },
        ]
    }

    /// Daemon-matched file rows carry no client score, so they are pinned (rank 4).
    fn file_section(&self, ctx: &UiContext) -> Section {
        let target = Self::target(ctx);
        let rows = match &target {
            Some(target) => self
                .file_entries
                .iter()
                .map(|(path, name, directory)| {
                    let effect = UiEffect::OpenFile { host_id: target.server_id.clone(), workspace_id: target.workspace_id.clone(), path: path.clone() };
                    let mut row = ResultRow::new(format!("file:{path}"), Kind::File, name.clone(), Run::Effect(effect));
                    row.subtitle = directory.clone();
                    row.icon = Icon::File(name.clone());
                    row
                })
                .collect(),
            None => Vec::new(),
        };
        Section { id: "files".into(), band: 1, rank: 4, title: Some(t("shell.commandCenter.files")), results: rows, score: None }
    }

    fn rebuild(&mut self, ctx: &UiContext) {
        let sections = if self.files_scope {
            vec![self.file_section(ctx)]
        } else {
            let mut sections = results::contribution_sections(&self.contributions(ctx), &self.query);
            sections.push(self.file_section(ctx));
            sections.extend(self.built_in_sections(ctx));
            sections
        };
        let projection = results::project_rows(sections);
        if !self.selection_frozen() {
            let ids: Vec<String> = projection.selectable.iter().filter_map(|i| row_id(&projection.rows[*i])).collect();
            if !self.active_id.as_ref().is_some_and(|id| ids.contains(id)) {
                self.active_id = ids.first().cloned();
            }
        }
        self.projection = Some(projection);
    }

    fn selection_frozen(&self) -> bool {
        self.files_scope && self.file_loading
    }

    fn move_active(&mut self, delta: isize) {
        let Some(projection) = &self.projection else { return };
        if self.selection_frozen() || projection.selectable.is_empty() {
            return;
        }
        let ids: Vec<String> = projection.selectable.iter().filter_map(|i| row_id(&projection.rows[*i])).collect();
        let current = self.active_id.as_ref().and_then(|id| ids.iter().position(|i| i == id));
        let start = match current {
            None if delta < 0 => 0,
            None => ids.len() - 1,
            Some(position) => position,
        } as isize;
        let next = (start + delta).rem_euclid(ids.len() as isize) as usize;
        self.active_id = Some(ids[next].clone());
        self.reveal_pending = true;
    }

    fn select(&mut self, cx: &mut Cx, scope: &mut Scope, row: ResultRow) {
        self.close_panel(cx);
        let uid = self.widget_uid();
        match row.run {
            Run::Command(action, args) => cx.widget_action(uid, &scope.path, CommandCenterAction::Command { action, args }),
            Run::Effect(effect) => self.pending_effects.push(effect),
            Run::Error(message) => self.pending_effects.push(UiEffect::Toast { message, is_error: true }),
            Run::OpenWorkspace { host_id, workspace } => {
                if let Some(ctx) = scope.data.get_mut::<UiContext>() {
                    self.pending_effects.push(open_workspace(ctx, &host_id, &workspace));
                }
            }
        }
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            ctx.effects.append(&mut self.pending_effects);
        }
    }

    fn close_panel(&mut self, cx: &mut Cx) {
        if self.open {
            self.open = false;
            self.focus_query = false;
            self.pending_search = None;
            let query = self.view.wa_input(id!(query));
            query.input().release_focus(cx);
            self.pending_effects.push(UiEffect::SetFocusScope { owner: query.widget_uid(), scope: None });
            self.view.wa_modal(id!(modal)).close(cx);
            cx.stop_timer(self.file_timer);
            self.file_timer = Timer::default();
            cx.stop_timer(self.clock);
            self.clock = Timer::default();
        }
    }

    fn set_files_scope(&mut self, cx: &mut Cx, ctx: Option<&UiContext>, files: bool) {
        self.files_scope = files;
        self.active_id = None;
        self.reveal_pending = true;
        if let Some(ctx) = ctx {
            self.sync_file_search(cx, ctx);
        }
        self.view.redraw(cx);
    }

    fn present(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let view = self.root();
        self.compact = ctx.compact;
        self.rebuild(ctx);
        view.widget(id!(scope_chip)).set_visible(cx, self.files_scope);
        view.wa_button(id!(scope_chip)).set_text(cx, &t("shell.commandCenter.files"));
        view.wa_input(id!(query)).set_empty_text(cx, t(if self.files_scope { "shell.commandCenter.filePlaceholder" } else { "shell.commandCenter.placeholder" }));
        view.widget(id!(file_spinner)).set_visible(cx, self.file_loading);
        let error = self.file_error.as_ref().map(|e| format!("{}: {e}", t("common.errors.error"))).unwrap_or_default();
        set_optional(cx, &view.widget(id!(file_error)), &error);
        let empty = self.projection.as_ref().is_some_and(|p| p.rows.is_empty()) && !self.file_loading && self.file_error.is_none();
        set_optional(cx, &view.widget(id!(no_matches)), if empty { t("shell.commandCenter.noMatches") } else { String::new() }.as_str());
        // Reveal the active row once per user command.
        if self.reveal_pending {
            if let (Some(projection), Some(active)) = (&self.projection, &self.active_id) && let Some(index) = projection.rows.iter().position(|r| row_id(r).as_deref() == Some(active.as_str())) {
                let list = view.portal_list(id!(list));
                let first = list.first_id();
                let visible = list.visible_items().max(1);
                if index < first {
                    list.set_first_id(index.saturating_sub(1));
                } else if index >= first + visible.saturating_sub(1) {
                    list.set_first_id((index + 2).saturating_sub(visible));
                }
            }
            if !self.file_loading {
                self.reveal_pending = false;
            }
        }
    }

    fn draw_rows(&mut self, cx: &mut Cx2d, scope: &mut Scope, list: &mut PortalList, ctx_compact: bool) {
        let Some(projection) = &self.projection else { return };
        list.set_item_range(cx, 0, projection.rows.len());
        let now = chrono::Utc::now();
        let inset = if ctx_compact { 12.0 } else { 6.0 };
        while let Some(index) = list.next_visible_item(cx) {
            let Some(row) = projection.rows.get(index) else { continue };
            match row {
                ListRow::Section { title, divider } => {
                    let item = list.item(cx, index, live_id!(SectionRow));
                    item.widget(id!(divider)).set_visible(cx, *divider);
                    set_optional(cx, &item.widget(id!(title)), title.as_deref().unwrap_or(""));
                    item.draw_all(cx, scope);
                }
                ListRow::Result(result) => {
                    let template = match result.kind {
                        Kind::File => live_id!(FileRow),
                        _ if result.two_line() => live_id!(TwoLineRow),
                        _ => live_id!(ActionRow),
                    };
                    let item = list.item(cx, index, template);
                    let active = self.active_id.as_deref() == Some(result.id.as_str());
                    item.as_surface().set_role(cx, if active { SurfaceRole::Surface1 } else { SurfaceRole::Transparent });
                    item.apply_over(cx, live! { padding: { top: (inset), bottom: (inset) } });
                    let query = self.query.as_str();
                    let title = item.wa_match_text(id!(title));
                    match result.kind {
                        Kind::File => {
                            let mut spans = vec![MatchSpan::new(&result.title, query, false)];
                            if !result.subtitle.is_empty() {
                                spans.push(MatchSpan { text: " ".into(), marks: Vec::new(), muted: true });
                                spans.push(MatchSpan::new(&result.subtitle, query, true));
                            }
                            title.set(cx, spans, "");
                        }
                        Kind::Workspace | Kind::Agent => {
                            title.set(cx, vec![MatchSpan::new(&result.title, query, false)], "");
                            // The timestamp is its own text so a long subtitle truncates without
                            // pushing it out of view, and it is never matched.
                            let time = time_ago(result.activity_at, now);
                            let tail = if time.is_empty() { String::new() } else if result.subtitle.is_empty() { time } else { format!("\u{a0}· {time}") };
                            item.wa_match_text(id!(subtitle)).set(cx, vec![MatchSpan::new(&result.subtitle, query, true)], &tail);
                        }
                        Kind::Action => {
                            // A one-line action keeps the 20px line; with a subtitle 18 + 16.
                            let single = result.subtitle.is_empty();
                            title.apply_over(cx, live! { line_height: (if single { 20.0 } else { 18.0 }) });
                            title.set(cx, vec![MatchSpan::new(&result.title, query, false)], "");
                            let subtitle = item.wa_match_text(id!(subtitle));
                            subtitle.set_visible(cx, !single);
                            subtitle.set(cx, vec![MatchSpan::new(&result.subtitle, query, true)], "");
                            let shortcut = item.widget(id!(shortcut));
                            shortcut.set_visible(cx, !result.shortcut.is_empty());
                            shortcut.wa_shortcut_badge(id!(badge)).set_label(cx, &result.shortcut);
                        }
                    }
                    let glyph = item.wa_icon(id!(glyph));
                    let file = item.wa_file_icon(id!(file));
                    let project = item.widget(id!(project));
                    let provider = item.widget(id!(provider_box));
                    let mark = item.widget(id!(mark_box));
                    glyph.set_visible(cx, false);
                    file.set_visible(cx, false);
                    project.set_visible(cx, false);
                    provider.set_visible(cx, false);
                    mark.set_visible(cx, false);
                    match &result.icon {
                        Icon::None => {}
                        Icon::Glyph(name) => {
                            glyph.set_visible(cx, true);
                            glyph.set_icon(cx, name);
                        }
                        Icon::File(name) => {
                            file.set_visible(cx, true);
                            file.set_name(cx, name);
                        }
                        Icon::Project { name, view_key, bucket } => {
                            project.set_visible(cx, true);
                            set_project_icon(cx, &project, name, view_key);
                            mark.set_visible(cx, bucket.is_some());
                            item.status_mark(id!(mark)).set_status(cx, *bucket);
                        }
                        Icon::Provider { provider: name, bucket } => {
                            provider.set_visible(cx, true);
                            item.provider_icon(id!(provider)).set_provider(cx, name, None);
                            mark.set_visible(cx, bucket.is_some());
                            item.status_mark(id!(mark)).set_status(cx, *bucket);
                        }
                    }
                    item.draw_all(cx, scope);
                }
            }
        }
    }
}

fn row_id(row: &ListRow) -> Option<String> {
    match row {
        ListRow::Result(result) => Some(result.id.clone()),
        ListRow::Section { .. } => None,
    }
}

/// `navigateToWorkspace` without a target: the workspace agent that needs attention most
/// opens first; otherwise its most recently active session, or a new session tab.
fn open_workspace(ctx: &UiContext, host_id: &str, workspace: &Value) -> UiEffect {
    let id = str_of(workspace, "id");
    let agents: Vec<&Value> = host::host(&ctx.state, host_id)
        .map(|h| h.agents().iter().map(agent_of).filter(|a| str_of(a, "workspaceId") == id).collect())
        .unwrap_or_default();
    let top_level = agents.iter().copied().filter(|a| str_of(object_of(a, "labels"), "paseo.parent-agent-id").trim().is_empty());
    if let Some(agent_id) = results::pick_attention_agent(top_level) && let Some(agent) = agents.iter().find(|a| str_of(a, "id") == agent_id) {
        let target = crate::shell::commands::navigation_agent_target(host_id, agent, None);
        return UiEffect::OpenTab { target, placement: TabPlacement::Main };
    }
    let mut latest: Option<(&Value, Option<i64>)> = None;
    for agent in agents.iter().copied().filter(|a| listed_agent(a)) {
        let at = parse_ms(agent.get("lastActivityAt").filter(|v| !v.is_null()).or_else(|| agent.get("updatedAt")));
        if latest.is_none_or(|(_, best)| at.is_some() && best.is_none_or(|b| at.unwrap_or(0) > b)) {
            latest = Some((agent, at));
        }
    }
    if let Some((agent, _)) = latest {
        let target = crate::shell::commands::navigation_agent_target(host_id, agent, None);
        return UiEffect::OpenTab { target, placement: TabPlacement::Main };
    }
    let target = PanelTarget::new("new_tab", host_id, id, str_of(workspace, "workspaceDirectory")).with_id(new_id()).with_title(l("New session"));
    UiEffect::OpenTab { target, placement: TabPlacement::Main }
}

impl Widget for CommandCenterPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
            if self.file_timer.is_event(event).is_some() {
                self.file_timer = Timer::default();
                self.run_file_search(ctx);
            }
            if self.clock.is_event(event).is_some() {
                self.view.redraw(cx);
            }
            let revision = crate::features::files::revision();
            if self.open && revision != self.files_revision {
                self.files_revision = revision;
                self.view.redraw(cx);
            }
        }
        if !self.open {
            if matches!(event, Event::KeyFocus(_) | Event::KeyFocusLost(_) | Event::Actions(_)) {
                self.view.handle_event(cx, event, scope);
            }
            return;
        }
        // Backspace in an empty scoped query drops the Files scope.
        if let Event::KeyDown(key) = event {
            let input = self.view.wa_input(id!(query));
            if key.key_code == KeyCode::Backspace && self.files_scope && self.query.trim().is_empty() && input.key_focus(cx) {
                let ctx = scope.data.get_mut::<UiContext>();
                self.set_files_scope(cx, ctx.as_deref(), false);
                return;
            }
        }
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        let view = self.root();
        if !view.wa_modal(id!(modal)).is_open() {
            self.close_panel(cx);
            return;
        }
        let input = view.wa_input(id!(query));
        if let Some(text) = input.changed(actions) && text != self.query {
            self.query = text;
            self.active_id = None;
            self.reveal_pending = true;
            if let Some(ctx) = scope.data.get_mut::<UiContext>() {
                self.sync_file_search(cx, ctx);
            }
            self.view.redraw(cx);
        }
        if let Some(key) = input.key_down_unhandled(actions) {
            match key.key_code {
                KeyCode::ArrowDown => self.move_active(1),
                KeyCode::ArrowUp => self.move_active(-1),
                _ => {}
            }
            self.view.redraw(cx);
        }
        if input.escaped(actions) {
            self.close_panel(cx);
            return;
        }
        if view.wa_button(id!(scope_chip)).clicked(actions) {
            let ctx = scope.data.get_mut::<UiContext>();
            self.set_files_scope(cx, ctx.as_deref(), false);
        }
        let chosen = if input.returned(actions).is_some() {
            self.active_id.clone()
        } else {
            let list = view.portal_list(id!(list));
            list.items_with_actions(actions).into_iter().find_map(|(index, item)| {
                item.as_surface().clicked(actions).then(|| self.projection.as_ref().and_then(|p| p.rows.get(index)).and_then(row_id)).flatten()
            })
        };
        if let Some(id) = chosen {
            let row = self.projection.as_ref().and_then(|p| p.rows.iter().find_map(|r| match r {
                ListRow::Result(result) if result.id == id => Some((**result).clone()),
                _ => None,
            }));
            if let Some(row) = row {
                self.select(cx, scope, row);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.open {
            return self.view.draw_walk(cx, scope, walk);
        }
        let compact = scope.data.get_mut::<UiContext>().map(|ctx| {
            let compact = ctx.compact;
            self.present(cx, ctx);
            compact
        });
        let size = cx.current_pass_size();
        let sheet = host::is_mobile() && compact.unwrap_or(false);
        if self.panel_bounds != Some((sheet, size)) {
            self.panel_bounds = Some((sheet, size));
            let (align_y, top, width, height) = if sheet {
                (1.0, 0.0, size.x, size.y * 0.6)
            } else {
                // Flutter's desktop container has a fixed height, not a content-sized maximum.
                (0.0, 48.0, 640.0_f64.min(size.x * 0.92), 560.0_f64.min(size.y * 0.8))
            };
            self.view.widget(id!(modal)).apply_over(cx, live! { align: { y: (align_y) } });
            self.view.widget(id!(panel)).apply_over(cx, live! {
                width: (width), height: (height), margin: { top: (top) }
            });
        }
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            let list_ref = item.as_portal_list();
            if let Some(mut list) = list_ref.borrow_mut() {
                self.draw_rows(cx, scope, &mut list, compact.unwrap_or(false));
            }
        }
        if self.focus_query {
            self.focus_query = false;
            self.view.wa_input(id!(query)).set_key_focus(cx);
        }
        DrawStep::done()
    }
}

impl CommandCenterPanelRef {
    /// Opens the palette, optionally in the Files scope.
    pub fn open(&self, cx: &mut Cx, files: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.open = true;
        inner.files_scope = files;
        inner.query.clear();
        inner.active_id = None;
        inner.reveal_pending = true;
        inner.file_request = None;
        inner.focus_query = true;
        inner.view.wa_input(id!(query)).set_text(cx, "");
        inner.view.wa_modal(id!(modal)).open(cx);
        cx.stop_timer(inner.clock);
        inner.clock = cx.start_interval(60.0);
        inner.view.redraw(cx);
    }

    pub fn close(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.close_panel(cx);
        }
    }

    pub fn is_open(&self) -> bool {
        self.borrow().is_some_and(|inner| inner.open)
    }
}
