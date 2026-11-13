//! Port of `features/navigation/host_dialog.dart`: the add/edit connection flow — the method
//! picker (`AddHostMethodModal`), the direct / remote SSH / pairing-link forms, host
//! confirmation for relay pairings, the QR scan screen (mobile) and the "Pair a device"
//! sheet with its QR code. A connection is saved only after `probe_host` answered hello.

use std::sync::Arc;

use makepad_widgets::*;
use paseo_gui_core::{CoreError, HostConfigInfo, inspect_host_config};
use paseo_gui_platform::qr::{QrScanState, QrScanViewWidgetRefExt};
use paseo_gui_platform::{PlatformAction, PlatformEvent};
use serde_json::{Map, Value, json};

use crate::common::paint::{Font, rect};
use crate::common::{error_message, json as j, tr, tr_args};
use crate::shell::hosts;
use crate::shell::sheets::qr_code;
use crate::shell::sheets::{Field, FieldEvent, ShellOp, Sheet, SheetFlow, SheetOps, SheetStyle, draw_widget, spinner};
use crate::shell::ui::{Ui, Variant};
use crate::shell::{PaseoShell, platform, response_message};
use crate::theme::{ControlSize, MOBILE, argb, with_alpha};

/// Original `showHostConnectionFlow`. Without a `method`, `config` or `link` it opens the
/// method picker (a form's Cancel returns to it); with a `method` (`direct`, `ssh`, `link`,
/// `scan`) the welcome screen opens that form directly and Cancel just closes it.
pub fn open_flow(shell: &mut PaseoShell, cx: &mut Cx, method: Option<&str>, config: Option<Value>, link: Option<String>) {
    let sheet: Box<dyn Sheet> = if config.is_some() || link.is_some() || method.is_some() {
        if method == Some("scan") && config.is_none() && link.is_none() {
            Box::new(ScanSheet::new())
        } else {
            let initial = if link.is_some() { "link" } else { method.unwrap_or("direct") };
            Box::new(ConnectionSheet::new(config, link, initial, false))
        }
    } else {
        Box::new(MethodSheet)
    };
    shell.data.overlays.open_sheet(sheet);
    shell.redraw(cx);
}

/// Original `getConnectionAuthFailureReason`: the host refused the (missing) password.
fn is_auth_failure(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("password")
        || lower.contains("unauthorized")
        || lower.contains("unauthorised")
        || lower.contains("authentication required")
        || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| word == "401")
}

/// Original `formatDaemonKeyFingerprint`: groups of four; keys longer than 16 characters
/// show their first and last 8.
fn key_fingerprint(key: &str) -> String {
    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let group = |value: &str| -> String {
        value.chars().collect::<Vec<_>>().chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
    };
    if normalized.len() <= 16 {
        return group(&normalized);
    }
    format!("{} … {}", group(&normalized[..8]), group(&normalized[normalized.len() - 8..]))
}

/// Original `HostConfirmation.confirmLink`: `None` when a saved host of that server already has
/// this relay connection (endpoint, TLS and daemon key); otherwise the confirmation sheet.
fn host_confirmation(state: &Value, info: &HostConfigInfo, reply_to: Option<u64>) -> Option<HostConfirmSheet> {
    let server_id = info.expected_server_id.clone().unwrap_or_default();
    let saved = hosts::host(state, &server_id);
    let same_relay = |connection: &Value| {
        inspect_host_config(connection).is_ok_and(|other| {
            info.is_relay
                && other.is_relay
                && other.daemon_public_key == info.daemon_public_key
                && other.scheme == info.scheme
                && other.host == info.host
                && other.port == info.port
        })
    };
    if saved.is_some_and(|host| host.connections().iter().any(same_relay)) {
        return None;
    }
    Some(HostConfirmSheet {
        server_id,
        fingerprint: key_fingerprint(info.daemon_public_key.as_deref().unwrap_or("")),
        relay: format!("{}:{}", info.host, info.port.map(|p| p.to_string()).unwrap_or_default()),
        changed: saved.is_some(),
        reply_to,
        answered: false,
    })
}

/// Original `AddHostModal` / `PairLinkModal` primary action row geometry.
const BUTTON_H: f64 = 40.0;

fn field_h(ui: &Ui) -> f64 {
    if ui.compact { 44.0 } else { 32.0 }
}

/// Muted label (medium for direct/link, normal for SSH) above an input; returns its height.
fn field_label(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, text: &str, medium: bool) -> f64 {
    let spec = ui.spec(if medium { Font::Medium } else { Font::Regular }, 14.0, ui.p.muted);
    let h = ui.painter.paragraph_height(cx, w, spec, text);
    ui.painter.paragraph(cx, dvec2(x, y), w, spec, text);
    h + 8.0
}

/// Destructive selectable error copy; returns its height.
#[allow(clippy::too_many_arguments)]
fn error_text(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, text: &str, size: f64, color: Vec4) -> f64 {
    let spec = ui.spec(Font::Regular, size, color);
    let h = ui.painter.paragraph_height(cx, w, spec, text);
    ui.painter.paragraph(cx, dvec2(x, y), w, spec, text);
    h
}

/// Chevron + medium label disclosure row (`_toggle`); returns its height.
#[allow(clippy::too_many_arguments)]
fn toggle_row(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, id: &str, open: bool, label: &str, enabled: bool) -> f64 {
    let p = ui.p;
    let spec = ui.spec(Font::Medium, 14.0, p.foreground);
    let w = ui.measure(cx, spec, label) + 24.0;
    let r = rect(x, y, w, 28.0);
    ui.icon(cx, rect(x, y + 6.0, 16.0, 16.0), if open { "chevron-down" } else { "chevron-right" }, 16.0, p.muted);
    ui.text(cx, rect(x + 24.0, y, w - 24.0, 28.0), spec, label);
    if enabled {
        ui.hit(r, id.to_string());
    }
    28.0
}

/// Original password visibility toggle: surface2 square, muted 18px eye glyph.
fn eye_button(ui: &mut Ui, cx: &mut Cx2d, r: Rect, id: &str, visible: bool, tooltip: &str, enabled: bool) {
    let p = ui.p;
    let border = if ui.is_hover(id) || ui.is_pressed(id) { p.border_accent } else { p.border };
    ui.painter.outline(cx, r, p.surface2, border, 1.0, if ui.compact { 8.0 } else { 6.0 });
    ui.icon(cx, rect(r.pos.x + (r.size.x - 18.0) * 0.5, r.pos.y + (r.size.y - 18.0) * 0.5, 18.0, 18.0), if visible { "eye-off" } else { "eye" }, 18.0, p.muted);
    if enabled {
        ui.hit_tip(r, id, tooltip);
    }
}

/// Two expanded buttons 12 apart (Cancel / primary action); returns the height.
#[allow(clippy::too_many_arguments)]
fn button_pair(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, cancel: &str, primary: &str, icon: Option<&str>, primary_variant: Variant, enabled: bool, gap: f64, size: ControlSize) -> f64 {
    let h = if size == ControlSize::Sm { 32.0 } else { BUTTON_H };
    let bw = (w - gap) * 0.5;
    ui.button(cx, rect(x, y, bw, h), "cancel", cancel, None, Variant::Secondary, size, enabled);
    ui.button(cx, rect(x + bw + gap, y, bw, h), "primary", primary, icon, primary_variant, size, enabled);
    h
}

// ---------------------------------------------------------------------------------------------
// Method picker

/// Original `AddHostMethodModal`: one bordered surface2 option per method, 16px apart. Remote
/// SSH is desktop only; QR scanning needs a native camera.
struct MethodSheet;

impl MethodSheet {
    fn options() -> Vec<(&'static str, &'static str, String, String)> {
        let mut options = vec![("direct", "link-2", tr("pairing.connectionMethods.direct.title"), tr("pairing.connectionMethods.direct.description"))];
        if !MOBILE {
            options.push(("ssh", "terminal", tr("pairing.connectionMethods.remoteSsh.title"), tr("pairing.connectionMethods.remoteSsh.description")));
        }
        if MOBILE && paseo_gui_platform::platform().is_some_and(|h| h.capabilities().qr_scan) {
            options.push(("scan", "qr-code", tr("pairing.connectionMethods.scanQr.title"), tr("pairing.connectionMethods.scanQr.description")));
        }
        options.push(("link", "clipboard-paste", tr("pairing.connectionMethods.pasteLink.title"), tr("pairing.connectionMethods.pasteLink.description")));
        options
    }
}

