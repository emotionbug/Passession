//! One file tab (Flutter `file_panel.dart` FilePanel): the load states, the panel the file
//! resolves to (editable/read-only source, Markdown preview, HTML preview card, image, binary),
//! the toolbar (metadata, save status, Preview/Source, Save, Reload, More), conflict alerts,
//! find/replace, go to line, editor settings, copies/downloads, chat references and the
//! local editor handoff.

use std::collections::HashMap;
use std::sync::Arc;

use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::overlay::{self, MenuEntry, MenuItem};
use crate::common::paint::{Font, HitMap};
use crate::common::{UiContext, UiEffect, clock};
use crate::theme::{self, Palette, Typography, with_alpha};

use super::editor::buffer::{find_matches, match_len};
use super::editor::field::{FieldEvent, TextField};
use super::editor::view::{EditorLook, SourceEditor};
use super::handle::Snapshot;
use super::host;
use super::html_preview::HtmlPreview;
use super::i18n::{l, lf, t, ta};
use super::images::{self, ImageSlot};
use super::kit::{self, Kit, rect, spec};
use super::markdown::{ImageState, MarkdownLook, MarkdownPreview, PreviewEvent};
use super::model::{Conflict, FileEditorStatus};
use super::path::{FileLocation, parse_link_fragment, remote_absolute, remote_basename, remote_parent, remote_relative};
use super::resolve::{FileTextPresentation, HTML_FILE_PANEL, IMAGE_FILE_PANEL, MARKDOWN_FILE_PANEL};
use super::session::TextView;
use super::store::{self, Asset, OwnerEvent};
use super::transfer::TransferMode;
use super::ui::{self, ButtonSize, ButtonVariant, Interaction, TOOLBAR_HEIGHT};

/// Original `palette.amber[500]` (conflict alert) and `palette.red[300]` ("Save failed").
const AMBER_500: Vec4 = theme::argb(0xfff59e0b);
const RED_300: Vec4 = theme::argb(0xfffca5a5);
/// The code font size range of the appearance settings.
const MIN_CODE_FONT: f64 = 9.0;
const MAX_CODE_FONT: f64 = 22.0;

#[derive(Clone, Debug, PartialEq)]
enum Hot {
    Save,
    Reload,
    More,
    Segment(usize),
    ToggleReplace,
    MatchCase,
    FindPrev,
    FindNext,
    FindClose,
    FindField,
    ReplaceField,
    ReplaceOne,
    ReplaceAll,
    Overwrite,
    AlertReload,
    Retry,
    Download,
    OpenCopy,
    OpenHtml,
    ShowSource,
    SetWrap,
    SetHighlight,
    FontMinus,
    FontPlus,
    SettingsClose,
    TransferCancel(u64),
    Editor,
    Preview,
    Image,
    Scrim,
    /// A dialog surface: blocks what is under it.
    Card,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    None,
    Editor,
    Find,
    Replace,
}

/// Overlays this view opened, by id.
#[derive(Clone, Debug)]
enum Pending {
    More,
    GoTo,
    Reload,
    Context,
    OpenWith { cwd: String, path: String, line: usize, column: usize },
}

/// What the file tab targets.
#[derive(Clone, Debug, PartialEq)]
pub struct FileTarget {
    pub host_id: String,
    pub workspace_id: String,
    pub cwd: String,
    pub path: String,
    pub location: Option<FileLocation>,
}

#[derive(Default)]
struct Zoom {
    scale: f64,
    offset: DVec2,
    drag: Option<(DVec2, DVec2)>,
    key: String,
}

pub struct FileView {
    pub owner: String,
    focus_owner: WidgetUid,
    /// Screen position minus drawn position of the root widget (see `kit::unshift_hit`).
    pub hit_shift: DVec2,
    target: Option<FileTarget>,
    session_id: Option<u64>,
    editor: Option<SourceEditor>,
    editor_session: Option<u64>,
    wrap: bool,
    font_size: f64,
    highlighting: bool,
    preferences_loaded: bool,
    find_visible: bool,
    replace_expanded: bool,
    match_case: bool,
    query: TextField,
    replacement: TextField,
    match_count: usize,
    match_index: usize,
    focus: Focus,
    markdown: MarkdownPreview,
    html: Option<HtmlPreview>,
    overlays: HashMap<u64, Pending>,
    settings_open: bool,
    hits: HitMap<Hot>,
    hovered: Option<Hot>,
    pressed: Option<Hot>,
    editor_drag: bool,
    preview_drag: bool,
    zoom: Zoom,
    location_applied: bool,
    observing: bool,
    /// Web images this view fetched: the request while in flight, `None` once answered.
    http_images: HashMap<String, Option<LiveId>>,
    area: Area,
    /// What the last `sync` saw (session, revisions, visibility).
    sync_gate: Option<SyncGate>,
}

type SyncGate = (u64, Option<u64>, Option<u64>, bool);

/// Theme inputs of the centered state views.
#[derive(Clone, Copy)]
struct Look<'a> {
    p: &'a Palette,
    ty: &'a Typography,
}

/// A bytes reader of the session: (loading, error, bytes).
type BytesState = (bool, Option<String>, Option<Arc<Vec<u8>>>);

/// A centered state view button: hit target, label, optional Lucide icon, variant.
type CenterButton<'a> = (Hot, String, Option<&'a str>, ButtonVariant);

impl FileView {
    pub fn new(owner: &str, focus_owner: WidgetUid) -> FileView {
        FileView {
            owner: owner.to_owned(),
            focus_owner,
            hit_shift: DVec2::default(),
            target: None,
            session_id: None,
            editor: None,
            editor_session: None,
            wrap: true,
            font_size: 12.0,
            highlighting: true,
            preferences_loaded: false,
            find_visible: false,
            replace_expanded: false,
            match_case: false,
            query: TextField::new(""),
            replacement: TextField::new(""),
            match_count: 0,
            match_index: 0,
            focus: Focus::None,
            markdown: MarkdownPreview::default(),
            html: None,
            overlays: HashMap::new(),
            settings_open: false,
            hits: HitMap::default(),
            hovered: None,
            pressed: None,
            editor_drag: false,
            preview_drag: false,
            zoom: Zoom { scale: 1.0, ..Zoom::default() },
            location_applied: false,
            observing: false,
            http_images: HashMap::new(),
            area: Area::Empty,
            sync_gate: None,
        }
    }

    /// A new target: another file resets everything file-specific (the store releases the
    /// previous identity on mount); another location only re-applies it.
    pub fn set_target(&mut self, target: FileTarget) {
        let same_file = self.target.as_ref().is_some_and(|old| {
            old.host_id == target.host_id && old.cwd == target.cwd && old.path == target.path
        });
        if same_file {
            let moved = self.target.as_ref().map(|old| old.location) != Some(target.location);
            self.target = Some(target);
            if moved {
                self.location_applied = false;
            }
            return;
        }
        self.target = Some(target);
        self.reset_file();
    }

    fn reset_file(&mut self) {
        if let Some(editor) = self.editor.take() {
            editor.forget();
        }
        self.editor_session = None;
        self.session_id = None;
        self.markdown = MarkdownPreview::default();
        if let Some(mut html) = self.html.take() {
            html.close();
        }
        self.location_applied = false;
        self.match_count = 0;
        self.match_index = 0;
        self.zoom = Zoom { scale: 1.0, ..Zoom::default() };
        // Fetches still in flight are nobody's once this view stops listening.
        for (url, request) in self.http_images.drain() {
            if request.is_some() {
                images::abandon_web_fetch(&url);
            }
        }
        self.overlays.clear();
        self.preferences_loaded = false;
        self.sync_gate = None;
    }

    /// The panel went away.
    pub fn dispose(&mut self) {
        self.reset_file();
    }

    // -----------------------------------------------------------------------------------------
    // Synchronisation with the store

    /// Mounts the session for the target and keeps editor and preview in step with the model.
    fn sync(&mut self, cx: &mut Cx, ui: &mut UiContext, observing: bool) {
        let Some(target) = self.target.clone() else { return };
        if !self.preferences_loaded {
            self.preferences_loaded = true;
            let preferences = ui.preferences();
            self.font_size = preferences.get("codeFontSize").and_then(Value::as_f64).unwrap_or(12.0).clamp(MIN_CODE_FONT, MAX_CODE_FONT);
            self.wrap = preferences.get("fileWordWrap") != Some(&Value::Bool(false));
        }
        let owner = self.owner.clone();
        let id = store::with(|s| s.mount(ui, &owner, &target.host_id, &target.cwd, &target.path));
        if self.session_id != Some(id) {
            self.session_id = Some(id);
            self.observing = false;
        }
        if observing != self.observing {
            self.observing = observing;
            store::with(|s| s.set_observing(cx, ui, &owner, observing));
        }
        let line_target = target.location.is_some();
        let editor_revision = self.editor.as_ref().filter(|_| self.editor_session == Some(id)).map(|e| e.buffer.revision);
        let gate = (id, store::with(|s| s.session(&owner).map(|session| session.revision)), editor_revision, observing);
        if self.sync_gate.as_ref() == Some(&gate) && self.location_applied {
            return;
        }
        self.sync_gate = Some(gate);
        // The editor's current text, compared in the store without copying the buffer.
        let editor_text = self.editor.as_ref().filter(|_| self.editor_session == Some(id)).map(|e| e.text());
        let snapshot = store::with(|s| {
            let session = s.session_mut(&owner)?;
            let reset = session.sync_view_mount(line_target);
            let text = session.current_text();
            let changed_text = match (&editor_text, text) {
                (Some(current), Some(text)) if *current == text => None,
                (_, text) => text.map(str::to_owned),
            };
            Some((
                reset,
                session.presentation(),
                text.is_some(),
                changed_text,
                session.model().is_some(),
            ))
        });
        let Some((reset, presentation, has_text, changed_text, editable)) = snapshot else { return };
        if reset {
            // Original `selectSourcePresentation`: text past the highlighting budget is plain.
            self.highlighting = presentation != Some(FileTextPresentation::Plain);
        }
        if !has_text {
            return;
        }
        // The editor follows the buffer (reload, remote change).
        match self.editor.as_mut() {
            Some(editor) if self.editor_session == Some(id) => {
                if editor.read_only == editable {
                    editor.read_only = !editable;
                }
                if let Some(text) = &changed_text {
                    editor.sync_text(text);
                }
            }
            _ => {
                let key = format!("{owner}\u{0}{id}");
                let text = changed_text.clone().unwrap_or_default();
                self.editor = Some(SourceEditor::new(text, &target.path, key, !editable));
                self.editor_session = Some(id);
            }
        }
        let text = self.editor.as_ref().map(|e| e.text().to_owned()).unwrap_or_default();
        if let Some(html) = self.html.as_ref() {
            html.update(&text);
        }
        if !self.location_applied {
            self.location_applied = true;
            if let Some(location) = target.location {
                store::with(|s| {
                    if let Some(session) = s.session_mut(&owner) {
                        session.view = TextView::Source;
                    }
                });
                if let Some(editor) = self.editor.as_mut() {
                    editor.go_to(location.line, 1, location.line_end);
                }
            }
        }
        // Markdown preview images.
        if self.is_markdown() {
            self.markdown.set_text(&text);
        }
    }

