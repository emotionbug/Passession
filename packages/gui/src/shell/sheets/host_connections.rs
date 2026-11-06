//! Port of `features/navigation/host_connections_dialog.dart`: connection labels, the
//! searchable host picker (above ten hosts), the host chooser (`HostChooserModal`) and a
//! host's connections sheet (use / edit / remove / add a connection).

use makepad_widgets::*;
use paseo_gui_core::inspect_host_config;
use serde_json::{Value, json};

use crate::common::paint::{Font, rect};
use crate::common::{json as j, tr, tr_args};
use crate::shell::commands::{self, HostPurpose};
use crate::shell::hosts::{self, Host};
use crate::shell::sheets::{ConfirmSheet, Field, FieldEvent, Sheet, SheetFlow, SheetOps, SheetStyle, host_dialog};
use crate::shell::ui::{Ui, Variant};
use crate::shell::{PaseoShell, sidebar};
use crate::theme::ControlSize;

/// Original `formatHostConnectionLabel`: "TCP (endpoint)", "Relay (…)", "Local (path)",
/// "Remote SSH (host)".
pub fn connection_label(config: &Value) -> String {
    match inspect_host_config(config) {
        Ok(info) => {
            let endpoint = match info.port {
                Some(port) if !(info.scheme == "wss" && port == 443 || info.scheme == "ws" && port == 80) => format!("{}:{port}", info.host),
                _ => info.host.clone(),
            };
            if info.is_relay {
                format!("{} ({endpoint})", tr("settings.host.badges.relay"))
            } else if info.scheme == "unix" || info.scheme == "pipe" {
                format!("{} ({})", tr("settings.host.badges.local"), info.host)
            } else if info.scheme == "ssh" {
                format!("{} ({})", tr("settings.host.badges.remoteSsh"), info.host)
            } else {
                format!("TCP ({endpoint})")
            }
        }
        Err(_) => "Invalid endpoint — edit this connection".to_string(),
    }
}

/// Original `formatActiveConnectionLabel`: sockets/pipes read as "Local"; standard web ports
/// are dropped from the address.
pub fn active_connection_label(config: &Value) -> String {
    match inspect_host_config(config) {
        Ok(info) if info.scheme == "unix" || info.scheme == "pipe" => "Local".to_string(),
        Ok(info) => match info.port {
            Some(port) if port != 443 && port != 80 => format!("{}:{port}", info.host),
            _ => info.host,
        },
        Err(_) => String::new(),
    }
}

fn host_status_color(ui: &Ui, host: &Host<'_>) -> Vec4 {
    sidebar::status_color(&ui.p, host)
}

// ---------------------------------------------------------------------------------------------
// Host chooser / searchable picker

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `showHostChooser`: choose a host for `purpose`.
    Chooser,
    /// Sidebar `HostPicker` above ten hosts: a host, its settings, or Add host.
    Picker,
}

pub struct ChooserSheet {
    title: String,
    ids: Vec<String>,
    purpose: Option<HostPurpose>,
    mode: Mode,
    search: Field,
    query: String,
    active: usize,
    state: std::sync::Arc<Value>,
    /// Enter in the search field: choose the active row with the next ops.
    submit: bool,
}

impl ChooserSheet {
    pub fn new(title: String, ids: Vec<String>, purpose: HostPurpose) -> Self {
        Self {
            title,
            ids,
            purpose: Some(purpose),
            mode: Mode::Chooser,
            search: Field::new("", "Search hosts...").autofocus(),
            query: String::new(),
            active: 0,
            state: std::sync::Arc::new(Value::Null),
            submit: false,
        }
    }

    pub fn picker(ids: Vec<String>) -> Self {
        Self {
            title: "Host".to_string(),
            ids,
            purpose: None,
            mode: Mode::Picker,
            search: Field::new("", "Search hosts"),
            query: String::new(),
            active: 0,
            state: std::sync::Arc::new(Value::Null),
            submit: false,
        }
    }

