//! Chat chrome around the stream (stream_states.dart, chrome_controls.dart):
//! non-ready views, the history sync overlay, the sync-error and preparation
//! callouts, the archived callout, the archiving overlay, the empty state, the
//! jump-to-latest button, the panel toast viewport, the pane subtitle and the
//! error alert. Every control registers a `HitTarget::Chrome(id)`.

use super::canvas::*;
use super::i18n::{t, ta};
use super::paint::{Draw, now_seconds};
use makepad_widgets::*;

fn base(d: &Draw) -> f64 {
    d.look.ui(14.0)
}

/// ChromeSpinner (large = 24, default = 16).
pub fn spinner(cx: &mut Cx2d, d: &mut Draw, center: DVec2, size: f64, color: Vec4) {
    d.kit.spinner(cx, rect_xywh(center.x - size * 0.5, center.y - size * 0.5, size, size), color, now_seconds() % 1.0);
    d.animating = true;
}

/// PaseoButton secondary/sm; returns its size.
pub fn small_button(cx: &mut Cx2d, d: &mut Draw, x: f64, y: f64, label: &str, enabled: bool, id: &str) -> DVec2 {
    let p = d.look.palette;
    let size = d.look.ui(14.0);
    let text = d.kit.measure(cx, Font::Medium, size, 1.3, label, None);
    let w = text.x + 24.0;
    let h = 32.0;
    let hovered = d.state.hover_hit.as_deref() == Some(id);
    let alpha = if enabled { 1.0 } else { 0.5 };
    d.kit.bordered(cx, rect_xywh(x, y, w, h), with_alpha(if hovered && enabled { p.surface3 } else { p.surface2 }, alpha), 6.0, with_alpha(p.border_accent, alpha), 1.0);
    d.kit.label(cx, Font::Medium, size, 1.3, with_alpha(p.foreground, alpha), dvec2(x + 12.0, y + (h - text.y) * 0.5), label, None);
    if enabled {
        d.canvas.add_hit(rect_xywh(x, y, w, h), HitTarget::Chrome(id.to_owned()));
    }
    dvec2(w, h)
}

pub fn small_button_width(cx: &mut Cx2d, d: &mut Draw, label: &str) -> f64 {
    d.kit.measure(cx, Font::Medium, d.look.ui(14.0), 1.3, label, None).x + 24.0
}

fn centered_lines(cx: &mut Cx2d, d: &mut Draw, lines: &[(String, Font, f64, Vec4)], rect: Rect, gap: f64, extra: f64) -> f64 {
    let w = (rect.size.x - 48.0).max(1.0);
    let mut total = extra;
    let mut laid = Vec::new();
    for (text, font, size, color) in lines {
        let l = d.kit.layout(cx, *font, *size, 1.4, text, Some(w), 0.0);
        total += laidout_size(&l).y;
        laid.push((l, *font, *color));
    }
    total += gap * lines.len().saturating_sub(1) as f64;
    let mut y = rect.pos.y + (rect.size.y - total) * 0.5 + extra;
    for (l, font, color) in laid {
        let size = laidout_size(&l);
        let pos = dvec2(rect.pos.x + (rect.size.x - size.x) * 0.5, y);
        d.kit.draw_laidout(cx, font, color, pos, &l);
        y += size.y + gap;
    }
    rect.pos.y + (rect.size.y - total) * 0.5
}

/// The whole panel before the agent resolves (renderChatAgentNonReadyView).
pub enum NonReady {
    Loading,
    NotFound,
    LoadError(String),
    Unavailable { label: String, connecting: bool, error: Option<String> },
}