impl Sheet for MethodSheet {
    fn title(&self) -> String {
        tr("pairing.connectionMethods.title")
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        for (i, (id, icon, title, description)) in Self::options().into_iter().enumerate() {
            if i > 0 {
                y += 16.0;
            }
            let title_spec = ui.spec(Font::Regular, 14.0, p.foreground);
            let desc_spec = ui.spec(Font::Regular, 14.0, p.muted);
            let tw = w - 32.0 - 18.0 - 16.0;
            let th = ui.painter.paragraph_height(cx, tw, title_spec, &title);
            let dh = ui.painter.paragraph_height(cx, tw, desc_spec, &description);
            let h = 16.0 + th + 4.0 + dh + 16.0;
            let r = rect(x, y, w, h);
            let hit = format!("method:{id}");
            let pressed = ui.is_pressed(&hit);
            ui.painter.outline(cx, r, if pressed { p.surface3 } else { p.surface2 }, p.border, 1.0, 12.0);
            ui.icon(cx, rect(x + 16.0, y + (h - 18.0) * 0.5, 18.0, 18.0), icon, 18.0, p.foreground);
            let tx = x + 16.0 + 18.0 + 16.0;
            ui.painter.paragraph(cx, dvec2(tx, y + 16.0), tw, title_spec, &title);
            ui.painter.paragraph(cx, dvec2(tx, y + 16.0 + th + 4.0), tw, desc_spec, &description);
            if interactive {
                ui.hit(r, hit);
            }
            y += h;
        }
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, _ops: &mut SheetOps) -> SheetFlow {
        match id.strip_prefix("method:") {
            Some("scan") => SheetFlow::Replace(Box::new(ScanSheet::new())),
            Some(method) => SheetFlow::Replace(Box::new(ConnectionSheet::new(None, None, method, true))),
            None => SheetFlow::Stay,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Host confirmation

/// Original `hosts/host-confirmation-sheet.tsx`: muted description, a card of Host / Key
/// fingerprint / Relay rows, then Cancel / Connect. Answers its opener with `confirm`.
pub struct HostConfirmSheet {
    server_id: String,
    fingerprint: String,
    relay: String,
    changed: bool,
    reply_to: Option<u64>,
    answered: bool,
}

impl HostConfirmSheet {
    fn answer(&mut self, ops: &mut SheetOps, approved: bool) {
        if std::mem::replace(&mut self.answered, true) {
            return;
        }
        if let Some(sheet) = self.reply_to {
            ops.shell.push(ShellOp::Deliver { sheet, tag: "confirm".into(), value: Value::Bool(approved) });
        }
    }
}

impl Sheet for HostConfirmSheet {
    fn title(&self) -> String {
        tr("pairing.hostConfirmation.title")
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        let description = tr(if self.changed { "pairing.hostConfirmation.descriptionChanged" } else { "pairing.hostConfirmation.description" });
        let spec = ui.spec(Font::Regular, 14.0, p.muted);
        let h = ui.painter.paragraph_height(cx, w, spec, &description);
        ui.painter.paragraph(cx, dvec2(x, y), w, spec, &description);
        y += h + 16.0;
        let rows = [
            (tr("pairing.hostConfirmation.hostLabel"), self.server_id.clone()),
            (tr("pairing.hostConfirmation.fingerprintLabel"), self.fingerprint.clone()),
            (tr("pairing.hostConfirmation.relayLabel"), self.relay.clone()),
        ];
        let label_spec = ui.spec(Font::Regular, 14.0, p.foreground);
        let value_spec = ui.spec(Font::Regular, 12.0, p.muted);
        let heights: Vec<f64> = rows
            .iter()
            .map(|(label, value)| {
                16.0 + ui.painter.paragraph_height(cx, w - 44.0, label_spec, label) + 4.0 + ui.painter.paragraph_height(cx, w - 44.0, value_spec, value) + 16.0
            })
            .collect();
        let total: f64 = heights.iter().sum();
        ui.painter.outline(cx, rect(x, y, w, total), p.surface1, p.border, 1.0, 8.0);
        for (i, ((label, value), h)) in rows.iter().zip(&heights).enumerate() {
            if i > 0 {
                ui.painter.hline(cx, x, y, w, p.border);
            }
            let lh = ui.painter.paragraph_height(cx, w - 44.0, label_spec, label);
            ui.painter.paragraph(cx, dvec2(x + 16.0, y + 16.0), w - 44.0, label_spec, label);
            ui.painter.paragraph(cx, dvec2(x + 16.0, y + 16.0 + lh + 4.0), w - 44.0, value_spec, value);
            y += h;
        }
        y += 16.0 + 16.0;
        y += button_pair(ui, cx, x, y, w, &tr("common.actions.cancel"), &tr("pairing.hostConfirmation.connect"), None, Variant::Primary, interactive, 8.0, ControlSize::Sm);
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        match id {
            "primary" => {
                self.answer(ops, true);
                SheetFlow::Close
            }
            "cancel" => {
                self.answer(ops, false);
                SheetFlow::Close
            }
            _ => SheetFlow::Stay,
        }
    }

    fn closed(&mut self, _cx: &mut Cx, ops: &mut SheetOps) {
        self.answer(ops, false);
    }
}

// ---------------------------------------------------------------------------------------------
// Connection forms

/// Input that asked for follow-up with the next ops (Enter, link edits).
#[derive(Clone, Copy, PartialEq)]
enum Pending {
    None,
    Save,
    ToggleAdvanced,
}

/// The original connection forms in one sheet: `AddHostModal` (`direct`),
/// `AddRemoteSshHostModal` (`ssh`) and `PairLinkModal` (`link`). An existing config with an
/// endpoint edits that connection.
pub struct ConnectionSheet {
    id: String,
    original: Option<Value>,
    method: String,
    editing: bool,
    /// Opened from the method picker: Cancel returns to it.
    from_picker: bool,
    label: Field,
    password: Field,
    endpoint: Field,
    identity: Field,
    color: Field,
    ssh_target: Field,
    direct_host: Field,
    direct_port: Field,
    use_tls: bool,
    show_password: bool,
    advanced: bool,
    details: bool,
    busy: bool,
    saved: bool,
    error: Option<String>,
    approved_pairing: Option<String>,
    needs_password: bool,
    password_link: String,
    /// The config being confirmed / probed, its endpoint and whether it is a relay.
    candidate: Option<(Value, String, bool)>,
    pending: Pending,
    focus_port: bool,
    state: Arc<Value>,
}

impl ConnectionSheet {
    pub fn new(config: Option<Value>, link: Option<String>, method: &str, from_picker: bool) -> Self {
        Self::with(config, link, method, from_picker, false, false, None)
    }

    /// The pairing-link form after a scanned link was refused for a missing password.
    fn password_prompt(link: String, connection_id: String) -> Self {
        Self::with(None, Some(link), "link", false, true, true, Some(connection_id))
    }

    fn with(config: Option<Value>, link: Option<String>, method: &str, from_picker: bool, needs_password: bool, pre_approved: bool, connection_id: Option<String>) -> Self {
        let text = |key: &str| config.as_ref().map(|c| j::text(c, key).to_string()).unwrap_or_default();
        let editing = !text("endpoint").is_empty();
        let id = config
            .as_ref()
            .map(|c| j::text(c, "id").to_string())
            .filter(|id| !id.is_empty())
            .or(connection_id)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let method = if link.is_some() { "link".to_string() } else { method.to_string() };
        let endpoint = link.clone().unwrap_or_else(|| text("endpoint"));
        let optional = tr("pairing.direct.fields.optional");
        let password_hint = match method.as_str() {
            "ssh" => tr("pairing.remoteSsh.fields.optional"),
            "link" => String::new(),
            _ => optional.clone(),
        };
        let focus = |field: Field, focused: bool| if focused { field.autofocus() } else { field };
        let endpoint_hint = if method == "link" { "https://app.paseo.sh/#offer=..." } else { "tcp://localhost:6767?ssl=true" };
        Self {
            id,
            method: method.clone(),
            editing,
            from_picker,
            label: Field::new(&text("label"), &optional),
            password: Field::new(&text("password"), &password_hint).password(),
            endpoint: focus(Field::new(&endpoint, endpoint_hint), method == "link"),
            identity: Field::new(&text("expectedServerId"), &optional),
            color: Field::new(&text("color"), "#4488CC"),
            ssh_target: focus(Field::new("", "ssh://user@host"), method == "ssh"),
            direct_host: focus(Field::new("", "localhost"), method == "direct" && !editing),
            direct_port: Field::new("6767", "6767"),
            use_tls: false,
            show_password: false,
            advanced: editing,
            details: config.is_some(),
            busy: false,
            saved: false,
            error: None,
            approved_pairing: (pre_approved && link.is_some()).then(|| format!("{}\n", link.as_deref().unwrap_or("").trim())),
            needs_password,
            password_link: link.as_deref().unwrap_or("").trim().to_string(),
            candidate: None,
            pending: Pending::None,
            focus_port: false,
            state: Arc::new(Value::Null),
            original: config,
        }
    }

    fn scope(&self) -> &'static str {
        match self.method.as_str() {
            "ssh" => "pairing.remoteSsh",
            "link" => "pairing.link",
            _ => "pairing.direct",
        }
    }

    fn direct_uri(&self) -> String {
        let host = self.direct_host.text().trim().to_string();
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
        let port = self.direct_port.text().trim().parse::<u16>().unwrap_or(6767);
        format!("tcp://{host}:{port}{}", if self.use_tls { "?ssl=true" } else { "" })
    }

    /// Original `prepareDirectConnection` / `parseSshTransportUri` / pairing-link checks.
    fn resolve_endpoint(&self) -> Result<String, String> {
        match self.method.as_str() {
            "ssh" => {
                let raw = self.ssh_target.text().trim().to_string();
                if raw.is_empty() {
                    return Err(tr("pairing.remoteSsh.errors.targetRequired"));
                }
                let text = if raw.contains("://") { raw } else { format!("ssh://{raw}") };
                let invalid = || tr("pairing.remoteSsh.errors.invalidTarget");
                let uri = url::Url::parse(&text).map_err(|_| invalid())?;
                let host = uri.host_str().unwrap_or("");
                if uri.scheme() != "ssh" || host.is_empty() || host.starts_with('-') || uri.fragment().is_some() || uri.query_pairs().any(|(key, _)| key != "daemonPort") {
                    return Err(invalid());
                }
                Ok(uri.to_string())
            }
            "link" => {
                let link = self.endpoint.text().trim().to_string();
                if link.is_empty() {
                    return Err(tr("pairing.link.errors.required"));
                }
                if !link.contains("#offer=") && !link.starts_with("relay://") && !link.contains("#connect=") {
                    return Err(tr("pairing.link.errors.missingOffer"));
                }
                Ok(link)
            }
            _ => {
                if self.advanced {
                    let uri = self.endpoint.text().trim().to_string();
                    if uri.is_empty() {
                        return Err(tr("pairing.direct.errors.hostRequired"));
                    }
                    return Ok(uri);
                }
                if self.direct_host.text().trim().is_empty() {
                    return Err(tr("pairing.direct.errors.hostRequired"));
                }
                match self.direct_port.text().trim().parse::<u32>() {
                    Ok(port) if (1..=65535).contains(&port) => Ok(self.direct_uri()),
                    _ => Err(tr("pairing.direct.errors.invalidPort")),
                }
            }
        }
    }

    /// Original `handleToggleAdvanced`: the URI and the host/port/SSL draft convert both ways.
    fn toggle_advanced(&mut self, cx: &mut Cx) {
        if self.busy {
            return;
        }
        if !self.advanced {
            let port = self.direct_port.text().trim().parse::<u32>().ok();
            let valid = !self.direct_host.text().trim().is_empty() && port.is_some_and(|p| (1..=65535).contains(&p));
            let uri = if valid { self.direct_uri() } else { String::new() };
            self.endpoint.set_text(cx, &uri);
        } else {
            let input = self.endpoint.text().trim().to_string();
            let text = if input.contains("://") { input } else { format!("tcp://{input}") };
            if let Ok(uri) = url::Url::parse(&text)
                && matches!(uri.scheme(), "tcp" | "ws" | "wss")
                && uri.host_str().is_some_and(|h| !h.is_empty())
            {
                self.direct_host.set_text(cx, uri.host_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']'));
                self.direct_port.set_text(cx, &uri.port().unwrap_or(6767).to_string());
                self.use_tls = uri.scheme() == "wss" || uri.query_pairs().any(|(k, v)| k == "ssl" && v == "true");
                if let Some((_, password)) = uri.query_pairs().find(|(k, _)| k == "password") {
                    self.password.set_text(cx, &password);
                }
            }
        }
        self.error = None;
        self.advanced = !self.advanced;
    }

    fn without_password(endpoint: &str) -> String {
        let Ok(mut uri) = url::Url::parse(endpoint) else { return endpoint.to_string() };
        if !uri.query_pairs().any(|(k, _)| k == "password") {
            return endpoint.to_string();
        }
        let rest: Vec<(String, String)> = uri.query_pairs().filter(|(k, _)| k != "password").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        if rest.is_empty() {
            uri.set_query(None);
        } else {
            uri.query_pairs_mut().clear().extend_pairs(rest);
        }
        uri.to_string()
    }

    /// Original `buildConnectionFailureCopy` detail.
    fn direct_failure_detail(raw: &str) -> String {
        let lower = raw.to_lowercase();
        if is_auth_failure(raw) {
            return raw.to_string();
        }
        let key = if lower.contains("timed out") {
            "pairing.direct.errors.timedOut"
        } else if lower.contains("econnrefused") || lower.contains("connection refused") || lower.contains("err_connection_refused") {
            "pairing.direct.errors.refused"
        } else if lower.contains("unable to resolve host") || lower.contains("enotfound") || lower.contains("failed host lookup") || lower.contains("failed to lookup address") {
            "pairing.direct.errors.hostNotFound"
        } else if ["certificate", "certpathvalidator", "trust anchor", "handshake", "tls", "ssl"].iter().any(|w| lower.contains(w)) {
            "pairing.direct.errors.tlsError"
        } else if lower.contains("not found") {
            "pairing.direct.errors.hostNotFound"
        } else if ["ehostunreach", "host is unreachable", "no route to host", "network is unreachable"].iter().any(|w| lower.contains(w)) {
            "pairing.direct.errors.hostUnreachable"
        } else {
            "pairing.direct.errors.unableToConnect"
        };
        tr(key)
    }

    /// Original failure copy: SSH wraps the detail, pairing links show the error itself, and
    /// direct connections use `buildConnectionFailureCopy`.
    fn failure_message(&self, endpoint: &str, raw: &str, relay: bool) -> String {
        let raw = raw.trim();
        match self.method.as_str() {
            "ssh" => tr_args("pairing.remoteSsh.errors.failedToConnect", &[("detail", raw)]),
            "link" => {
                if raw.is_empty() {
                    tr("pairing.link.errors.unableToPair")
                } else {
                    raw.to_string()
                }
            }
            _ if relay => {
                if raw.is_empty() {
                    tr("pairing.direct.errors.invalidConnection")
                } else {
                    raw.to_string()
                }
            }
            _ => {
                let title = tr_args("pairing.direct.errors.failedToConnect", &[("endpoint", &Self::without_password(endpoint))]);
                let detail = Self::direct_failure_detail(raw);
                if !raw.is_empty() && raw != detail {
                    format!("{title}\n{detail}\n{}", tr_args("pairing.direct.errors.details", &[("detail", raw)]))
                } else {
                    format!("{title}\n{detail}")
                }
            }
        }
    }

    fn save(&mut self, ops: &mut SheetOps) -> SheetFlow {
        if self.busy {
            return SheetFlow::Stay;
        }
        let endpoint = match self.resolve_endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.error = Some(error);
                return SheetFlow::Stay;
            }
        };
        let color = self.color.text().trim().to_string();
        let valid_color = color.is_empty() || (color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit()));
        if !valid_color {
            self.error = Some("Badge color must use #RRGGBB".into());
            return SheetFlow::Stay;
        }
        self.error = None;
        let original_endpoint = self.original.as_ref().map(|c| j::text(c, "endpoint").to_string());
        let mut config = Map::new();
        config.insert("id".into(), Value::from(self.id.clone()));
        config.insert("label".into(), Value::from(self.label.text().trim().to_string()));
        config.insert("endpoint".into(), Value::from(endpoint.clone()));
        let password = self.password.text();
        if !password.is_empty() {
            config.insert("password".into(), Value::from(password));
        }
        if original_endpoint.as_deref() == Some(endpoint.as_str()) {
            // A local daemon's credential stays with its unchanged endpoint.
            for key in ["localCredential", "localCredentialPath"] {
                if let Some(value) = self.original.as_ref().and_then(|c| c.get(key)).filter(|v| !v.is_null()) {
                    config.insert(key.into(), value.clone());
                }
            }
        }
        let identity = self.identity.text().trim().to_string();
        if !identity.is_empty() {
            config.insert("expectedServerId".into(), Value::from(identity.clone()));
        }
        if !color.is_empty() {
            config.insert("color".into(), Value::from(color));
        }
        let config = Value::Object(config);
        // Resolve before storing: pairing URLs retain their approved public-key floor.
        let info = match inspect_host_config(&config) {
            Ok(info) => info,
            Err(error) => {
                self.error = Some(error.message);
                return SheetFlow::Stay;
            }
        };
        let relay = info.is_relay;
        let pairing_identity = format!("{endpoint}\n{identity}");
        self.busy = true;
        self.candidate = Some((config, endpoint, relay));
        if relay && self.approved_pairing.as_deref() != Some(pairing_identity.as_str()) {
            match host_confirmation(&self.state, &info, ops.origin) {
                Some(confirm) => return SheetFlow::Push(Box::new(confirm)),
                None => self.approved_pairing = Some(pairing_identity),
            }
        }
        self.probe(ops);
        SheetFlow::Stay
    }

    /// Original `connectToDaemon`: only a connection that answers hello is saved.
    fn probe(&mut self, ops: &mut SheetOps) {
        if let Some((config, _, _)) = &self.candidate {
            let request = ops.core.probe_host(config.clone());
            ops.track(request, "probe");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn form_body(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        let fh = field_h(ui);
        let enabled = interactive && !self.busy;
        let error_color = p.destructive;
        match self.method.as_str() {
            "ssh" => {
                y += field_label(ui, cx, x, y, w, &tr("pairing.remoteSsh.fields.target"), false);
                self.ssh_target.set_read_only(cx, self.busy);
                self.ssh_target.draw(cx, scope, rect(x, y, w, fh));
                y += fh;
                if let Some(error) = self.error.clone() {
                    y += 8.0 + error_text(ui, cx, x, y + 8.0, w, &error, 12.0, argb(0xfffca5a5));
                }
                y += 16.0;
                y += field_label(ui, cx, x, y, w, &tr("pairing.remoteSsh.fields.password"), false);
                self.password.set_read_only(cx, self.busy);
                self.password.set_password(cx, !self.show_password);
                self.password.draw(cx, scope, rect(x, y, w - fh - 8.0, fh));
                let tip = tr(&format!("pairing.remoteSsh.passwordVisibility.{}", if self.show_password { "hide" } else { "show" }));
                eye_button(ui, cx, rect(x + w - fh, y, fh, fh), "eye", self.show_password, &tip, enabled);
                y += fh;
            }
            "link" => {
                y += field_label(ui, cx, x, y, w, &tr("pairing.link.label"), true);
                self.endpoint.draw(cx, scope, rect(x, y, w, fh));
                y += fh;
                if let Some(error) = self.error.clone() {
                    y += 8.0 + error_text(ui, cx, x, y + 8.0, w, &error, 14.0, error_color);
                }
                if self.needs_password {
                    y += 16.0;
                    let probe = json!({ "id": "", "label": "", "endpoint": self.endpoint.text().trim() });
                    let host = inspect_host_config(&probe).ok().and_then(|i| i.expected_server_id).unwrap_or_else(|| "host".to_string());
                    y += field_label(ui, cx, x, y, w, &tr_args("pairing.hostPassword.title", &[("host", &host)]), true);
                    self.password.set_password(cx, true);
                    self.password.draw(cx, scope, rect(x, y, w, fh));
                    y += fh;
                }
            }
            _ => {
                if !self.advanced || !self.editing {
                    let port_w = 112.0;
                    let host_w = w - port_w - 12.0;
                    let lh = field_label(ui, cx, x, y, host_w, &tr("pairing.direct.fields.host"), true);
                    field_label(ui, cx, x + host_w + 12.0, y, port_w, &tr("pairing.direct.fields.port"), true);
                    y += lh;
                    self.direct_host.set_read_only(cx, self.busy);
                    self.direct_port.set_read_only(cx, self.busy);
                    self.direct_host.draw(cx, scope, rect(x, y, host_w, fh));
                    self.direct_port.draw(cx, scope, rect(x + host_w + 12.0, y, port_w, fh));
                    if std::mem::take(&mut self.focus_port) {
                        self.direct_port.focus(cx);
                    }
                    y += fh + 16.0;
                    // Use SSL checkbox: 22px, accent when checked.
                    let label = tr("pairing.direct.fields.useSsl");
                    let spec = ui.spec(Font::Medium, 14.0, p.muted);
                    let lw = ui.measure(cx, spec, &label);
                    let box_r = rect(x, y, 22.0, 22.0);
                    ui.painter.outline(cx, box_r, if self.use_tls { p.accent } else { Vec4::default() }, if self.use_tls { p.accent } else { p.border }, 1.0, 2.0);
                    if self.use_tls {
                        ui.icon(cx, rect(x + 4.0, y + 4.0, 14.0, 14.0), "check", 14.0, p.on_tertiary);
                    }
                    ui.text(cx, rect(x + 34.0, y, lw, 22.0), spec, &label);
                    if enabled {
                        ui.hit(rect(x, y, 34.0 + lw, 22.0), "tls".to_string());
                    }
                    y += 22.0 + 16.0;
                }
                y += field_label(ui, cx, x, y, w, &tr("pairing.direct.fields.password"), true);
                self.password.set_read_only(cx, self.busy);
                self.password.set_password(cx, !self.show_password);
                self.password.draw(cx, scope, rect(x, y, w - fh - 8.0, fh));
                let tip = tr(&format!("pairing.direct.passwordVisibility.{}", if self.show_password { "hide" } else { "show" }));
                eye_button(ui, cx, rect(x + w - fh, y, fh, fh), "eye", self.show_password, &tip, enabled);
                y += fh + 16.0;
                let label = tr("pairing.direct.advanced.label");
                y += toggle_row(ui, cx, x, y, "advanced", self.advanced, &label, enabled);
                if self.advanced {
                    y += 8.0;
                    self.endpoint.set_read_only(cx, self.busy);
                    self.endpoint.draw(cx, scope, rect(x, y, w, fh));
                    y += fh;
                }
                if let Some(error) = self.error.clone() {
                    y += 8.0 + error_text(ui, cx, x, y + 8.0, w, &error, 14.0, error_color);
                }
            }
        }
        if self.editing {
            y += 16.0;
            y += toggle_row(ui, cx, x, y, "details", self.details, "Host details", interactive);
            if self.details {
                for (label, field) in [("Host name", &mut self.label), ("Expected server ID", &mut self.identity), ("Badge color", &mut self.color)] {
                    y += 8.0;
                    y += field_label(ui, cx, x, y, w, label, true);
                    field.set_read_only(cx, self.busy);
                    field.draw(cx, scope, rect(x, y, w, fh));
                    y += fh;
                }
            }
        }
        y - start
    }
}

impl Sheet for ConnectionSheet {
    fn title(&self) -> String {
        if self.editing { "Edit connection".to_string() } else { tr(&format!("{}.title", self.scope())) }
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        self.state = Arc::new(state.clone());
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        let helper = tr(&format!("{}.helper", self.scope()));
        let spec = ui.spec(Font::Regular, if self.method == "ssh" { 12.0 } else { 14.0 }, p.muted);
        let h = ui.painter.paragraph_height(cx, w, spec, &helper);
        ui.painter.paragraph(cx, dvec2(x, y), w, spec, &helper);
        y += h + 16.0;
        y += self.form_body(cx, ui, scope, x, y, w, interactive);
        y += 16.0 + 8.0;
        let scope_key = self.scope();
        let primary = if self.method == "link" {
            tr(if self.busy { "pairing.link.actions.pairing" } else { "pairing.link.actions.pair" })
        } else {
            tr(&format!("{scope_key}.actions.{}", if self.busy { "connecting" } else { "connect" }))
        };
        let icon = match self.method.as_str() {
            "ssh" => "terminal",
            "link" => "link",
            _ => "link-2",
        };
        y += button_pair(ui, cx, x, y, w, &tr(&format!("{scope_key}.actions.cancel")), &primary, Some(icon), Variant::Primary, interactive && !self.busy, 12.0, ControlSize::Md);
        if self.busy {
            spinner(ui, cx, rect(x + w - 24.0, y - BUTTON_H + 12.0, 16.0, 16.0), p.on_tertiary);
        }
        y - start
    }

    fn click(&mut self, cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        if self.busy && id != "dialog:close" {
            return SheetFlow::Stay;
        }
        match id {
            "primary" => return self.save(ops),
            "cancel" => {
                return if self.from_picker { SheetFlow::Replace(Box::new(MethodSheet)) } else { SheetFlow::Close };
            }
            "tls" => self.use_tls = !self.use_tls,
            "eye" => self.show_password = !self.show_password,
            "advanced" => self.toggle_advanced(cx),
            "details" => self.details = !self.details,
            _ => {}
        }
        SheetFlow::Stay
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> SheetFlow {
        let fields: [(&mut Field, &str); 8] = [
            (&mut self.label, "label"),
            (&mut self.password, "password"),
            (&mut self.endpoint, "endpoint"),
            (&mut self.identity, "identity"),
            (&mut self.color, "color"),
            (&mut self.ssh_target, "ssh"),
            (&mut self.direct_host, "host"),
            (&mut self.direct_port, "port"),
        ];
        let mut changes = Vec::new();
        for (field, name) in fields {
            if let Some(change) = field.handle_event(cx, event, scope) {
                changes.push((name, change));
            }
        }
        for (name, change) in changes {
            match (name, change) {
                ("endpoint", FieldEvent::Changed(text)) if self.method == "link" => {
                    // Original `PairingTargetTracker`: another link drops the password prompt.
                    if self.needs_password && text.trim() != self.password_link {
                        self.password.set_text(cx, "");
                        self.needs_password = false;
                        self.error = None;
                    }
                }
                ("host", FieldEvent::Returned(_)) => self.focus_port = true,
                ("endpoint", FieldEvent::Returned(_)) if self.method == "direct" && !self.editing => self.pending = Pending::ToggleAdvanced,
                ("label" | "identity" | "color", FieldEvent::Returned(_)) => {}
                (_, FieldEvent::Returned(_)) => self.pending = Pending::Save,
                _ => {}
            }
        }
        SheetFlow::Stay
    }

    fn after_event(&mut self, cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Save => self.save(ops),
            Pending::ToggleAdvanced => {
                self.toggle_advanced(cx);
                SheetFlow::Stay
            }
            Pending::None => SheetFlow::Stay,
        }
    }

    fn on_deliver(&mut self, _cx: &mut Cx, tag: &str, value: &Value, ops: &mut SheetOps) -> SheetFlow {
        if tag != "confirm" {
            return SheetFlow::Stay;
        }
        if value.as_bool() == Some(true) {
            if let Some((config, endpoint, _)) = &self.candidate {
                self.approved_pairing = Some(format!("{endpoint}\n{}", j::text(config, "expectedServerId")));
            }
            self.probe(ops);
        } else {
            self.busy = false;
            self.candidate = None;
        }
        SheetFlow::Stay
    }

    fn on_response(&mut self, _cx: &mut Cx, tag: &str, result: &Result<Value, CoreError>, ops: &mut SheetOps) -> SheetFlow {
        if tag != "probe" {
            return SheetFlow::Stay;
        }
        self.busy = false;
        let Some((config, endpoint, relay)) = self.candidate.take() else { return SheetFlow::Stay };
        match result {
            Ok(_) => {
                // `updateHost` for a saved connection id, else `addHost`; the core decides.
                ops.core.connect_host(config);
                self.saved = true;
                SheetFlow::Close
            }
            Err(error) => {
                let raw = error_message(error);
                self.error = Some(self.failure_message(&endpoint, &raw, relay));
                if self.method == "link" && is_auth_failure(&raw) {
                    self.needs_password = true;
                    self.password_link = self.endpoint.text().trim().to_string();
                }
                SheetFlow::Stay
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Alert (original `components/ui/alert.tsx` at size sm)

#[derive(Clone, Copy, PartialEq)]
enum AlertKind {
    Warning,
    Error,
}

/// Transparent with a 1px border at half the accent alpha, 16 radius, 16/12 padding, a 14px
/// glyph 12px before muted text; an optional action button 8px below, indented under the text.
#[allow(clippy::too_many_arguments)]
fn alert(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, kind: AlertKind, text: &str, action: Option<(&str, &str, &str)>, interactive: bool) -> f64 {
    let p = ui.p;
    let accent = if kind == AlertKind::Warning { argb(0xfff59e0b) } else { p.destructive };
    let spec = ui.spec(Font::Regular, 14.0, p.muted);
    let tx = x + 16.0 + 14.0 + 12.0;
    let tw = w - (tx - x) - 16.0;
    let th = ui.painter.paragraph_height(cx, tw, spec, text);
    let mut h = 12.0 + th.max(16.0) + 12.0;
    if action.is_some() {
        h += 2.0 + 8.0 + 32.0;
    }
    ui.painter.outline(cx, rect(x, y, w, h), Vec4::default(), with_alpha(accent, 0.5), 1.0, 16.0);
    let icon = if kind == AlertKind::Warning { "triangle-alert" } else { "circle-x" };
    ui.icon(cx, rect(x + 16.0, y + 12.0 + (th.min(20.0) - 14.0).max(0.0) * 0.5, 14.0, 14.0), icon, 14.0, accent);
    ui.painter.paragraph(cx, dvec2(tx, y + 12.0), tw, spec, text);
    if let Some((id, label, glyph)) = action {
        let bw = ui.button_width(cx, label, Some(glyph), ControlSize::Sm);
        ui.button(cx, rect(tx, y + 12.0 + th + 2.0 + 8.0, bw, 32.0), id, label, Some(glyph), Variant::Outline, ControlSize::Sm, interactive);
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Pair a device

/// Original `PairDeviceModal` / `PairDeviceSection`: loads the host's pairing offer and shows,
/// in order, the disconnected / loading / error states, the relay consent when relay is off,
/// or the QR code with the copyable link and the security warning.
pub struct PairingSheet {
    host_id: String,
    pending: bool,
    loaded: bool,
    error: Option<String>,
    offer: Option<Value>,
    enabling: bool,
    enable_error: Option<String>,
    copied_at: Option<f64>,
    qr: Option<Result<Vec<Vec<bool>>, String>>,
    status: hosts::Status,
    features: Value,
    reload: bool,
}

impl PairingSheet {
    pub fn new(host_id: String) -> Self {
        Self {
            host_id,
            pending: true,
            loaded: false,
            error: None,
            offer: None,
            enabling: false,
            enable_error: None,
            copied_at: None,
            qr: None,
            status: hosts::Status::Disconnected,
            features: Value::Null,
            reload: false,
        }
    }

    fn connected(&self) -> bool {
        self.status == hosts::Status::Connected
    }

    fn load(&mut self, ops: &mut SheetOps) {
        if !self.connected() {
            self.pending = false;
            return;
        }
        self.pending = true;
        self.error = None;
        ops.request(&self.host_id, json!({ "type": "daemon.get_pairing_offer.request" }), "daemon.get_pairing_offer.response", "offer");
    }

    fn supports(&self, feature: &str) -> bool {
        j::boolean(&self.features, feature, false)
    }

    fn state_line(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, text: &str) -> f64 {
        let spec = ui.spec(Font::Regular, 14.0, ui.p.muted);
        let h = ui.painter.paragraph_height(cx, w, spec, text);
        let tw = ui.painter.measure(cx, spec, text).x.min(w);
        ui.painter.paragraph(cx, dvec2(x + (w - tw) * 0.5, y + 24.0), tw, spec, text);
        h + 48.0
    }

    fn consent(&mut self, cx: &mut Cx2d, ui: &mut Ui, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        let circle = rect(x, y, 48.0, 48.0);
        ui.painter.outline(cx, circle, p.surface2, p.border, 1.0, 24.0);
        ui.icon(cx, rect(x + 14.0, y + 14.0, 20.0, 20.0), "shield-check", 20.0, p.accent_bright);
        y += 48.0 + 4.0 + 8.0;
        let title = tr("pairing.device.enableTitle");
        let spec = ui.spec(Font::Medium, 14.0, p.foreground);
        let h = ui.painter.paragraph_height(cx, w, spec, &title);
        ui.painter.paragraph(cx, dvec2(x, y), w, spec, &title);
        y += h + 8.0;
        let description = tr("pairing.device.enableDescription");
        let spec = ui.spec(Font::Regular, 14.0, p.muted);
        let h = ui.painter.paragraph_height(cx, w, spec, &description);
        ui.painter.paragraph(cx, dvec2(x, y), w, spec, &description);
        y += h + 8.0;
        let docs = tr("pairing.device.relayDocs");
        let spec = ui.spec(Font::Regular, 12.0, p.muted);
        let dw = ui.measure(cx, spec, &docs);
        ui.text(cx, rect(x, y, dw, 16.0), spec, &docs);
        ui.icon(cx, rect(x + dw + 4.0, y + 2.0, 12.0, 12.0), "arrow-up-right", 12.0, p.muted);
        if interactive {
            ui.hit_tip(rect(x, y, dw + 16.0, 16.0), "docs", &tr("pairing.device.relayDocsAccessibility"));
        }
        y += 16.0 + 16.0;
        if let Some(error) = self.enable_error.clone() {
            y += alert(ui, cx, x, y, w, AlertKind::Error, &error, None, interactive) + 16.0;
        }
        let can_configure = self.supports("daemonStatusRpc") && self.supports("relayConfig");
        if !can_configure {
            y += alert(ui, cx, x, y, w, AlertKind::Warning, &tr("pairing.device.updateRequired"), None, interactive) + 16.0;
        }
        let not_now = tr("pairing.device.notNow");
        if can_configure {
            let label = tr(if self.enabling {
                "pairing.device.enablingRelay"
            } else if self.enable_error.is_some() {
                "pairing.device.retry"
            } else {
                "pairing.device.enableRelay"
            });
            button_pair(ui, cx, x, y, w, &not_now, &label, None, Variant::Primary, interactive && !self.enabling, 12.0, ControlSize::Md);
            if self.enabling {
                spinner(ui, cx, rect(x + w - 28.0, y + 12.0, 16.0, 16.0), p.on_tertiary);
            }
        } else {
            ui.button(cx, rect(x, y, w, BUTTON_H), "cancel", &not_now, None, Variant::Secondary, ControlSize::Md, interactive);
        }
        y += BUTTON_H + 16.0;
        ui.painter.hline(cx, x, y, w, p.border);
        y += 16.0;
        ui.icon(cx, rect(x, y + 1.0, 14.0, 14.0), "network", 14.0, p.muted);
        let hint = tr("pairing.device.directConnectionHint");
        let spec = ui.spec(Font::Regular, 12.0, p.muted);
        let h = ui.painter.paragraph_height(cx, w - 22.0, spec, &hint);
        ui.painter.paragraph(cx, dvec2(x + 22.0, y), w - 22.0, spec, &hint);
        y += h;
        y - start
    }

    #[allow(clippy::too_many_arguments)]
    fn offer_view(&mut self, cx: &mut Cx2d, ui: &mut Ui, x: f64, y: f64, w: f64, url: &str, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        let hint = tr("pairing.device.hint");
        let spec = ui.spec(Font::Regular, 14.0, p.muted);
        let h = ui.painter.paragraph_height(cx, w, spec, &hint);
        let tw = ui.painter.measure(cx, spec, &hint).x.min(w);
        ui.painter.paragraph(cx, dvec2(x + (w - tw) * 0.5, y), tw, spec, &hint);
        y += h + 16.0;
        // White tile, 12 padding, 12 radius, at most 304 square.
        let side = w.min(304.0);
        let tile = rect(x + (w - side) * 0.5, y, side, side);
        ui.painter.outline(cx, tile, argb(0xffffffff), p.border, 1.0, 12.0);
        let inner = rect(tile.pos.x + 12.0, tile.pos.y + 12.0, side - 24.0, side - 24.0);
        match &self.qr {
            Some(Ok(modules)) => {
                // One-module quiet zone (`margin: 1`).
                let count = modules.len() as f64 + 2.0;
                let cell = inner.size.x / count;
                let origin = dvec2(inner.pos.x + cell, inner.pos.y + cell);
                let black = argb(0xff000000);
                for (my, row) in modules.iter().enumerate() {
                    let mut mx = 0;
                    while mx < row.len() {
                        if !row[mx] {
                            mx += 1;
                            continue;
                        }
                        // Runs of dark modules in a row draw as one rect.
                        let run_start = mx;
                        while mx < row.len() && row[mx] {
                            mx += 1;
                        }
                        let r = rect(origin.x + run_start as f64 * cell, origin.y + my as f64 * cell, (mx - run_start) as f64 * cell + 0.5, cell + 0.5);
                        ui.fill(cx, r, black, 0.0);
                    }
                }
            }
            Some(Err(_)) => {
                let text = tr("pairing.device.qrUnavailable");
                let spec = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text_centered(cx, inner, spec, &text);
            }
            None => spinner(ui, cx, rect(inner.pos.x + inner.size.x * 0.5 - 10.0, inner.pos.y + inner.size.y * 0.5 - 10.0, 20.0, 20.0), p.muted),
        }
        y += side + 16.0;
        // Link box + copy button.
        let copied = self.copied_at.is_some_and(|t| cx.seconds_since_app_start() - t < 2.0);
        if copied {
            crate::shell::sheets::SPINNING.with(|s| s.set(true));
        } else {
            self.copied_at = None;
        }
        let label = tr(if copied { "pairing.device.copied" } else { "pairing.device.copy" });
        let icon = if copied { "check" } else { "copy" };
        let bw = ui.button_width(cx, &label, Some(icon), ControlSize::Sm);
        let box_w = w - bw - 8.0;
        let link_box = rect(x, y, box_w, 34.0);
        ui.painter.outline(cx, link_box, p.surface2, p.border, 1.0, 8.0);
        let spec = ui.spec(Font::Mono, 12.0, p.muted);
        let fitted = ui.painter.fit(cx, spec, url, box_w - 24.0);
        ui.text(cx, rect(x + 12.0, y, box_w - 24.0, 34.0), spec, &fitted);
        ui.button(cx, rect(x + box_w + 8.0, y + 1.0, bw, 32.0), "copy", &label, Some(icon), Variant::Outline, ControlSize::Sm, interactive);
        y += 34.0 + 16.0;
        y += alert(ui, cx, x, y, w, AlertKind::Warning, &tr("pairing.device.securityWarning"), None, interactive);
        y - start
    }
}

impl Sheet for PairingSheet {
    fn title(&self) -> String {
        tr("settings.host.pairDevices.rowTitle")
    }

    fn max_width(&self) -> f64 {
        480.0
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        let host = hosts::host(state, &self.host_id);
        let status = host.map_or(hosts::Status::Disconnected, |h| h.status());
        self.features = host.map(|h| h.features().clone()).unwrap_or(Value::Null);
        // The original query runs once the host is online.
        if status == hosts::Status::Connected && self.status != status && self.offer.is_none() && self.error.is_none() && !self.pending {
            self.reload = true;
        }
        self.status = status;
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        if matches!(self.status, hosts::Status::Disconnected | hosts::Status::Error) {
            return alert(ui, cx, x, y, w, AlertKind::Error, &tr("workspace.terminal.hostDisconnected"), Some(("retry", &tr("pairing.device.retry"), "rotate-cw")), interactive);
        }
        if self.pending {
            return Self::state_line(ui, cx, x, y, w, &tr("pairing.device.loadingOffer"));
        }
        if let Some(error) = self.error.clone() {
            return alert(ui, cx, x, y, w, AlertKind::Error, &error, Some(("retry", &tr("pairing.device.retry"), "rotate-cw")), interactive);
        }
        let offer = self.offer.clone();
        match offer {
            Some(offer) if j::boolean(&offer, "relayEnabled", false) => {
                let url = j::text(&offer, "url").to_string();
                if url.is_empty() {
                    Self::state_line(ui, cx, x, y, w, &tr("pairing.device.unavailable"))
                } else {
                    self.offer_view(cx, ui, x, y, w, &url, interactive)
                }
            }
            _ => self.consent(cx, ui, x, y, w, interactive),
        }
    }

    fn click(&mut self, cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        match id {
            "retry" => self.load(ops),
            "docs" => {
                platform::open_external("https://paseo.sh/docs/security");
            }
            "cancel" => return SheetFlow::Close,
            "primary" => {
                if !self.enabling {
                    self.enabling = true;
                    self.enable_error = None;
                    if !self.supports("relayConfig") {
                        self.enabling = false;
                        self.enable_error = Some(tr("pairing.device.updateRequired"));
                    } else {
                        ops.request(
                            &self.host_id,
                            json!({ "type": "set_daemon_config_request", "config": { "relay": { "enabled": true } } }),
                            "set_daemon_config_response",
                            "enable",
                        );
                    }
                }
            }
            "copy" => {
                if let Some(url) = self.offer.as_ref().map(|o| j::text(o, "url").to_string()) {
                    platform::copy_text(url);
                    self.copied_at = Some(cx.seconds_since_app_start());
                }
            }
            _ => {}
        }
        SheetFlow::Stay
    }

    fn after_event(&mut self, _cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        if !self.loaded || std::mem::take(&mut self.reload) {
            self.loaded = true;
            self.load(ops);
        }
        SheetFlow::Stay
    }

    fn on_response(&mut self, _cx: &mut Cx, tag: &str, result: &Result<Value, CoreError>, ops: &mut SheetOps) -> SheetFlow {
        let failure = response_message(result);
        match tag {
            "offer" => {
                self.pending = false;
                match failure {
                    Some(error) => self.error = Some(error),
                    None => {
                        let offer = result.as_ref().ok().cloned().unwrap_or(Value::Null);
                        let url = j::text(&offer, "url").to_string();
                        self.qr = (!url.is_empty()).then(|| qr_code::encode(&url));
                        self.offer = Some(offer);
                    }
                }
            }
            "enable" => match failure {
                Some(error) => {
                    self.enabling = false;
                    self.enable_error = Some(error);
                }
                None => {
                    self.enabling = false;
                    self.load(ops);
                }
            },
            _ => {}
        }
        SheetFlow::Stay
    }
}

// ---------------------------------------------------------------------------------------------
// QR scan (mobile)

#[derive(Clone, Copy, PartialEq)]
enum ScanStatus {
    Scanning,
    Pairing,
    Stopped,
}

/// Original `app/pair-scan.tsx` + `pair-scan-model.ts`: the scan screen pairs each scanned link
/// itself (host confirmation, then connect), stays open on failure with "Scan QR code" again,
/// and asks for the host password in the pairing-link form when the host requires one.
pub struct ScanSheet {
    view: Option<WidgetRef>,
    status: ScanStatus,
    error: Option<String>,
    scanned: Option<String>,
    /// The scanned config being confirmed or probed.
    candidate: Option<(Value, String)>,
    state: Arc<Value>,
    /// Height below the back header (the camera view's `Expanded`).
    body_height: f64,
}

impl Default for ScanSheet {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanSheet {
    pub fn new() -> Self {
        Self { view: None, status: ScanStatus::Scanning, error: None, scanned: None, candidate: None, state: Arc::new(Value::Null), body_height: 0.0 }
    }

    fn pair(&mut self, link: String, ops: &mut SheetOps) -> SheetFlow {
        self.status = ScanStatus::Pairing;
        // A saved connection for the same link (an earlier failed attempt) is reused.
        let existing = hosts::hosts(&self.state)
            .iter()
            .flat_map(|h| h.connections().iter())
            .find(|c| j::text(c, "endpoint").trim() == link)
            .cloned();
        let text = |key: &str| existing.as_ref().map(|c| c.get(key).cloned().unwrap_or(Value::Null)).unwrap_or(Value::Null);
        let id = existing.as_ref().map(|c| j::text(c, "id").to_string()).filter(|id| !id.is_empty()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut config = Map::new();
        config.insert("id".into(), Value::from(id));
        config.insert("label".into(), existing.as_ref().map_or(Value::from(""), |c| Value::from(j::text(c, "label"))));
        config.insert("endpoint".into(), Value::from(link.clone()));
        for key in ["password", "expectedServerId", "color"] {
            let value = text(key);
            if !value.is_null() {
                config.insert(key.into(), value);
            }
        }
        let config = Value::Object(config);
        let info = match inspect_host_config(&config) {
            Ok(info) => info,
            Err(error) => return self.failed(error.message),
        };
        self.candidate = Some((config, link));
        match host_confirmation(&self.state, &info, ops.origin) {
            Some(confirm) => SheetFlow::Push(Box::new(confirm)),
            None => {
                self.probe(ops);
                SheetFlow::Stay
            }
        }
    }

    fn probe(&mut self, ops: &mut SheetOps) {
        if let Some((config, _)) = &self.candidate {
            let request = ops.core.probe_host(config.clone());
            ops.track(request, "probe");
        }
    }

    fn failed(&mut self, message: String) -> SheetFlow {
        self.status = ScanStatus::Stopped;
        self.error = (!message.is_empty()).then_some(message);
        self.candidate = None;
        SheetFlow::Stay
    }

    fn view(&mut self, cx: &mut Cx) -> WidgetRef {
        if self.view.is_none() {
            let view = WidgetRef::new_from_ptr(cx, crate::shell::sheets::qr_ptr());
            view.as_qr_scan_view().start(cx);
            self.view = Some(view);
        }
        self.view.clone().unwrap()
    }

    /// Original `permissionCard`: surface2, 12 radius, 24 padding, 16 gap, blue pill action.
    /// Within `avail` (the full-screen body height, 0 when unknown) the body text is clipped
    /// first so the action stays reachable.
    #[allow(clippy::too_many_arguments)]
    fn card(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, avail: f64, title: &str, body: &str, action: &str, interactive: bool) -> f64 {
        let p = ui.p;
        let title_spec = ui.spec(Font::Medium, 14.0, p.foreground);
        let body_spec = ui.spec(Font::Regular, 14.0, p.muted);
        let th = ui.painter.paragraph_height(cx, w - 48.0, title_spec, title);
        let chrome = 24.0 + 24.0 + th + 16.0 + 16.0 + 44.0 + 24.0;
        let bh = ui.painter.paragraph_height(cx, w - 48.0, body_spec, body);
        let bh = if avail > 0.0 { bh.min((avail - chrome).max(0.0)) } else { bh };
        let action_spec = ui.spec(Font::Medium, 14.0, argb(0xffffffff));
        let aw = ui.measure(cx, action_spec, action) + 48.0;
        let h = 24.0 + th + 16.0 + bh + 16.0 + 44.0 + 24.0;
        let top = y + 24.0;
        ui.fill(cx, rect(x, top, w, h), p.surface2, 12.0);
        ui.painter.paragraph(cx, dvec2(x + 24.0, top + 24.0), w - 48.0, title_spec, title);
        cx.begin_turtle(Walk::abs_rect(rect(x + 24.0, top + 24.0 + th + 16.0, w - 48.0, bh)), Layout { clip_x: true, clip_y: true, ..Layout::default() });
        ui.painter.paragraph(cx, dvec2(x + 24.0, top + 24.0 + th + 16.0), w - 48.0, body_spec, body);
        cx.end_turtle();
        let button = rect(x + 24.0, top + 24.0 + th + 16.0 + bh + 16.0, aw, 44.0);
        ui.fill(cx, button, argb(0xff3b82f6), 8.0);
        ui.text_centered(cx, button, action_spec, action);
        if interactive {
            ui.hit(button, "card".to_string());
        }
        24.0 + h
    }
}

impl Sheet for ScanSheet {
    fn title(&self) -> String {
        tr("pairing.scan.title")
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        self.state = Arc::new(state.clone());
    }

    /// Original: `Navigator.push(MaterialPageRoute(MobileQrScanner))`, a full-screen route.
    fn style(&self) -> SheetStyle {
        SheetStyle::FullScreen
    }

    fn set_body_height(&mut self, height: f64) {
        self.body_height = height;
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let view = self.view(cx);
        let state = view.as_qr_scan_view().state();
        match state {
            QrScanState::Unavailable => {
                return Self::card(ui, cx, x, y, w, self.body_height, &tr("pairing.scan.webUnavailableTitle"), &tr("pairing.scan.webUnavailableBody"), &tr("pairing.scan.backToSettings"), interactive);
            }
            QrScanState::PermissionDenied => {
                return Self::card(ui, cx, x, y, w, self.body_height, &tr("pairing.scan.cameraPermissionTitle"), &tr("pairing.scan.cameraPermissionBody"), &tr("pairing.scan.grantPermission"), interactive);
            }
            QrScanState::Failed(error) => {
                return Self::card(ui, cx, x, y, w, self.body_height, &tr("pairing.scan.errorTitle"), &error, &tr("common.actions.retry"), interactive);
            }
            _ => {}
        }
        // Camera (`Expanded`): surface2 with 12 radius, the 260px frame of four 36px accent corners.
        let h = if self.body_height > 0.0 { self.body_height } else { (w * 1.2).clamp(320.0, 520.0) };
        let area = rect(x, y, w, h);
        ui.fill(cx, area, p.surface2, 12.0);
        draw_widget(cx, scope, &view, area);
        let helper = ui.spec(Font::Regular, 14.0, p.foreground);
        // Below the frame: the pairing line, or the error and the scan-again button. In a short
        // body the frame shrinks (down to 96px), then the error text is clipped, so the button
        // always stays inside the body.
        const MIN_SIDE: f64 = 96.0;
        let error = self.error.clone().filter(|_| self.status == ScanStatus::Stopped);
        let eh = error.as_ref().map_or(0.0, |error| ui.painter.paragraph_height(cx, w - 32.0, helper, error));
        let below = |eh: f64| match self.status {
            ScanStatus::Scanning => 0.0,
            ScanStatus::Stopped => 24.0 + if error.is_some() { eh + 24.0 } else { 0.0 } + BUTTON_H,
            _ => 24.0 + 20.0,
        };
        let eh = eh.min((eh + h - 16.0 - MIN_SIDE - below(eh)).max(0.0));
        let side = 260.0_f64.min(w - 16.0).min(h - 16.0 - below(eh)).max(0.0);
        let content_h = side + below(eh);
        let fy = y + ((h - content_h) * 0.5).max(0.0);
        let frame = rect(x + (w - side) * 0.5, fy, side, side);
        let (l, t, r, b) = (frame.pos.x, frame.pos.y, frame.pos.x + side, frame.pos.y + side);
        for (cxp, cyp, dx, dy) in [(l, t, 1.0, 1.0), (r, t, -1.0, 1.0), (l, b, 1.0, -1.0), (r, b, -1.0, -1.0)] {
            let hx = if dx > 0.0 { cxp } else { cxp - 36.0 };
            let vy = if dy > 0.0 { cyp } else { cyp - 36.0 };
            ui.fill(cx, rect(hx, if dy > 0.0 { cyp } else { cyp - 4.0 }, 36.0, 4.0), p.accent, 2.0);
            ui.fill(cx, rect(if dx > 0.0 { cxp } else { cxp - 4.0 }, vy, 4.0, 36.0), p.accent, 2.0);
        }
        let mut ty = frame.pos.y + side + 24.0;
        if self.status == ScanStatus::Pairing {
            ui.text_centered(cx, rect(x, ty, w, 20.0), helper, &tr("pairing.scan.pairing"));
        }
        if self.status == ScanStatus::Stopped {
            if let Some(error) = error {
                let ew = ui.painter.measure(cx, helper, &error).x.min(w - 32.0);
                cx.begin_turtle(Walk::abs_rect(rect(x, ty, w, eh)), Layout { clip_x: true, clip_y: true, ..Layout::default() });
                ui.painter.paragraph(cx, dvec2(x + (w - ew) * 0.5, ty), ew, helper, &error);
                cx.end_turtle();
                ty += eh + 24.0;
            }
            let label = tr("pairing.connectionMethods.scanQr.title");
            let bw = ui.button_width(cx, &label, None, ControlSize::Md);
            ui.button(cx, rect(x + (w - bw) * 0.5, ty, bw, BUTTON_H), "again", &label, None, Variant::Secondary, ControlSize::Md, interactive);
        }
        h
    }

    fn click(&mut self, cx: &mut Cx, id: &str, _ops: &mut SheetOps) -> SheetFlow {
        let view = self.view.clone();
        match id {
            "card" => {
                let Some(view) = view else { return SheetFlow::Stay };
                match view.as_qr_scan_view().state() {
                    QrScanState::Unavailable => return SheetFlow::Close,
                    _ => view.as_qr_scan_view().resume_scanning(cx),
                }
            }
            "again" if self.status == ScanStatus::Stopped => {
                self.status = ScanStatus::Scanning;
                self.error = None;
                if let Some(view) = view {
                    view.as_qr_scan_view().resume_scanning(cx);
                }
            }
            _ => {}
        }
        SheetFlow::Stay
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> SheetFlow {
        if let Some(view) = self.view.clone() {
            view.handle_event(cx, event, scope);
        }
        if let Event::Actions(actions) = event {
            for action in platform::actions(actions) {
                if let PlatformAction::Event(PlatformEvent::QrScanned { link }) = action
                    && self.status == ScanStatus::Scanning
                {
                    self.scanned = Some(link);
                }
            }
        }
        SheetFlow::Stay
    }

    fn after_event(&mut self, _cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        match self.scanned.take() {
            Some(link) => self.pair(link, ops),
            None => SheetFlow::Stay,
        }
    }

    fn on_deliver(&mut self, _cx: &mut Cx, tag: &str, value: &Value, ops: &mut SheetOps) -> SheetFlow {
        if tag == "confirm" {
            if value.as_bool() == Some(true) {
                self.probe(ops);
            } else {
                return self.failed(String::new());
            }
        }
        SheetFlow::Stay
    }

    fn on_response(&mut self, cx: &mut Cx, tag: &str, result: &Result<Value, CoreError>, ops: &mut SheetOps) -> SheetFlow {
        if tag != "probe" {
            return SheetFlow::Stay;
        }
        let Some((config, link)) = self.candidate.take() else { return SheetFlow::Stay };
        match result {
            Ok(_) => {
                ops.core.connect_host(config);
                if let Some(view) = &self.view {
                    view.as_qr_scan_view().stop(cx);
                }
                SheetFlow::Close
            }
            Err(error) => {
                let message = error_message(error);
                if is_auth_failure(&message) {
                    if let Some(view) = &self.view {
                        view.as_qr_scan_view().stop(cx);
                    }
                    return SheetFlow::Replace(Box::new(ConnectionSheet::password_prompt(link, j::text(&config, "id").to_string())));
                }
                self.failed(message)
            }
        }
    }

    fn closed(&mut self, cx: &mut Cx, _ops: &mut SheetOps) {
        if let Some(view) = &self.view {
            view.as_qr_scan_view().stop(cx);
        }
    }
}
