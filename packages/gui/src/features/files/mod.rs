//! Files feature (Flutter `features/files`): the workspace explorer, file tabs with the native
//! source/Markdown/HTML/image views, the remote folder picker, file observation, saves,
//! conflicts, close protection and transfers. Root widget [`FilesPanel`] (`files` and `file`
//! tabs); [`RemoteFolderPicker`] for remote directory selection (Add Project / New Tab).
//!
//! Shell integration: [`handle_event`] once per event before widget dispatch; the
//! [`close_requested`] / [`close_confirmed`] / [`close_cancelled`] flow for dirty file tabs.

pub mod editor;
pub mod explorer;
pub mod explorer_panel;
pub mod explorer_view;
pub mod file_view;
pub mod handle;
pub mod highlight;
pub mod host;
pub mod html_preview;
pub mod i18n;
pub mod icons_data;
pub mod images;
pub mod kit;
pub mod markdown;
pub mod model;
pub mod path;
pub mod picker;
pub mod resolve;
pub mod service;
pub mod session;
pub mod store;
pub mod transfer;
pub mod ui;

use makepad_widgets::*;
use serde_json::Value;

use crate::common::{PanelTarget, UiContext};

pub use images::{file_icon, file_icon_name};
pub use kit::DrawFilesImage;
pub use path::{FileLocation, parse_file_location, remote_absolute, remote_basename, remote_parent, remote_relative};
pub use picker::{RemoteFolderPicker, RemoteFolderPickerAction, RemoteFolderPickerRef, RemoteFolderPickerWidgetRefExt};
pub use resolve::{BUILTIN_FILE_PANELS, FilePanelCandidate, FilePanelResolution, resolve_file_panel};
pub use service::upload_bytes;
pub use store::{CloseCheck, close_cancelled, close_confirmed, close_requested, handle_event, release};

use explorer_panel::{ExplorerPanel, ExplorerTarget};
use file_view::{FileTarget, FileView};
use kit::Kit;

/// Draws the material file icon of `file_name` (Flutter `FileIcon`) into `rect` with a
/// `DrawFilesImage` from the caller's DSL (`use crate::features::files::DrawFilesImage;`).
/// Returns `false` while the icon is still decoding: redraw when [`revision`] changes
/// (decode arrivals land on `Event::Signal` through [`handle_event`]).
pub fn draw_file_icon(cx: &mut Cx2d, image: &mut DrawFilesImage, rect: Rect, file_name: &str) -> bool {
    match file_icon(file_name) {
        Some(texture) => {
            image.draw_texture(cx, rect, &texture, 0.0, 1.0);
            true
        }
        None => false,
    }
}

/// Bumps whenever files state visible to drawing changed (sessions, trees, decoded images).
pub fn revision() -> u64 {
    store::with(|s| s.epoch)
}

/// Registers the feature DSL: the kit, the root (`FilesPanel`), then the picker.
pub fn live_design(cx: &mut Cx) {
    kit::live_design(cx);
    root_live_design(cx);
    picker::live_design(cx);
}

/// The root DSL lives in this module (the shell imports `crate::features::files::FilesPanel`);
/// the macro's generated registration function is scoped to this body.
fn root_live_design(cx: &mut Cx) {
live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::features::files::kit::FilesKit;

    pub FilesPanelBase = {{FilesPanel}} {}
    pub FilesPanel = <FilesPanelBase> {
        width: Fill, height: Fill
        kit: <FilesKit> {}
    }

}
    live_design(cx);
}