pub fn non_ready(cx: &mut Cx2d, d: &mut Draw, view: &NonReady, rect: Rect) {
    let p = d.look.palette;
    d.kit.rect(cx, rect, p.surface0, 0.0);
    let size = base(d);
    match view {
        NonReady::Loading => spinner(cx, d, rect.center(), 24.0, p.muted),
        NonReady::NotFound => {
            centered_lines(cx, d, &[(t("agentPanel.states.notFound"), Font::Regular, size, p.muted)], rect, 0.0, 0.0);
        }
        NonReady::LoadError(message) => {
            let retry = t("common.actions.retry");
            let lines = [(t("agentPanel.states.failedToLoad"), Font::Regular, size, p.muted), (message.clone(), Font::Regular, size, p.muted)];
            let top = centered_lines(cx, d, &lines, Rect { size: dvec2(rect.size.x, rect.size.y - 48.0), ..rect }, 8.0, 0.0);
            let mut h = 0.0;
            for (text, font, size, _) in &lines {
                h += d.kit.measure(cx, *font, *size, 1.4, text, Some(rect.size.x - 48.0)).y + 8.0;
            }
            let bw = small_button_width(cx, d, &retry);
            small_button(cx, d, rect.center().x - bw * 0.5, top + h + 8.0 + 24.0, &retry, true, "retry-sync");
        }
        NonReady::Unavailable { label, connecting, error } => {
            if *connecting {
                let lines = [
                    (ta("agentPanel.unavailable.connecting", &[("serverLabel", label)]), Font::Regular, size, p.muted),
                    (t("agentPanel.unavailable.showWhenOnline"), Font::Regular, size, p.muted),
                ];
                let top = centered_lines(cx, d, &lines, rect, 12.0, 36.0);
                spinner(cx, d, dvec2(rect.center().x, top + 12.0), 24.0, p.muted);
            } else {
                let mut lines = vec![
                    (ta("agentPanel.unavailable.reconnectingTo", &[("serverLabel", label)]), Font::Semibold, size, p.foreground),
                    (t("agentPanel.unavailable.showAgainWhenReachable"), Font::Regular, size, p.muted),
                ];
                if let Some(error) = error.as_ref().filter(|e| !e.is_empty()) {
                    lines.push((error.clone(), Font::Regular, d.look.ui(12.0), p.muted));
                }
                centered_lines(cx, d, &lines, rect, 12.0, 0.0);
            }
        }
    }
}

/// historySyncOverlay: surface0 over the dock with the large spinner.
pub fn history_overlay(cx: &mut Cx2d, d: &mut Draw, rect: Rect) {
    d.kit.rect(cx, rect, d.look.palette.surface0, 0.0);
    spinner(cx, d, rect.center(), 24.0, d.look.palette.muted);
}

/// _StreamCallout rows under the stream; returns the height (incl. 8 top).
pub enum Callout {
    SyncError { retrying: bool },
    Preparation { subagent: bool, failed: bool, error: Option<String>, retrying: bool },
}

/// A callout's trailing button: label, enabled, chrome hit id.
type CalloutButton = (String, bool, &'static str);

pub fn callout(cx: &mut Cx2d, d: &mut Draw, callout: &Callout, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let size = base(d);
    let outer = (w - 32.0).min(d.look.typo.content_max_width);
    let bx = x + (w - outer) * 0.5;
    let by = y + 8.0;
    let (border, message, detail, spin, button): (Vec4, String, Option<String>, bool, Option<CalloutButton>) = match callout {
        Callout::SyncError { retrying } => (
            p.danger,
            t("agentPanel.states.timelineSyncFailed"),
            None,
            false,
            Some((t(if *retrying { "agentPanel.states.timelineSyncRetrying" } else { "common.actions.retry" }), !*retrying, "retry-sync")),
        ),
        Callout::Preparation { subagent, failed, error, retrying } => {
            let key = match (*subagent, *failed) {
                (false, false) => "agentPanel.states.preparingAgent",
                (false, true) => "agentPanel.states.preparationFailed",
                (true, false) => "agentPanel.states.subagentRefreshing",
                (true, true) => "agentPanel.states.subagentRefreshFailed",
            };
            let detail = error.as_ref().map(|e| e.trim().to_owned()).filter(|e| *failed && !e.is_empty());
            let button = failed.then(|| (t(if *retrying { "agentPanel.states.timelineSyncRetrying" } else { "common.actions.retry" }), !*retrying, if *subagent { "retry-child" } else { "retry-preparation" }));
            (if *failed { p.danger } else { p.border_accent }, t(key), detail, !*failed, button)
        }
    };
    let button_w = button.as_ref().map(|(label, _, _)| small_button_width(cx, d, label) + 12.0).unwrap_or(0.0);
    let spin_w = if spin { 28.0 } else { 0.0 };
    let text_w = (outer - 32.0 - button_w - spin_w).max(40.0);
    let msg = d.kit.layout(cx, Font::Regular, size, 1.4, &message, Some(text_w), 0.0);
    let det = detail.as_ref().map(|e| d.kit.layout(cx, Font::Regular, size, 1.4, e, Some(text_w), 0.0));
    let text_h = laidout_size(&msg).y + det.as_ref().map(|l| laidout_size(l).y).unwrap_or(0.0);
    let inner_h = text_h.max(if button.is_some() { 32.0 } else { 16.0 });
    let h = inner_h + 16.0;
    d.kit.bordered(cx, rect_xywh(bx, by, outer, h), p.surface1, 16.0, border, 1.0);
    let content_w = spin_w + laidout_size(&msg).x.max(det.as_ref().map(|l| laidout_size(l).x).unwrap_or(0.0)) + button_w;
    let mut cxp = bx + (outer - content_w).max(32.0) * 0.5;
    if spin {
        spinner(cx, d, dvec2(cxp + 8.0, by + h * 0.5), 16.0, p.muted);
        cxp += spin_w;
    }
    let ty = by + (h - text_h) * 0.5;
    d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(cxp, ty), &msg);
    let text_x = cxp;
    cxp += laidout_size(&msg).x.max(det.as_ref().map(|l| laidout_size(l).x).unwrap_or(0.0)) + 12.0;
    if let Some(det) = det {
        let pos = dvec2(text_x, ty + laidout_size(&msg).y);
        d.kit.draw_laidout(cx, Font::Regular, p.danger, pos, &det);
        d.canvas.add_run(det, pos, Some('\n'));
    }
    if let Some((label, enabled, id)) = button {
        small_button(cx, d, cxp, by + (h - 32.0) * 0.5, &label, enabled, id);
    }
    h + 8.0
}