    fn is_markdown(&self) -> bool {
        store::with(|s| s.session(&self.owner).and_then(|session| session.panel())) == Some(MARKDOWN_FILE_PANEL)
    }

    /// Markdown images: workspace paths read through the daemon, http(s) fetched natively.
    fn request_markdown_images(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(target) = self.target.clone() else { return };
        let sources: Vec<String> = self.markdown.image_keys().to_vec();
        for source in sources {
            if source.starts_with("http://") || source.starts_with("https://") {
                // A web image another panel fetched (or is fetching) is shared, like Flutter's
                // process-wide image cache.
                if self.http_images.contains_key(&source) || !images::begin_web_fetch(&source) {
                    continue;
                }
                let id = LiveId::from_str(&format!("{}\u{0}{source}", self.owner));
                self.http_images.insert(source.clone(), Some(id));
                cx.http_request(id, HttpRequest::new(source.clone(), HttpMethod::GET));
                continue;
            }
            let Some(path) = self.resolve_link_path(&target, &source) else { continue };
            let key = format!("{}\u{0}md\u{0}{path}", self.owner);
            if images::slot(&key).is_some() {
                continue;
            }
            let owner = self.owner.clone();
            let asset = store::with(|s| s.asset(ui, &owner, &target.host_id, &target.cwd, &path));
            if let Asset::Ready { bytes, mime } = asset {
                images::request(&key, bytes, &mime, 0);
            }
        }
    }

    /// A relative/`file:` link or image source as a host path (Flutter `_link`/`_markdownImage`).
    fn resolve_link_path(&self, target: &FileTarget, source: &str) -> Option<String> {
        let (without_fragment, _) = source.split_once('#').unwrap_or((source, ""));
        let without_query = without_fragment.split('?').next().unwrap_or("");
        if let Some(rest) = without_query.strip_prefix("file://") {
            let path = rest.find('/').map(|i| &rest[i..]).unwrap_or(rest);
            let decoded = percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned();
            // `file:///C:/x` on Windows hosts.
            let windows = super::path::is_windows_path(&target.cwd);
            if windows {
                let trimmed = decoded.trim_start_matches('/');
                return Some(trimmed.replace('/', "\\"));
            }
            return Some(decoded);
        }
        if has_scheme(without_query) {
            return None;
        }
        let decoded = percent_encoding::percent_decode_str(without_query).decode_utf8_lossy().into_owned();
        let file = remote_absolute(&target.cwd, &target.path);
        Some(remote_absolute(&remote_parent(&file), &decoded))
    }

    fn markdown_image_state(&self, target: &FileTarget, source: &str) -> ImageState {
        if source.starts_with("http://") || source.starts_with("https://") {
            return slot_state(images::slot(&images::web_key(source)));
        }
        let Some(path) = self.resolve_link_path(target, source) else {
            return ImageState::Failed(l("Unsupported image URI").to_owned());
        };
        let key = format!("{}\u{0}md\u{0}{path}", self.owner);
        match images::slot(&key) {
            Some(slot) => slot_state(Some(slot)),
            None => {
                let failed = store::with(|s| {
                    s.asset_peek(&self.owner, &path)
                });
                match failed {
                    Some(Asset::Failed(error)) => ImageState::Failed(error),
                    _ => ImageState::Loading,
                }
            }
        }
    }

