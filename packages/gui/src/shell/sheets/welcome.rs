//! Port of `features/navigation/welcome_screen.dart` (original `components/welcome-screen.tsx`),
//! drawn in the content area while no host is saved: the Passession logo, onboarding title and
//! subtitle (plus the Paseo daemon docs link on mobile), the connection method buttons, a ghost Settings
//! button and the app version. Each method opens the matching connection flow; the screen
//! disappears by itself once a host is saved.

use std::cell::RefCell;
use std::collections::HashMap;

use makepad_widgets::scroll_bars::TouchScroll;
use makepad_widgets::*;

use crate::common::paint::{Font, rect};
use crate::common::{UiContext, tr, tr_args};
use paseo_gui_platform::desktop::LocalDaemon;
use crate::shell::chrome::make_ui;
use crate::shell::ui::Variant;
use crate::shell::{Page, PaseoShell, hosts, platform};
use crate::theme::{ControlSize, MOBILE};

const LOGO_SVG: &str = include_str!("../../../resources/icons/passession-logo.svg");
/// Width over height of the logo's path bounds (its viewBox). `DrawIcon` scales uniformly
/// and anchors the path at the rect's top-left, so the rect must share this aspect to centre.
const LOGO_ASPECT: f64 = 524.0 / 799.2;

struct Action {
    id: &'static str,
    label: String,
    icon: &'static str,
    primary: bool,
}

/// Mobile: Scan QR code (primary, when the camera scanner exists), Direct connection, Paste
/// pairing link. Desktop: Direct connection (primary), Remote SSH, Paste pairing link.
fn actions() -> Vec<Action> {
    let scan = MOBILE && paseo_gui_platform::platform().is_some_and(|h| h.capabilities().qr_scan);
    let desktop = platform::desktop_host() || !MOBILE;
    let mut actions = Vec::new();
    if scan {
        actions.push(Action { id: "scan", label: tr("pairing.connectionMethods.scanQr.title"), icon: "qr-code", primary: true });
    }
    actions.push(Action { id: "direct", label: tr("pairing.connectionMethods.direct.title"), icon: "link-2", primary: !scan });
    if desktop {
        actions.push(Action { id: "ssh", label: tr("pairing.connectionMethods.remoteSsh.title"), icon: "terminal", primary: false });
    }
    actions.push(Action { id: "link", label: tr("pairing.connectionMethods.pasteLink.title"), icon: "clipboard-paste", primary: false });
    actions
}

/// Original `formatVersionWithPrefix`.
fn version_text() -> String {
    let value = crate::app::app_version().trim();
    if value.is_empty() {
        "\u{2014}".to_string()
    } else if value.starts_with('v') {
        value.to_string()
    } else {
        format!("v{value}")
    }
}

/// Desktop: the built-in daemon's start, shown above the connection methods while it is
/// starting or after it failed (once it listens, its host appears and Welcome goes away).
fn local_daemon() -> Option<LocalDaemon> {
    if MOBILE || !platform::desktop_host() {
        return None;
    }
    match paseo_gui_platform::desktop::local_daemon() {
        state @ (LocalDaemon::Starting | LocalDaemon::Failed(_)) => Some(state),
        _ => None,
    }
}

/// The failure text, at most three lines of the daemon's message.
fn failure_text(reason: &str) -> String {
    let reason: String = reason.trim().lines().take(3).collect::<Vec<_>>().join("\n");
    tr_args("onboarding.daemon.failed", &[("reason", &reason)])
}

/// Per-window scroll of the welcome body: it scrolls only when it is taller than the area.
#[derive(Default)]
struct WelcomeScroll {
    scroll: f64,
    max: f64,
    viewport: Rect,
    touch: TouchScroll,
}

thread_local! {
    static SCROLL: RefCell<HashMap<String, WelcomeScroll>> = RefCell::new(HashMap::new());
}