/// ArchivedAgentCallout in place of the composer; returns its height.
#[allow(clippy::too_many_arguments)]
pub fn archived_callout(cx: &mut Cx2d, d: &mut Draw, connected: bool, unarchiving: bool, error: Option<&str>, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let size = base(d);
    let outer = (w - 32.0).min(d.look.typo.content_max_width);
    let bx = x + (w - outer) * 0.5;
    let (ph, pv) = if d.look.compact { (16.0, 12.0) } else { (24.0, 16.0) };
    let message = t("agentPanel.archived.callout");
    let label = t("agentPanel.archived.unarchive");
    let bw = small_button_width(cx, d, &label);
    let msg = d.kit.layout(cx, Font::Regular, size, 1.4, &message, Some((outer - 2.0 * ph - bw - 12.0).max(40.0)), 0.0);
    let inner_h = laidout_size(&msg).y.max(32.0);
    let card_h = inner_h + 2.0 * pv;
    let by = y + 16.0;
    d.kit.bordered(cx, rect_xywh(bx, by, outer, card_h), p.surface1, 16.0, p.border_accent, 1.0);
    let content_w = laidout_size(&msg).x + 12.0 + bw;
    let start = bx + (outer - content_w) * 0.5;
    d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(start, by + (card_h - laidout_size(&msg).y) * 0.5), &msg);
    small_button(cx, d, start + laidout_size(&msg).x + 12.0, by + (card_h - 32.0) * 0.5, &label, connected && !unarchiving, "unarchive");
    let mut h = 16.0 + card_h;
    if let Some(error) = error {
        let l = d.kit.layout(cx, Font::Regular, size, 1.4, error, Some(outer), 0.0);
        let s = laidout_size(&l);
        d.kit.draw_laidout(cx, Font::Regular, p.danger, dvec2(bx + (outer - s.x) * 0.5, y + h + 8.0), &l);
        h += 8.0 + s.y;
    }
    (h + 16.0).max(75.0)
}

/// archivingOverlay: an 86% near-black scrim with spinner, title, subtitle.
pub fn archiving_overlay(cx: &mut Cx2d, d: &mut Draw, rect: Rect) {
    let p = d.look.palette;
    d.kit.rect(cx, rect, rgba(0xdb080a0e), 0.0);
    let size = base(d);
    let lines = [
        (t("agentPanel.states.archivingTitle"), Font::Semibold, size, p.foreground),
        (t("agentPanel.states.archivingSubtitle"), Font::Regular, size, p.muted),
    ];
    let top = centered_lines(cx, d, &lines, rect, 12.0, 36.0);
    spinner(cx, d, dvec2(rect.center().x, top + 12.0), 24.0, p.foreground);
}

/// StreamEmptyState resting on the composer edge; returns its height.
pub fn empty_state(cx: &mut Cx2d, d: &mut Draw, x: f64, bottom: f64, w: f64) {
    let p = d.look.palette;
    let text = t("agentStream.empty");
    let l = d.kit.layout(cx, Font::Regular, base(d), 1.4, &text, Some((w - 2.0 * d.look.list_inset - 16.0).max(1.0)), 0.0);
    let s = laidout_size(&l);
    d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(x + (w - s.x) * 0.5, bottom - 48.0 - s.y), &l);
}

