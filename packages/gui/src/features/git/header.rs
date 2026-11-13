//! Workspace header widgets: the git actions split button (original
//! `WorkspaceGitActionsButton`) and the Open-in split button (original
//! `WorkspaceOpenInButton`). The shell lays them out right-aligned in the wide
//! header and calls `set_workspace` every frame; rebinding happens only when
//! the checkout identity changes.

use super::actions::{ArchiveConfirm, Checkout, menu_entries};
use super::chrome::{palette, ui as ui_px};
use super::forge_url::{forge_from_remote_url, forge_web_url};
use super::json::{flag, text};
use super::overlay::{MenuEntry, MenuItem, Overlay, OverlayHost, OverlayKind, OverlayResult};
use super::paint::{Font, GitPaint, rect, with_alpha};
use super::policy::{ActionStatus, forge_presentation, normalize_forge};
use super::store::{GitStoreChanged, with_store};
use super::text::{t, tr};
use crate::common::icons::forge_brand_svg;
use crate::common::overlay::tooltip_at;
use crate::common::provider_icon::icon_path;
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect, is_compact_layout};
use makepad_widgets::*;
use paseo_gui_platform::{PlatformAction, PlatformValue, platform};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Primary,
    Caret,
}

/// The Git actions menu entry that opens the Git Log tab (not a checkout action).
const GIT_LOG_ITEM: &str = "git-log";

#[derive(Debug)]
enum ActionsPurpose {
    Menu,
    Archive(String),
}

/// Original `WorkspaceGitActionsButton`.
#[derive(Live, LiveHook, Widget)]
pub struct GitActionsButton {
    #[walk]
    walk: Walk,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    paint: GitPaint,
    #[rust]
    checkout: Checkout,
    #[rust]
    acquired: Option<Checkout>,
    #[rust]
    overlay: OverlayHost<ActionsPurpose>,
    #[rust]
    hovered: Option<Part>,
    #[rust]
    pressed: Option<Part>,
    #[rust]
    primary_width: f64,
    #[rust]
    phase: f64,
    #[rust]
    next_frame: NextFrame,
}

impl Drop for GitActionsButton {
    fn drop(&mut self) {
        if let Some(checkout) = self.acquired.take() {
            with_store(|store| store.release_checkout(&checkout.host, &checkout.cwd));
        }
    }
}

impl GitActionsButton {
    fn ensure_bound(&mut self, cx: &mut Cx, ui: &UiContext) {
        if self.acquired.as_ref() == Some(&self.checkout) {
            return;
        }
        let next = self.checkout.clone();
        let previous = self.acquired.take();
        with_store(|store| {
            // The replacement is acquired before the previous one is released.
            if !next.cwd.is_empty() {
                store.acquire_checkout(cx, ui, &next.host, &next.cwd);
            }
            if let Some(previous) = previous {
                store.release_checkout(&previous.host, &previous.cwd);
            }
        });
        if !next.cwd.is_empty() {
            self.acquired = Some(next);
        }
        self.redraw(cx);
    }

    fn open_confirm(&mut self, ui: &mut UiContext, confirm: ArchiveConfirm) {
        self.overlay.open(Overlay {
            purpose: ActionsPurpose::Archive(confirm.workspace_id),
            kind: OverlayKind::Confirm {
                title: confirm.title,
                message: confirm.message,
                confirm: confirm.confirm,
                destructive: true,
            },
        });
        self.overlay.flush(ui);
    }
}