/// Wheel and touch scrolling of the welcome body (shell window events). Only while the welcome
/// screen is what the content area shows and no shell modal covers it.
pub fn handle_event(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, state: &serde_json::Value) {
    let shown = hosts::hosts(state).is_empty()
        && !matches!(shell.data.page, Some(Page::Settings { .. }))
        && !shell.data.overlays.is_modal();
    let window = shell.data.window_id.clone();
    let hit = if shown && matches!(event, Event::TouchUpdate(_) | Event::LongPress(_)) { event.hits(cx, shell.data.bg_area) } else { Hit::Nothing };
    let moved = SCROLL.with(|scroll| {
        let mut scroll = scroll.borrow_mut();
        let Some(s) = scroll.get_mut(&window) else { return false };
        if !shown {
            s.touch.reset();
            return false;
        }
        let mut moved = false;
        if let Hit::FingerDown(e) = &hit
            && !s.viewport.contains(e.abs)
        {
            s.touch.reset();
        } else if let Some(dy) = s.touch.handle_event(cx, event, &hit) {
            moved = s.touch.apply_delta(&mut s.scroll, dy, s.max);
        }
        if let Event::Scroll(e) = event
            && s.viewport.contains(e.abs)
            && s.max > 0.0
        {
            s.touch.reset();
            s.scroll = (s.scroll + e.scroll.y).clamp(0.0, s.max);
            moved = true;
        }
        moved
    });
    if moved {
        // A drag that scrolled is not a tap on the button it started on.
        shell.data.pointer.pressed = None;
        shell.redraw(cx);
    }
}

