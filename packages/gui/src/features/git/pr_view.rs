//! The pull request pane (original pull_request_panel.dart `PullRequestPanel`
//! drawing): toolbar, header, checks (or GitLab pipeline), and the activity
//! timeline. Bodies are `PaseoMarkdown(compact: true)`: the timeline's native
//! Markdown painter, each body its own selection region.

use super::chrome::{palette, toolbar_band, toolbar_button, ui as ui_px};
use super::json::{array, flag, int, object, text};
use super::model::{format_age, format_duration, now_ms};
use super::overlay::{MenuEntry, MenuItem, Overlay, OverlayHost, OverlayKind, OverlayResult};
use super::paint::{Font, GitPaint, Hits, rect, rgb, with_alpha};
use super::pr_pane::PrPane;
use super::pull_request::{
    AVATAR_FILLS, Check, Entry, Thread, activity_counts, avatar_fill_index, build_timeline, check_groups,
    check_timing, checks_outcome, classify_check, classify_gitlab_job, map_activity, pr_state, pull_request_checks,
    pull_request_number, repo_identity, thread_collapsed_by_default, thread_id, thread_path, timeline_items,
};
use super::host;
use super::store::{GitStore, pr_status};
use super::text::{t, tr};
use super::wire::payload_error;
use crate::common::overlay::tooltip_at;
use crate::common::{PanelTarget, UiContext, UiEffect};
use crate::features::timeline::canvas::{Canvas, ChatKit, HitTarget};
use crate::features::timeline::chat_row::hover_key;
use crate::features::timeline::images::{self, ImageState};
use crate::features::timeline::markdown::{self as md, MdOpts};
use crate::features::timeline::paint::{Draw, Look, MOBILE, RowState, now_seconds};
use makepad_widgets::*;
use serde_json::Value;
use std::collections::HashMap;

/// One Markdown body: its selectable runs and hit regions, and the painter's
/// per-body state (parse cache, code-block scroll, copy feedback, hover).
#[derive(Default)]
struct MdBody {
    canvas: Canvas,
    state: RowState,
}

#[derive(Clone, Debug, PartialEq)]
enum PrHit {
    ViewPr,
    Refresh,
    Header,
    Retry,
    ChecksToggle,
    Group(Check),
    Launch(String),
    ActivityToggle,
    Entry { id: String, default_collapsed: bool },
    Kebab { key: String, body: String, url: String, thread: bool },
}

#[derive(Debug)]
enum PrPurpose {
    Activity { body: String, url: String },
    Thread { url: String },
}

pub struct PrView {
    pub target: PanelTarget,
    pub pane: PrPane,
    overlay: OverlayHost<PrPurpose>,
    scroll: f64,
    content: f64,
    viewport: Rect,
    checks_scroll: f64,
    checks_area: Rect,
    checks_content: f64,
    hits: Hits<PrHit>,
    /// Entry regions for reveal-on-hover kebabs.
    regions: Vec<(Rect, String)>,
    hovered: Option<PrHit>,
    hovered_region: Option<String>,
    pressed: Option<PrHit>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    bodies: HashMap<String, MdBody>,
    /// Bodies drawn this frame (key, rect), topmost last.
    body_rects: Vec<(String, Rect)>,
    body_hovered: Option<String>,
    /// A link / copy glyph pressed inside a body.
    body_pressed: Option<(String, HitTarget)>,
    /// The body whose text is being drag-selected.
    body_selecting: Option<String>,
    /// The Markdown look of the frame being drawn.
    look: Option<Look>,
    heights: HashMap<String, f64>,
    pub spinning: bool,
    /// An avatar was loading or a Markdown body (which may show images) was
    /// drawn: the panel redraws when the shared image cache changes.
    pub images_pending: bool,
}

/// Original `_launch`: only http(s) URLs open.
fn launch(ui: &mut UiContext, url: &str) {
    if url.starts_with("https://") || url.starts_with("http://") {
        ui.push(UiEffect::OpenExternal { url: url.to_owned() });
    }
}

fn check_color(check: Check) -> Vec4 {
    let colors = palette();
    match check {
        Check::Success => colors.success,
        Check::Failure => colors.danger,
        Check::Warning | Check::ActionRequired | Check::Pending => colors.warning,
        Check::Manual | Check::Ignored => colors.muted,
    }
}

/// Original `CheckPresentationIcon`.
fn check_icon(paint: &mut GitPaint, cx: &mut Cx2d, check: Check, size: f64, center: DVec2) {
    let icon = match check {
        Check::Success => "circle-check",
        Check::Failure | Check::Warning => "circle-x",
        Check::Pending => "circle-dot",
        Check::Ignored => "circle-slash",
        // The GitLab manual gear (original inline SVG).
        Check::ActionRequired | Check::Manual => return paint.manual_check(cx, size, check_color(check), center),
    };
    paint.icon(cx, icon, size, check_color(check), center);
}

impl PrView {
    pub fn new(cx: &mut Cx, ui: &UiContext, store: &mut GitStore, target: PanelTarget) -> Self {
        store.acquire_checkout(cx, ui, &target.server_id, &target.cwd);
        let pane = PrPane::new(&target.server_id, &target.cwd, store);
        Self {
            target,
            pane,
            overlay: OverlayHost::default(),
            scroll: 0.0,
            content: 0.0,
            viewport: Rect::default(),
            checks_scroll: 0.0,
            checks_area: Rect::default(),
            checks_content: 0.0,
            hits: Hits::default(),
            regions: Vec::new(),
            hovered: None,
            hovered_region: None,
            pressed: None,
            touch_scroll: Default::default(),
            bodies: HashMap::new(),
            body_rects: Vec::new(),
            body_hovered: None,
            body_pressed: None,
            body_selecting: None,
            look: None,
            heights: HashMap::new(),
            spinning: false,
            images_pending: false,
        }
    }

    pub fn dispose(&mut self, store: &mut GitStore) {
        store.release_checkout(&self.target.server_id, &self.target.cwd);
    }

    pub fn close_overlay(&mut self, cx: &mut Cx) {
        self.overlay.close(cx);
    }

    fn refreshing(&self, store: &GitStore) -> bool {
        store.action_status(&self.target.server_id, &self.target.cwd, "refresh") == super::policy::ActionStatus::Pending
    }

    /// Original `useCheckoutGitActionsStore.refresh` (also the error state's retry).
    fn refresh(&self, cx: &mut Cx, store: &mut GitStore) {
        if self.refreshing(store) {
            return;
        }
        store.run_action(
            cx,
            &self.target.server_id,
            &self.target.cwd,
            "refresh",
            vec![("checkout.refresh.request", serde_json::json!({}))],
            "workspace.git.diff.failedRefresh",
            None,
            super::store::ActionFollowUp::None,
        );
    }

    /// Applies an event to the pane state; returns true when a redraw is due.
    pub fn process(&mut self, cx: &mut Cx, event: &Event, ui: &UiContext, store: &mut GitStore, visible: bool) -> bool {
        self.pane.set_visible(cx, ui, store, visible);
        self.pane.process(cx, event, ui, store)
    }

    /// The body under `abs` among those drawn this frame.
    fn body_at(&self, abs: DVec2) -> Option<String> {
        self.body_rects.iter().rev().find(|(_, r)| r.contains(abs)).map(|(key, _)| key.clone())
    }