impl Widget for GitActionsButton {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return;
        };
        with_store(|store| store.begin_event(cx, event, ui));
        self.ensure_bound(cx, ui);
        if let Event::Actions(actions) = event
            && actions.iter().any(|a| a.downcast_ref::<GitStoreChanged>().is_some())
        {
            self.redraw(cx);
        }
        if self.next_frame.is_event(event).is_some() {
            self.redraw(cx);
        }
        if let Some(result) = self.overlay.poll() {
            let checkout = self.checkout.clone();
            match result {
                OverlayResult::Chose(ActionsPurpose::Menu, id) if id == GIT_LOG_ITEM => {
                    let target = PanelTarget::new("git_log", &checkout.host, &checkout.workspace_id, &checkout.cwd);
                    ui.push(UiEffect::OpenTab { target, placement: TabPlacement::Main });
                }
                OverlayResult::Chose(ActionsPurpose::Menu, id) => {
                    let confirm = with_store(|store| {
                        let actions = checkout.build(ui, store);
                        let action = actions.secondary.iter().find(|a| a.id == id).cloned()?;
                        checkout.run(cx, ui, store, &action)
                    });
                    if let Some(confirm) = confirm {
                        self.open_confirm(ui, confirm);
                    }
                }
                OverlayResult::Confirmed(ActionsPurpose::Archive(workspace_id)) => {
                    with_store(|store| checkout.archive_confirmed(cx, ui, store, &workspace_id));
                }
                _ => {}
            }
            self.redraw(cx);
        }
        let point_part = |abs: DVec2, area: Rect, primary_width: f64| {
            if abs.x < area.pos.x + primary_width { Part::Primary } else { Part::Caret }
        };
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let part = point_part(e.abs, self.area.rect(cx), self.primary_width);
                if self.hovered != Some(part) {
                    self.hovered = Some(part);
                    cx.set_cursor(MouseCursor::Hand);
                    self.redraw(cx);
                }
            }
            Hit::FingerHoverOut(_) => {
                self.hovered = None;
                self.redraw(cx);
            }
            Hit::FingerDown(e) if e.is_primary_hit() => {
                self.pressed = Some(point_part(e.abs, self.area.rect(cx), self.primary_width));
            }
            Hit::FingerUp(e) => {
                let pressed = self.pressed.take();
                let area = self.area.rect(cx);
                if !e.is_over || (e.device.is_touch() && !e.was_tap()) || pressed != Some(point_part(e.abs, area, self.primary_width)) {
                    return;
                }
                let checkout = self.checkout.clone();
                match pressed {
                    Some(Part::Primary) => {
                        let confirm = with_store(|store| {
                            let primary = checkout.build(ui, store).primary?;
                            checkout.run(cx, ui, store, &primary)
                        });
                        if let Some(confirm) = confirm {
                            self.open_confirm(ui, confirm);
                        }
                    }
                    Some(Part::Caret) => {
                        let entries = with_store(|store| {
                            let actions = checkout.build(ui, store);
                            let mut entries = menu_entries(&checkout, store, &actions.secondary);
                            if !entries.is_empty() {
                                entries.push(MenuEntry::Separator);
                            }
                            entries.push(MenuEntry::Item(MenuItem::new(GIT_LOG_ITEM, t("workspace.git.log.open")).icon("git-graph")));
                            entries
                        });
                        self.overlay.open(Overlay {
                            purpose: ActionsPurpose::Menu,
                            kind: OverlayKind::Menu {
                                title: Some(t("workspace.git.actions.moreOptions")),
                                anchor: area,
                                align_end: true,
                                width: None,
                                entries,
                            },
                        });
                        self.overlay.flush(ui);
                    }
                    None => {}
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, _walk: Walk) -> DrawStep {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return DrawStep::done();
        };
        self.ensure_bound(cx, ui);
        let checkout = self.checkout.clone();
        // The caret menu always has at least the Git Log entry.
        let built = with_store(|store| {
            let actions = checkout.build(ui, store);
            let primary = actions.primary.clone()?;
            Some((checkout.label(store, &primary), primary, true))
        });
        // Nothing outside a git checkout or without a primary action.
        let Some((label, primary, has_secondary)) = built else {
            // A zero-size absolute rect keeps a redrawable area (no layout spacing):
            // with `Area::Empty` the store's change notification could never redraw it.
            let origin = cx.turtle().pos();
            cx.walk_turtle_with_area(&mut self.area, Walk::abs_rect(Rect { pos: origin, size: DVec2::default() }));
            return DrawStep::done();
        };
        let colors = palette();
        let compact = is_compact_layout(cx);
        let height = 28.0;
        let size = ui_px(14.0);
        let pending = primary.status == ActionStatus::Pending;
        let content = if pending {
            14.0
        } else {
            16.0 + if compact { 8.0 } else { 4.0 } + self.paint.measure(cx, Font::Regular, size, &label).x
        };
        let primary_width = content + 2.0 * if compact { 12.0 } else { 8.0 };
        let width = primary_width + if has_secondary { height } else { 0.0 };
        self.primary_width = primary_width;
        let frame = cx.walk_turtle_with_area(&mut self.area, Walk::fixed(width, height));
        let primary_rect = rect(frame.pos.x, frame.pos.y, primary_width, height);
        if self.hovered == Some(Part::Primary) && !primary.disabled {
            self.paint.rounded(cx, primary_rect, colors.surface2, 6.0, None);
        }
        let alpha = if primary.disabled { 0.6 } else { 1.0 };
        let center_y = frame.pos.y + height / 2.0;
        if pending {
            self.phase = cx.seconds_since_app_start();
            self.paint.spinner(cx, dvec2(primary_rect.pos.x + primary_width / 2.0, center_y), 14.0, colors.foreground, self.phase);
            self.next_frame = cx.new_next_frame();
        } else {
            let mut x = primary_rect.pos.x + if compact { 12.0 } else { 8.0 };
            self.paint.icon(cx, Checkout::icon(&primary), 16.0, with_alpha(colors.muted, alpha), dvec2(x + 8.0, center_y));
            x += 16.0 + if compact { 8.0 } else { 4.0 };
            self.paint.text_centered(cx, Font::Regular, size, with_alpha(colors.foreground, alpha), x, frame.pos.y, height, &label, primary_width);
        }
        if has_secondary {
            let caret = rect(frame.pos.x + primary_width, frame.pos.y, height, height);
            if self.hovered == Some(Part::Caret) || self.overlay.is_open() {
                self.paint.fill(cx, caret, colors.surface2);
            }
            self.paint.fill(cx, rect(caret.pos.x, caret.pos.y, 1.0, height), colors.border_accent);
            self.paint.icon(cx, "chevron-down", 16.0, colors.extra_muted, caret.center());
        }
        self.paint.rounded(cx, frame, vec4(0.0, 0.0, 0.0, 0.0), 6.0, Some((colors.border_accent, 1.0)));
        DrawStep::done()
    }
}

