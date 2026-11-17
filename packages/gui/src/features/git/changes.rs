//! Historical commit patches with lazy loading, file navigation and text selection.

use super::chrome::{TOOLBAR_HEIGHT, palette, toolbar_band, toolbar_button, ui as ui_px};
use super::controller::DiffController;
use super::diff_doc::{DiffDoc, DocHit, DocInput, LineTarget};
use super::host;
use super::json::{flag, text};
use super::model::{ChangeKind, absolute_path, change_kind, now_ms};
use super::overlay::{
    MenuEntry, MenuItem, Overlay, OverlayHost, OverlayKind, OverlayResult, SheetRequest,
};
use super::paint::{Font, GitPaint, Hits, Retained, rect};
use super::store::GitStore;
use super::text::t;
use crate::common::overlay::tooltip_at;
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use crate::theme::DrawBox;
use makepad_widgets::Hit as Pointer;
use makepad_widgets::*;
use serde_json::{Map, Value, json};
use std::collections::HashSet;

/// The widget parts a sheet draws with.
pub struct OverlayParts<'a> {
    pub list: &'a mut DrawList2d,
    pub bg: &'a mut DrawBox,
    pub input: &'a mut TextInput,
}

#[derive(Debug)]
enum Purpose {
    FileMenu(String),
    LineMenu(LineTarget),
    Jump,
}