/// What a `files` or `file` tab shows.
#[derive(Clone, Debug, PartialEq)]
pub enum FilesKind {
    Explorer,
    File,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FilesTarget {
    pub kind: FilesKind,
    pub host_id: String,
    pub workspace_id: String,
    pub cwd: String,
    /// The file (file tabs).
    pub path: Option<String>,
    pub location: Option<FileLocation>,
    /// A folder to reveal (explorer tabs; assistant folder links).
    pub reveal: Option<String>,
    pub tab_key: String,
}

fn int(data: &Value, key: &str) -> Option<usize> {
    data.get(key).and_then(Value::as_u64).map(|v| v as usize).filter(|v| *v > 0)
}

impl FilesTarget {
    /// Flutter `PanelTarget` of kind `files` / `file` (`data.path`, `data.line`, `data.lineEnd`,
    /// `data.column`, or the shell's `data.location` from a `#L…` link; `data.reveal`).
    pub fn from_panel(target: &PanelTarget) -> FilesTarget {
        let data = &target.data;
        let text = |key: &str| data.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
        if target.kind == "file" {
            let path = text("path").or_else(|| target.id.clone()).unwrap_or_default();
            let (path, link) = parse_file_location(&path);
            let mut location = int(data, "line").map(|line| FileLocation { line, line_end: int(data, "lineEnd"), column: int(data, "column") });
            if location.is_none() {
                location = link.or_else(|| {
                    text("location").and_then(|raw| {
                        let raw = raw.trim_start_matches('#');
                        let suffix = if raw.starts_with('L') { format!("#{raw}") } else { format!("#L{raw}") };
                        parse_file_location(&suffix).1
                    })
                });
            }
            FilesTarget {
                kind: FilesKind::File,
                host_id: target.server_id.clone(),
                workspace_id: target.workspace_id.clone(),
                cwd: target.cwd.clone(),
                path: Some(path),
                location,
                reveal: None,
                tab_key: target.key(),
            }
        } else {
            FilesTarget {
                kind: FilesKind::Explorer,
                host_id: target.server_id.clone(),
                workspace_id: target.workspace_id.clone(),
                cwd: target.cwd.clone(),
                path: None,
                location: None,
                reveal: text("reveal").or_else(|| text("path")).map(|p| p.trim_end_matches('/').to_owned()),
                tab_key: target.key(),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// FilesPanel

#[derive(Live, LiveHook, Widget)]
pub struct FilesPanel {
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    kit: Kit,
    #[rust]
    target: Option<FilesTarget>,
    #[rust]
    explicit: bool,
    #[rust(true)]
    visible: bool,
    #[rust]
    explorer: Option<ExplorerPanel>,
    #[rust]
    file: Option<FileView>,
    #[rust]
    next_frame: NextFrame,
    #[rust]
    seen_epoch: u64,
    /// Top-left of the last draw (before any parent alignment moved it).
    #[rust]
    drawn_at: DVec2,
    #[rust]
    theme_epoch: u64,
}

impl Drop for FilesPanel {
    fn drop(&mut self) {
        // The tab closed (or the window went away): a modified buffer is still written to its
        // file unless the shell's discard confirmation already disposed it.
        if let Some(target) = &self.target {
            if let Some(file) = self.file.as_mut() {
                file.dispose();
            }
            if let Some(explorer) = self.explorer.as_mut() {
                explorer.dispose();
            }
            store::release(&target.tab_key);
        }
    }
}

impl FilesPanel {
    fn apply_target(&mut self, cx: &mut Cx, target: FilesTarget) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        let owner_changed = self.target.as_ref().is_some_and(|old| old.tab_key != target.tab_key);
        if owner_changed {
            if let Some(file) = self.file.as_mut() {
                file.release_focus(cx);
            }
            if let Some(old) = self.target.take() {
                store::release(&old.tab_key);
            }
            self.file = None;
            self.explorer = None;
        }
        match target.kind {
            FilesKind::Explorer => {
                self.file = None;
                let explorer = self.explorer.get_or_insert_with(|| ExplorerPanel::new(&target.tab_key));
                explorer.set_target(ExplorerTarget {
                    host_id: target.host_id.clone(),
                    workspace_id: target.workspace_id.clone(),
                    cwd: target.cwd.clone(),
                    reveal: target.reveal.clone(),
                });
            }
            FilesKind::File => {
                self.explorer = None;
                let uid = self.widget_uid();
                let file = self.file.get_or_insert_with(|| FileView::new(&target.tab_key, uid));
                file.set_target(cx, FileTarget {
                    host_id: target.host_id.clone(),
                    workspace_id: target.workspace_id.clone(),
                    cwd: target.cwd.clone(),
                    path: target.path.clone().unwrap_or_default(),
                    location: target.location,
                });
            }
        }
        self.target = Some(target);
        // The next frame mounts the session/tree with the dispatch context.
        self.next_frame = cx.new_next_frame();
        self.area.redraw(cx);
    }

    /// A target without a cwd (the shell could not resolve the workspace yet) takes the
    /// workspace directory once the directory knows it.
    fn resolve_cwd(&mut self, cx: &mut Cx, ui: &UiContext) {
        let Some(target) = self.target.clone() else { return };
        if !target.cwd.is_empty() {
            return;
        }
        if let Some(cwd) = host::workspace_directory(&ui.state, &target.host_id, &target.workspace_id) {
            let mut next = target;
            next.cwd = cwd;
            self.apply_target(cx, next);
        }
    }
}

impl Widget for FilesPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let Some(ui) = scope.data.get_mut::<UiContext>() else { return };
        if !self.explicit
            && let Some(target) = ui.target.clone().filter(|t| t.kind == "files" || t.kind == "file") {
                self.apply_target(cx, FilesTarget::from_panel(&target));
            }
        self.resolve_cwd(cx, ui);
        let observing = self.visible && ui.activity.visible && ui.activity.app_active;
        let area = self.area;
        let shift = if area.is_valid(cx) { area.rect(cx).pos - self.drawn_at } else { DVec2::default() };
        let mut changed = false;
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.hit_shift = shift;
            changed |= explorer.handle_event(cx, &mut self.kit, event, ui, area);
        }
        if let Some(file) = self.file.as_mut()
            && !self.target.as_ref().is_some_and(|t| t.cwd.is_empty()) {
                file.hit_shift = shift;
                changed |= file.handle_event(cx, &mut self.kit, event, ui, area, observing);
            }
        let epoch = store::with(|s| s.epoch);
        if changed || epoch != self.seen_epoch || theme_epoch::changed(&mut self.theme_epoch) {
            self.seen_epoch = epoch;
            self.area.redraw(cx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        let rect = cx.turtle().rect();
        self.drawn_at = rect.pos;
        if let Some(ui) = scope.data.get::<UiContext>() {
            let area = self.area;
            if let Some(explorer) = self.explorer.as_mut() {
                explorer.draw(cx, &mut self.kit, rect, ui, area);
            } else if let Some(file) = self.file.as_mut() {
                file.draw(cx, &mut self.kit, rect, ui, area);
            } else {
                self.kit.fill(cx, rect, crate::theme::current_palette().surface0, 0.0);
            }
        }
        cx.end_turtle_with_area(&mut self.area);
        DrawStep::done()
    }
}

impl FilesPanelRef {
    pub fn set_target(&self, cx: &mut Cx, target: FilesTarget) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.explicit = true;
            inner.apply_target(cx, target);
        }
    }

    /// Flutter `PanelActivity.visible`: a hidden tab stops observing its file; showing it again
    /// reports what changed meanwhile.
    pub fn set_visible(&self, cx: &mut Cx, visible: bool) {
        if let Some(mut inner) = self.borrow_mut()
            && inner.visible != visible {
                inner.visible = visible;
                if !visible && let Some(file) = inner.file.as_mut() {
                    file.release_focus(cx);
                }
                inner.next_frame = cx.new_next_frame();
                inner.area.redraw(cx);
            }
    }
}

mod theme_epoch {
    /// Whether the app theme changed since `seen` (and records it).
    pub fn changed(seen: &mut u64) -> bool {
        let now = crate::theme::epoch();
        if *seen != now {
            *seen = now;
            true
        } else {
            false
        }
    }
}