impl GitActionsButtonRef {
    /// Binds the button to a checkout; a no-op while the identity is unchanged.
    pub fn set_workspace(&self, cx: &mut Cx, host_id: &str, workspace_id: &str, cwd: &str) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let next = Checkout {
            host: host_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            cwd: cwd.to_owned(),
        };
        if inner.checkout != next {
            inner.checkout = next;
            // A late answer for the previous checkout is ignored.
            inner.overlay.close(cx);
            inner.redraw(cx);
        }
    }
}

// ---------------------------------------------------------------------------
// Open in.

/// One entry of the Open-in menu (original open-in-editor/planner.ts).
#[derive(Clone, Debug, PartialEq)]
struct OpenTarget {
    id: String,
    label: String,
    icon: &'static str,
    svg: Option<String>,
    kind: OpenKind,
}

#[derive(Clone, Debug, PartialEq)]
enum OpenKind {
    Editor,
    Forge(String),
}

#[derive(Debug)]
struct ChoosePurpose;

/// Original `WorkspaceOpenInButton` (desktop header, `hideLabels`).
#[derive(Live, LiveHook, Widget)]
pub struct OpenInButton {
    #[walk]
    walk: Walk,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    paint: GitPaint,
    #[live]
    brand_icon: DrawIcon,
    #[rust]
    host: String,
    #[rust]
    cwd: String,
    #[rust]
    active_file: Option<String>,
    #[rust]
    acquired: Option<(String, String)>,
    #[rust]
    overlay: OverlayHost<ChoosePurpose>,
    /// The desktop's own daemon server id (`desktop_daemon_status.serverId`).
    #[rust]
    local_server_id: Option<String>,
    #[rust]
    local_request: Option<u64>,
    #[rust]
    local_resolved: bool,
    #[rust]
    editors: Vec<paseo_gui_platform::EditorTarget>,
    #[rust]
    editors_request: Option<u64>,
    #[rust]
    open_request: Option<u64>,
    #[rust]
    opening: bool,
    #[rust]
    hovered: Option<Part>,
    #[rust]
    pressed: Option<Part>,
    #[rust]
    phase: f64,
    #[rust]
    next_frame: NextFrame,
}

