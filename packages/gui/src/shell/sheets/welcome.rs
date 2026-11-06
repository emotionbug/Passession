//! Port of `features/navigation/welcome_screen.dart` (original `components/welcome-screen.tsx`),
//! drawn in the content area while no host is saved: the Paseo logo, onboarding title and
//! subtitle (plus the paseo.sh link on mobile), the connection method buttons, a ghost Settings
//! button and the app version. Each method opens the matching connection flow; the screen
//! disappears by itself once a host is saved.

use makepad_widgets::*;

use crate::common::paint::{Font, rect};
use crate::common::{UiContext, tr};
use crate::shell::chrome::make_ui;
use crate::shell::ui::Variant;
use crate::shell::{PaseoShell, platform};
use crate::theme::{ControlSize, MOBILE};

const LOGO_SVG: &str = include_str!("../../../resources/icons/paseo-logo.svg");

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

pub fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, area: Rect) {
    let actions = actions();
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
    let content_h = 96.0 + title_h + 8.0 + subtitle_h + link_h + 48.0 + buttons_h + 24.0 + 32.0;
    let version_spec = ui.spec(Font::Regular, 12.0, p.muted);
    let version_h = 24.0 + ui.painter.paragraph_height(cx, inner.size.x, version_spec, "Ag");
    let available = inner.size.y - version_h;
    let mut y = inner.pos.y + ((available - content_h) * 0.5).max(0.0);
    let center = inner.pos.x + inner.size.x * 0.5;

    // Logo: the 700×700 Paseo mark in the foreground colour.
    if let Some(path) = crate::common::provider_icon::icon_path(LOGO_SVG) {
        ui.draw_icon.svg_path = ArcStringMut::Arc(path);
        ui.draw_icon.color = p.foreground;
        ui.draw_icon.draw_walk(cx, Walk::abs_rect(rect(center - 48.0, y, 96.0, 96.0)));
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
        let spec = ui.spec(Font::Medium, 14.0, p.accent);
        let w = ui.measure(cx, spec, "paseo.sh");
        let total = w + 6.0 + 14.0;
        let x = center - total * 0.5;
        ui.text(cx, rect(x, y, w, line), spec, "paseo.sh");
        ui.icon(cx, rect(x + w + 6.0, y + (line - 14.0) * 0.5, 14.0, 14.0), "external-link", 14.0, p.accent);
        ui.hit(rect(x, y, total, line), "welcome:website");
        y += line;
    }
    y += 48.0;

    let width = inner.size.x.min(420.0);
    let left = center - width * 0.5;
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
}

pub fn click(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: &str) {
    let _ = ui;
    match id {
        "website" => {
            platform::open_external("https://paseo.sh");
        }
        "settings" => crate::shell::commands::open_settings(shell, cx, None, ""),
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