    /// Pointer input over Markdown bodies (original `PaseoMarkdown` inside a
    /// `SelectionArea`): link taps, code copy glyphs, drag / double / triple
    /// tap selection, copy. Returns `Some(redraw)` when a body took the event.
    fn body_event(&mut self, cx: &mut Cx, hit: &Hit, area: Area, ui: &mut UiContext) -> Option<bool> {
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let key = self.body_at(e.abs);
                let mut changed = key != self.body_hovered;
                if let Some(previous) = self.body_hovered.take()
                    && key.as_ref() != Some(&previous)
                    && let Some(body) = self.bodies.get_mut(&previous)
                {
                    body.state.hovered = false;
                    body.state.hover_hit = None;
                    body.state.hover_link = None;
                }
                self.body_hovered = key.clone();
                let Some(key) = key else {
                    if changed {
                        area.redraw(cx);
                    }
                    return None;
                };
                let body = self.bodies.get_mut(&key)?;
                let (hover_hit, hover_link) = match body.canvas.hit_at(e.abs - body.canvas.origin) {
                    Some(HitTarget::Link { href, .. }) => (None, Some(href.clone())),
                    Some(target) => (Some(hover_key(target)), None),
                    None => (None, None),
                };
                let hand = hover_hit.is_some() || hover_link.is_some();
                changed |= body.state.hover_hit != hover_hit || body.state.hover_link != hover_link || !body.state.hovered;
                body.state.hovered = true;
                body.state.hover_hit = hover_hit;
                body.state.hover_link = hover_link;
                cx.set_cursor(if hand { MouseCursor::Hand } else { MouseCursor::Text });
                // Region hover (kebab reveal) still applies under a body.
                let region = self.regions.iter().rev().find(|(r, _)| r.contains(e.abs)).map(|(_, id)| id.clone());
                changed |= region != self.hovered_region || self.hovered.is_some();
                if self.hovered.is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.3);
                }
                self.hovered_region = region;
                self.hovered = None;
                Some(changed)
            }
            Hit::FingerHoverOut(_) => {
                if let Some(previous) = self.body_hovered.take()
                    && let Some(body) = self.bodies.get_mut(&previous)
                {
                    body.state.hovered = false;
                    body.state.hover_hit = None;
                    body.state.hover_link = None;
                    area.redraw(cx);
                }
                None
            }
            Hit::FingerDown(e) => {
                let key = self.body_at(e.abs);
                // A press elsewhere ends every body selection (one region at a time).
                let mut changed = false;
                for (other, body) in self.bodies.iter_mut() {
                    if Some(other) != key.as_ref() && body.canvas.selection.take().is_some() {
                        changed = true;
                    }
                }
                let Some(key) = key else {
                    if changed {
                        area.redraw(cx);
                    }
                    return None;
                };
                let body = self.bodies.get_mut(&key)?;
                let rel = e.abs - body.canvas.origin;
                if let Some(target) = body.canvas.hit_at(rel).cloned() {
                    self.body_pressed = Some((key, target));
                    // Still arm the outer pan when a touch starts on a link/copy.
                    return if e.device.is_touch() { None } else { Some(changed) };
                }
                // Touch drags scroll; touch selection starts with a long press.
                if e.device.is_touch() {
                    if changed {
                        area.redraw(cx);
                    }
                    return None;
                }
                cx.set_key_focus(area);
                body.canvas.selection = body.canvas.offset_at(rel).map(|offset| {
                    if e.tap_count >= 3 {
                        (0, body.canvas.plain.len())
                    } else if e.tap_count == 2 {
                        body.canvas.word_at(offset)
                    } else {
                        (offset, offset)
                    }
                });
                self.body_selecting = Some(key);
                Some(true)
            }
            Hit::FingerLongPress(e) => {
                let key = self.body_at(e.abs)?;
                let body = self.bodies.get_mut(&key)?;
                let offset = body.canvas.offset_at(e.abs - body.canvas.origin)?;
                self.body_pressed = None;
                body.canvas.selection = Some(body.canvas.word_at(offset));
                self.body_selecting = Some(key);
                Some(true)
            }
            Hit::FingerMove(e) => {
                let key = self.body_selecting.clone()?;
                let body = self.bodies.get_mut(&key)?;
                if let (Some(offset), Some((anchor, _))) = (body.canvas.offset_at(e.abs - body.canvas.origin), body.canvas.selection) {
                    body.canvas.selection = Some((anchor, offset));
                }
                Some(true)
            }
            Hit::FingerUp(e) => {
                if let Some((key, target)) = self.body_pressed.take() {
                    let body = self.bodies.get_mut(&key)?;
                    if e.is_over && (!e.device.is_touch() || e.was_tap()) && body.canvas.hit_at(e.abs - body.canvas.origin) == Some(&target) {
                        match target {
                            HitTarget::Link { href, .. } => launch(ui, &href),
                            HitTarget::Copy { key, text } => {
                                cx.copy_to_clipboard(&text);
                                body.state.copied.insert(key, now_seconds());
                            }
                            _ => {}
                        }
                    }
                    return Some(true);
                }
                let key = self.body_selecting.take()?;
                // Touch has no copy shortcut: a finished selection is copied.
                if e.device.is_touch()
                    && let Some(text) = self.bodies.get(&key).and_then(|b| b.canvas.selected_text())
                {
                    ui.push(UiEffect::CopyText { text, toast: Some(t("common.actions.copied")) });
                }
                Some(false)
            }
            Hit::TextCopy(ce) => {
                let text = self.bodies.values().find_map(|b| b.canvas.selected_text())?;
                *ce.response.borrow_mut() = Some(text);
                Some(false)
            }
            Hit::KeyDown(ke) if ke.key_code == KeyCode::KeyA && (ke.modifiers.control || ke.modifiers.logo) => {
                let body = self.bodies.values_mut().find(|b| b.canvas.selection.is_some())?;
                body.canvas.selection = Some((0, body.canvas.plain.len()));
                Some(true)
            }
            Hit::KeyFocusLost(_) => {
                let mut changed = false;
                for body in self.bodies.values_mut() {
                    changed |= body.canvas.selection.take().is_some();
                }
                Some(changed)
            }
            _ => None,
        }
    }

    /// Answers of the activity / thread menus (shared overlays).
    pub fn handle_overlay(&mut self, ui: &mut UiContext) -> bool {
        match self.overlay.poll() {
            Some(OverlayResult::Chose(PrPurpose::Activity { body, url }, value)) => {
                if value == "copy" {
                    ui.push(UiEffect::CopyText { text: body, toast: None });
                } else {
                    launch(ui, &url);
                }
                true
            }
            Some(OverlayResult::Chose(PrPurpose::Thread { url }, _)) => {
                launch(ui, &url);
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    pub fn handle_pointer(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        area: Area,
        ui: &mut UiContext,
        store: &mut GitStore,
    ) -> bool {
        let hit = event.hits(cx, area);
        let pan = self.touch_scroll.handle_hit(&hit);
        if let Some(redraw) = self.body_event(cx, &hit, area, ui) {
            return redraw;
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let (anchor, hovered) = self.hits.find(e.abs).map(|(anchor, hit)| (anchor, Some(hit))).unwrap_or_default();
                let region = self.regions.iter().rev().find(|(r, _)| r.contains(e.abs)).map(|(_, id)| id.clone());
                if hovered != self.hovered || region != self.hovered_region {
                    cx.set_cursor(if hovered.is_some() { MouseCursor::Hand } else { MouseCursor::Default });
                    if hovered != self.hovered {
                        // Original refresh Tooltip: "Refreshing…" or "Refresh <forge> state".
                        let tip = (hovered == Some(PrHit::Refresh)).then(|| {
                            if self.refreshing(store) {
                                t("workspace.git.diff.refreshing")
                            } else {
                                tr("workspace.git.diff.refreshState", &[("brand", forge_brand(store, &self.target))])
                            }
                        });
                        tooltip_at(ui, tip.as_deref(), anchor, 0.3);
                    }
                    self.hovered = hovered;
                    self.hovered_region = region;
                    return true;
                }
            }
            Hit::FingerHoverOut(_) => {
                let hovered = self.hovered.take().is_some();
                if hovered {
                    tooltip_at(ui, None, Rect::default(), 0.3);
                }
                return self.hovered_region.take().is_some() || hovered;
            }
            Hit::FingerScroll(e) => return self.scroll_at(e.abs, e.scroll.y),
            Hit::FingerDown(e) => {
                tooltip_at(ui, None, Rect::default(), 0.3);
                self.pressed = self.hits.find(e.abs).map(|(_, hit)| hit);
            }
            Hit::FingerMove(e) => {
                let Some(dy) = pan else { return false };
                self.pressed = None;
                self.body_pressed = None;
                return self.scroll_at(e.abs_start, dy);
            }
            Hit::FingerUp(e) => {
                let pressed = self.pressed.take();
                if !e.is_over || (e.device.is_touch() && !e.was_tap()) {
                    return false;
                }
                let Some((anchor, hit)) = self.hits.find(e.abs) else { return false };
                if pressed.as_ref() != Some(&hit) {
                    return false;
                }
                self.activate(cx, ui, store, hit, anchor);
                return true;
            }
            _ => {}
        }
        false
    }

    fn scroll_at(&mut self, point: DVec2, delta: f64) -> bool {
        if self.checks_area.contains(point) && self.checks_content > self.checks_area.size.y {
            let max = self.checks_content - self.checks_area.size.y;
            let next = (self.checks_scroll + delta).clamp(0.0, max);
            if next != self.checks_scroll {
                self.checks_scroll = next;
                return true;
            }
        }
        let max = (self.content - self.viewport.size.y).max(0.0);
        let next = (self.scroll + delta).clamp(0.0, max);
        let changed = next != self.scroll;
        self.scroll = next;
        changed
    }

    fn activate(&mut self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, hit: PrHit, anchor: Rect) {
        let status = pr_status(store.checkout(&self.target.server_id, &self.target.cwd)).clone();
        let number = pull_request_number(&status);
        match hit {
            PrHit::ViewPr | PrHit::Header => launch(ui, text(&status, "url")),
            PrHit::Refresh | PrHit::Retry => self.refresh(cx, store),
            PrHit::ChecksToggle => self.pane.toggle_checks(cx, ui, store),
            PrHit::Group(group) => self.pane.toggle_group(group),
            PrHit::Launch(url) => launch(ui, &url),
            PrHit::ActivityToggle => self.pane.activity_open = !self.pane.activity_open,
            PrHit::Entry { id, default_collapsed } => self.pane.toggle_entry(number, &id, default_collapsed),
            PrHit::Kebab { body, url, thread, .. } => {
                let brand = forge_brand(store, &self.target);
                let open = MenuEntry::Item(
                    MenuItem::new("open", tr("workspace.git.pr.actions.openOn", &[("brand", brand)])).icon("external-link"),
                );
                let (purpose, entries) = if thread {
                    (PrPurpose::Thread { url }, vec![open])
                } else {
                    let mut entries = Vec::new();
                    if !body.trim().is_empty() {
                        entries.push(MenuEntry::Item(MenuItem::new("copy", "Copy").icon("copy")));
                    }
                    entries.push(open);
                    (PrPurpose::Activity { body, url }, entries)
                };
                self.overlay.open(Overlay {
                    purpose,
                    kind: OverlayKind::Menu { title: None, anchor, align_end: true, width: Some(200.0), entries },
                });
                self.overlay.flush(ui);
            }
        }
    }
}

/// Original `_forgeBrand`.
fn forge_brand(store: &GitStore, target: &PanelTarget) -> String {
    let forge = store
        .checkout(&target.server_id, &target.cwd)
        .and_then(|s| s.pr_payload.as_ref())
        .map(|p| text(p, "forge").to_owned())
        .unwrap_or_default();
    match forge.as_str() {
        "gitlab" => "GitLab",
        "gitea" => "Gitea",
        "forgejo" => "Forgejo",
        "codeberg" => "Codeberg",
        _ => "GitHub",
    }
    .to_owned()
}

/// Layout cursor: draws only what intersects the viewport, always advances.
struct Cursor {
    y: f64,
    left: f64,
    width: f64,
    view: Rect,
}

impl Cursor {
    fn visible(&self, top: f64, height: f64) -> bool {
        top + height >= self.view.pos.y && top <= self.view.pos.y + self.view.size.y
    }
}

impl PrView {
    /// Original `resolvePullRequestContentState` + pane drawing.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, cx: &mut Cx2d, ui: &UiContext, store: &GitStore, paint: &mut GitPaint, area: Rect, kit: &mut ChatKit) {
        let colors = palette();
        self.hits.clear();
        self.regions.clear();
        self.spinning = false;
        self.images_pending = false;
        self.checks_area = Rect::default();
        self.viewport = area;
        paint.fill(cx, area, colors.sidebar);
        let state = store.checkout(&self.target.server_id, &self.target.cwd);
        let payload = state.and_then(|s| s.pr_payload.clone()).unwrap_or(Value::Null);
        let status = pr_status(state).clone();
        let compact = crate::common::is_compact_layout(cx);
        // `PaseoMarkdown` look: runtime palette and typography; block gaps follow
        // the window's compact layout (markdownBlockGap).
        let syntax = ui.preference("syntaxTheme").and_then(Value::as_str).unwrap_or("one");
        self.look = Some(Look {
            palette: crate::theme::current_palette(),
            typo: crate::theme::current_typography(),
            compact,
            mobile: MOBILE,
            list_inset: 12.0,
            syntax: paseo_highlight::SyntaxThemeId::from_name(syntax).unwrap_or(paseo_highlight::SyntaxThemeId::One),
        });
        self.body_rects.clear();
        if let Some(number) = pull_request_number(&status) {
            self.scroll = self.scroll.clamp(0.0, (self.content - area.size.y).max(0.0));
            paint.begin_clip(cx, area);
            let mut cursor = Cursor { y: area.pos.y - self.scroll, left: area.pos.x, width: area.size.x, view: area };
            self.pane_content(cx, ui, store, paint, &mut cursor, &payload, &status, number, kit, compact);
            self.content = cursor.y + self.scroll - area.pos.y;
            paint.end_clip(cx);
        } else if payload.get("error").is_some_and(|e| !e.is_null())
            || payload_error(&payload).is_some()
            || state.is_some_and(|s| s.pr_error.is_some())
        {
            // Original `_PaneError`.
            let message = t("workspace.git.diff.failedRefresh");
            let size = ui_px(12.0);
            let center = area.center();
            let width = paint.measure(cx, Font::Regular, size, &message).x.min(area.size.x - 32.0);
            paint.text_wrapped(cx, Font::Regular, size, colors.muted, dvec2(center.x - width / 2.0, center.y - 24.0), &message, area.size.x - 32.0);
            let label = t("common.actions.retry");
            let width = super::chrome::measure_button(cx, paint, &label, true, super::chrome::ButtonSize::Xs, compact);
            let button = rect(center.x - width / 2.0, center.y, width, super::chrome::button_height(super::chrome::ButtonSize::Xs, compact));
            let hovered = self.hovered == Some(PrHit::Retry);
            super::chrome::button(cx, paint, button, &label, Some("rotate-cw"), super::chrome::ButtonVariant::Ghost, super::chrome::ButtonSize::Xs, compact, true, hovered);
            self.hits.push(button, PrHit::Retry);
        } else if state.is_some_and(|s| s.pr_loading) {
            self.skeleton(cx, paint, area);
        } else {
            // Original `_PaneEmpty`.
            let center = area.center();
            let inner = area.size.x - 48.0;
            let size = ui_px(14.0);
            for (index, (key, color)) in [("panels.pullRequest.emptyTitle", colors.foreground), ("panels.pullRequest.emptyDescription", colors.muted)].into_iter().enumerate() {
                let copy = t(key);
                let width = paint.measure(cx, Font::Regular, size, &copy).x.min(inner);
                paint.text_wrapped(cx, Font::Regular, size, color, dvec2(center.x - width / 2.0, center.y - 20.0 + index as f64 * 28.0), &copy, inner);
            }
        }
    }

    /// Original `useSkeletonPulse`: block opacity 0.4 ↔ 0.8, linear, 1s each
    /// way. Requests frames only while a skeleton is drawn.
    fn pulse_fill(&mut self, cx: &mut Cx2d) -> Vec4 {
        self.spinning = true;
        let phase = cx.seconds_since_app_start() % 2.0;
        let t = if phase < 1.0 { phase } else { 2.0 - phase };
        with_alpha(palette().surface2, (0.4 + 0.4 * t) as f32)
    }

    /// Original `_PaneSkeleton`.
    fn skeleton(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect) {
        let colors = palette();
        let fill = self.pulse_fill(cx);
        let x = area.pos.x;
        let w = area.size.x;
        let mut y = area.pos.y + 16.0;
        paint.rounded(cx, rect(x + 16.0, y, (w - 32.0) * 0.75, 16.0), fill, 2.0, None);
        y += 24.0;
        paint.rounded(cx, rect(x + 16.0, y, (w - 32.0) * 0.4, 12.0), fill, 2.0, None);
        y += 28.0;
        toolbar_band(cx, paint, rect(x, y, w, 36.0), true);
        paint.rounded(cx, rect(x + 12.0, y + 6.0, 96.0, 24.0), fill, 8.0, None);
        paint.rounded(cx, rect(x + 116.0, y + 6.0, 96.0, 24.0), fill, 8.0, None);
        y += 36.0 + 8.0;
        paint.text(cx, Font::Medium, ui_px(12.0), colors.muted, dvec2(x + 12.0, y + 8.0), &t("workspace.git.pr.sections.checks"));
        y += 32.0;
        for _ in 0..3 {
            paint.rounded(cx, rect(x + 12.0, y + 9.0, 14.0, 14.0), fill, 7.0, None);
            paint.rounded(cx, rect(x + 34.0, y + 10.0, (w - 46.0) * 0.6, 12.0), fill, 2.0, None);
            y += 36.0;
        }
        y += 8.0;
        self.activity_skeleton(cx, paint, x, w, y);
    }

    /// Original `_ActivitySkeleton`: rows of a 20px avatar centred on a 12px + 10px text stack.
    fn activity_skeleton(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, x: f64, w: f64, mut y: f64) -> f64 {
        let fill = self.pulse_fill(cx);
        for _ in 0..3 {
            paint.rounded(cx, rect(x + 12.0, y + 3.0, 20.0, 20.0), fill, 10.0, None);
            paint.rounded(cx, rect(x + 40.0, y, (w - 52.0) * 0.7, 12.0), fill, 2.0, None);
            paint.rounded(cx, rect(x + 40.0, y + 16.0, (w - 52.0) * 0.45, 10.0), fill, 2.0, None);
            y += 26.0 + 12.0;
        }
        // Column spacing sits between rows only.
        y - 12.0
    }

    #[allow(clippy::too_many_arguments)]
    fn pane_content(
        &mut self,
        cx: &mut Cx2d,
        ui: &UiContext,
        store: &GitStore,
        paint: &mut GitPaint,
        c: &mut Cursor,
        payload: &Value,
        pr: &Value,
        number: i64,
        kit: &mut ChatKit,
        compact: bool,
    ) {
        let colors = palette();
        let forge = match text(payload, "forge") {
            "" => "github",
            forge => forge,
        }
        .to_owned();
        // Toolbar.
        let band = rect(c.left, c.y, c.width, 36.0);
        toolbar_band(cx, paint, band, true);
        let center_y = band.pos.y + 17.5;
        let label = t("workspace.git.pr.actions.viewPullRequest");
        let label_size = ui_px(12.0);
        let view_width = 4.0 + 12.0 + 4.0 + paint.measure(cx, Font::Regular, label_size, &label).x + 4.0;
        let view = rect(band.pos.x + 12.0, center_y - 12.0, view_width, 24.0);
        let hovered = self.hovered == Some(PrHit::ViewPr);
        let fg = if hovered { colors.foreground } else { colors.muted };
        paint.icon(cx, "external-link", 12.0, fg, dvec2(view.pos.x + 4.0 + 6.0, center_y));
        paint.text_centered(cx, Font::Regular, label_size, fg, view.pos.x + 20.0, view.pos.y, 24.0, &label, view_width);
        self.hits.push_clipped(view, c.view, PrHit::ViewPr);
        if host::supports(ui, &self.target.server_id, "checkoutRefresh") {
            let right = band.pos.x + band.size.x - 12.0;
            if self.refreshing(store) {
                paint.spinner(cx, dvec2(right - 10.0, center_y), 14.0, colors.muted, cx.seconds_since_app_start());
                self.spinning = true;
            } else {
                let hovered = self.hovered == Some(PrHit::Refresh);
                let button = toolbar_button(cx, paint, compact, right, center_y, Some("rotate-cw"), false, true, hovered);
                self.hits.push_clipped(button, c.view, PrHit::Refresh);
            }
        }
        c.y += 36.0;
        self.header(cx, paint, c, pr, &forge);
        let facts = object(pr, "forgeSpecific").clone();
        if self.pane.gitlab_pipeline_section(ui, &facts) {
            self.pipeline_section(cx, paint, c, &facts);
        } else {
            self.checks_section(cx, paint, c, pr);
        }
        paint.fill(cx, rect(c.left, c.y, c.width, 1.0), colors.border);
        c.y += 1.0;
        // Activity.
        let activities: Vec<Value> = timeline_items(self.pane.timeline.as_ref(), number)
            .iter()
            .filter_map(map_activity)
            .collect();
        let entries = build_timeline(&activities);
        let (approvals, changes, comments) = activity_counts(&activities);
        let open = self.pane.activity_open;
        self.section_header(cx, paint, c, &t("workspace.git.pr.sections.activity"), open, PrHit::ActivityToggle, &[
            (approvals, Some(Check::Success), colors.success),
            (changes, Some(Check::Failure), colors.danger),
            (comments, None, colors.muted),
        ]);
        if open {
            if self.pane.timeline_fetching && self.pane.timeline.is_none() {
                c.y = self.activity_skeleton(cx, paint, c.left, c.width, c.y);
            } else {
                if entries.is_empty() {
                    self.empty_text(cx, paint, c, &t("workspace.git.pr.empty.noActivity"));
                }
                for entry in &entries {
                    self.entry(cx, paint, c, entry, Some(number), kit);
                }
            }
            c.y += 12.0;
        }
    }

    /// Original `_Header`.
    fn header(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, c: &mut Cursor, pr: &Value, forge: &str) {
        let colors = palette();
        let top = c.y;
        let inner = c.width - 32.0;
        let base = ui_px(14.0);
        let title = format!("{} {}{}", text(pr, "title"), if forge == "gitlab" { "!" } else { "#" }, int(pr, "number"));
        let title_height = paint.measure_wrapped(cx, Font::Regular, base, &title, inner);
        if c.visible(top, title_height + 48.0) {
            paint.text_wrapped(cx, Font::Regular, base, colors.foreground, dvec2(c.left + 16.0, top + 16.0), &title, inner);
        }
        let row_y = top + 16.0 + title_height + 8.0;
        let state = pr_state(pr);
        let (icon, color) = match state {
            "merged" => ("git-merge", colors.merged),
            "closed" => ("git-pull-request-closed", colors.danger),
            "draft" => ("git-pull-request-draft", colors.muted),
            _ => ("git-pull-request", colors.success),
        };
        let small = ui_px(12.0);
        let center_y = row_y + 8.0;
        let mut x = c.left + 16.0;
        paint.icon(cx, icon, 14.0, color, dvec2(x + 7.0, center_y));
        x += 18.0;
        x += paint.text_centered(cx, Font::Regular, small, color, x, row_y, 16.0, &t(&format!("workspace.git.pr.states.{state}")), inner) + 4.0;
        let facts = object(pr, "forgeSpecific");
        let required = if text(facts, "forge") == "gitlab" { int(facts, "approvalsRequired") } else { 0 };
        if required > 0 {
            let given = int(facts, "approvalsGiven");
            paint.icon(cx, "circle-check", 11.0, if given >= required { colors.success } else { colors.muted }, dvec2(x + 6.0, center_y));
            x += 16.0;
            let copy = tr("workspace.git.pr.approvals", &[("given", given.to_string()), ("required", required.to_string())]);
            x += paint.text_centered(cx, Font::Regular, small, colors.muted, x, row_y, 16.0, &copy, inner) + 4.0;
        }
        let identity = repo_identity(pr);
        let right = c.left + c.width - 16.0 - 16.0;
        if !identity.is_empty() {
            x += 4.0;
            x += paint.text_centered(cx, Font::Regular, small, colors.muted, x, row_y, 16.0, &identity, (right - x).max(0.0));
        }
        let region = rect(c.left, top, c.width, 16.0 + title_height + 8.0 + 16.0 + 16.0);
        if self.hovered == Some(PrHit::Header) {
            paint.icon(cx, "external-link", 12.0, colors.muted, dvec2(x + 4.0 + 6.0, center_y));
        }
        self.hits.push_clipped(region, c.view, PrHit::Header);
        c.y = region.pos.y + region.size.y;
    }

    /// Original `_Section` header with summary pills.
    #[allow(clippy::too_many_arguments)]
    fn section_header(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, c: &mut Cursor, title: &str, open: bool, hit: PrHit, summary: &[(usize, Option<Check>, Vec4)]) {
        let colors = palette();
        let height = 8.0 + 16.0 + 8.0;
        let band = rect(c.left, c.y, c.width, height);
        let center_y = band.pos.y + height / 2.0;
        paint.icon(cx, if open { "chevron-down" } else { "chevron-right" }, 14.0, colors.muted, dvec2(c.left + 12.0 + 7.0, center_y));
        paint.text_centered(cx, Font::Medium, ui_px(12.0), colors.muted, c.left + 12.0 + 14.0 + 8.0, band.pos.y, height, title, c.width / 2.0);
        let mut right = c.left + c.width - 12.0;
        for (count, check, color) in summary.iter().rev() {
            if *count == 0 {
                continue;
            }
            let label = count.to_string();
            let width = paint.measure(cx, Font::Regular, ui_px(12.0), &label).x;
            paint.text_centered(cx, Font::Regular, ui_px(12.0), *color, right - width, band.pos.y, height, &label, width + 1.0);
            right -= width + 3.0;
            match check {
                Some(check) => check_icon(paint, cx, *check, 12.0, dvec2(right - 6.0, center_y)),
                None => paint.icon(cx, "message-square", 11.0, colors.muted, dvec2(right - 6.0, center_y)),
            }
            right -= 12.0 + 8.0;
        }
        self.hits.push_clipped(band, c.view, hit);
        c.y += height;
    }

    fn empty_text(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, c: &mut Cursor, copy: &str) {
        let height = paint.text_wrapped(cx, Font::Regular, ui_px(12.0), palette().muted, dvec2(c.left + 12.0, c.y + 8.0), copy, c.width - 24.0);
        c.y += height + 16.0;
    }

    /// Original `_CheckRow`; returns its height.
    #[allow(clippy::too_many_arguments)]
    fn check_row(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, x: f64, y: f64, width: f64, height: f64, left_pad: f64, icon: Check, name: &str, detail: &str, trailing: Option<String>, url: Option<String>, clip: Rect) {
        let colors = palette();
        let band = rect(x, y, width, height);
        if let Some(url) = &url {
            let hit = PrHit::Launch(url.clone());
            if self.hovered == Some(hit.clone()) {
                paint.fill(cx, band, colors.sidebar_hover);
            }
            self.hits.push_clipped(band, clip, hit);
        }
        let center_y = y + height / 2.0;
        check_icon(paint, cx, icon, 14.0, dvec2(x + left_pad + 7.0, center_y));
        let mut right = x + width - 12.0;
        if let Some(trailing) = trailing {
            let w = paint.measure(cx, Font::Regular, ui_px(12.0), &trailing).x;
            paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, right - w, y, height, &trailing, w + 1.0);
            right -= w + 8.0;
        }
        let text_left = x + left_pad + 14.0 + 8.0;
        let available = (right - text_left).max(0.0);
        let name_width = if detail.is_empty() { available } else { available * 0.6 };
        let drawn = paint.text_centered(cx, Font::Regular, ui_px(14.0), colors.foreground, text_left, y, height, name, name_width);
        if !detail.is_empty() {
            paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, text_left + drawn + 8.0, y, height, detail, (available - drawn - 8.0).max(0.0));
        }
    }

    /// Original `_ChecksSection` with the checks ring.
    fn checks_section(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, c: &mut Cursor, pr: &Value) {
        let colors = palette();
        let checks = pull_request_checks(pr);
        let groups = check_groups(&checks);
        let outcome = checks_outcome(&checks, &groups);
        const COPY: &str = "workspace.git.pr.checksOverview";
        let parts: Vec<String> = groups
            .iter()
            .map(|(presentation, matching)| tr(&format!("{COPY}.count.{}", presentation.name()), &[("count", matching.len().to_string())]))
            .collect();
        let headline = t(&format!("{COPY}.headline.{outcome}"));
        let detail = if parts.is_empty() {
            String::new()
        } else {
            let key = if checks.len() == 1 { format!("{COPY}.detailOne") } else { format!("{COPY}.detailMany") };
            tr(&key, &[("parts", parts.join(", "))])
        };
        let height = 12.0 + 36.0 + 12.0;
        let band = rect(c.left, c.y, c.width, if detail.is_empty() { 12.0 + 20.0 + 12.0 } else { height });
        if self.hovered == Some(PrHit::ChecksToggle) {
            paint.fill(cx, band, colors.sidebar_hover);
        }
        let center_y = band.pos.y + band.size.y / 2.0;
        // Original `ChecksRing`.
        let ring = rect(c.left + 12.0, center_y - 10.0, 20.0, 20.0);
        paint.arc(cx, ring, colors.surface3, 0.0, 1.0, 3.0);
        let total = checks.len() as f32;
        if total > 0.0 {
            let circumference = 2.0 * std::f32::consts::PI * 8.5;
            let mut offset = 0.0f32;
            for (presentation, matching) in &groups {
                let color = if *presentation == Check::Ignored { colors.extra_muted } else { check_color(*presentation) };
                let share = matching.len() as f32 / total;
                let dash = if groups.len() == 1 { 1.0 } else { (share - 2.0 / circumference).max(1.5 / circumference) };
                paint.arc(cx, ring, color, offset, offset + dash, 3.0);
                offset += share;
            }
        }
        let text_left = c.left + 12.0 + 20.0 + 8.0;
        let text_width = c.width - 12.0 - 20.0 - 8.0 - 8.0 - 14.0 - 12.0;
        if detail.is_empty() {
            paint.text_centered(cx, Font::Medium, ui_px(14.0), colors.foreground, text_left, band.pos.y, band.size.y, &headline, text_width);
        } else {
            paint.text_fit(cx, Font::Medium, ui_px(14.0), colors.foreground, dvec2(text_left, band.pos.y + 12.0), &headline, text_width);
            paint.text_fit(cx, Font::Regular, ui_px(12.0), colors.muted, dvec2(text_left, band.pos.y + 12.0 + 20.0), &detail, text_width);
        }
        paint.icon(cx, if self.pane.checks_open { "chevron-down" } else { "chevron-right" }, 14.0, colors.muted, dvec2(c.left + c.width - 12.0 - 7.0, center_y));
        self.hits.push_clipped(band, c.view, PrHit::ChecksToggle);
        c.y += band.size.y;
        if !self.pane.checks_open || groups.is_empty() {
            return;
        }
        // Roughly eight rows; past this the list scrolls in place.
        let mut inner = 0.0;
        for (presentation, matching) in &groups {
            inner += 8.0 + 16.0 + 4.0;
            if !self.pane.collapsed_groups.contains(presentation) {
                inner += matching.len() as f64 * 32.0;
            }
        }
        inner += 8.0;
        let list_height = inner.min(268.0);
        let list = rect(c.left, c.y, c.width, list_height);
        let clip = crate::common::paint::intersect(list, c.view);
        self.checks_area = clip;
        self.checks_content = inner;
        self.checks_scroll = self.checks_scroll.clamp(0.0, (inner - list_height).max(0.0));
        paint.begin_clip(cx, clip);
        let mut y = c.y - self.checks_scroll;
        let fallback = text(pr, "url").to_owned();
        for (presentation, matching) in &groups {
            let collapsed = self.pane.collapsed_groups.contains(presentation);
            let header = rect(c.left, y, c.width, 8.0 + 16.0 + 4.0);
            let key = format!("{COPY}.{}.{}", if matching.len() == 1 { "groupOne" } else { "groupMany" }, presentation.name());
            let copy = tr(&key, &[("count", matching.len().to_string())]);
            let width = paint.text_centered(cx, Font::Medium, ui_px(12.0), colors.muted, c.left + 12.0, y + 8.0, 16.0, &copy, c.width - 40.0);
            paint.icon(cx, if collapsed { "chevron-right" } else { "chevron-down" }, 12.0, colors.muted, dvec2(c.left + 12.0 + width + 4.0 + 6.0, y + 16.0));
            self.hits.push_clipped(header, clip, PrHit::Group(*presentation));
            y += header.size.y;
            if collapsed {
                continue;
            }
            for check in matching {
                if y + 32.0 >= clip.pos.y && y <= clip.pos.y + clip.size.y {
                    let url = Some(text(check, "url")).filter(|u| !u.is_empty()).unwrap_or(&fallback).to_owned();
                    self.check_row(cx, paint, c.left, y, c.width, 32.0, 12.0, classify_check(check), text(check, "name"), text(check, "workflow"), check_timing(check), Some(url), clip);
                }
                y += 32.0;
            }
        }
        paint.end_clip(cx);
        c.y += list_height;
    }

    /// Original `_GitlabPipelineSection`.
    fn pipeline_section(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, c: &mut Cursor, facts: &Value) {
        let colors = palette();
        let raw_status = text(facts, "pipelineStatus").to_owned();
        let url = text(facts, "pipelineUrl").to_owned();
        let pipeline = self.pane.pipeline.clone();
        let stages: Vec<Value> = pipeline.as_ref().map(|p| array(p, "stages").to_vec()).unwrap_or_default();
        let mut counts: Vec<(Check, usize)> = Vec::new();
        for job in stages.iter().flat_map(|s| array(s, "jobs").iter()) {
            let presentation = classify_gitlab_job(text(job, "status"), flag(job, "allowFailure"));
            if presentation == Check::Ignored {
                continue;
            }
            match counts.iter_mut().find(|(c, _)| *c == presentation) {
                Some((_, n)) => *n += 1,
                None => counts.push((presentation, 1)),
            }
        }
        let summary: Vec<(usize, Option<Check>, Vec4)> = Check::COUNTED
            .iter()
            .map(|p| (counts.iter().find(|(c, _)| c == p).map(|(_, n)| *n).unwrap_or(0), Some(*p), check_color(*p)))
            .collect();
        let open = self.pane.checks_open;
        let header_top = c.y;
        self.section_header(cx, paint, c, &t("workspace.git.pr.sections.pipeline"), open, PrHit::ChecksToggle, &summary);
        if counts.is_empty() {
            let status = classify_gitlab_job(&raw_status, false);
            check_icon(paint, cx, status, 14.0, dvec2(c.left + c.width - 12.0 - 7.0, header_top + 16.0));
        }
        if !open {
            return;
        }
        let pipeline_name = format!("Pipeline #{}", int(facts, "pipelineId"));
        let launch = Some(url.clone()).filter(|u| !u.is_empty());
        let trailing = None;
        self.check_row(cx, paint, c.left, c.y, c.width, 32.0, 12.0, classify_gitlab_job(&raw_status, false), &pipeline_name, &raw_status, trailing, launch.clone(), c.view);
        if launch.is_some() {
            paint.icon(cx, "external-link", 12.0, colors.muted, dvec2(c.left + c.width - 12.0 - 6.0, c.y + 16.0));
        }
        c.y += 32.0;
        if self.pane.pipeline_loading {
            self.empty_text(cx, paint, c, &t("workspace.git.pr.empty.loadingPipeline"));
        } else if pipeline.is_some() && stages.is_empty() {
            self.empty_text(cx, paint, c, &t("workspace.git.pr.empty.noJobs"));
        } else if pipeline.is_none() && self.pane.pipeline_error.is_some() {
            self.empty_text(cx, paint, c, &t("workspace.git.pr.empty.pipelineJobsLoadFailed"));
        }
        if !self.pane.pipeline_loading {
            for stage in &stages {
                let center_y = c.y + 8.0 + 8.0;
                check_icon(paint, cx, classify_gitlab_job(text(stage, "status"), false), 14.0, dvec2(c.left + 12.0 + 7.0, center_y));
                paint.text_centered(cx, Font::Medium, ui_px(12.0), colors.muted, c.left + 12.0 + 22.0, c.y + 8.0, 16.0, &text(stage, "name").to_uppercase(), c.width - 46.0);
                c.y += 8.0 + 16.0 + 4.0;
                for job in array(stage, "jobs") {
                    let allow = flag(job, "allowFailure");
                    let detail = if allow { t("workspace.git.pr.empty.allowedToFail") } else { String::new() };
                    let duration = int(job, "durationSeconds");
                    let trailing = (duration > 0).then(|| format_duration(duration));
                    let job_url = Some(text(job, "url").to_owned()).filter(|u| !u.is_empty());
                    self.check_row(cx, paint, c.left, c.y, c.width, 36.0, 24.0, classify_gitlab_job(text(job, "status"), allow), text(job, "name"), &detail, trailing, job_url, c.view);
                    c.y += 36.0;
                }
            }
        }
        c.y += 12.0;
    }

    /// Original `_Avatar`: a round `avatarUrl` image (cover) over the author's
    /// identity colour; the white 10px initial only without a URL or when the
    /// image fails. Images load through the shared timeline image cache.
    fn avatar(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, activity: &Value, size: f64, center: DVec2) {
        let author = text(activity, "author");
        let fill = rgb(AVATAR_FILLS[avatar_fill_index(author)]);
        let circle = rect(center.x - size / 2.0, center.y - size / 2.0, size, size);
        paint.rounded(cx, circle, fill, size / 2.0, None);
        let url = text(activity, "avatarUrl");
        if !url.is_empty() {
            match images::image(cx, None, &self.target.server_id, None, url, "") {
                ImageState::Ready { texture, .. } => {
                    paint.texture(cx, circle, &texture, size / 2.0);
                    return;
                }
                ImageState::Loading => {
                    self.images_pending = true;
                    return;
                }
                ImageState::Failed => {}
            }
        }
        let initial: String = author.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        let px = ui_px(10.0);
        let width = paint.measure(cx, Font::Regular, px, &initial).x;
        paint.text_centered(cx, Font::Regular, px, vec4(1.0, 1.0, 1.0, 1.0), center.x - width / 2.0, center.y - size / 2.0, size, &initial, width + 1.0);
    }

    /// Original `_ActivityHeader` in a row of `height` from `top`.
    #[allow(clippy::too_many_arguments)]
    fn activity_header(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, activity: &Value, avatar: f64, x: f64, top: f64, width: f64, height: f64, trailing_extra: Option<usize>, kebab: Option<(String, bool)>, clip: Rect) {
        let colors = palette();
        let center_y = top + height / 2.0;
        self.avatar(cx, paint, activity, avatar, dvec2(x + avatar / 2.0, center_y));
        let mut cursor = x + avatar + 8.0;
        let right_edge = x + width;
        // Trailing: kebab, count, age (right to left).
        let mut right = right_edge;
        if let Some((key, thread)) = kebab {
            let visible = self.hovered_region.as_deref() == Some(key.as_str()) || crate::common::is_compact_layout(cx) || host::is_mobile_platform();
            let slot = rect(right - 22.0, center_y - 9.0, 22.0, 18.0);
            if visible {
                let hit = PrHit::Kebab {
                    key: key.clone(),
                    body: text(activity, "body").to_owned(),
                    url: text(activity, "url").to_owned(),
                    thread,
                };
                let hovered = self.hovered.as_ref() == Some(&hit);
                if hovered {
                    paint.rounded(cx, slot, colors.surface2, 4.0, None);
                }
                paint.icon(cx, "ellipsis", 14.0, if hovered { colors.foreground } else { colors.muted }, slot.center());
                self.hits.push_clipped(slot, clip, hit);
            }
            right -= 22.0 + 8.0;
        }
        if let Some(count) = trailing_extra {
            let label = count.to_string();
            let w = paint.measure(cx, Font::Regular, ui_px(12.0), &label).x;
            paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, right - w, top, height, &label, w + 1.0);
            paint.icon(cx, "message-square", 11.0, colors.muted, dvec2(right - w - 3.0 - 6.0, center_y));
            right -= w + 3.0 + 12.0 + 8.0;
        }
        let age = format_age(int(activity, "createdAt"), now_ms());
        let age_width = paint.measure(cx, Font::Regular, ui_px(12.0), &age).x;
        paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, right - age_width, top, height, &age, age_width + 1.0);
        right -= age_width + 8.0;
        let review = text(activity, "kind") == "review";
        let review_state = text(activity, "reviewState");
        let verb_key = if !review {
            "commented"
        } else {
            match review_state {
                "approved" => "approved",
                "changes_requested" => "requestedChanges",
                _ => "reviewed",
            }
        };
        let verb = t(&format!("workspace.git.pr.activity.{verb_key}")).to_lowercase();
        let (verb_icon, verb_color) = match (review, review_state) {
            (true, "approved") => (Some("circle-check"), colors.success),
            (true, "changes_requested") => (Some("circle-x"), colors.danger),
            _ => (None, colors.muted),
        };
        let verb_width = paint.measure(cx, Font::Regular, ui_px(12.0), &verb).x + if verb_icon.is_some() { 16.0 } else { 0.0 };
        let author_width = (right - cursor - 8.0 - verb_width).max(0.0);
        cursor += paint.text_centered(cx, Font::Regular, ui_px(14.0), colors.foreground, cursor, top, height, text(activity, "author"), author_width) + 8.0;
        if let Some(icon) = verb_icon {
            paint.icon(cx, icon, 12.0, verb_color, dvec2(cursor + 6.0, center_y));
            cursor += 16.0;
        }
        paint.text_centered(cx, Font::Regular, ui_px(12.0), verb_color, cursor, top, height, &verb, (right - cursor).max(0.0));
    }

    /// Original `_markdownBody`: `PaseoMarkdown(data: body, compact: true,
    /// onTapLink: _launch)` at `pos`, through the timeline's native Markdown
    /// painter (palette colours, selection, link taps, code copy). Returns its
    /// height; offscreen bodies reuse their last height without drawing.
    #[allow(clippy::too_many_arguments)]
    fn markdown_body(&mut self, cx: &mut Cx2d, kit: &mut ChatKit, key: &str, body: &str, pos: DVec2, width: f64, clip: Rect) -> f64 {
        let cached = self.heights.get(key).copied();
        let estimate = cached.unwrap_or(40.0);
        let visible = pos.y + estimate >= clip.pos.y && pos.y <= clip.pos.y + clip.size.y;
        if !visible && cached.is_some() {
            return estimate;
        }
        let Some(look) = self.look else { return estimate };
        let width = width.max(1.0);
        let entry = self.bodies.entry(key.to_owned()).or_default();
        entry.canvas.begin(pos);
        kit.begin_layer(cx);
        let mut opts = MdOpts {
            font_size: look.ui(14.0),
            key,
            find: None,
            core: None,
            host_id: &self.target.server_id,
            workspace_root: None,
            syntax_theme: look.syntax.as_str(),
            counter: 0,
        };
        let mut d = Draw {
            kit,
            canvas: &mut entry.canvas,
            look,
            state: &mut entry.state,
            now: now_seconds(),
            area: Area::Empty,
            animating: false,
        };
        let height = md::draw(cx, &mut d, &mut opts, body, pos.x, pos.y, width, false, false);
        // Copy-glyph feedback and loading images animate through frames.
        self.spinning |= d.animating;
        // Material's default selection colour: colorScheme.primary at 40%.
        let selection = with_alpha(look.palette.primary, 0.4);
        for r in entry.canvas.selection_rects() {
            kit.rect(cx, r, selection, 0.0);
        }
        self.body_rects.push((key.to_owned(), rect(pos.x, pos.y, width, height)));
        self.images_pending = true;
        self.heights.insert(key.to_owned(), height);
        height
    }

    fn card_border(cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, radius: f64, fill: Vec4) {
        paint.rounded(cx, area, fill, radius, Some((palette().border, 1.0)));
    }

    /// Original `_TimelineEntryCard`.
    fn entry(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, c: &mut Cursor, entry: &Entry, number: Option<i64>, kit: &mut ChatKit) {
        let colors = palette();
        let collapsed = self.pane.entry_collapsed(number, entry);
        let id = entry.id();
        let x = c.left + 12.0;
        let width = c.width - 24.0;
        match entry {
            Entry::Single(activity) => {
                let has_body = !text(activity, "body").trim().is_empty();
                if !has_body {
                    // Original `eventRow`: an unbordered header row, 8px below.
                    let band = rect(x, c.y, width, 36.0);
                    let hit = PrHit::Launch(text(activity, "url").to_owned());
                    if self.hovered.as_ref() == Some(&hit) {
                        paint.fill(cx, band, colors.sidebar_hover);
                    }
                    self.hits.push_clipped(band, c.view, hit);
                    self.regions.push((band, id.clone()));
                    self.activity_header(cx, paint, activity, 20.0, x + 12.0, c.y, width - 24.0, 36.0, None, Some((id, false)), c.view);
                    c.y += 36.0 + 8.0;
                    return;
                }
                let top = c.y;
                let body_height = if collapsed {
                    0.0
                } else {
                    self.heights.get(&format!("{id}:body")).copied().unwrap_or(40.0) + 8.0 + 12.0
                };
                let card = rect(x, top, width, 36.0 + body_height);
                Self::card_border(cx, paint, card, 8.0, colors.sidebar);
                let header = rect(x, top, width, 36.0);
                let hit = PrHit::Entry { id: id.clone(), default_collapsed: entry.collapsed_by_default() };
                if self.hovered.as_ref() == Some(&hit) {
                    paint.rounded(cx, header, colors.sidebar_hover, 8.0, None);
                }
                self.hits.push_clipped(header, c.view, hit);
                self.regions.push((card, id.clone()));
                self.activity_header(cx, paint, activity, 20.0, x + 12.0, top, width - 24.0, 36.0, None, Some((id.clone(), false)), c.view);
                let mut y = top + 36.0;
                if !collapsed {
                    let height = self.markdown_body(cx, kit, &format!("{id}:body"), text(activity, "body"), dvec2(x + 12.0, y + 8.0), width - 24.0, c.view);
                    y += 8.0 + height + 12.0;
                }
                c.y = y + 12.0;
            }
            Entry::Thread(thread) => {
                let top = c.y;
                let start = self.markdown_estimate(thread, collapsed);
                let card = rect(x, top, width, start);
                Self::card_border(cx, paint, card, 8.0, colors.sidebar);
                let end = self.thread_block(cx, paint, thread, x, top, width, number, c.view, kit);
                c.y = end + 12.0;
            }
            Entry::Review { review, threads } => {
                let top = c.y;
                let estimate = self.heights.get(&format!("{id}:card")).copied().unwrap_or(36.0);
                let card = rect(x, top, width, estimate);
                Self::card_border(cx, paint, card, 8.0, colors.sidebar);
                let header = rect(x, top, width, 36.0);
                let hit = PrHit::Entry { id: id.clone(), default_collapsed: false };
                if self.hovered.as_ref() == Some(&hit) {
                    paint.rounded(cx, header, colors.sidebar_hover, 8.0, None);
                }
                self.hits.push_clipped(header, c.view, hit);
                self.regions.push((card, id.clone()));
                let count = collapsed.then_some(threads.len());
                self.activity_header(cx, paint, review, 20.0, x + 12.0, top, width - 24.0, 36.0, count, Some((id.clone(), false)), c.view);
                let mut y = top + 36.0;
                if !collapsed {
                    if !text(review, "body").trim().is_empty() {
                        let height = self.markdown_body(cx, kit, &format!("{id}:body"), text(review, "body"), dvec2(x + 12.0, y + 8.0), width - 24.0, c.view);
                        y += 8.0 + height + 12.0;
                    }
                    if !threads.is_empty() {
                        y += 4.0;
                        for thread in threads {
                            let inner_x = x + 12.0;
                            let inner_width = width - 24.0;
                            let thread_top = y;
                            let estimate = self.markdown_estimate(thread, self.pane.is_collapsed(number, &thread_id(thread), thread_collapsed_by_default(thread)));
                            Self::card_border(cx, paint, rect(inner_x, thread_top, inner_width, estimate), 6.0, colors.sidebar);
                            y = self.thread_block(cx, paint, thread, inner_x, thread_top, inner_width, number, c.view, kit) + 8.0;
                        }
                        y += 4.0;
                    }
                }
                self.heights.insert(format!("{id}:card"), y - top);
                c.y = y + 12.0;
            }
        }
    }

    /// The last drawn height of a thread block (for its border before drawing).
    fn markdown_estimate(&self, thread: &Thread, collapsed: bool) -> f64 {
        let key = format!("{}:block:{collapsed}", thread_id(thread));
        self.heights.get(&key).copied().unwrap_or(36.0)
    }

    /// Original `_ThreadBlock`; returns its bottom.
    #[allow(clippy::too_many_arguments)]
    fn thread_block(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, thread: &Thread, x: f64, top: f64, width: f64, number: Option<i64>, clip: Rect, kit: &mut ChatKit) -> f64 {
        let colors = palette();
        let id = thread_id(thread);
        let collapsed = self.pane.is_collapsed(number, &id, thread_collapsed_by_default(thread));
        let header = rect(x + 1.0, top + 1.0, width - 2.0, 36.0);
        let hit = PrHit::Entry { id: id.clone(), default_collapsed: thread_collapsed_by_default(thread) };
        let hovered = self.hovered.as_ref() == Some(&hit);
        paint.fill(cx, header, if hovered { colors.sidebar_hover } else { colors.surface1 });
        self.hits.push_clipped(header, clip, hit);
        let center_y = header.pos.y + 18.0;
        let mut right = header.pos.x + header.size.x - 12.0;
        let root = &thread.comments[0];
        let kebab_visible = self.hovered_region.as_deref() == Some(id.as_str()) || crate::common::is_compact_layout(cx) || host::is_mobile_platform();
        let slot = rect(right - 22.0, center_y - 9.0, 22.0, 18.0);
        if kebab_visible {
            let kebab = PrHit::Kebab { key: id.clone(), body: String::new(), url: text(root, "url").to_owned(), thread: true };
            if self.hovered.as_ref() == Some(&kebab) {
                paint.rounded(cx, slot, colors.surface2, 4.0, None);
            }
            paint.icon(cx, "ellipsis", 14.0, colors.muted, slot.center());
            self.hits.push_clipped(slot, clip, kebab);
        }
        right -= 22.0 + 8.0;
        if collapsed {
            let label = thread.comments.len().to_string();
            let w = paint.measure(cx, Font::Regular, ui_px(12.0), &label).x;
            paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, right - w, header.pos.y, 36.0, &label, w + 1.0);
            paint.icon(cx, "message-square", 11.0, colors.muted, dvec2(right - w - 9.0, center_y));
            right -= w + 3.0 + 12.0 + 8.0;
        }
        let mut cursor = header.pos.x + 12.0;
        let path_label = if text(&thread.location, "path").is_empty() {
            t("workspace.git.pr.thread.discussion")
        } else {
            thread_path(&thread.location)
        };
        let mut badges: Vec<(String, Vec4)> = Vec::new();
        if thread.is_resolved == Some(true) {
            badges.push((t("workspace.git.pr.thread.resolved"), colors.success));
        }
        if flag(&thread.location, "isOutdated") {
            badges.push((t("workspace.git.pr.thread.outdated"), colors.muted));
        }
        let badge_width: f64 = badges.iter().map(|(l, _)| paint.measure(cx, Font::Regular, ui_px(11.0), l).x + 12.0 + 8.0).sum();
        cursor += paint.text_centered(cx, Font::Mono, ui_px(12.0), colors.foreground, cursor, header.pos.y, 36.0, &path_label, (right - cursor - badge_width).max(0.0)) + 8.0;
        for (label, color) in badges {
            let w = paint.measure(cx, Font::Regular, ui_px(11.0), &label).x + 12.0;
            let pill = rect(cursor, center_y - 9.0, w, 18.0);
            paint.rounded(cx, pill, with_alpha(color, 0.15), 9.0, None);
            paint.text_centered(cx, Font::Regular, ui_px(11.0), color, cursor + 6.0, pill.pos.y, 18.0, &label, w);
            cursor += w + 8.0;
        }
        self.regions.push((rect(x, top, width, self.markdown_estimate(thread, collapsed)), id.clone()));
        let mut y = top + 37.0;
        if !collapsed {
            y = self.thread_comment(cx, paint, root, x, y, width, true, clip, kit);
            let replies = &thread.comments[1..];
            if !replies.is_empty() {
                y += 8.0;
                for reply in replies {
                    let key = format!("{}:reply", text(reply, "id"));
                    let estimate = self.heights.get(&key).copied().unwrap_or(40.0);
                    let box_area = rect(x + 12.0, y, width - 24.0, estimate);
                    paint.rounded(cx, box_area, colors.surface1, 6.0, Some((colors.border, 1.0)));
                    let end = self.thread_comment(cx, paint, reply, x + 12.0, y, width - 24.0, false, clip, kit);
                    self.heights.insert(key, end - y);
                    y = end + 8.0;
                }
            }
        }
        self.heights.insert(format!("{id}:block:{collapsed}"), y - top);
        y
    }

    /// Original `_ThreadComment`; returns its bottom.
    #[allow(clippy::too_many_arguments)]
    fn thread_comment(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, comment: &Value, x: f64, top: f64, width: f64, top_border: bool, clip: Rect, kit: &mut ChatKit) -> f64 {
        let colors = palette();
        if top_border {
            paint.fill(cx, rect(x, top, width, 1.0), colors.border);
        }
        let id = text(comment, "id").to_owned();
        let region = rect(x, top, width, self.heights.get(&format!("{id}:comment")).copied().unwrap_or(40.0));
        self.regions.push((region, id.clone()));
        self.activity_header(cx, paint, comment, 16.0, x + 12.0, top + 8.0, width - 24.0, 24.0, None, Some((id.clone(), false)), clip);
        let mut y = top + 8.0 + 24.0 + 4.0;
        if !text(comment, "body").trim().is_empty() {
            y += 8.0;
            y += self.markdown_body(cx, kit, &format!("{id}:body"), text(comment, "body"), dvec2(x + 12.0, y), width - 24.0, clip);
        }
        y += 8.0;
        self.heights.insert(format!("{id}:comment"), y - top);
        y
    }
}