impl Drop for OpenInButton {
    fn drop(&mut self) {
        if let Some((host, cwd)) = self.acquired.take() {
            with_store(|store| store.release_checkout(&host, &cwd));
        }
    }
}

impl OpenInButton {
    fn ensure_bound(&mut self, cx: &mut Cx, ui: &UiContext) {
        let next = (self.host.clone(), self.cwd.clone());
        if self.acquired.as_ref() == Some(&next) {
            return;
        }
        let previous = self.acquired.take();
        with_store(|store| {
            if !next.1.is_empty() {
                store.acquire_checkout(cx, ui, &next.0, &next.1);
            }
            if let Some((host, cwd)) = previous {
                store.release_checkout(&host, &cwd);
            }
        });
        if !next.1.is_empty() {
            self.acquired = Some(next);
        }
        // Original use-is-local-daemon.ts: ask the desktop host once; the
        // desktop's own daemon also lists local editors.
        if let Some(platform) = platform()
            && platform.capabilities().desktop_host
        {
            if !self.local_resolved && self.local_request.is_none() {
                self.local_request = Some(platform.desktop_invoke("desktop_daemon_status", json!({})));
            } else if self.local_server_id.as_deref() == Some(self.host.as_str())
                && self.editors.is_empty()
                && self.editors_request.is_none()
            {
                self.editors_request = Some(platform.list_editor_targets());
            }
        }
        self.redraw(cx);
    }

    fn relative_file(&self) -> Option<String> {
        let path = self.active_file.as_deref().filter(|p| !p.is_empty())?;
        let root = self.cwd.trim_end_matches('/');
        if let Some(rest) = path.strip_prefix(&format!("{root}/")) {
            return Some(rest.to_owned());
        }
        if path.starts_with('/') { None } else { Some(path.to_owned()) }
    }

    /// Original `planWorkspaceOpenTargets`.
    fn targets(&self) -> Vec<OpenTarget> {
        let mut targets = Vec::new();
        if self.local_server_id.as_deref() == Some(self.host.as_str()) {
            for editor in &self.editors {
                targets.push(OpenTarget {
                    id: editor.id.clone(),
                    label: editor.label.clone(),
                    icon: if editor.kind == "file-manager" { "folder" } else { "code-xml" },
                    svg: None,
                    kind: OpenKind::Editor,
                });
            }
        }
        let forge_target = with_store(|store| {
            let state = store.checkout(&self.host, &self.cwd)?;
            let status = state.status.as_ref().filter(|s| flag(s, "isGit"))?;
            let forge = match &state.pr_payload {
                Some(payload) => normalize_forge(payload.get("forge").and_then(Value::as_str)),
                None => forge_from_remote_url(Some(text(status, "remoteUrl")))?.to_owned(),
            };
            let url = forge_web_url(
                &forge,
                Some(text(status, "remoteUrl")),
                Some(text(status, "currentBranch")),
                self.relative_file().as_deref(),
                None,
                None,
            )?;
            Some(OpenTarget {
                id: forge.clone(),
                label: forge_presentation(Some(&forge)).brand_label,
                icon: "git-pull-request",
                svg: forge_brand_svg(&forge),
                kind: OpenKind::Forge(url),
            })
        });
        targets.extend(forge_target);
        targets
    }

    /// Original primary Tooltip copy: the active file name when a file tab is focused.
    fn primary_tooltip(&self, ui: &UiContext) -> Option<String> {
        let targets = self.targets();
        let primary = self.primary(ui, &targets)?;
        Some(match self.relative_file() {
            Some(file) => tr(
                "workspace.git.openInEditor.openFileIn",
                &[("fileName", file.rsplit('/').next().unwrap_or(&file).to_owned()), ("target", primary.label)],
            ),
            None => tr("workspace.git.openInEditor.openIn", &[("target", primary.label)]),
        })
    }

    fn primary(&self, ui: &UiContext, targets: &[OpenTarget]) -> Option<OpenTarget> {
        let stored = ui.preference("preferredEditor").and_then(Value::as_str);
        targets
            .iter()
            .find(|t| Some(t.id.as_str()) == stored)
            .or_else(|| targets.first())
            .cloned()
    }