    /// Flutter `_link`.
    fn open_link(&mut self, ui: &mut UiContext, href: &str) {
        let Some(target) = self.target.clone() else { return };
        if href.is_empty() {
            return;
        }
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
            host::open_external(ui, href);
            return;
        }
        if has_scheme(href) && !lower.starts_with("file:") {
            let scheme = href.split(':').next().unwrap_or("");
            host::toast(ui, lf("This link scheme is not allowed: {scheme}", &[("scheme", scheme)]), true);
            return;
        }
        let Some(path) = self.resolve_link_path(&target, href) else { return };
        let fragment = href.split_once('#').map(|(_, f)| f).unwrap_or("");
        let suffix = match parse_link_fragment(fragment) {
            Some((line, Some(column))) => format!("#L{line}:{column}"),
            Some((line, None)) => format!("#L{line}"),
            None => String::new(),
        };
        host::open_file(ui, &target.host_id, &target.workspace_id, &format!("{path}{suffix}"), false);
    }

    // -----------------------------------------------------------------------------------------
    // Actions

    fn with_model(&mut self, cx: &mut Cx, ui: &mut UiContext, f: impl FnOnce(&mut super::model::FileEditorModel, &mut dyn super::model::ModelIo)) {
        let owner = self.owner.clone();
        store::with(|s| s.with_model(cx, ui, &owner, f));
    }

    fn save(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        self.with_model(cx, ui, |m, io| m.save(io));
    }

    /// Original `handleReload`: confirms first when the buffer has unsaved changes.
    fn reload(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let modified = store::with(|s| s.session(&self.owner).is_some_and(|session| session.modified()));
        if modified {
            let id = overlay::confirm(ui, &t("panels.file.editor.reloadTitle"), &t("panels.file.editor.reloadMessage"), &t("panels.file.editor.reload"), true);
            self.overlays.insert(id, Pending::Reload);
            return;
        }
        self.with_model(cx, ui, |m, io| m.reload(io));
    }

    fn overwrite(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        self.with_model(cx, ui, |m, io| m.overwrite(io));
    }

    fn edited(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(editor) = self.editor.as_ref() else { return };
        let text = editor.text().to_owned();
        self.with_model(cx, ui, move |m, io| m.edit(text, io));
        if self.find_visible {
            self.count_matches();
        }
    }

    fn open_find(&mut self) {
        self.find_visible = true;
        self.focus = Focus::Find;
        self.query.select_all();
    }

    fn close_find(&mut self) {
        self.find_visible = false;
        self.focus = Focus::Editor;
    }

    fn count_matches(&mut self) {
        let Some(editor) = self.editor.as_ref() else { return };
        self.match_count = find_matches(editor.text(), self.query.text(), self.match_case).len();
        self.match_index = 0;
    }

    fn find_next(&mut self, ui: &mut UiContext, previous: bool) {
        let Some(editor) = self.editor.as_mut() else { return };
        let needle = self.query.text().to_owned();
        let matches = find_matches(editor.text(), &needle, self.match_case);
        self.match_count = matches.len();
        self.match_index = 0;
        if matches.is_empty() {
            return;
        }
        let owner = self.owner.clone();
        store::with(|s| {
            if let Some(session) = s.session_mut(&owner) {
                session.view = TextView::Source;
            }
        });
        let selection = editor.buffer.selection;
        let next = if previous {
            matches.iter().rev().find(|&&index| index < selection.start()).copied().unwrap_or(*matches.last().unwrap())
        } else {
            matches.iter().find(|&&index| index >= selection.end()).copied().unwrap_or(matches[0])
        };
        let len = match_len(editor.text(), next, &needle, self.match_case);
        editor.select(next, next + len);
        self.match_index = matches.iter().position(|&index| index == next).unwrap_or(0) + 1;
        let _ = ui;
    }

    fn replace_selection(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(editor) = self.editor.as_mut() else { return };
        if editor.read_only {
            return;
        }
        let selection = editor.buffer.selection;
        let selected = editor.buffer.selected_text().to_owned();
        let needle = self.query.text().to_owned();
        let same = if self.match_case { selected == needle } else { selected.to_lowercase() == needle.to_lowercase() };
        if selection.is_empty() || !same {
            self.find_next(ui, false);
            return;
        }
        let replacement = self.replacement.text().to_owned();
        editor.buffer.replace(selection.start(), selection.end(), &replacement, super::editor::buffer::EditKind::Other);
        self.edited(cx, ui);
        self.find_next(ui, false);
    }

    fn replace_all(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let Some(editor) = self.editor.as_mut() else { return };
        let needle = self.query.text().to_owned();
        if editor.read_only || needle.is_empty() {
            return;
        }
        let replacement = self.replacement.text().to_owned();
        let text = editor.text().to_owned();
        let matches = find_matches(&text, &needle, self.match_case);
        if matches.is_empty() {
            return;
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for start in &matches {
            out.push_str(&text[last..*start]);
            out.push_str(&replacement);
            last = start + match_len(&text, *start, &needle, self.match_case);
        }
        out.push_str(&text[last..]);
        let head = editor.buffer.selection.head.min(out.len());
        editor.buffer.replace(0, text.len(), &out, super::editor::buffer::EditKind::Other);
        editor.buffer.set_selection(super::editor::buffer::Selection::caret(super::editor::buffer::floor_boundary(&out, head)));
        self.edited(cx, ui);
        if let Some(editor) = self.editor.as_ref() {
            self.match_count = find_matches(editor.text(), &needle, self.match_case).len();
        }
    }

    /// Flutter `_attach`: whole file or the selected lines.
    fn attach(&mut self, ui: &mut UiContext, selection_only: bool) {
        let Some(target) = self.target.clone() else { return };
        let mut selection = json!({ "kind": "whole_file" });
        if selection_only
            && let Some(editor) = self.editor.as_ref() {
                let sel = editor.buffer.selection;
                if !sel.is_empty() {
                    let start = editor.buffer.location(sel.start()).0;
                    let end = editor.buffer.location((sel.end() - 1).max(sel.start())).0;
                    selection = json!({ "kind": "line_range", "startLine": start, "endLine": end });
                }
            }
        host::add_to_chat(
            &target.host_id,
            &target.workspace_id,
            vec![json!({ "kind": "workspace_file", "path": remote_absolute(&target.cwd, &target.path), "selection": selection })],
        );
        host::toast(ui, l("File reference added to chat; unsaved edits are not uploaded"), false);
    }

    fn download(&mut self, ui: &mut UiContext, mode: TransferMode) {
        let Some(target) = self.target.clone() else { return };
        let owner = self.owner.clone();
        let (mime, current) = store::with(|s| {
            let session = s.session(&owner);
            let mime = session.and_then(|session| session.stat()).and_then(|stat| stat.mime_type().map(str::to_owned));
            let current = session
                .and_then(|session| session.model())
                .filter(|model| model.modified())
                .map(|model| Arc::new(model.file_content().into_bytes()));
            (mime, current)
        });
        store::with(|s| s.start_transfer(ui, &owner, &target.host_id, &target.cwd, &target.path, mode, mime, current));
    }

    fn open_html(&mut self, ui: &mut UiContext) {
        let Some(target) = self.target.clone() else { return };
        let text = self.editor.as_ref().map(|e| e.text().to_owned()).unwrap_or_default();
        if self.html.is_none() {
            self.html = Some(HtmlPreview::start(ui.core.clone(), &target.host_id, &target.cwd, &target.path, text.clone()));
        }
        let html = self.html.as_mut().unwrap();
        html.update(&text);
        match html.url.clone() {
            Some(url) => host::open_external(ui, &url),
            None => html.open_pending = true,
        }
    }

    fn copy_path(&mut self, ui: &mut UiContext, absolute: bool) {
        let Some(target) = self.target.as_ref() else { return };
        let path = if absolute { remote_absolute(&target.cwd, &target.path) } else { remote_relative(&target.cwd, &target.path) };
        host::copy_text(ui, path, Some(t("common.states.copied")));
    }

    fn location(&self) -> (usize, usize) {
        self.editor.as_ref().map(SourceEditor::location).unwrap_or((1, 1))
    }

    fn more_menu(&mut self, ui: &mut UiContext, anchor: Rect, has_text: bool, panel_html: bool, has_selection: bool) {
        let mut entries = Vec::new();
        if has_text {
            if panel_html {
                entries.push(MenuItem::new("html", l("Preview HTML in system browser")).icon("globe").entry());
                entries.push(MenuEntry::Separator);
            }
            entries.push(MenuItem::new("find", l("Find / Replace")).icon("search").entry());
            entries.push(MenuItem::new("goto", l("Go to line")).icon("hash").entry());
            entries.push(MenuItem::new("settings", l("Editor settings")).icon("settings-2").entry());
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuItem::new("download", l("Save a copy / Download")).icon("download").entry());
        entries.push(MenuItem::new("share", t("downloads.shareFile")).icon("share-2").entry());
        entries.push(MenuItem::new("external", l("Open downloaded copy externally")).icon("external-link").entry());
        let local = self.target.as_ref().is_some_and(|target| host::is_local_desktop_host(ui, &target.host_id));
        if local {
            entries.push(MenuItem::new("local", l("Open in editor / file manager")).icon("square-pen").entry());
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuItem::new("chat", t("workspace.fileActions.addToChat")).icon("message-square-plus").entry());
        if has_text && has_selection {
            entries.push(MenuItem::new("selection", l("Add selected lines to chat")).icon("text-select").entry());
        }
        entries.push(MenuItem::new("relative", t("workspace.fileActions.copyRelativePath")).icon("copy").entry());
        entries.push(MenuItem::new("absolute", t("workspace.fileActions.copyPath")).icon("copy").entry());
        let id = overlay::open_menu(ui, kit::shifted(anchor, self.hit_shift), None, entries);
        self.overlays.insert(id, Pending::More);
    }

    fn run_more(&mut self, cx: &mut Cx, ui: &mut UiContext, action: &str) {
        match action {
            "find" => self.open_find(),
            "settings" => self.settings_open = true,
            "html" => {
                let owner = self.owner.clone();
                store::with(|s| {
                    if let Some(session) = s.session_mut(&owner) {
                        session.view = TextView::Preview;
                    }
                });
                self.open_html(ui);
            }
            "download" => self.download(ui, TransferMode::Save),
            "share" => self.download(ui, TransferMode::Share),
            "external" => self.download(ui, TransferMode::Open),
            "local" => {
                let Some(target) = self.target.clone() else { return };
                let (line, column) = self.location();
                let owner = self.owner.clone();
                store::with(|s| s.open_local_editor(ui, &owner, &target.cwd, &target.path, line, column));
            }
            "chat" => self.attach(ui, false),
            "selection" => self.attach(ui, true),
            "absolute" => self.copy_path(ui, true),
            "relative" => self.copy_path(ui, false),
            "goto" => {
                let (line, column) = self.location();
                let id = overlay::prompt_text(ui, l("Go to line:column"), None, &format!("{line}:{column}"), "", &t("common.actions.save"));
                self.overlays.insert(id, Pending::GoTo);
            }
            _ => {}
        }
        let _ = cx;
    }

    fn context_menu(&mut self, ui: &mut UiContext, at: DVec2) {
        let Some(editor) = self.editor.as_ref() else { return };
        let has_selection = !editor.buffer.selection.is_empty();
        let editable = !editor.read_only;
        let mut entries = Vec::new();
        if editable {
            entries.push(MenuItem::new("cut", l("Cut")).icon("scissors").enabled(has_selection).entry());
        }
        entries.push(MenuItem::new("copy", t("common.actions.copy")).icon("copy").enabled(has_selection).entry());
        if editable {
            entries.push(MenuItem::new("paste", l("Paste")).icon("clipboard-paste").entry());
        }
        entries.push(MenuItem::new("all", t("common.actions.selectAll")).icon("text-select").entry());
        let id = overlay::open_menu(ui, kit::shifted(rect(at.x, at.y, 0.0, 0.0), self.hit_shift), None, entries);
        self.overlays.insert(id, Pending::Context);
    }

    fn run_context(&mut self, cx: &mut Cx, ui: &mut UiContext, action: &str) {
        let Some(editor) = self.editor.as_mut() else { return };
        match action {
            "cut" => {
                if let Some(text) = editor.cut() {
                    host::copy_text(ui, text, None);
                    self.edited(cx, ui);
                }
            }
            "copy" => {
                let text = editor.buffer.selected_text().to_owned();
                if !text.is_empty() {
                    host::copy_text(ui, text, None);
                }
            }
            "paste" => {
                let owner = self.owner.clone();
                store::with(|s| s.read_clipboard(&owner));
            }
            "all" => editor.select_all(),
            _ => {}
        }
    }

    /// Overlay answers addressed to this tab.
    fn handle_commands(&mut self, cx: &mut Cx, ui: &mut UiContext) -> bool {
        let mut answers = Vec::new();
        for command in ui.commands.iter_mut() {
            if command.handled || command.tab_key != self.owner {
                continue;
            }
            if let Some((id, args)) = overlay::result_of(command)
                && let Some(pending) = self.overlays.remove(&id) {
                    answers.push((pending, args.clone()));
                    command.handled = true;
                }
        }
        let any = !answers.is_empty();
        for (pending, args) in answers {
            if args.get("cancelled") == Some(&Value::Bool(true)) {
                continue;
            }
            match pending {
                Pending::More => {
                    if let Some(value) = args.get("value").and_then(Value::as_str) {
                        self.run_more(cx, ui, value);
                    }
                }
                Pending::Context => {
                    if let Some(value) = args.get("value").and_then(Value::as_str) {
                        self.run_context(cx, ui, value);
                    }
                }
                Pending::Reload => {
                    if args.get("confirmed") == Some(&Value::Bool(true)) {
                        self.with_model(cx, ui, |m, io| m.reload(io));
                    }
                }
                Pending::GoTo => {
                    if let Some(text) = args.get("text").and_then(Value::as_str) {
                        let mut parts = text.split(':');
                        let line = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(1);
                        let column = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(1);
                        let owner = self.owner.clone();
                        store::with(|s| {
                            if let Some(session) = s.session_mut(&owner) {
                                session.view = TextView::Source;
                            }
                        });
                        if let Some(editor) = self.editor.as_mut() {
                            editor.go_to(line, column, None);
                        }
                        self.focus = Focus::Editor;
                        // The prompt owned keyboard focus; return it to the revealed source.
                        cx.set_key_focus(self.area);
                    }
                }
                Pending::OpenWith { cwd, path, line, column } => {
                    if let Some(value) = args.get("value").and_then(Value::as_str) {
                        store::with(|s| s.open_editor(value, &cwd, &path, line, column));
                    }
                }
            }
        }
        any
    }

    fn handle_owner_events(&mut self, cx: &mut Cx, ui: &mut UiContext) -> bool {
        let events = store::with(|s| s.take_owner_events(&self.owner));
        let any = !events.is_empty();
        for event in events {
            match event {
                OwnerEvent::Clipboard(text) => {
                    if let Some(editor) = self.editor.as_mut()
                        && editor.paste(&text) {
                            self.edited(cx, ui);
                        }
                }
                OwnerEvent::EditorTargets { targets, cwd, path, line, column } => {
                    let entries = targets.into_iter().map(|(id, label)| MenuItem::new(id, label).entry()).collect();
                    let anchor = self.hits.rect_of(&Hot::More).unwrap_or_default();
                    let id = overlay::open_menu(ui, kit::shifted(anchor, self.hit_shift), Some(l("Open with").to_owned()), entries);
                    self.overlays.insert(id, Pending::OpenWith { cwd, path, line, column });
                }
            }
        }
        any
    }

    // -----------------------------------------------------------------------------------------
    // Events

    /// Returns whether the view changed.
    pub fn handle_event(&mut self, cx: &mut Cx, kit: &mut Kit, event: &Event, ui: &mut UiContext, area: Area, observing: bool) -> bool {
        self.area = area;
        let mut changed = false;
        if let Event::Timer(_) = event {
            if let Some(editor) = self.editor.as_mut() {
                changed |= editor.blink.handle_event(event);
            }
            changed |= self.query.blink.handle_event(event);
            changed |= self.replacement.blink.handle_event(event);
        }
        if let Event::NetworkResponses(responses) = event {
            for item in responses.iter() {
                if let Some((source, request)) = self.http_images.iter_mut().find(|(_, id)| **id == Some(item.request_id)) {
                    let body = match &item.response {
                        NetworkResponse::HttpResponse(response) if response.status_code < 400 => {
                            Some(Ok(response.get_body().cloned().unwrap_or_default()))
                        }
                        // Flutter `NetworkImageLoadException` text.
                        NetworkResponse::HttpResponse(response) => {
                            Some(Err(format!("HTTP request failed, statusCode: {}, {source}", response.status_code)))
                        }
                        NetworkResponse::HttpRequestError(error) => Some(Err(error.message.clone())),
                        _ => None,
                    };
                    if let Some(body) = body {
                        *request = None;
                        images::finish_web_fetch(source, body);
                        changed = true;
                    }
                }
            }
        }
        if let Event::Signal = event
            && let Some(html) = self.html.as_mut() {
                let (ready, errors) = html.poll();
                if ready && html.open_pending {
                    html.open_pending = false;
                    if let Some(url) = html.url.clone() {
                        host::open_external(ui, &url);
                    }
                }
                for error in errors {
                    host::toast(ui, error, true);
                }
            }
        changed |= self.handle_commands(cx, ui);
        changed |= self.handle_owner_events(cx, ui);
        self.sync(cx, ui, observing);
        if matches!(event, Event::Signal) && !self.markdown.image_keys().is_empty() {
            self.request_markdown_images(cx, ui);
        }
        if let Event::BackPressed { handled } = event
            && !handled.get()
        {
            if self.settings_open {
                self.close_settings(ui);
            } else if self.find_visible {
                self.close_find();
            } else {
                return changed;
            }
            handled.set(true);
            return true;
        }
        let hit = kit::unshift_hit(event.hits(cx, self.area), self.hit_shift);
        if !matches!(hit, Hit::Nothing) {
            changed |= self.hit(cx, kit, &hit, ui);
        }
        changed
    }

    fn editor_look(&self, ui: &UiContext) -> EditorLook {
        let palette = theme::current_palette();
        let theme_name = ui.preference("syntaxTheme").and_then(Value::as_str).unwrap_or("one").to_owned();
        EditorLook {
            font_size: self.font_size,
            compact: ui.compact,
            wrap: self.wrap,
            highlighting: self.highlighting,
            syntax: super::highlight::colors(&theme_name, palette.is_dark),
            palette,
        }
    }

    fn hit(&mut self, cx: &mut Cx, kit: &mut Kit, hit: &Hit, ui: &mut UiContext) -> bool {
        let look = self.editor_look(ui);
        match hit {
            Hit::FingerHoverIn(fe) | Hit::FingerHoverOver(fe) => {
                let hot = self.hits.at(fe.abs);
                let cursor = match &hot {
                    Some(Hot::Editor) | Some(Hot::FindField) | Some(Hot::ReplaceField) => MouseCursor::Text,
                    Some(Hot::Preview) if self.markdown.is_link(fe.abs) => MouseCursor::Hand,
                    Some(Hot::Preview) => MouseCursor::Text,
                    Some(Hot::Image) | Some(Hot::Scrim) | Some(Hot::Card) | None => MouseCursor::Default,
                    Some(_) => MouseCursor::Hand,
                };
                cx.set_cursor(cursor);
                if hot != self.hovered {
                    self.hovered = hot;
                    return true;
                }
                false
            }
            Hit::FingerHoverOut(_) => self.hovered.take().is_some(),
            Hit::FingerScroll(fe) => {
                match self.hits.at(fe.abs) {
                    Some(Hot::Editor) => {
                        if let Some(editor) = self.editor.as_mut() {
                            editor.handle_hit(cx, kit, &look, hit);
                        }
                    }
                    Some(Hot::Preview) => {
                        self.markdown.handle_hit(hit);
                    }
                    Some(Hot::Image) => {
                        let factor = if fe.scroll.y > 0.0 { 1.0 / 1.1 } else { 1.1 };
                        self.zoom.scale = (self.zoom.scale * factor).clamp(0.1, 16.0);
                    }
                    _ => return false,
                }
                true
            }
            Hit::FingerDown(fe) => {
                let hot = self.hits.at(fe.abs);
                cx.set_key_focus(self.area);
                self.editor_drag = false;
                self.preview_drag = false;
                match hot {
                    Some(Hot::Editor) => {
                        self.focus = Focus::Editor;
                        self.editor_drag = true;
                        let result = self.editor.as_mut().map(|editor| {
                            editor.focused = true;
                            editor.handle_hit(cx, kit, &look, hit)
                        });
                        if let Some(at) = result.and_then(|result| result.context_menu) {
                            self.context_menu(ui, at);
                        }
                    }
                    Some(Hot::FindField) => {
                        self.focus = Focus::Find;
                        self.query.handle_hit(cx, kit, hit);
                    }
                    Some(Hot::ReplaceField) => {
                        self.focus = Focus::Replace;
                        self.replacement.handle_hit(cx, kit, hit);
                    }
                    Some(Hot::Preview) => {
                        self.preview_drag = true;
                        self.markdown.handle_hit(hit);
                    }
                    Some(Hot::Image) => {
                        self.zoom.drag = Some((fe.abs, self.zoom.offset));
                        if fe.tap_count == 2 {
                            self.zoom.scale = 1.0;
                            self.zoom.offset = DVec2::default();
                        }
                    }
                    _ => {
                        self.pressed = hot;
                    }
                }
                true
            }
            Hit::FingerMove(fe) => {
                if self.editor_drag {
                    if let Some(editor) = self.editor.as_mut() {
                        editor.handle_hit(cx, kit, &look, hit);
                    }
                } else if self.preview_drag {
                    self.markdown.handle_hit(hit);
                } else if let Some((start, offset)) = self.zoom.drag {
                    self.zoom.offset = offset + (fe.abs - start);
                } else if self.focus == Focus::Find {
                    self.query.handle_hit(cx, kit, hit);
                } else if self.focus == Focus::Replace {
                    self.replacement.handle_hit(cx, kit, hit);
                }
                true
            }
            Hit::FingerLongPress(fe) => {
                if self.hits.at(fe.abs) == Some(Hot::Editor) {
                    self.context_menu(ui, fe.abs);
                }
                true
            }
            Hit::FingerUp(fe) => {
                if self.editor_drag
                    && let Some(editor) = self.editor.as_mut() {
                        editor.handle_hit(cx, kit, &look, hit);
                    }
                if self.preview_drag
                    && let PreviewEvent::Link(href) = self.markdown.handle_hit(hit) {
                        self.open_link(ui, &href);
                    }
                self.editor_drag = false;
                self.preview_drag = false;
                self.zoom.drag = None;
                let pressed = self.pressed.take();
                if fe.is_over && fe.was_tap() {
                    let hot = self.hits.at(fe.abs);
                    if let Some(hot) = hot.filter(|hot| Some(hot) == pressed.as_ref()) {
                        self.activate(cx, ui, hot);
                    }
                }
                true
            }
            Hit::KeyDown(ke) => self.key_down(cx, kit, ui, &look, hit, ke),
            Hit::TextInput(_) | Hit::TextIme(_) => {
                match self.focus {
                    Focus::Find => {
                        if self.query.handle_hit(cx, kit, hit) == FieldEvent::Changed {
                            self.count_matches();
                        }
                    }
                    Focus::Replace => {
                        self.replacement.handle_hit(cx, kit, hit);
                    }
                    Focus::Editor => {
                        let result = self.editor.as_mut().map(|editor| editor.handle_hit(cx, kit, &look, hit));
                        if result.is_some_and(|r| r.text_changed) {
                            self.edited(cx, ui);
                        }
                    }
                    Focus::None => return false,
                }
                true
            }
            Hit::TextCopy(_) | Hit::TextCut(_) => {
                match self.focus {
                    Focus::Find => {
                        self.query.handle_hit(cx, kit, hit);
                    }
                    Focus::Replace => {
                        self.replacement.handle_hit(cx, kit, hit);
                    }
                    Focus::Editor => {
                        if self.preview_visible() {
                            self.markdown.handle_hit(hit);
                        } else {
                            let result = self.editor.as_mut().map(|editor| editor.handle_hit(cx, kit, &look, hit));
                            if result.is_some_and(|r| r.text_changed) {
                                self.edited(cx, ui);
                            }
                        }
                    }
                    Focus::None => {}
                }
                true
            }
            Hit::KeyFocusLost(_) => {
                ui.push(UiEffect::SetFocusScope { owner: self.focus_owner, scope: None });
                if let Some(editor) = self.editor.as_mut() {
                    editor.focused = false;
                }
                cx.clear_text_input(self.area);
                #[cfg(not(target_os = "android"))]
                cx.hide_text_ime();
                true
            }
            Hit::KeyFocus(_) => {
                ui.push(UiEffect::SetFocusScope { owner: self.focus_owner, scope: Some("editable".into()) });
                if self.focus == Focus::Editor
                    && let Some(editor) = self.editor.as_mut() {
                        editor.focused = true;
                    }
                true
            }
            _ => false,
        }
    }

    fn preview_visible(&self) -> bool {
        store::with(|s| {
            s.session(&self.owner).is_some_and(|session| {
                session.view == TextView::Preview
                    && matches!(session.panel(), Some(p) if p == MARKDOWN_FILE_PANEL || p == HTML_FILE_PANEL)
            })
        })
    }

    fn key_down(&mut self, cx: &mut Cx, kit: &mut Kit, ui: &mut UiContext, look: &EditorLook, hit: &Hit, ke: &KeyEvent) -> bool {
        let primary = ke.modifiers.is_primary();
        if primary && ke.key_code == KeyCode::KeyS {
            self.save(cx, ui);
            return true;
        }
        if primary && ke.key_code == KeyCode::KeyF {
            self.open_find();
            return true;
        }
        if self.settings_open && ke.key_code == KeyCode::Escape {
            self.close_settings(ui);
            return true;
        }
        match self.focus {
            Focus::Find | Focus::Replace => {
                if ke.key_code == KeyCode::Escape {
                    self.close_find();
                    return true;
                }
                let field = if self.focus == Focus::Find { &mut self.query } else { &mut self.replacement };
                match field.handle_hit(cx, kit, hit) {
                    FieldEvent::Submitted { shift } if self.focus == Focus::Find => self.find_next(ui, shift),
                    FieldEvent::Changed if self.focus == Focus::Find => self.count_matches(),
                    _ => {}
                }
                true
            }
            Focus::Editor => {
                if self.preview_visible() {
                    self.markdown.handle_hit(hit);
                    return true;
                }
                let result = self.editor.as_mut().map(|editor| editor.handle_hit(cx, kit, look, hit));
                if result.is_some_and(|r| r.text_changed) {
                    self.edited(cx, ui);
                }
                true
            }
            Focus::None => false,
        }
    }

    fn close_settings(&mut self, ui: &mut UiContext) {
        self.settings_open = false;
        host::set_preference(ui, "codeFontSize", json!(self.font_size));
        host::set_preference(ui, "fileWordWrap", json!(self.wrap));
    }

    fn activate(&mut self, cx: &mut Cx, ui: &mut UiContext, hot: Hot) {
        match hot {
            Hot::Save => self.save(cx, ui),
            Hot::Reload | Hot::AlertReload => self.reload(cx, ui),
            Hot::More => {
                let anchor = self.hits.rect_of(&Hot::More).unwrap_or_default();
                let (has_text, panel_html, has_selection) = store::with(|s| {
                    let session = s.session(&self.owner);
                    (
                        session.is_some_and(|s| s.current_text().is_some()),
                        session.and_then(|s| s.panel()) == Some(HTML_FILE_PANEL),
                        false,
                    )
                });
                let has_selection = has_selection || self.editor.as_ref().is_some_and(|e| !e.buffer.selection.is_empty());
                self.more_menu(ui, anchor, has_text, panel_html, has_selection);
            }
            Hot::Segment(index) => {
                let owner = self.owner.clone();
                store::with(|s| {
                    if let Some(session) = s.session_mut(&owner) {
                        session.view = if index == 0 { TextView::Preview } else { TextView::Source };
                    }
                });
            }
            Hot::ToggleReplace => self.replace_expanded = !self.replace_expanded,
            Hot::MatchCase => {
                self.match_case = !self.match_case;
                self.count_matches();
            }
            Hot::FindPrev => self.find_next(ui, true),
            Hot::FindNext => self.find_next(ui, false),
            Hot::FindClose => self.close_find(),
            Hot::ReplaceOne => self.replace_selection(cx, ui),
            Hot::ReplaceAll => self.replace_all(cx, ui),
            Hot::Overwrite => self.overwrite(cx, ui),
            Hot::Retry => {
                let owner = self.owner.clone();
                let state = store::with(|s| s.session(&owner).map(|session| (session.status(), session.model().map(|m| m.status()))));
                match state {
                    Some((Snapshot::Ready(_), Some(FileEditorStatus::Failed))) => self.with_model(cx, ui, |m, io| m.reload(io)),
                    Some((Snapshot::Ready(_), _)) => store::with(|s| s.retry_read(cx, ui, &owner)),
                    _ => store::with(|s| s.retry(cx, ui, &owner)),
                }
            }
            Hot::Download => self.download(ui, TransferMode::Save),
            Hot::OpenCopy => self.download(ui, TransferMode::Open),
            Hot::OpenHtml => self.open_html(ui),
            Hot::ShowSource => {
                let owner = self.owner.clone();
                store::with(|s| {
                    if let Some(session) = s.session_mut(&owner) {
                        session.view = TextView::Source;
                    }
                });
            }
            Hot::SetWrap => self.wrap = !self.wrap,
            Hot::SetHighlight => self.highlighting = !self.highlighting,
            Hot::FontMinus => self.font_size = (self.font_size - 1.0).max(MIN_CODE_FONT),
            Hot::FontPlus => self.font_size = (self.font_size + 1.0).min(MAX_CODE_FONT),
            Hot::SettingsClose | Hot::Scrim => {
                if self.settings_open {
                    self.close_settings(ui);
                }
            }
            Hot::TransferCancel(id) => store::with(|s| s.cancel_transfer(ui, id)),
            Hot::FindField | Hot::ReplaceField | Hot::Editor | Hot::Preview | Hot::Image | Hot::Card => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Drawing

    pub fn draw(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, ui: &UiContext, widget_area: Area) {
        self.area = widget_area;
        self.hits.clear();
        let p = theme::current_palette();
        let ty = theme::current_typography();
        kit.fill(cx, area, p.surface0, 0.0);
        let owner = self.owner.clone();
        let Some(target) = self.target.clone() else { return };
        let state = store::with(|s| {
            s.session(&owner).map(|session| {
                (
                    session.status(),
                    session.panel(),
                    session.presentation(),
                    session.view,
                    session.current_text().is_some(),
                    session.model().map(|m| (m.status(), m.conflict(), m.modified(), m.error().map(str::to_owned))),
                    session.text().map(|r| (r.loading(), r.error().map(str::to_owned))),
                    session.bytes().map(|r| (r.loading(), r.error().map(str::to_owned), r.value().cloned())),
                    session.stat().cloned(),
                )
            })
        });
        let Some((status, panel, presentation, view, has_text, model, reader, bytes, stat)) = state else { return };
        let snapshot_ready = matches!(status, Snapshot::Ready(_));
        if !snapshot_ready {
            self.draw_load_state(cx, kit, area, &status, Look { p: &p, ty: &ty }, widget_area);
            self.draw_transfers(cx, kit, area, &p, &ty);
            return;
        }
        let compact = ui.compact;
        // Toolbar.
        let bar = rect(area.pos.x, area.pos.y, area.size.x, TOOLBAR_HEIGHT);
        ui::toolbar(cx, kit, bar, false, &p);
        let size = ui::toolbar_button_size(compact);
        let mut right = bar.pos.x + bar.size.x - ui::toolbar_trailing_padding(compact);
        let by = bar.pos.y + (TOOLBAR_HEIGHT - size) / 2.0;
        let more = rect(right - size, by, size, size);
        ui::icon_button(cx, kit, more, "ellipsis", self.state_of(&Hot::More, true), compact, &p);
        self.hits.push(more, Hot::More);
        right -= size + 1.0;
        if has_text && model.is_some() {
            let model_status = model.as_ref().map(|m| m.0);
            let reload = rect(right - size, by, size, size);
            let reload_enabled = model_status != Some(FileEditorStatus::Saving);
            ui::icon_button(cx, kit, reload, "rotate-cw", self.state_of(&Hot::Reload, reload_enabled), compact, &p);
            if reload_enabled {
                self.hits.push(reload, Hot::Reload);
            }
            right -= size + 1.0;
            let save = rect(right - size, by, size, size);
            let save_enabled = matches!(model_status, Some(FileEditorStatus::Dirty | FileEditorStatus::Error));
            ui::icon_button(cx, kit, save, "save", self.state_of(&Hot::Save, save_enabled), compact, &p);
            if save_enabled {
                self.hits.push(save, Hot::Save);
            }
            right -= size + 1.0;
        }
        right -= 12.0;
        let has_preview = matches!(panel, Some(p) if p == MARKDOWN_FILE_PANEL || p == HTML_FILE_PANEL);
        let show_preview = has_preview && view == TextView::Preview;
        if has_text && has_preview {
            let labels = [t("panels.file.editor.preview"), t("panels.file.editor.source")];
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            let selected = if view == TextView::Preview { 0 } else { 1 };
            let hovered = match self.hovered {
                Some(Hot::Segment(i)) => Some(i),
                _ => None,
            };
            let width = ui::segmented_width(cx, kit, &labels, &ty);
            let rects = ui::segmented(cx, kit, right, bar.pos.y + TOOLBAR_HEIGHT / 2.0, &labels, selected, hovered, &p, &ty);
            for (index, r) in rects.into_iter().enumerate() {
                self.hits.push(r, Hot::Segment(index));
            }
            right -= width + 12.0;
        }
        let small = ty.ui_size(12.0);
        // Status (editable text only).
        if has_text
            && let Some((model_status, _, _, _)) = &model {
                let mut items: Vec<(String, Vec4)> = Vec::new();
                if !show_preview {
                    let (line, column) = self.location();
                    items.push((format!("Ln {line}, Col {column}"), p.extra_muted));
                }
                match model_status {
                    FileEditorStatus::Error => items.push((t("panels.file.editor.saveFailed"), RED_300)),
                    FileEditorStatus::Saving => items.push((t("panels.file.editor.saving"), p.muted)),
                    _ => {}
                }
                for (label, color) in items {
                    let font = Font::Regular;
                    let w = kit.measure(cx, spec(font, small, color), &label).x;
                    kit.text_in(cx, rect(right - w, bar.pos.y, w + 1.0, TOOLBAR_HEIGHT), spec(font, small, color), &label, 0.0);
                    right -= w + 8.0;
                }
                if *model_status == FileEditorStatus::Saving {
                    ui::spinner(cx, kit, rect(right - 14.0, bar.pos.y, 14.0, TOOLBAR_HEIGHT), 14.0, p.muted, clock::phase(900));
                    clock::register(cx, widget_area);
                    right -= 14.0 + 8.0;
                }
                if *model_status == FileEditorStatus::Dirty {
                    kit.dot(cx, dvec2(right - 3.0, bar.pos.y + TOOLBAR_HEIGHT / 2.0), 6.0, p.extra_muted);
                    right -= 6.0 + 8.0;
                }
                right -= 4.0;
            }
        // Metadata (left).
        let meta_w = (right - bar.pos.x - 12.0).max(0.0);
        let file_size = ui::file_size(stat.as_ref().map(|s| s.size()).unwrap_or(0));
        let meta = if has_text {
            let lines = self.editor.as_ref().map(SourceEditor::line_count).unwrap_or(1);
            format!("{file_size}   {}", ta("panels.file.editor.lines", &[("count", &lines.to_string())]))
        } else {
            format!("{}   {file_size}", remote_basename(&target.path, Some(&target.cwd)))
        };
        kit.text_in(cx, rect(bar.pos.x + 12.0, bar.pos.y, meta_w, TOOLBAR_HEIGHT), spec(Font::Regular, small, p.extra_muted), &meta, 0.0);
        let mut top = bar.pos.y + TOOLBAR_HEIGHT;
        // Alerts.
        if let Some((model_status, conflict, modified, _)) = &model
            && *model_status == FileEditorStatus::Conflict {
                top = match conflict {
                    Some(Conflict::Deleted) => self.draw_alert(cx, kit, area, top, &t("panels.file.editor.deletedTitle"), Some(&t("panels.file.editor.preservedDescription")), AMBER_500, true, &[], &p, &ty),
                    _ => {
                        let mut actions: Vec<(Hot, String)> = Vec::new();
                        if *modified {
                            actions.push((Hot::Overwrite, t("panels.file.editor.overwrite")));
                        }
                        actions.push((Hot::AlertReload, t("panels.file.editor.reload")));
                        let description = modified.then(|| t("panels.file.editor.conflictDescription"));
                        self.draw_alert(cx, kit, area, top, &t("panels.file.editor.changedOnDisk"), description.as_deref(), AMBER_500, true, &actions, &p, &ty)
                    }
                };
            }
        let bottom = area.pos.y + area.size.y;
        let content = rect(area.pos.x, top, area.size.x, (bottom - top).max(0.0));
        self.draw_content(cx, kit, content, ui, &target, panel, presentation, show_preview, has_text, model.as_ref().map(|m| (m.0, m.3.clone())), reader, bytes, stat.as_ref().map(|s| s.size()).unwrap_or(0), stat.as_ref().and_then(|s| s.mime_type().map(str::to_owned)), widget_area);
        if self.find_visible && has_text {
            self.draw_find(cx, kit, content, &p, &ty, compact, widget_area);
        }
        if self.settings_open {
            self.draw_settings(cx, kit, area, &p, &ty);
        }
        self.draw_transfers(cx, kit, area, &p, &ty);
    }

    fn state_of(&self, hot: &Hot, enabled: bool) -> Interaction {
        Interaction {
            hovered: self.hovered.as_ref() == Some(hot),
            pressed: self.pressed.as_ref() == Some(hot),
            enabled,
            selected: false,
        }
    }

    /// Centred column of a message and optional actions (Flutter `_centerState`).
    fn center_state(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, lines: &[(String, Vec4)], buttons: &[CenterButton], look: Look) {
        let Look { p, ty } = look;
        let base = ty.ui_size(14.0);
        let width = (area.size.x - 32.0).max(1.0);
        let mut heights = Vec::new();
        let mut total = 0.0;
        for (text, color) in lines {
            let h = kit.paragraph_height(cx, width, spec(Font::Regular, base, *color), text);
            heights.push(h);
            total += h + 12.0;
        }
        let button_h = ButtonSize::Sm.height();
        if !buttons.is_empty() {
            total += button_h;
        } else {
            total -= 12.0;
        }
        let mut y = area.pos.y + ((area.size.y - total) / 2.0).max(16.0);
        for ((text, color), h) in lines.iter().zip(heights) {
            let w = kit.measure(cx, spec(Font::Regular, base, *color), text).x.min(width);
            kit.paragraph(cx, dvec2(area.pos.x + (area.size.x - w) / 2.0, y), width, spec(Font::Regular, base, *color), text);
            y += h + 12.0;
        }
        if !buttons.is_empty() {
            let widths: Vec<f64> = buttons.iter().map(|(_, label, icon, _)| ui::button_width(cx, kit, label, *icon, ButtonSize::Sm, ty)).collect();
            let total_w = widths.iter().sum::<f64>() + 8.0 * (buttons.len() - 1) as f64;
            let mut x = area.pos.x + (area.size.x - total_w) / 2.0;
            for ((hot, label, icon, variant), w) in buttons.iter().zip(widths) {
                let r = rect(x, y, w, button_h);
                let st = self.state_of(hot, true);
                ui::button(cx, kit, r, label, *icon, *variant, ButtonSize::Sm, st, p, ty);
                self.hits.push(r, hot.clone());
                x += w + 8.0;
            }
        }
    }

    fn draw_load_state(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, status: &Snapshot, look: Look, widget_area: Area) {
        let Look { p, ty } = look;
        match status {
            Snapshot::Loading => self.loading(cx, kit, area, p, ty, widget_area),
            Snapshot::Missing => {
                self.center_state(cx, kit, area, &[(t("panels.file.notFound"), p.danger)], &[(Hot::Retry, t("common.actions.retry"), None, ButtonVariant::Outline)], look)
            }
            Snapshot::Error(error) => {
                let message = if error.is_empty() { t("panels.file.failedToLoad") } else { error.clone() };
                self.center_state(cx, kit, area, &[(message, p.danger)], &[(Hot::Retry, t("common.actions.retry"), None, ButtonVariant::Outline)], look)
            }
            Snapshot::Ready(_) => {}
        }
    }

    fn loading(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, p: &Palette, ty: &Typography, widget_area: Area) {
        let cy = area.pos.y + area.size.y / 2.0;
        ui::spinner(cx, kit, rect(area.pos.x + area.size.x / 2.0 - 10.0, cy - 26.0, 20.0, 20.0), 20.0, p.muted, clock::phase(900));
        clock::register(cx, widget_area);
        let base = ty.ui_size(14.0);
        kit.text_in(cx, rect(area.pos.x, cy + 2.0, area.size.x, base * 1.5), spec(Font::Regular, base, p.muted), &t("panels.file.loading"), 0.5);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_content(
        &mut self,
        cx: &mut Cx2d,
        kit: &mut Kit,
        area: Rect,
        ui: &UiContext,
        target: &FileTarget,
        panel: Option<super::resolve::FilePanelCandidate>,
        presentation: Option<FileTextPresentation>,
        show_preview: bool,
        has_text: bool,
        model: Option<(FileEditorStatus, Option<String>)>,
        reader: Option<(bool, Option<String>)>,
        bytes: Option<BytesState>,
        size: u64,
        mime: Option<String>,
        widget_area: Area,
    ) {
        let p = theme::current_palette();
        let ty = theme::current_typography();
        if panel == Some(IMAGE_FILE_PANEL) {
            let Some((loading, error, value)) = bytes else { return };
            if loading {
                return self.loading(cx, kit, area, &p, &ty, widget_area);
            }
            if let Some(error) = error {
                return self.center_state(cx, kit, area, &[(error, p.danger)], &[], Look { p: &p, ty: &ty });
            }
            if let Some(bytes) = value {
                self.draw_image(cx, kit, area, bytes, mime.as_deref().unwrap_or(""), &p, &ty, widget_area);
            }
            return;
        }
        let Some(panel) = panel else {
            let lines = [(t("panels.file.binaryPreviewUnavailable"), p.muted), (ui::file_size(size), p.muted)];
            let buttons = [
                (Hot::Download, l("Save / Download").to_owned(), Some("download"), ButtonVariant::Outline),
                (Hot::OpenCopy, l("Open a downloaded copy").to_owned(), Some("external-link"), ButtonVariant::Outline),
            ];
            return self.center_state(cx, kit, area, &lines, &buttons, Look { p: &p, ty: &ty });
        };
        if presentation == Some(FileTextPresentation::Unsupported) {
            let lines = [(t("panels.file.tooLargeToDisplay"), p.muted), (ui::file_size(size), p.muted)];
            return self.center_state(cx, kit, area, &lines, &[], Look { p: &p, ty: &ty });
        }
        if let Some((status, error)) = model {
            match status {
                FileEditorStatus::Loading => return self.loading(cx, kit, area, &p, &ty, widget_area),
                FileEditorStatus::Failed => {
                    let message = error.unwrap_or_else(|| t("panels.file.failedToLoad"));
                    return self.center_state(cx, kit, area, &[(message, p.danger)], &[(Hot::Retry, t("common.actions.retry"), None, ButtonVariant::Outline)], Look { p: &p, ty: &ty });
                }
                _ => {}
            }
        } else if let Some((loading, error)) = reader {
            if loading {
                return self.loading(cx, kit, area, &p, &ty, widget_area);
            }
            if let Some(error) = error {
                return self.center_state(cx, kit, area, &[(error, p.danger)], &[], Look { p: &p, ty: &ty });
            }
        }
        if !has_text {
            return;
        }
        if show_preview && panel == MARKDOWN_FILE_PANEL {
            let look = MarkdownLook { palette: p, typography: ty, compact_width: area.size.x < 768.0 };
            let images_epoch = self.markdown_images_epoch(target);
            let mut markdown = std::mem::take(&mut self.markdown);
            {
                let this = &*self;
                let resolver = |source: &str| this.markdown_image_state(target, source);
                markdown.draw(cx, kit, area, &look, &resolver, images_epoch);
            }
            self.markdown = markdown;
            self.hits.push(area, Hot::Preview);
            return;
        }
        if show_preview && panel == HTML_FILE_PANEL {
            let base = ty.ui_size(14.0);
            let cy = area.pos.y + area.size.y / 2.0;
            kit.icon(cx, rect(area.pos.x, cy - 80.0, area.size.x, 32.0), "file-code", 32.0, p.muted);
            let inner = rect(area.pos.x, cy - 36.0, area.size.x, area.size.y / 2.0 + 36.0);
            let lines = [
                (l("HTML previews open in your system browser.").to_owned(), p.foreground),
                (l("Relative workspace assets are served locally; scripts are disabled.").to_owned(), p.muted),
            ];
            let buttons = [
                (Hot::OpenHtml, l("Open HTML preview").to_owned(), Some("external-link"), ButtonVariant::Primary),
                (Hot::ShowSource, t("panels.file.editor.source"), None, ButtonVariant::Outline),
            ];
            let _ = base;
            return self.center_state(cx, kit, inner, &lines, &buttons, Look { p: &p, ty: &ty });
        }
        let look = self.editor_look(ui);
        if let Some(editor) = self.editor.as_mut() {
            editor.focused = self.focus == Focus::Editor && cx.has_key_focus(widget_area);
            editor.draw(cx, kit, area, &look, widget_area);
            self.hits.push(area, Hot::Editor);
        }
    }

    fn markdown_images_epoch(&self, target: &FileTarget) -> u64 {
        let mut epoch = 0u64;
        for source in self.markdown.image_keys() {
            epoch = epoch.wrapping_mul(31).wrapping_add(match self.markdown_image_state(target, source) {
                ImageState::Loading => 1,
                ImageState::Ready { width, height, .. } => 2 + (width * 7 + height) as u64,
                ImageState::Failed(_) => 3,
            });
        }
        epoch
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_image(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, bytes: Arc<Vec<u8>>, mime: &str, p: &Palette, ty: &Typography, widget_area: Area) {
        let key = format!("{}\u{0}file\u{0}{:p}", self.owner, Arc::as_ptr(&bytes));
        if self.zoom.key != key {
            if !self.zoom.key.is_empty() {
                images::forget_prefix(&self.zoom.key);
            }
            self.zoom = Zoom { scale: 1.0, key: key.clone(), ..Zoom::default() };
            images::request(&key, bytes, mime, 512);
        }
        match images::slot(&key) {
            Some(ImageSlot::Ready { texture, width, height }) => {
                cx.begin_turtle(Walk::abs_rect(area), Layout::default());
                let (w, h) = (width as f64, height as f64);
                let fit = (area.size.x / w).min(area.size.y / h).min(1.0);
                let scale = fit * self.zoom.scale;
                let (dw, dh) = (w * scale, h * scale);
                let r = rect(area.pos.x + (area.size.x - dw) / 2.0 + self.zoom.offset.x, area.pos.y + (area.size.y - dh) / 2.0 + self.zoom.offset.y, dw, dh);
                kit.texture(cx, r, &texture, 0.0, 1.0);
                cx.end_turtle();
                self.hits.push(area, Hot::Image);
            }
            Some(ImageSlot::Failed(error)) => self.center_state(cx, kit, area, &[(error, p.danger)], &[], Look { p, ty }),
            _ => self.loading(cx, kit, area, p, ty, widget_area),
        }
    }

    /// Original `FileConflictAlert` strip; returns the new top.
    #[allow(clippy::too_many_arguments)]
    fn draw_alert(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, top: f64, title: &str, description: Option<&str>, tone: Vec4, icon: bool, actions: &[(Hot, String)], p: &Palette, ty: &Typography) -> f64 {
        let base = ty.ui_size(14.0);
        let small = ty.ui_size(12.0);
        let glyph = if icon { 16.0 + 12.0 } else { 0.0 };
        let button_widths: Vec<f64> = actions.iter().map(|(_, label)| ui::button_width(cx, kit, label, None, ButtonSize::Sm, ty)).collect();
        let buttons_w: f64 = button_widths.iter().sum::<f64>() + 8.0 * actions.len().saturating_sub(1) as f64;
        let wide = area.size.x >= 420.0;
        let text_w = (area.size.x - 24.0 - glyph - if wide && !actions.is_empty() { buttons_w + 12.0 } else { 0.0 }).max(1.0);
        let title_h = kit.paragraph_height(cx, text_w, spec(Font::Medium, base, tone), title);
        let desc_h = description.map(|d| kit.paragraph_height(cx, text_w, spec(Font::Regular, small, p.muted), d) + 4.0).unwrap_or(0.0);
        let message_h = title_h + desc_h;
        let button_h = ButtonSize::Sm.height();
        let content_h = if actions.is_empty() || wide { message_h.max(if actions.is_empty() { 0.0 } else { button_h }) } else { message_h + 8.0 + button_h };
        let h = (content_h + 16.0).max(48.0);
        let strip = rect(area.pos.x, top, area.size.x, h);
        kit.fill(cx, strip, p.surface1, 0.0);
        kit.hline(cx, strip.pos.x, strip.pos.y + h - 1.0, strip.size.x, tone);
        let mut x = strip.pos.x + 12.0;
        let message_top = if wide || actions.is_empty() { strip.pos.y + (h - message_h) / 2.0 } else { strip.pos.y + 8.0 };
        if icon {
            kit.icon(cx, rect(x, message_top, 16.0, title_h), "triangle-alert", 16.0, tone);
            x += glyph;
        }
        kit.paragraph(cx, dvec2(x, message_top), text_w, spec(Font::Medium, base, tone), title);
        if let Some(description) = description {
            kit.paragraph(cx, dvec2(x, message_top + title_h + 4.0), text_w, spec(Font::Regular, small, p.muted), description);
        }
        if !actions.is_empty() {
            let mut bx = strip.pos.x + strip.size.x - 12.0 - buttons_w;
            let by = if wide { strip.pos.y + (h - button_h) / 2.0 } else { strip.pos.y + h - 8.0 - button_h };
            for ((hot, label), w) in actions.iter().zip(button_widths) {
                let r = rect(bx, by, w, button_h);
                let st = self.state_of(hot, true);
                ui::button(cx, kit, r, label, None, ButtonVariant::Outline, ButtonSize::Sm, st, p, ty);
                self.hits.push(r, hot.clone());
                bx += w + 8.0;
            }
        }
        top + h
    }

    /// Original `PaneFind`: a floating card ≤340px at the top right.
    #[allow(clippy::too_many_arguments)]
    fn draw_find(&mut self, cx: &mut Cx2d, kit: &mut Kit, content: Rect, p: &Palette, ty: &Typography, compact: bool, widget_area: Area) {
        kit.painter.new_layer(cx);
        let can_replace = self.editor.as_ref().is_some_and(|e| !e.read_only);
        let button = ui::toolbar_button_size(compact);
        let row_h = button.max(28.0);
        let rows = if can_replace && self.replace_expanded { 2.0 } else { 1.0 };
        let width = (content.size.x - 16.0).min(340.0);
        let card = rect(content.pos.x + content.size.x - 8.0 - width, content.pos.y + 8.0, width, 12.0 + rows * row_h + (rows - 1.0) * 4.0);
        ui::card(cx, kit, card, p, 8.0);
        let mut x = card.pos.x + 6.0;
        let y = card.pos.y + 6.0;
        if can_replace {
            let r = rect(x, y + (row_h - button) / 2.0, button, button);
            let icon = if self.replace_expanded { "chevron-down" } else { "chevron-right" };
            ui::icon_button(cx, kit, r, icon, self.state_of(&Hot::ToggleReplace, true), compact, p);
            self.hits.push(r, Hot::ToggleReplace);
            x += button + 4.0;
        }
        let right = card.pos.x + card.size.x - 6.0;
        let can_navigate = self.match_count > 0;
        // Trailing buttons of the first row.
        let buttons: [(Hot, &str, bool, bool); 4] = [
            (Hot::MatchCase, "case-sensitive", true, self.match_case),
            (Hot::FindPrev, "arrow-up", can_navigate, false),
            (Hot::FindNext, "arrow-down", can_navigate, false),
            (Hot::FindClose, "x", true, false),
        ];
        let mut bx = right - (button * 4.0 + 12.0);
        let field_right = bx - 4.0;
        for (hot, icon, enabled, selected) in buttons {
            let r = rect(bx, y + (row_h - button) / 2.0, button, button);
            let mut st = self.state_of(&hot, enabled);
            st.selected = selected;
            ui::icon_button(cx, kit, r, icon, st, compact, p);
            if enabled {
                self.hits.push(r, hot);
            }
            bx += button + 4.0;
        }
        let status = if self.query.text().is_empty() {
            String::new()
        } else if self.match_count == 0 {
            t("paneFind.noMatches")
        } else if self.match_index > 0 {
            ta("paneFind.position", &[("current", &self.match_index.to_string()), ("total", &self.match_count.to_string())])
        } else {
            ta("paneFind.total", &[("total", &self.match_count.to_string())])
        };
        let field = rect(x, y, (field_right - x).max(40.0), row_h);
        self.query.focused = self.focus == Focus::Find;
        self.query.draw(cx, kit, field, &t("paneFind.placeholder"), Some(status.as_str()), ty.ui_size(14.0), p, widget_area);
        self.hits.push(field, Hot::FindField);
        if can_replace && self.replace_expanded {
            let y2 = y + row_h + 4.0;
            let size = if compact { ButtonSize::Sm } else { ButtonSize::Xs };
            let labels = [t("paneFind.replace"), t("paneFind.replaceAll")];
            let widths: Vec<f64> = labels.iter().map(|label| ui::button_width(cx, kit, label, None, size, ty)).collect();
            let mut rx = right - widths.iter().sum::<f64>() - 4.0;
            let field = rect(x, y2, (rx - 4.0 - x).max(40.0), row_h);
            self.replacement.focused = self.focus == Focus::Replace;
            self.replacement.draw(cx, kit, field, &t("paneFind.replaceWith"), None, ty.ui_size(14.0), p, widget_area);
            self.hits.push(field, Hot::ReplaceField);
            for ((label, w), hot) in labels.iter().zip(widths).zip([Hot::ReplaceOne, Hot::ReplaceAll]) {
                let r = rect(rx, y2 + (row_h - size.height()) / 2.0, w, size.height());
                let st = self.state_of(&hot, can_navigate);
                ui::button(cx, kit, r, label, None, ButtonVariant::Ghost, size, st, p, ty);
                if can_navigate {
                    self.hits.push(r, hot);
                }
                rx += w + 4.0;
            }
        }
    }

    /// Editor settings (Flutter `_settings` adaptive sheet) drawn as an in-panel dialog.
    fn draw_settings(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, p: &Palette, ty: &Typography) {
        kit.painter.new_layer(cx);
        ui::scrim(cx, kit, area);
        self.hits.push(area, Hot::Scrim);
        let base = ty.ui_size(14.0);
        let width = (area.size.x - 32.0).min(420.0);
        let rows = 3.0;
        let row_h = 44.0;
        let h = 56.0 + rows * row_h + (rows - 1.0) * 16.0 + 24.0;
        let card = rect(area.pos.x + (area.size.x - width) / 2.0, area.pos.y + ((area.size.y - h) / 2.0).max(16.0), width, h);
        kit.outline(cx, card, p.dialog, p.border, 1.0, 12.0);
        self.hits.push(card, Hot::Card);
        kit.text_in(cx, rect(card.pos.x + 20.0, card.pos.y + 12.0, width - 64.0, 32.0), spec(Font::Medium, ty.ui_size(16.0), p.foreground), l("Editor settings"), 0.0);
        let close = rect(card.pos.x + width - 40.0, card.pos.y + 16.0, 24.0, 24.0);
        ui::icon_button(cx, kit, close, "x", self.state_of(&Hot::SettingsClose, true), false, p);
        self.hits.push(close, Hot::SettingsClose);
        let mut y = card.pos.y + 56.0;
        let switches = [(l("Word wrap"), self.wrap, Hot::SetWrap), (l("Syntax highlighting"), self.highlighting, Hot::SetHighlight)];
        for (label, on, hot) in switches {
            let text_w = width - 40.0 - 52.0;
            let label_h = base * 1.4;
            kit.text_in(cx, rect(card.pos.x + 20.0, y + (row_h - label_h) / 2.0, text_w, label_h), spec(Font::Regular, base, p.foreground), label, 0.0);
            let sw = rect(card.pos.x + width - 20.0 - 40.0, y + (row_h - 24.0) / 2.0, 40.0, 24.0);
            kit.fill(cx, sw, if on { p.accent } else { p.surface3 }, 12.0);
            let knob_x = if on { sw.pos.x + 18.0 } else { sw.pos.x + 2.0 };
            kit.fill(cx, rect(knob_x, sw.pos.y + 2.0, 20.0, 20.0), if on { p.on_tertiary } else { p.foreground }, 10.0);
            self.hits.push(rect(card.pos.x + 12.0, y, width - 24.0, row_h), hot);
            y += row_h + 16.0;
        }
        // Code font size stepper.
        kit.text_in(cx, rect(card.pos.x + 20.0, y, width - 160.0, row_h), spec(Font::Regular, base, p.foreground), l("Code font size"), 0.0);
        let bh = ButtonSize::Sm.height();
        let plus = rect(card.pos.x + width - 20.0 - bh, y + (row_h - bh) / 2.0, bh, bh);
        let value = rect(plus.pos.x - 8.0 - 28.0, y, 28.0, row_h);
        let minus = rect(value.pos.x - 8.0 - bh, plus.pos.y, bh, bh);
        let minus_enabled = self.font_size > MIN_CODE_FONT;
        let plus_enabled = self.font_size < MAX_CODE_FONT;
        ui::button(cx, kit, minus, "", Some("minus"), ButtonVariant::Outline, ButtonSize::Sm, self.state_of(&Hot::FontMinus, minus_enabled), p, ty);
        ui::button(cx, kit, plus, "", Some("plus"), ButtonVariant::Outline, ButtonSize::Sm, self.state_of(&Hot::FontPlus, plus_enabled), p, ty);
        kit.text_in(cx, value, spec(Font::Regular, base, p.foreground), &format!("{}", self.font_size.round()), 0.5);
        if minus_enabled {
            self.hits.push(minus, Hot::FontMinus);
        }
        if plus_enabled {
            self.hits.push(plus, Hot::FontPlus);
        }
    }

    /// The download progress dialog (Flutter `saveRemoteFile` modal).
    fn draw_transfers(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, p: &Palette, ty: &Typography) {
        let owner = self.owner.clone();
        let transfers: Vec<(u64, String, u64, u64)> =
            store::with(|s| s.transfers_of(&owner).iter().map(|t| (t.id, t.name.clone(), t.received, t.total)).collect());
        draw_transfer_cards(cx, kit, area, &transfers, &mut self.hits, Hot::Card, Hot::TransferCancel, &self.hovered, p, ty);
    }
}

/// Draws progress cards for `transfers` and registers their Cancel buttons.
#[allow(clippy::too_many_arguments)]
pub fn draw_transfer_cards<H: Clone + PartialEq>(
    cx: &mut Cx2d,
    kit: &mut Kit,
    area: Rect,
    transfers: &[(u64, String, u64, u64)],
    hits: &mut HitMap<H>,
    block: H,
    cancel: impl Fn(u64) -> H,
    hovered: &Option<H>,
    p: &Palette,
    ty: &Typography,
) {
    let Some((id, name, received, total)) = transfers.first().cloned() else { return };
    kit.painter.new_layer(cx);
    kit.fill(cx, area, with_alpha(vec4(0.0, 0.0, 0.0, 1.0), 0.55), 0.0);
    let width = (area.size.x - 32.0).min(520.0);
    let base = ty.ui_size(14.0);
    let small = ty.ui_size(12.0);
    let h = 24.0 + base * 1.4 * 2.0 + 12.0 + 4.0 + 12.0 + ButtonSize::Sm.height() + 24.0;
    let card = rect(area.pos.x + (area.size.x - width) / 2.0, area.pos.y + (area.size.y - h) / 2.0, width, h);
    kit.outline(cx, card, p.surface1, p.surface2, 1.0, 12.0);
    let title = lf("Downloading {name}", &[("name", &name)]);
    let mut y = card.pos.y + 24.0;
    let title_h = kit.paragraph_height(cx, width - 48.0, spec(Font::Medium, base, p.foreground), &title).min(base * 1.4 * 2.0);
    kit.paragraph(cx, dvec2(card.pos.x + 24.0, y), width - 48.0, spec(Font::Medium, base, p.foreground), &title);
    y += title_h + 12.0;
    let bar = rect(card.pos.x + 24.0, y, width - 48.0, 4.0);
    kit.fill(cx, bar, p.surface3, 2.0);
    if total > 0 {
        kit.fill(cx, rect(bar.pos.x, bar.pos.y, bar.size.x * (received as f64 / total as f64).min(1.0), 4.0), p.accent, 2.0);
    } else {
        let phase = clock::phase(1200);
        kit.fill(cx, rect(bar.pos.x + bar.size.x * phase * 0.7, bar.pos.y, bar.size.x * 0.3, 4.0), p.accent, 2.0);
    }
    y += 4.0 + 12.0;
    let button_label = l("Cancel");
    let bw = ui::button_width(cx, kit, button_label, None, ButtonSize::Sm, ty);
    let bh = ButtonSize::Sm.height();
    let progress = lf("{received} / {total} bytes", &[("received", &received.to_string()), ("total", &total.to_string())]);
    kit.text_in(cx, rect(card.pos.x + 24.0, y, width - 48.0 - bw - 8.0, bh), spec(Font::Regular, small, p.muted), &progress, 0.0);
    let button = rect(card.pos.x + width - 24.0 - bw, y, bw, bh);
    let hot = cancel(id);
    let state = Interaction { hovered: hovered.as_ref() == Some(&hot), pressed: false, enabled: true, selected: false };
    ui::button(cx, kit, button, button_label, None, ButtonVariant::Outline, ButtonSize::Sm, state, p, ty);
    hits.push(area, block);
    hits.push(button, hot);
}

fn has_scheme(value: &str) -> bool {
    let Some(colon) = value.find(':') else { return false };
    let scheme = &value[..colon];
    // A Windows drive letter is not a scheme.
    scheme.len() > 1
        && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn slot_state(slot: Option<ImageSlot>) -> ImageState {
    match slot {
        Some(ImageSlot::Ready { texture, width, height }) => ImageState::Ready { texture, width, height },
        Some(ImageSlot::Failed(error)) => ImageState::Failed(error),
        _ => ImageState::Loading,
    }
}