/// scrollToBottomButton: a 48px surface2 circle with a 24px ChevronDown.
pub fn jump_button(cx: &mut Cx2d, d: &mut Draw, center_x: f64, bottom: f64) {
    let p = d.look.palette;
    let r = rect_xywh(center_x - 24.0, bottom - 48.0, 48.0, 48.0);
    let shadow = if d.look.dark() { rgba(0x40000000) } else { rgba(0x14000000) };
    d.kit.rect(cx, r.translate(dvec2(0.0, 2.0)), shadow, 24.0);
    d.kit.rect(cx, r, p.surface2, 24.0);
    d.kit.icon(cx, "chevron-down", r, 24.0, p.foreground);
    d.canvas.add_hit(r, HitTarget::Chrome("jump-latest".into()));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastVariant {
    Normal,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub id: String,
    pub message: String,
    pub variant: ToastVariant,
    pub spinner: bool,
    /// Seconds; None keeps it until replaced.
    pub duration: Option<f64>,
}

/// PanelToastViewport: 12px under the pane top, 92% wide up to 480, surface0
/// card with a 1px border, 16px radius, 8/12 padding, an 18px icon.
pub fn toast(cx: &mut Cx2d, d: &mut Draw, toast: &Toast, rect: Rect) -> Rect {
    let p = d.look.palette;
    let max = ((rect.size.x - 32.0) * 0.92).clamp(0.0, 480.0);
    let size = base(d);
    let icon_w = if toast.spinner || toast.variant == ToastVariant::Error { 26.0 } else { 0.0 };
    let l = d.kit.layout(cx, Font::Regular, size, 1.4, &toast.message, Some((max - 24.0 - icon_w).max(1.0)), 0.0);
    let s = laidout_size(&l);
    let w = s.x + 24.0 + icon_w;
    let h = s.y.max(18.0) + 16.0;
    let r = rect_xywh(rect.pos.x + (rect.size.x - w) * 0.5, rect.pos.y + 12.0, w, h);
    let shadow = if d.look.dark() { rgba(0x33000000) } else { rgba(0x0a000000) };
    d.kit.rect(cx, r.translate(dvec2(0.0, 4.0)), shadow, 16.0);
    let border = if toast.variant == ToastVariant::Error { p.destructive } else { p.border };
    d.kit.bordered(cx, r, p.surface0, 16.0, border, 1.0);
    let mut tx = r.pos.x + 12.0;
    if toast.spinner {
        spinner(cx, d, dvec2(tx + 9.0, r.center().y), 18.0, p.foreground);
        tx += icon_w;
    } else if toast.variant == ToastVariant::Error {
        d.kit.icon(cx, "triangle-alert", rect_xywh(tx, r.pos.y, 18.0, h), 18.0, p.destructive);
        tx += icon_w;
    }
    let pos = dvec2(tx, r.pos.y + (h - s.y) * 0.5);
    d.kit.draw_laidout(cx, Font::Regular, p.foreground, pos, &l);
    d.canvas.add_run(l, pos, Some('\n'));
    d.canvas.add_hit(r, HitTarget::Chrome(format!("toast:{}", toast.id)));
    r
}

/// PaneSubtitle: one 12px muted line (12/4 padding) over a bottom border.
pub fn pane_subtitle(cx: &mut Cx2d, d: &mut Draw, text: &str, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let size = d.look.ui(12.0);
    let h = size * 1.4 + 8.0;
    cx.begin_turtle(Walk { abs_pos: Some(dvec2(x, y)), margin: Margin::default(), width: Size::Fixed(w), height: Size::Fixed(h) }, Layout::default());
    d.kit.label(cx, Font::Regular, size, 1.4, p.muted, dvec2(x + 12.0, y + 4.0), text, None);
    cx.end_turtle();
    d.kit.rect(cx, rect_xywh(x, y + h - 1.0, w, 1.0), p.border, 0.0);
    h
}

/// components/ui/alert.tsx error/sm: destructive@50% border, 16px radius,
/// 12/16 padding, 14px XCircle 12px from muted base text.
pub fn error_alert(cx: &mut Cx2d, d: &mut Draw, message: &str, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let l = d.kit.layout(cx, Font::Regular, base(d), 1.4, message, Some((w - 58.0).max(1.0)), 0.0);
    let s = laidout_size(&l);
    let h = s.y + 24.0;
    d.kit.bordered(cx, rect_xywh(x, y, w, h), TRANSPARENT, 16.0, with_alpha(p.destructive, 0.5), 1.0);
    d.kit.icon(cx, "circle-x", rect_xywh(x + 16.0, y, 14.0, h), 14.0, p.destructive);
    d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(x + 42.0, y + 12.0), &l);
    h
}