    fn run(&mut self, cx: &mut Cx, ui: &mut UiContext, target: &OpenTarget) {
        match &target.kind {
            OpenKind::Forge(url) => ui.push(UiEffect::OpenExternal { url: url.clone() }),
            OpenKind::Editor => {
                let Some(platform) = platform() else { return };
                let root = self.cwd.trim_end_matches('/').to_owned();
                let file = self.relative_file().map(|relative| std::path::PathBuf::from(format!("{root}/{relative}")));
                self.opening = true;
                self.open_request = Some(platform.open_editor(paseo_gui_platform::OpenEditorRequest {
                    editor_id: target.id.clone(),
                    workspace_path: std::path::PathBuf::from(&self.cwd),
                    file_path: file,
                    line: None,
                    column: None,
                }));
                self.redraw(cx);
            }
        }
    }

    fn on_platform(&mut self, cx: &mut Cx, ui: &mut UiContext, action: &PlatformAction) {
        let PlatformAction::Completed { request, result } = action else { return };
        if Some(*request) == self.local_request {
            self.local_request = None;
            if let Ok(PlatformValue::Json(status)) = result {
                let id = text(status, "serverId").trim().to_owned();
                // A daemon still starting has no id yet; ask again on the next bind.
                if !id.is_empty() {
                    self.local_resolved = true;
                    self.local_server_id = Some(id);
                }
            }
            if self.local_server_id.as_deref() == Some(self.host.as_str())
                && self.editors.is_empty()
                && self.editors_request.is_none()
                && let Some(platform) = platform()
            {
                self.editors_request = Some(platform.list_editor_targets());
            }
            self.redraw(cx);
        } else if Some(*request) == self.editors_request {
            self.editors_request = None;
            if let Ok(PlatformValue::EditorTargets(editors)) = result {
                self.editors = editors.clone();
            }
            self.redraw(cx);
        } else if Some(*request) == self.open_request {
            self.open_request = None;
            self.opening = false;
            if let Err(error) = result {
                let message = if error.message.trim().is_empty() {
                    t("workspace.git.openInEditor.failedOpen")
                } else {
                    error.message.clone()
                };
                ui.toast(message, true);
            }
            self.redraw(cx);
        }
    }
}