    fn options(&self) -> Vec<String> {
        let query = self.query.trim().to_lowercase();
        self.ids
            .iter()
            .filter(|id| {
                if query.is_empty() {
                    return true;
                }
                let label = hosts::host(&self.state, id).map(|h| h.display_label()).unwrap_or_default().to_lowercase();
                label.contains(&query) || (self.mode == Mode::Chooser && id.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    }

    fn choose(&mut self, id: String, ops: &mut SheetOps) -> SheetFlow {
        match self.mode {
            Mode::Chooser => {
                if let Some(purpose) = self.purpose.take() {
                    ops.run(move |shell, cx, ui| commands::host_chosen(shell, cx, ui, purpose, id));
                }
            }
            Mode::Picker => ops.run(move |shell, cx, _| commands::open_host(shell, cx, &id)),
        }
        SheetFlow::Close
    }
}

impl Sheet for ChooserSheet {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn max_width(&self) -> f64 {
        if self.mode == Mode::Chooser { 640.0 } else { 400.0 }
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        self.state = std::sync::Arc::new(state.clone());
    }

    /// The compact host picker is the original mobile `Combobox` sheet (60%/90% snap, Material
    /// drag handle); the chooser keeps the adaptive sheet.
    fn style(&self) -> SheetStyle {
        if self.mode == Mode::Picker { SheetStyle::Draggable { material_handle: true } } else { SheetStyle::Adaptive }
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        // Draggable frame: `x`/`w` are the list's content box; rows span it.
        let draggable = ui.compact && self.mode == Mode::Picker;
        let (rx, rw) = if draggable { (x, w) } else { (x - 12.0, w + 24.0) };
        // `_HostPickerList` shows "Search hosts" only past ten hosts; the chooser always does.
        if self.mode == Mode::Chooser || self.ids.len() > 10 {
            let fh = if ui.compact { 44.0 } else { 32.0 };
            self.search.draw(cx, scope, rect(x, y, w, fh));
            y += fh + 8.0;
        }
        let options = self.options();
        if options.is_empty() {
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            ui.text(cx, rect(x + 16.0 - 24.0 + 12.0, y + 12.0, w, 20.0), spec, "No matching hosts");
            y += 44.0;
        }
        let active = self.active.min(options.len().saturating_sub(1));
        for (index, id) in options.iter().enumerate() {
            let Some(host) = hosts::host(&self.state, id) else { continue };
            let row_id = format!("host:{index}");
            match self.mode {
                Mode::Chooser => {
                    // `_HostChooserRow`: 56px, 16/12 padding, dot in an 18px slot, label over
                    // server id, server glyph.
                    let r = rect(x - 24.0, y, w + 48.0, 56.0);
                    if index == active || ui.is_hover(&row_id) || ui.is_pressed(&row_id) {
                        ui.fill(cx, r, p.surface1, 0.0);
                    }
                    let dot = host_status_color(ui, &host);
                    ui.dot(cx, dvec2(r.pos.x + 16.0 + 9.0, r.pos.y + 28.0), 8.0, dot);
                    let tx = r.pos.x + 16.0 + 18.0 + 12.0;
                    let tw = r.size.x - (tx - r.pos.x) - 16.0 - 14.0 - 12.0;
                    let label_spec = ui.spec(Font::Regular, 14.0, p.foreground);
                    ui.text(cx, rect(tx, r.pos.y + 10.0, tw, 18.0), label_spec, &host.display_label());
                    let id_spec = ui.spec(Font::Regular, 12.0, p.muted);
                    ui.text(cx, rect(tx, r.pos.y + 30.0, tw, 16.0), id_spec, id);
                    ui.icon(cx, rect(r.pos.x + r.size.x - 16.0 - 14.0, r.pos.y + 21.0, 14.0, 14.0), "server", 14.0, p.muted);
                    if interactive {
                        ui.hit(r, row_id);
                    }
                    y += 56.0;
                }
                Mode::Picker => {
                    // `ComboboxItem`: 36px, 12/8 insets, dot, label + inline description, gear.
                    let r = rect(rx, y, rw, 36.0);
                    if ui.is_hover(&row_id) || ui.is_pressed(&row_id) {
                        ui.fill(cx, r, p.surface1, 0.0);
                    }
                    let dot = host_status_color(ui, &host);
                    ui.dot(cx, dvec2(r.pos.x + 12.0 + 8.0, r.pos.y + 18.0), 8.0, dot);
                    let tx = r.pos.x + 12.0 + 16.0 + 8.0;
                    let gear = rect(r.pos.x + r.size.x - 12.0 - 24.0, r.pos.y + 6.0, 24.0, 24.0);
                    let label_spec = ui.spec(Font::Regular, 14.0, p.foreground);
                    let label = host.display_label();
                    let available = gear.pos.x - 8.0 - tx;
                    let lw = ui.measure(cx, label_spec, &label).min(available);
                    ui.text(cx, rect(tx, r.pos.y, lw, 36.0), label_spec, &label);
                    let description = active_connection_label(host.config());
                    if !description.is_empty() && available - lw > 24.0 {
                        let spec = ui.spec(Font::Regular, 12.0, p.muted);
                        ui.text(cx, rect(tx + lw + 8.0, r.pos.y, available - lw - 8.0, 36.0), spec, &description);
                    }
                    if interactive {
                        ui.hit(r, row_id);
                    }
                    let tip = format!("Open {label} settings");
                    let gear_id = if interactive { format!("gear:{index}") } else { String::new() };
                    ui.icon_button(cx, gear, &gear_id, "settings", 16.0, 6.0, &tip);
                    y += 36.0;
                }
            }
        }
        if self.mode == Mode::Picker {
            let r = rect(rx, y, rw, 36.0);
            if ui.is_hover("add") || ui.is_pressed("add") {
                ui.fill(cx, r, p.surface1, 0.0);
            }
            ui.icon(cx, rect(r.pos.x + 12.0, r.pos.y + 10.0, 16.0, 16.0), "plus", 16.0, p.muted);
            let spec = ui.spec(Font::Regular, 14.0, p.foreground);
            // Original SYSTEM_HOST_PICKER_OPTION_LABELS.add is an untranslated literal.
            ui.text(cx, rect(r.pos.x + 12.0 + 16.0 + 8.0, r.pos.y, r.size.x - 48.0, 36.0), spec, "Add host");
            if interactive {
                ui.hit(r, "add");
            }
            y += 36.0;
        }
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        let options = self.options();
        if id == "add" {
            ops.run(|shell, cx, _| commands::open_host(shell, cx, "add"));
            return SheetFlow::Close;
        }
        if let Some(host) = id.strip_prefix("gear:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| options.get(i).cloned()) {
            return self.choose(format!("settings:{host}"), ops);
        }
        if let Some(host) = id.strip_prefix("host:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| options.get(i).cloned()) {
            return self.choose(host, ops);
        }
        SheetFlow::Stay
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> SheetFlow {
        match self.search.handle_event(cx, event, scope) {
            Some(FieldEvent::Changed(text)) => {
                self.query = text;
                self.active = 0;
            }
            Some(FieldEvent::Returned(_)) if self.mode == Mode::Chooser => self.submit = true,
            _ => {}
        }
        if self.mode == Mode::Chooser
            && let Event::KeyDown(key) = event
        {
            let count = self.options().len();
            if count > 0 {
                match key.key_code {
                    KeyCode::ArrowDown => self.active = (self.active.min(count - 1) + 1) % count,
                    KeyCode::ArrowUp => self.active = (self.active.min(count - 1) + count - 1) % count,
                    _ => {}
                }
            }
        }
        SheetFlow::Stay
    }

    fn after_event(&mut self, _cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        if !std::mem::take(&mut self.submit) {
            return SheetFlow::Stay;
        }
        let options = self.options();
        match options.get(self.active.min(options.len().saturating_sub(1))).cloned() {
            Some(id) => self.choose(id, ops),
            None => SheetFlow::Stay,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Host connections

pub struct ConnectionsSheet {
    host_id: String,
    busy: bool,
    error: Option<String>,
    state: std::sync::Arc<Value>,
    pending_remove: Option<Value>,
}

impl ConnectionsSheet {
    pub fn new(host_id: String) -> Self {
        Self { host_id, busy: false, error: None, state: std::sync::Arc::new(Value::Null), pending_remove: None }
    }

    fn connections(&self) -> (Vec<Value>, String) {
        hosts::host(&self.state, &self.host_id)
            .map(|h| (h.connections().to_vec(), j::text(h.config(), "id").to_string()))
            .unwrap_or_default()
    }
}

impl Sheet for ConnectionsSheet {
    fn title(&self) -> String {
        tr("settings.host.connections.title")
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        self.state = std::sync::Arc::new(state.clone());
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        let host = hosts::host(&self.state, &self.host_id);
        if let Some(host) = host {
            let spec = ui.spec(Font::Regular, 12.0, p.muted);
            ui.text(cx, rect(x, y - 12.0, w, 16.0), spec, &host.display_label());
            y += 8.0;
        }
        if let Some(error) = &self.error {
            let spec = ui.spec(Font::Regular, 14.0, p.destructive);
            let h = ui.painter.paragraph_height(cx, w, spec, error);
            ui.painter.paragraph(cx, dvec2(x, y), w, spec, error);
            y += h + 16.0;
        }
        let (connections, active) = self.connections();
        let enabled = interactive && !self.busy;
        // One bordered surface1 card, rows divided by hairlines.
        let row_h = 56.0;
        let card = rect(x, y, w, row_h * connections.len().max(1) as f64);
        ui.painter.outline(cx, card, p.surface1, p.border, 1.0, 12.0);
        for (index, connection) in connections.iter().enumerate() {
            let r = rect(x, y, w, row_h);
            if index > 0 {
                ui.painter.hline(cx, x, y, w, p.border);
            }
            let is_active = j::text(connection, "id") == active;
            let remove_label = tr("settings.host.connections.removeAction");
            let remove_spec = ui.spec(Font::Medium, 14.0, p.destructive);
            let rw = ui.measure(cx, remove_spec, &remove_label) + 16.0;
            let remove = rect(r.pos.x + r.size.x - 8.0 - rw, r.pos.y + 12.0, rw, 32.0);
            let mut right = remove.pos.x - 8.0;
            let use_rect = if !is_active {
                let spec = ui.spec(Font::Medium, 14.0, p.accent);
                let uw = ui.measure(cx, spec, "Use") + 16.0;
                let ur = rect(right - uw, r.pos.y + 12.0, uw, 32.0);
                right = ur.pos.x - 8.0;
                Some((ur, spec))
            } else {
                None
            };
            let row_id = format!("edit:{index}");
            if enabled && (ui.is_hover(&row_id) || ui.is_pressed(&row_id)) {
                ui.fill(cx, crate::common::paint::inset(r, 1.0, 1.0), p.surface2, 11.0);
            }
            if enabled {
                ui.hit(r, row_id);
            }
            let label_spec = ui.spec(Font::Regular, 14.0, p.foreground);
            let tw = right - (r.pos.x + 16.0);
            if is_active {
                ui.text(cx, rect(r.pos.x + 16.0, r.pos.y + 10.0, tw, 20.0), label_spec, &connection_label(connection));
                let spec = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(cx, rect(r.pos.x + 16.0, r.pos.y + 30.0, tw, 16.0), spec, "Active");
            } else {
                ui.text(cx, rect(r.pos.x + 16.0, r.pos.y, tw, row_h), label_spec, &connection_label(connection));
            }
            if let Some((ur, spec)) = use_rect {
                let id = format!("use:{index}");
                if enabled && ui.is_hover(&id) {
                    ui.fill(cx, ur, p.surface2, 8.0);
                }
                ui.text_centered(cx, ur, spec, "Use");
                if enabled {
                    ui.hit(ur, id);
                }
            }
            let id = format!("remove:{index}");
            if enabled && ui.is_hover(&id) {
                ui.fill(cx, remove, p.surface2, 8.0);
            }
            ui.text_centered(cx, remove, remove_spec, &remove_label);
            if enabled {
                ui.hit(remove, id);
            }
            y += row_h;
        }
        if connections.is_empty() {
            y += row_h;
        }
        y += 16.0;
        if self.busy {
            let t = (cx.seconds_since_app_start() % 1.5) / 1.5;
            ui.fill(cx, rect(x, y, w, 2.0), p.surface2, 0.0);
            ui.fill(cx, rect(x + w * t * 0.7, y, w * 0.3, 2.0), p.accent, 0.0);
            crate::shell::sheets::SPINNING.with(|s| s.set(true));
            y += 2.0 + 16.0;
        }
        if host.is_some() {
            let label = tr("pairing.connectionMethods.title");
            let bw = ui.button_width(cx, &label, Some("plus"), ControlSize::Sm);
            ui.button(cx, rect(x, y, bw, 32.0), "add", &label, Some("plus"), Variant::Outline, ControlSize::Sm, enabled);
            y += 32.0;
        }
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        let (connections, _) = self.connections();
        let at = |prefix: &str| id.strip_prefix(prefix).and_then(|i| i.parse::<usize>().ok()).and_then(|i| connections.get(i).cloned());
        if id == "add" {
            let Some(host) = hosts::host(&self.state, &self.host_id) else { return SheetFlow::Stay };
            let seed = json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "label": host.label(),
                "endpoint": "",
                "expectedServerId": host.server_id(),
                "color": host.config().get("color").cloned().unwrap_or(Value::Null),
            });
            ops.run(move |shell, cx, _| host_dialog::open_flow(shell, cx, None, Some(seed), None));
        } else if let Some(connection) = at("edit:") {
            ops.run(move |shell, cx, _| host_dialog::open_flow(shell, cx, None, Some(connection), None));
        } else if let Some(connection) = at("use:") {
            // `updateHost` with that connection makes it active.
            ops.core.connect_host(connection);
        } else if let Some(connection) = at("remove:") {
            let message = tr_args("settings.host.connections.removeMessage", &[("name", &connection_label(&connection))]);
            self.pending_remove = Some(connection);
            let confirm = ConfirmSheet::new(
                ops,
                &tr("settings.host.connections.removeTitle"),
                &message,
                &tr("settings.host.connections.removeAction"),
                true,
                "remove",
            );
            return SheetFlow::Push(Box::new(confirm));
        }
        SheetFlow::Stay
    }

    fn on_deliver(&mut self, _cx: &mut Cx, tag: &str, value: &Value, ops: &mut SheetOps) -> SheetFlow {
        let Some(connection) = self.pending_remove.take() else { return SheetFlow::Stay };
        if tag != "remove" || value.as_bool() != Some(true) {
            return SheetFlow::Stay;
        }
        let connections = self.connections().0;
        if connections.len() <= 1 {
            // The last connection removes the host: tab close guards run first (Flutter
            // `removeConnection` → `removeHost`).
            let host_id = self.host_id.clone();
            ops.effect(crate::common::UiEffect::RemoveHost { host_id });
            return SheetFlow::Close;
        }
        ops.core.remove_connection(&self.host_id, j::text(&connection, "id"));
        SheetFlow::Stay
    }
}