pub fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, area: Rect) {
    let actions = actions();
    let daemon = local_daemon();
    let window = shell.data.window_id.clone();
    let mut ui = make_ui(shell);
    let p = ui.p;
    ui.fill(cx, area, p.surface0, 0.0);
    let inner = rect(area.pos.x + 24.0, area.pos.y + 24.0, (area.size.x - 48.0).max(0.0), (area.size.y - 48.0).max(0.0));

    let title = tr("onboarding.title");
    let subtitle = tr("onboarding.subtitle");
    let title_spec = ui.spec(Font::Medium, 14.0, p.foreground);
    let subtitle_spec = ui.spec(Font::Regular, 14.0, p.muted);
    let line = ui.painter.paragraph_height(cx, inner.size.x, subtitle_spec, "Ag");
    let title_h = ui.painter.paragraph_height(cx, inner.size.x, title_spec, &title);
    let subtitle_h = ui.painter.paragraph_height(cx, inner.size.x, subtitle_spec, &subtitle);
    let link_h = if MOBILE { line + 8.0 } else { 0.0 };
    let button_h = 16.0 + line + 16.0;
    let buttons_h = actions.len() as f64 * button_h + (actions.len().saturating_sub(1)) as f64 * 12.0;
    // The daemon card: 16px padding around its text, plus the Retry / View logs row on failure.
    let card_w = inner.size.x.min(420.0);
    let card_text_w = (card_w - 32.0 - 24.0).max(40.0);
    let daemon_text = match &daemon {
        Some(LocalDaemon::Starting) => tr("onboarding.daemon.starting"),
        Some(LocalDaemon::Failed(reason)) => failure_text(reason),
        _ => String::new(),
    };
    let daemon_spec = ui.spec(Font::Regular, 14.0, if matches!(daemon, Some(LocalDaemon::Failed(_))) { p.danger } else { p.foreground });
    let daemon_text_h = if daemon.is_some() { ui.painter.paragraph_height(cx, card_text_w, daemon_spec, &daemon_text).max(line) } else { 0.0 };
    let daemon_buttons_h = if matches!(daemon, Some(LocalDaemon::Failed(_))) { 12.0 + 32.0 } else { 0.0 };
    let daemon_h = if daemon.is_some() { 16.0 + daemon_text_h + daemon_buttons_h + 16.0 + 16.0 } else { 0.0 };
    let content_h = 96.0 + title_h + 8.0 + subtitle_h + link_h + 48.0 + daemon_h + buttons_h + 24.0 + 32.0;
    let version_spec = ui.spec(Font::Regular, 12.0, p.muted);
    let version_h = 24.0 + ui.painter.paragraph_height(cx, inner.size.x, version_spec, "Ag");
    let available = inner.size.y - version_h;
    // Centred while everything fits; otherwise the body scrolls inside the clipped area.
    let max = (content_h + version_h - inner.size.y).max(0.0);
    let scroll = SCROLL.with(|scroll| {
        let mut scroll = scroll.borrow_mut();
        let s = scroll.entry(window).or_default();
        s.viewport = area;
        s.max = max;
        s.scroll = s.scroll.clamp(0.0, max);
        s.scroll
    });
    let mut y = if max > 0.0 { inner.pos.y - scroll } else { inner.pos.y + ((available - content_h) * 0.5).max(0.0) };
    let center = inner.pos.x + inner.size.x * 0.5;
    cx.begin_turtle(Walk::abs_rect(area), Layout { clip_x: true, clip_y: true, ..Layout::default() });
    ui.hits.set_clip(Some(area));

    // Logo: the Passession mark in the foreground colour.
    if let Some(path) = crate::common::provider_icon::icon_path(LOGO_SVG) {
        ui.draw_icon.svg_path = ArcStringMut::Arc(path);
        ui.draw_icon.color = p.foreground;
        // The mark fills its bounds, so it sits 80px tall in the 96px slot, clear of the title.
        let (h, w) = (80.0, 80.0 * LOGO_ASPECT);
        ui.draw_icon.draw_walk(cx, Walk::abs_rect(rect(center - w * 0.5, y + 4.0, w, h)));
    }
    y += 96.0;

    let title_w = ui.painter.measure(cx, title_spec, &title).x.min(inner.size.x);
    ui.painter.paragraph(cx, dvec2(center - title_w * 0.5, y), title_w, title_spec, &title);
    y += title_h + 8.0;
    let subtitle_w = ui.painter.measure(cx, subtitle_spec, &subtitle).x.min(inner.size.x);
    ui.painter.paragraph(cx, dvec2(center - subtitle_w * 0.5, y), subtitle_w, subtitle_spec, &subtitle);
    y += subtitle_h;
    if MOBILE {
        y += 8.0;
        // The daemon the app connects to is Paseo: the link is its docs, labeled as such.
        let spec = ui.spec(Font::Medium, 14.0, p.accent_bright);
        let label = tr("onboarding.daemonDocs");
        let w = ui.measure(cx, spec, &label).min(inner.size.x - 20.0);
        let total = w + 6.0 + 14.0;
        let x = center - total * 0.5;
        ui.text(cx, rect(x, y, w, line), spec, &label);
        ui.icon(cx, rect(x + w + 6.0, y + (line - 14.0) * 0.5, 14.0, 14.0), "external-link", 14.0, p.accent_bright);
        ui.hit(rect(x, y, total, line), "welcome:website");
        y += line;
    }
    y += 48.0;

    let width = inner.size.x.min(420.0);
    let left = center - width * 0.5;
    if let Some(state) = &daemon {
        let card = rect(left, y, card_w, daemon_h - 16.0);
        ui.painter.outline(cx, card, p.surface1, if matches!(state, LocalDaemon::Failed(_)) { p.danger } else { p.border }, 1.0, 12.0);
        let glyph = rect(card.pos.x + 16.0, card.pos.y + 16.0 + (line - 16.0) * 0.5, 16.0, 16.0);
        match state {
            LocalDaemon::Starting => {
                crate::shell::sheets::spinner(&mut ui, cx, glyph, p.muted);
                // Welcome is a page, not a sheet: it keeps its own spinner frames going.
                crate::shell::sheets::take_spinning();
                ui.painter.draw_box.area().redraw(cx);
            }
            _ => ui.icon(cx, glyph, "circle-alert", 16.0, p.danger),
        }
        ui.painter.paragraph(cx, dvec2(card.pos.x + 16.0 + 24.0, card.pos.y + 16.0), card_text_w, daemon_spec, &daemon_text);
        if matches!(state, LocalDaemon::Failed(_)) {
            let by = card.pos.y + 16.0 + daemon_text_h + 12.0;
            let retry = tr("common.actions.retry");
            let rw = ui.button_width(cx, &retry, Some("rotate-cw"), ControlSize::Sm);
            let bx = card.pos.x + 16.0 + 24.0;
            ui.button(cx, rect(bx, by, rw, 32.0), "welcome:daemonretry", &retry, Some("rotate-cw"), Variant::Primary, ControlSize::Sm, true);
            let logs = tr("onboarding.daemon.viewLogs");
            let lw = ui.button_width(cx, &logs, Some("file-text"), ControlSize::Sm).min((card.pos.x + card.size.x - 16.0 - (bx + rw + 8.0)).max(0.0));
            ui.button(cx, rect(bx + rw + 8.0, by, lw, 32.0), "welcome:daemonlogs", &logs, Some("file-text"), Variant::Ghost, ControlSize::Sm, true);
        }
        y += daemon_h;
    }
    for action in &actions {
        let r = rect(left, y, width, button_h);
        let id = format!("welcome:{}", action.id);
        let pressed = ui.is_pressed(&id);
        let (bg, border, fg) = if action.primary { (p.accent, p.accent, p.on_tertiary) } else { (p.surface2, p.border, p.foreground) };
        let opacity = if pressed { 0.85 } else { 1.0 };
        ui.painter.outline(cx, r, crate::theme::fade(bg, opacity), crate::theme::fade(border, opacity), 1.0, 12.0);
        let spec = ui.spec(Font::Medium, 14.0, fg);
        let label_w = ui.measure(cx, spec, &action.label).min(width - 48.0 - 30.0);
        let x = center - (18.0 + 12.0 + label_w) * 0.5;
        ui.icon(cx, rect(x, r.pos.y + (button_h - 18.0) * 0.5, 18.0, 18.0), action.icon, 18.0, fg);
        ui.text(cx, rect(x + 30.0, r.pos.y, label_w, button_h), spec, &action.label);
        ui.hit(r, id);
        y += button_h + 12.0;
    }
    y += 24.0 - 12.0;

    let settings = tr("onboarding.actions.settings");
    let sw = ui.button_width(cx, &settings, Some("settings"), ControlSize::Sm);
    ui.button(cx, rect(center - sw * 0.5, y, sw, 32.0), "welcome:settings", &settings, Some("settings"), Variant::Ghost, ControlSize::Sm, true);

    let version = version_text();
    let vw = ui.painter.measure(cx, version_spec, &version).x;
    let vy = (inner.pos.y + inner.size.y - version_h + 24.0).max(y + 32.0 + 24.0);
    ui.text(cx, rect(center - vw * 0.5, vy, vw, version_h - 24.0), version_spec, &version);
    ui.hits.set_clip(None);
    cx.end_turtle();
}

pub fn click(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: &str) {
    let _ = ui;
    match id {
        "website" => {
            platform::open_external("https://paseo.sh");
        }
        "settings" => crate::shell::commands::open_settings(shell, cx, None, ""),
        // Start the built-in daemon again; the card follows its state.
        "daemonretry" => {
            if let Some(handle) = paseo_gui_platform::platform() {
                handle.desktop_invoke("start_desktop_daemon", serde_json::json!({}));
            }
        }
        // The local daemon settings page holds its log viewer.
        "daemonlogs" => crate::shell::commands::open_settings(shell, cx, None, "local-daemon"),
        "scan" | "direct" | "ssh" | "link" => {
            // One connection flow at a time (Flutter `_busy`).
            if shell.data.overlays.stack.is_empty() {
                crate::shell::sheets::host_dialog::open_flow(shell, cx, Some(id), None, None);
            }
        }
        _ => {}
    }
    shell.redraw(cx);
}