impl Widget for OpenInButton {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return;
        };
        with_store(|store| store.begin_event(cx, event, ui));
        self.ensure_bound(cx, ui);
        if let Event::Actions(actions) = event {
            for action in actions.iter() {
                if action.downcast_ref::<GitStoreChanged>().is_some() {
                    self.redraw(cx);
                }
                if let Some(platform_action) = action.downcast_ref::<PlatformAction>() {
                    self.on_platform(cx, ui, platform_action);
                }
            }
        }
        if self.next_frame.is_event(event).is_some() {
            self.redraw(cx);
        }
        if let Some(OverlayResult::Chose(ChoosePurpose, id)) = self.overlay.poll() {
            // The menu only chooses the preferred target; the primary half opens it.
            let targets = self.targets();
            let current = self.primary(ui, &targets).map(|t| t.id);
            if current.as_deref() != Some(id.as_str()) {
                ui.push(UiEffect::SetPreference {
                    key: "preferredEditor".into(),
                    value: json!(id),
                });
            }
            self.redraw(cx);
        }
        let size = 28.0;
        let part = |abs: DVec2, area: Rect| if abs.x < area.pos.x + size { Part::Primary } else { Part::Caret };
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hovered = part(e.abs, self.area.rect(cx));
                if self.hovered != Some(hovered) {
                    self.hovered = Some(hovered);
                    cx.set_cursor(MouseCursor::Hand);
                    // Original Tooltip on the primary half: "Open in <target>" / "Open <file> in <target>".
                    let tip = (hovered == Part::Primary).then(|| self.primary_tooltip(ui)).flatten();
                    let area = self.area.rect(cx);
                    tooltip_at(ui, tip.as_deref(), rect(area.pos.x, area.pos.y, size, area.size.y), 0.5);
                    self.redraw(cx);
                }
            }
            Hit::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.5);
                    self.redraw(cx);
                }
            }
            Hit::FingerDown(e) if e.is_primary_hit() => {
                tooltip_at(ui, None, Rect::default(), 0.5);
                self.pressed = Some(part(e.abs, self.area.rect(cx)));
            }
            Hit::FingerUp(e) => {
                let area = self.area.rect(cx);
                let pressed = self.pressed.take();
                if !e.is_over || (e.device.is_touch() && !e.was_tap()) || pressed != Some(part(e.abs, area)) {
                    return;
                }
                let targets = self.targets();
                let Some(primary) = self.primary(ui, &targets) else { return };
                match pressed {
                    Some(Part::Primary) if !self.opening => self.run(cx, ui, &primary),
                    Some(Part::Caret) if targets.len() > 1 => {
                        let entries = targets
                            .iter()
                            .map(|target| {
                                let mut item = MenuItem::new(target.id.clone(), target.label.clone())
                                    .icon(target.icon)
                                    .selected(target.id == primary.id);
                                if let Some(svg) = &target.svg {
                                    item = item.svg(svg.clone());
                                }
                                MenuEntry::Item(item)
                            })
                            .collect();
                        self.overlay.open(Overlay {
                            purpose: ChoosePurpose,
                            kind: OverlayKind::Menu {
                                title: Some(t("workspace.git.openInEditor.chooseEditor")),
                                anchor: area,
                                align_end: true,
                                width: None,
                                entries,
                            },
                        });
                        self.overlay.flush(ui);
                    }
                    _ => {}
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, _walk: Walk) -> DrawStep {
        let Some(ui) = scope.data.get_mut::<UiContext>() else {
            return DrawStep::done();
        };
        self.ensure_bound(cx, ui);
        let targets = self.targets();
        let Some(primary) = self.primary(ui, &targets) else {
            let origin = cx.turtle().pos();
            cx.walk_turtle_with_area(&mut self.area, Walk::abs_rect(Rect { pos: origin, size: DVec2::default() }));
            return DrawStep::done();
        };
        let colors = palette();
        let size = 28.0;
        let width = size * if targets.len() > 1 { 2.0 } else { 1.0 };
        let frame = cx.walk_turtle_with_area(&mut self.area, Walk::fixed(width, size));
        let primary_rect = rect(frame.pos.x, frame.pos.y, size, size);
        if self.hovered == Some(Part::Primary) && !self.opening {
            self.paint.fill(cx, primary_rect, colors.surface2);
        }
        if self.opening {
            self.phase = cx.seconds_since_app_start();
            self.paint.spinner(cx, primary_rect.center(), 14.0, colors.foreground, self.phase);
            self.next_frame = cx.new_next_frame();
        } else if let Some(path) = primary.svg.as_deref().and_then(icon_path) {
            self.brand_icon.svg_path = ArcStringMut::Arc(path);
            self.brand_icon.color = colors.muted;
            let icon = rect(primary_rect.center().x - 8.0, primary_rect.center().y - 8.0, 16.0, 16.0);
            self.brand_icon.draw_walk(cx, Walk::abs_rect(icon));
        } else {
            self.paint.icon(cx, primary.icon, 16.0, colors.muted, primary_rect.center());
        }
        if targets.len() > 1 {
            let caret = rect(frame.pos.x + size, frame.pos.y, size, size);
            if self.hovered == Some(Part::Caret) || self.overlay.is_open() {
                self.paint.fill(cx, caret, colors.surface2);
            }
            self.paint.fill(cx, rect(caret.pos.x, caret.pos.y, 1.0, size), colors.border_accent);
            self.paint.icon(cx, "chevron-down", 16.0, colors.extra_muted, caret.center());
        }
        self.paint.rounded(cx, frame, vec4(0.0, 0.0, 0.0, 0.0), 6.0, Some((colors.border_accent, 1.0)));
        DrawStep::done()
    }
}

impl OpenInButtonRef {
    /// Binds to a checkout and the focused file tab's path; a no-op while unchanged.
    pub fn set_workspace(&self, cx: &mut Cx, host_id: &str, cwd: &str, active_file: Option<&str>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let file = active_file.map(str::to_owned);
        if inner.host != host_id || inner.cwd != cwd {
            inner.host = host_id.to_owned();
            inner.cwd = cwd.to_owned();
            inner.editors.clear();
            inner.editors_request = None;
            inner.local_resolved = inner.local_server_id.is_some();
            inner.redraw(cx);
        }
        if inner.active_file != file {
            inner.active_file = file;
            inner.redraw(cx);
        }
    }
}