#[derive(Clone, Debug, PartialEq, Hash)]
enum Hit {
    Refresh,
    ShowInLog,
    Toggle(&'static str),
    Jump,
    Doc(DocHit),
}

pub struct ChangesView {
    pub target: PanelTarget,
    pub controller: DiffController,
    pub doc: DiffDoc,
    overlay: OverlayHost<Purpose>,
    collapsed_files: HashSet<String>,
    initial_revealed: bool,
    /// A refocus request from the shell (`focusRequestId` changed), applied at the next draw.
    pub pending_focus: Option<String>,
    hits: Hits<Hit>,
    hovered: Option<Hit>,
    pressed: Option<Hit>,
    pub spinning: bool,
    /// A material file icon was still decoding at the last draw.
    pub icons_pending: bool,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    horizontal_scroll: makepad_widgets::scroll_bars::TouchScroll,
    background: Retained<Hit>,
    toolbar: Retained<Hit>,
    banner: Retained<Hit>,
    body: Retained<Hit>,
    body_spinning: bool,
    content_revision: u64,
    layout_revision: u64,
}

fn preferences(ui: &UiContext) -> Map<String, Value> {
    let mut merged: Map<String, Value> =
        [("layout", json!("unified")), ("wrapLines", json!(false))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
    if let Value::Object(stored) = host::preference(ui, "changesPreferences") {
        for (key, value) in stored {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn pref_flag(prefs: &Map<String, Value>, key: &str) -> bool {
    prefs.get(key) == Some(&Value::Bool(true))
}

impl ChangesView {
    pub fn new(_cx: &mut Cx, ui: &UiContext, store: &mut GitStore, target: PanelTarget) -> Self {
        let mut controller = DiffController::new(&target, store);
        controller.refresh(ui, store, false);
        Self {
            target,
            controller,
            doc: DiffDoc::default(),
            overlay: OverlayHost::default(),
            collapsed_files: HashSet::new(),
            initial_revealed: false,
            pending_focus: None,
            hits: Hits::default(),
            hovered: None,
            pressed: None,
            spinning: false,
            icons_pending: false,
            touch_scroll: Default::default(),
            horizontal_scroll: Default::default(),
            background: Retained::default(),
            toolbar: Retained::default(),
            banner: Retained::default(),
            body: Retained::default(),
            body_spinning: false,
            content_revision: 0,
            layout_revision: 0,
        }
    }

    fn host(&self) -> &str {
        &self.target.server_id
    }

    fn cwd(&self) -> &str {
        &self.target.cwd
    }

    /// Releases everything this view holds in the store and the core.
    pub fn dispose(&mut self, store: &mut GitStore) {
        if let Some(core) = store.core().cloned() {
            self.controller.dispose(&core, store);
        }
    }

    pub fn close_overlay(&mut self, cx: &mut Cx) {
        self.overlay.close(cx);
    }

    /// Original `canUseSplitLayout`.
    fn can_split(compact: bool) -> bool {
        !compact && !host::is_mobile_platform()
    }

    fn split(compact: bool, prefs: &Map<String, Value>) -> bool {
        Self::can_split(compact) && prefs.get("layout").and_then(Value::as_str) == Some("split")
    }

    fn update_preferences(ui: &mut UiContext, updates: &[(&str, Value)]) {
        let mut prefs = preferences(ui);
        for (key, value) in updates {
            prefs.insert((*key).to_owned(), value.clone());
        }
        ui.push(UiEffect::SetPreference {
            key: "changesPreferences".into(),
            value: Value::Object(prefs),
        });
    }

    /// Applies a core batch / store change; returns true when a redraw is due.
    pub fn process(
        &mut self,
        _cx: &mut Cx,
        ui: &mut UiContext,
        store: &mut GitStore,
        visible: bool,
    ) -> bool {
        let mut dirty = false;
        if self.controller.active() != visible {
            self.controller.set_active(ui, store, visible);
            dirty = true;
        }
        dirty |= self.controller.process(ui, store);
        if dirty {
            self.content_revision = self.content_revision.wrapping_add(1);
        }
        dirty
    }

    fn open_overlay(&mut self, purpose: Purpose, kind: OverlayKind) {
        self.overlay.open(Overlay { purpose, kind });
    }

    /// Sends a menu / confirm / select opened during this event to the shell.
    pub fn flush_overlay(&mut self, ui: &mut UiContext) -> bool {
        self.overlay.flush(ui);
        self.overlay.take_changed()
    }

    fn toolbar_key(&self, ui: &UiContext, compact: bool) -> u64 {
        let prefs = host::preference(ui, "changesPreferences");
        let hovered = self
            .hovered
            .as_ref()
            .filter(|hit| !matches!(hit, Hit::Doc(_)));
        crate::common::layer::inputs((
            &self.target.title,
            &self.controller.subject,
            self.controller.files.len(),
            self.all_files_collapsed(),
            Self::can_split(compact) && prefs["layout"].as_str() == Some("split"),
            prefs["wrapLines"].as_bool() == Some(true),
            hovered,
            host::supports(ui, self.host(), super::log::FEATURE),
            compact,
        ))
    }

    fn body_key(&self, ui: &UiContext, compact: bool) -> u64 {
        let prefs = host::preference(ui, "changesPreferences");
        let syntax = host::preference(ui, "syntaxTheme")
            .as_str()
            .unwrap_or("one");
        crate::common::layer::inputs((
            self.content_revision,
            self.layout_revision,
            self.doc.presentation_key(),
            Self::can_split(compact) && prefs["layout"].as_str() == Some("split"),
            prefs["wrapLines"].as_bool() == Some(true),
            syntax,
            (
                &self.pending_focus,
                self.controller.loading,
                self.controller.patch_revision,
                &self.controller.error,
                self.controller.patches.len(),
                self.controller.patch_errors.len(),
            ),
        ))
    }

    pub fn invalidate(&self, cx: &mut Cx, ui: &UiContext, compact: bool) {
        self.toolbar
            .invalidate_if_changed(cx, self.toolbar_key(ui, compact));
        self.body
            .invalidate_if_changed(cx, self.body_key(ui, compact));
        self.banner.invalidate_if_changed(
            cx,
            crate::common::layer::inputs((
                self.controller.status.is_some(),
                &self.controller.error,
            )),
        );
    }

    pub fn invalidate_media(&self, cx: &mut Cx) {
        if self.icons_pending {
            self.overlay.invalidate(cx);
        }
    }

    pub fn animate(&self, cx: &mut Cx) {
        if self.doc.animating_selection() {
            self.body.invalidate(cx);
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl ChangesView {
    fn all_files_collapsed(&self) -> bool {
        !self.controller.files.is_empty()
            && self
                .controller
                .files
                .iter()
                .all(|f| self.collapsed_files.contains(text(f, "path")))
    }

    fn toggle_collapse_all(&mut self) {
        if self.all_files_collapsed() {
            self.collapsed_files.clear();
        } else {
            let paths: Vec<String> = self
                .controller
                .files
                .iter()
                .map(|f| text(f, "path").to_owned())
                .collect();
            self.collapsed_files.extend(paths);
        }
    }

    fn open_target(ui: &mut UiContext, target: PanelTarget) {
        ui.push(UiEffect::OpenTab {
            target,
            placement: TabPlacement::Main,
        });
    }

    fn open_file(&self, ui: &mut UiContext, path: &str, line: Option<i64>) {
        let mut data = json!({ "path": path });
        if let Some(line) = line {
            data["line"] = json!(line);
        }
        let target = PanelTarget::new("file", self.host(), &self.target.workspace_id, self.cwd())
            .with_id(path)
            .with_title(basename(path))
            .with_data(data);
        Self::open_target(ui, target);
    }

    /// Expands, selects and scrolls to `path`.
    pub fn focus(&mut self, ui: &UiContext, path: &str) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.collapsed_files.remove(path);
        self.controller.select_file(ui, path);
        self.doc.reveal(path);
    }

    /// Original `FileActionsContextMenu` for a changed file.
    fn file_menu(&mut self, path: &str, anchor: Rect) {
        let file = self
            .controller
            .files
            .iter()
            .find(|f| text(f, "path") == path);
        let deleted = file.is_some_and(|f| change_kind(f) == ChangeKind::Deleted);
        let mut entries = Vec::new();
        if !deleted {
            entries.push(MenuEntry::Item(
                MenuItem::new("open", t("workspace.fileActions.openFile")).icon("file-text"),
            ));
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Item(
            MenuItem::new("copy", t("workspace.fileActions.copyPath")).icon("copy"),
        ));
        entries.push(MenuEntry::Item(
            MenuItem::new("copy-relative", t("workspace.fileActions.copyRelativePath"))
                .icon("copy"),
        ));
        self.open_overlay(
            Purpose::FileMenu(path.to_owned()),
            OverlayKind::Menu {
                title: Some(basename(path).to_owned()),
                anchor,
                align_end: false,
                width: None,
                entries,
            },
        );
    }

    fn copy(ui: &mut UiContext, text: String) {
        ui.push(UiEffect::CopyText { text, toast: None });
    }

    fn line_menu(&mut self, target: LineTarget, anchor: Rect) {
        let line = &target.line;
        let entries = vec![
            MenuEntry::Item(MenuItem::new("copy", t("common.actions.copyLine")).icon("copy")),
            MenuEntry::Item(
                MenuItem::new("open", t("workspace.git.diff.openFileAtLine")).icon("file-text"),
            ),
        ];
        let title = format!(
            "{}:{}",
            target.path,
            line.new_number.or(line.old_number).unwrap_or_default()
        );
        self.open_overlay(
            Purpose::LineMenu(target),
            OverlayKind::Menu {
                title: Some(title),
                anchor,
                align_end: true,
                width: None,
                entries,
            },
        );
    }

    /// Opens the overlay for a pressed region.
    #[allow(clippy::too_many_arguments)]
    fn activate(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        hit: Hit,
        anchor: Rect,
        secondary: bool,
        compact: bool,
    ) {
        let prefs = preferences(ui);
        if matches!(
            &hit,
            Hit::Toggle("collapse") | Hit::Doc(DocHit::Header(_) | DocHit::Expand { .. })
        ) {
            self.layout_revision = self.layout_revision.wrapping_add(1);
        }
        match hit {
            Hit::Refresh => self.controller.refresh(ui, store, false),
            Hit::ShowInLog => {
                // The log limited to this commit's history, with it selected.
                let target = PanelTarget::new(
                    "git_log",
                    self.host(),
                    &self.target.workspace_id,
                    self.cwd(),
                )
                .with_data(json!({ "revision": self.controller.sha, "requestId": now_ms() }));
                Self::open_target(ui, target);
            }
            Hit::Toggle(which) => match which {
                "collapse" => self.toggle_collapse_all(),
                "split" => {
                    let split = Self::split(compact, &prefs);
                    Self::update_preferences(
                        ui,
                        &[("layout", json!(if split { "unified" } else { "split" }))],
                    );
                }
                "wrap" => Self::update_preferences(
                    ui,
                    &[("wrapLines", json!(!pref_flag(&prefs, "wrapLines")))],
                ),
                _ => {}
            },
            Hit::Jump => {
                // Original `_jumpToFile`: the fully expanded tree in an adaptive sheet.
                self.overlay.open_sheet(
                    cx,
                    Purpose::Jump,
                    SheetRequest::FileTree {
                        title: t("workspace.git.diff.jumpToFile.title"),
                        files: self.controller.files.clone(),
                        selected: Some(self.controller.selected_path.clone()),
                    },
                    parts.input,
                );
            }
            Hit::Doc(doc_hit) => self.activate_doc(ui, doc_hit, anchor, secondary),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn activate_doc(&mut self, ui: &mut UiContext, hit: DocHit, anchor: Rect, secondary: bool) {
        match hit {
            DocHit::Header(path) => {
                if secondary {
                    self.file_menu(&path, anchor);
                } else if !self.collapsed_files.remove(&path) {
                    self.collapsed_files.insert(path);
                }
            }
            DocHit::Expand { path, start } => self.doc.toggle_expand(&path, start),
            DocHit::Gutter { path, line } => {
                if let Some(target) = self.doc.line_target(&path, line) {
                    self.open_file(
                        ui,
                        &target.path,
                        target.line.new_number.or(target.line.old_number),
                    );
                }
            }
            DocHit::LineMenu { path, line } => {
                if let Some(target) = self.doc.line_target(&path, line) {
                    self.line_menu(target, anchor);
                }
            }
            DocHit::Retry(path) => self.controller.ensure_patch(ui, &path),
        }
    }

    /// Routes an event to the open overlay. Returns `(consumed, redraw)`:
    /// `consumed` while a sheet held the pointer for this event, `redraw`
    /// only when the sheet changed visibly or an answer arrived.
    pub fn handle_overlay(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        compact: bool,
    ) -> (bool, bool) {
        if !self.overlay.is_open() {
            return (false, self.overlay.take_changed());
        }
        let was_open = self.overlay.blocks_input();
        let result = self.overlay.handle(cx, event, parts.input);
        let answered = result.is_some();
        if let Some(result) = result {
            self.on_overlay(cx, ui, parts, store, result, compact);
        }
        let changed = self.overlay.take_changed() || answered;
        (was_open || self.overlay.blocks_input(), changed)
    }

    fn on_overlay(
        &mut self,
        _cx: &mut Cx,
        ui: &mut UiContext,
        _parts: &mut OverlayParts<'_>,
        _store: &mut GitStore,
        result: OverlayResult<Purpose>,
        _compact: bool,
    ) {
        let cwd = self.cwd().to_owned();
        match result {
            OverlayResult::Chose(Purpose::FileMenu(path), action) => match action.as_str() {
                "open" => self.open_file(ui, &path, None),
                "copy" => Self::copy(ui, absolute_path(&cwd, &path)),
                "copy-relative" => Self::copy(ui, path),
                _ => {}
            },
            OverlayResult::Chose(Purpose::LineMenu(target), action) => match action.as_str() {
                "copy" => Self::copy(ui, target.line.content.clone()),
                "open" => self.open_file(
                    ui,
                    &target.path,
                    target.line.new_number.or(target.line.old_number),
                ),
                _ => {}
            },
            OverlayResult::Chose(Purpose::Jump, path) => self.focus(ui, &path),
            _ => {}
        }
    }

    /// The original desktop `Tooltip` copy of a hovered region.
    fn tooltip_for(&self, hit: &Hit, ui: &UiContext, compact: bool) -> Option<String> {
        let prefs = preferences(ui);
        Some(match hit {
            Hit::Refresh => t("workspace.git.diff.refresh"),
            Hit::Jump => t("workspace.git.diff.jumpToFile.title"),
            Hit::ShowInLog => t("workspace.git.log.showInLog"),
            Hit::Toggle("collapse") => t(if self.all_files_collapsed() {
                "workspace.git.diff.expandAllFiles"
            } else {
                "workspace.git.diff.collapseAllFiles"
            }),
            Hit::Toggle("split") => t(if Self::split(compact, &prefs) {
                "workspace.git.diff.switchToUnified"
            } else {
                "workspace.git.diff.switchToSplit"
            }),
            Hit::Toggle("wrap") => t(if pref_flag(&prefs, "wrapLines") {
                "workspace.git.diff.scrollLongLines"
            } else {
                "workspace.git.diff.wrapLongLines"
            }),
            Hit::Doc(DocHit::Header(path)) => path.clone(),
            _ => return None,
        })
    }

    /// Pointer input outside overlays; returns true when a redraw is due.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_pointer(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        area: Area,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        compact: bool,
    ) -> bool {
        let wrap = pref_flag(&preferences(ui), "wrapLines");
        let hit = event.hits(cx, area);
        if !area.is_valid(cx) {
            self.touch_scroll.reset();
            self.horizontal_scroll.reset();
        }
        let dy = self.touch_scroll.handle_event(cx, event, &hit);
        let dx = self
            .horizontal_scroll
            .handle_axis_event(cx, event, &hit, Vec2Index::X);
        let origin = self.touch_scroll.origin();
        if !self.doc.contains(origin) {
            self.horizontal_scroll.reset();
        }
        if dy.is_some() || dx.is_some() {
            self.pressed = None;
            if self.doc.contains(origin) {
                let mut changed = false;
                if let Some(delta) = dy {
                    let moved = self.doc.scroll_by(dvec2(0.0, delta), wrap);
                    if !moved {
                        self.touch_scroll.stop_at_edge();
                    }
                    changed |= moved;
                }
                if let Some(delta) = dx {
                    let moved = self.doc.scroll_by(dvec2(delta, 0.0), wrap);
                    if !moved {
                        self.horizontal_scroll.stop_at_edge();
                    }
                    changed |= moved;
                }
                return changed;
            }
            self.touch_scroll.reset();
            self.horizontal_scroll.reset();
        }
        match hit {
            Pointer::FingerHoverIn(e) | Pointer::FingerHoverOver(e) => {
                // The document's scrollbars sit over its edges.
                let bar = self.doc.bar_at(e.abs);
                if bar.is_some() {
                    cx.set_cursor(MouseCursor::Default);
                }
                if bar != self.doc.bar_hovered {
                    self.doc.bar_hovered = bar;
                    if bar.is_some() && self.hovered.take().is_some() {
                        tooltip_at(ui, None, Rect::default(), 0.3);
                        self.doc.hovered = None;
                    }
                    return true;
                }
                if bar.is_some() {
                    return false;
                }
                let (anchor, hovered) = self
                    .hits
                    .find(e.abs)
                    .map(|(anchor, hit)| (anchor, Some(hit)))
                    .unwrap_or_default();
                let doc_hovered = match &hovered {
                    Some(Hit::Doc(doc)) => Some(doc.clone()),
                    _ => None,
                };
                // Inside the document's `SelectionArea` text shows the text cursor.
                cx.set_cursor(if hovered.is_some() {
                    MouseCursor::Hand
                } else if self.doc.contains(e.abs) {
                    MouseCursor::Text
                } else {
                    MouseCursor::Default
                });
                if hovered != self.hovered {
                    let tip = hovered
                        .as_ref()
                        .and_then(|hit| self.tooltip_for(hit, ui, compact));
                    tooltip_at(ui, tip.as_deref(), anchor, 0.3);
                    self.hovered = hovered;
                    self.doc.hovered = doc_hovered;
                    return true;
                }
            }
            Pointer::FingerHoverOut(_) => {
                let left_bar = self.doc.bar_hovered.take().is_some();
                if self.hovered.take().is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.3);
                    self.doc.hovered = None;
                    return true;
                }
                if left_bar {
                    return true;
                }
            }
            Pointer::FingerScroll(e) => {
                // The platform already turns Shift+wheel into horizontal scroll.
                let delta = e.scroll;
                if self.doc.contains(e.abs) {
                    return self.doc.scroll_by(delta, wrap);
                }
            }
            Pointer::FingerDown(e) => {
                // Original Tooltip hides when its control is pressed.
                tooltip_at(ui, None, Rect::default(), 0.3);
                // A mouse press on a scrollbar drags its thumb or pages its track.
                if !e.device.is_touch() && e.is_primary_hit() && self.doc.press_bar(e.abs) {
                    self.pressed = None;
                    return true;
                }
                self.pressed = self.hits.find(e.abs).map(|(_, hit)| hit);
                // The document is one `SelectionArea`: a mouse press starts a
                // selection (double click a word, triple click the whole text).
                let had_selection = self.doc.selection.take().is_some();
                if !e.device.is_touch() && e.is_primary_hit() && self.doc.contains(e.abs) {
                    cx.set_key_focus(area);
                    if let Some(pos) = self.doc.pos_at(e.abs) {
                        self.doc.selection = Some(match e.tap_count {
                            1 => (pos, pos),
                            2 => self.doc.word_at(pos),
                            _ => self.doc.paragraph_at(pos),
                        });
                        self.doc.drag_pointer = Some(e.abs);
                    }
                    return true;
                }
                return had_selection;
            }
            Pointer::FingerLongPress(e) => {
                // Touch long-press opens the context menu.
                if let Some((rect, hit)) = self.hits.find(e.abs)
                    && matches!(hit, Hit::Doc(DocHit::Header(_)))
                {
                    self.pressed = None;
                    self.activate(cx, ui, parts, store, hit, rect, true, compact);
                    return true;
                }
                // Touch selection in the document: the word under the finger.
                if self.doc.contains(e.abs)
                    && let Some(pos) = self.doc.pos_at(e.abs)
                {
                    self.pressed = None;
                    self.doc.selection = Some(self.doc.word_at(pos));
                    cx.set_key_focus(area);
                    return true;
                }
            }
            Pointer::FingerMove(e) if self.doc.drag_bar(e.abs) => return true,
            Pointer::FingerMove(e) if self.doc.drag_pointer.is_some() => {
                // A drag extends the selection; leaving the viewport auto-scrolls.
                if e.move_distance() > 4.0 {
                    self.pressed = None;
                }
                self.doc.drag_pointer = Some(e.abs);
                let viewport = self.doc.viewport();
                let inside = dvec2(
                    e.abs
                        .x
                        .clamp(viewport.pos.x, viewport.pos.x + viewport.size.x),
                    e.abs
                        .y
                        .clamp(viewport.pos.y + 1.0, viewport.pos.y + viewport.size.y - 1.0),
                );
                if let (Some(pos), Some((anchor, _))) =
                    (self.doc.pos_at(inside), self.doc.selection)
                {
                    self.doc.selection = Some((anchor, pos));
                }
                return true;
            }
            Pointer::FingerUp(e) => {
                if self.doc.end_bar_drag() {
                    return true;
                }
                self.doc.drag_pointer = None;
                let pressed = self.pressed.take();
                if !e.is_over || e.has_long_press_occurred || (e.device.is_touch() && !e.was_tap())
                {
                    return false;
                }
                let secondary = e.mouse_button().is_some_and(|b| b.is_secondary());
                let Some((rect, hit)) = self.hits.find(e.abs) else {
                    return false;
                };
                if pressed.as_ref() != Some(&hit) {
                    return false;
                }
                self.activate(cx, ui, parts, store, hit, rect, secondary, compact);
                return true;
            }
            Pointer::TextCopy(ce) => {
                if let Some(text) = self.doc.selected_text() {
                    *ce.response.borrow_mut() = Some(text);
                }
            }
            Pointer::KeyDown(ke)
                if ke.key_code == KeyCode::KeyA && (ke.modifiers.control || ke.modifiers.logo) =>
            {
                self.doc.select_all();
                return true;
            }
            Pointer::KeyFocusLost(_) => {
                return self.doc.selection.take().is_some();
            }
            _ => {}
        }
        false
    }
}

impl ChangesView {
    fn push_hit(&mut self, area: Rect, hit: Hit) {
        self.hits.push(area, hit);
    }

    fn is_hovered(&self, hit: &Hit) -> bool {
        self.hovered.as_ref() == Some(hit)
    }

    /// A trailing toolbar button ending at `right`; returns its left edge.
    #[allow(clippy::too_many_arguments)]
    fn toolbar_icon(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        compact: bool,
        right: f64,
        center_y: f64,
        icon: Option<&str>,
        selected: bool,
        hit: Hit,
    ) -> f64 {
        let hovered = self.is_hovered(&hit);
        let button = toolbar_button(
            cx, paint, compact, right, center_y, icon, selected, true, hovered,
        );
        self.push_hit(super::chrome::toolbar_hit(button, compact), hit);
        button.pos.x - super::chrome::toolbar_gap(compact, 1.0)
    }

    /// Original `ChangesDiffOnlyToolbar` with the bare toggles.
    #[allow(clippy::too_many_arguments)]
    fn diff_only_toolbar(
        &mut self,
        cx: &mut Cx2d,
        ui: &UiContext,
        paint: &mut GitPaint,
        band: Rect,
        compact: bool,
        prefs: &Map<String, Value>,
    ) {
        toolbar_band(cx, paint, band, false);
        let colors = palette();
        let center_y = band.pos.y + (band.size.y - 1.0) / 2.0;
        let mut right =
            band.pos.x + band.size.x - super::chrome::toolbar_trailing_padding(compact, false);
        let wrap = pref_flag(prefs, "wrapLines");
        right = self.toolbar_icon(
            cx,
            paint,
            compact,
            right,
            center_y,
            Some("wrap-text"),
            wrap,
            Hit::Toggle("wrap"),
        );
        if Self::can_split(compact) {
            right = self.toolbar_icon(
                cx,
                paint,
                compact,
                right,
                center_y,
                Some("columns-2"),
                Self::split(compact, prefs),
                Hit::Toggle("split"),
            );
        }
        if !self.controller.files.is_empty() {
            let icon = if self.all_files_collapsed() {
                "list-chevrons-up-down"
            } else {
                "list-chevrons-down-up"
            };
            right = self.toolbar_icon(
                cx,
                paint,
                compact,
                right,
                center_y,
                Some(icon),
                false,
                Hit::Toggle("collapse"),
            );
        }
        if host::supports(ui, self.host(), super::log::FEATURE) {
            right = self.toolbar_icon(
                cx,
                paint,
                compact,
                right,
                center_y,
                Some("git-graph"),
                false,
                Hit::ShowInLog,
            );
        }
        right = self.toolbar_icon(
            cx,
            paint,
            compact,
            right,
            center_y,
            Some("rotate-cw"),
            false,
            Hit::Refresh,
        );
        if !self.controller.files.is_empty() {
            right = self.toolbar_icon(
                cx,
                paint,
                compact,
                right,
                center_y,
                Some("folder-tree"),
                false,
                Hit::Jump,
            );
        }
        let title = self
            .target
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .or_else(|| self.controller.subject.clone())
            .unwrap_or_else(|| t("panels.diff.commitSubtitle"));
        let left = band.pos.x + 8.0 + 4.0;
        paint.text_centered(
            cx,
            Font::Regular,
            ui_px(14.0),
            colors.foreground,
            left,
            band.pos.y,
            band.size.y - 1.0,
            &title,
            (right - 8.0 - left).max(0.0),
        );
    }

    /// Original `DiffBodyContent` states: 64px from the top, centred copy
    /// (destructive for errors) under an optional 32px spinner.
    #[allow(clippy::too_many_arguments)]
    fn state(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        body: Rect,
        message: Option<&str>,
        spinner: bool,
        error: bool,
    ) {
        self.doc.hide();
        let colors = palette();
        let mut y = body.pos.y + 64.0;
        let inner = body.size.x - 48.0;
        if spinner {
            paint.spinner(
                cx,
                dvec2(body.pos.x + body.size.x / 2.0, y + 16.0),
                32.0,
                colors.muted,
                cx.seconds_since_app_start(),
            );
            self.spinning = true;
            y += 32.0 + 16.0;
        }
        if let Some(message) = message {
            let size = ui_px(14.0);
            let width = paint.measure(cx, Font::Regular, size, message).x.min(inner);
            paint.text_wrapped(
                cx,
                Font::Regular,
                size,
                if error { colors.danger } else { colors.muted },
                dvec2(body.pos.x + (body.size.x - width) / 2.0, y),
                message,
                inner,
            );
        }
    }

    /// Draws the whole view into `area`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        cx: &mut Cx2d,
        ui: &mut UiContext,
        _store: &GitStore,
        paint: &mut GitPaint,
        parts: &mut OverlayParts<'_>,
        area: Rect,
        compact: bool,
    ) {
        if let Some(path) = self.pending_focus.take() {
            self.focus(ui, &path);
        }
        let colors = palette();
        let prefs = preferences(ui);
        let split = Self::split(compact, &prefs);
        let wrap = pref_flag(&prefs, "wrapLines");
        self.hits.clear();
        self.spinning = false;
        self.icons_pending = false;
        if self.background.begin(cx, area, 0, &mut self.hits, paint) {
            paint.fill(cx, area, colors.surface0);
            self.background.end(cx, &self.hits, paint);
        }
        let toolbar_h = TOOLBAR_HEIGHT.max(ui_px(14.0) * 1.4 + 12.0);
        let toolbar = rect(area.pos.x, area.pos.y, area.size.x, toolbar_h);
        let toolbar_key = self.toolbar_key(ui, compact);
        if self
            .toolbar
            .begin(cx, toolbar, toolbar_key, &mut self.hits, paint)
        {
            self.diff_only_toolbar(cx, ui, paint, toolbar, compact, &prefs);
            self.toolbar.end(cx, &self.hits, paint);
        }
        let mut y = area.pos.y + toolbar_h;
        let error = self
            .controller
            .error
            .as_deref()
            .filter(|_| self.controller.status.is_some());
        let banner_h = error.map_or(0.0, |error| {
            paint.measure_wrapped(cx, Font::Regular, ui_px(12.0), error, area.size.x - 24.0) + 4.0
        });
        let banner = rect(area.pos.x, y, area.size.x, banner_h);
        let banner_key = crate::common::layer::inputs((
            self.controller.status.is_some(),
            &self.controller.error,
        ));
        if self
            .banner
            .begin(cx, banner, banner_key, &mut self.hits, paint)
        {
            if let Some(error) = error {
                paint.text_wrapped(
                    cx,
                    Font::Regular,
                    ui_px(12.0),
                    colors.danger,
                    dvec2(area.pos.x + 12.0, y),
                    error,
                    area.size.x - 24.0,
                );
            }
            self.banner.end(cx, &self.hits, paint);
        }
        y += banner_h;
        let body = rect(
            area.pos.x,
            y,
            area.size.x,
            (area.pos.y + area.size.y - y).max(0.0),
        );
        let body_key = self.body_key(ui, compact);
        if self.body.begin(cx, body, body_key, &mut self.hits, paint) {
            self.draw_body(cx, ui, paint, body, split, wrap);
            self.body_spinning = self.spinning;
            self.body.end(cx, &self.hits, paint);
            self.body.set_inputs(self.body_key(ui, compact));
        } else {
            self.spinning = self.body_spinning;
        }
        if self
            .overlay
            .draw(cx, parts.list, parts.bg, paint, parts.input, compact)
        {
            self.icons_pending = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_body(
        &mut self,
        cx: &mut Cx2d,
        ui: &mut UiContext,
        paint: &mut GitPaint,
        body: Rect,
        split: bool,
        wrap: bool,
    ) {
        let Some(status) = self.controller.status.clone() else {
            match self
                .controller
                .error
                .clone()
                .filter(|_| !self.controller.loading)
            {
                Some(error) => self.state(cx, paint, body, Some(&error), false, true),
                None => self.state(
                    cx,
                    paint,
                    body,
                    Some(&t("workspace.git.diff.checkingRepository")),
                    true,
                    false,
                ),
            }
            return;
        };
        if !flag(&status, "isGit") {
            self.state(
                cx,
                paint,
                body,
                Some(&t("workspace.git.diff.notRepository")),
                false,
                false,
            );
            return;
        }
        let has_changes = !self.controller.files.is_empty();
        if !has_changes && self.controller.loading {
            self.state(cx, paint, body, None, true, false);
            return;
        }
        if !has_changes {
            let empty = t("diffViewer.empty");
            self.state(cx, paint, body, Some(&empty), false, false);
            return;
        }
        self.draw_doc(cx, ui, paint, body, split, wrap);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_doc(
        &mut self,
        cx: &mut Cx2d,
        ui: &mut UiContext,
        paint: &mut GitPaint,
        area: Rect,
        split: bool,
        wrap: bool,
    ) {
        if !self.initial_revealed && !self.controller.selected_path.is_empty() {
            self.initial_revealed = true;
            let path = self.controller.selected_path.clone();
            self.doc.reveal(&path);
        }
        let syntax = host::preference(ui, "syntaxTheme")
            .as_str()
            .unwrap_or("one")
            .to_owned();
        let input = DocInput {
            files: &self.controller.files,
            patches: &self.controller.patches,
            patch_errors: &self.controller.patch_errors,
            patch_revision: self.controller.patch_revision,
            collapsed: &self.collapsed_files,
            split,
            wrap,
            syntax_theme: &syntax,
        };
        self.doc.draw(cx, paint, area, &input);
        for (rect_area, clip, hit) in self.doc.hits.entries() {
            self.hits
                .push_clipped(*rect_area, clip.unwrap_or(area), Hit::Doc(hit.clone()));
        }
        self.spinning |= self.doc.spinning;
        // Placeholders near the viewport request their patch once.
        let wanted = std::mem::take(&mut self.doc.wanted);
        for path in wanted {
            self.controller.ensure_patch(ui, &path);
        }
    }
}
