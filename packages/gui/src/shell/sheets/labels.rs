//! Port of `features/navigation/workspace_labels_dialog.dart`: the workspace label menu —
//! assign (multi-select), create, and manage (search, edit, delete) pages of one host's label
//! catalog. Opened for a workspace (pick page) or as the standalone manager (display menu
//! "Manage labels…").

use std::collections::HashSet;

use makepad_widgets::*;
use paseo_gui_core::CoreError;
use serde_json::{Value, json};

use crate::common::paint::{Font, rect};
use crate::common::{json as j, tr, tr_args};
use crate::shell::hosts;
use crate::shell::sheets::{ConfirmSheet, Field, FieldEvent, Sheet, SheetFlow, SheetOps, hint, row, row_leading, row_trailing, separator, spinner};
use crate::shell::ui::Ui;
use crate::shell::response_message;
use crate::theme::{Palette, argb};

/// Original label identity colors (`styles/identity-colors.ts`).
const LABEL_COLORS: [&str; 10] = ["violet", "sky", "emerald", "orange", "pink", "indigo", "teal", "red", "amber", "blue"];

/// Foreground variant of a label identity color; muted for unknown names.
pub fn label_color(p: &Palette, name: &str) -> Vec4 {
    let (dark, light) = match name {
        "violet" => (0xffa392d5, 0xff6d49b5),
        "sky" => (0xff6aa6ce, 0xff3d6985),
        "emerald" => (0xff6cae96, 0xff3e6e5d),
        "orange" => (0xffcc8f64, 0xff845838),
        "pink" => (0xffd87da3, 0xff974168),
        "indigo" => (0xff9299d5, 0xff5251c2),
        "teal" => (0xff6cacab, 0xff3e6d6c),
        "red" => (0xffd88381, 0xff9c4243),
        "amber" => (0xffb29d64, 0xff716239),
        "blue" => (0xff7ba1d5, 0xff39649e),
        _ => return p.muted,
    };
    argb(if p.is_dark { dark } else { light })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Pick,
    Create,
    Manage,
    Edit,
}

pub struct LabelsSheet {
    host_id: String,
    workspace: Option<Value>,
    labels: Vec<Value>,
    assigned: HashSet<String>,
    pending: HashSet<String>,
    busy: bool,
    error: Option<String>,
    page: Page,
    editing: Option<Value>,
    name: Field,
    search: Field,
    color: String,
    query: String,
    offline: bool,
    loaded: bool,
    /// Enter pressed in the name field; submitted with the next ops.
    submit_pending: bool,
}

impl LabelsSheet {
    pub fn new(host_id: String, workspace: Option<Value>) -> Self {
        let assigned = workspace
            .as_ref()
            .map(|w| j::array(w, "labels").iter().map(|l| l.as_str().map_or_else(|| l.to_string(), str::to_string)).collect())
            .unwrap_or_default();
        let page = if workspace.is_some() { Page::Pick } else { Page::Manage };
        Self {
            host_id,
            workspace,
            labels: Vec::new(),
            assigned,
            pending: HashSet::new(),
            busy: true,
            error: None,
            page,
            editing: None,
            name: Field::new("", &tr("workspaceLabels.name")).autofocus(),
            search: Field::new("", &tr("workspaceLabels.manage.search")),
            color: "violet".to_string(),
            query: String::new(),
            offline: false,
            loaded: false,
            submit_pending: false,
        }
    }

    fn load(&self, ops: &mut SheetOps) {
        ops.request(&self.host_id, json!({ "type": "workspace.label.list.request" }), "workspace.label.list.response", "list");
    }

    fn open(&mut self, cx: &mut Cx, page: Page, label: Option<Value>) {
        self.page = page;
        self.error = None;
        let (name, color) = label.as_ref().map_or((String::new(), "violet".to_string()), |l| (j::text(l, "name").to_string(), j::text(l, "color").to_string()));
        self.editing = label;
        self.name.set_text(cx, &name);
        self.name.set_placeholder(cx, &tr(if page == Page::Edit { "workspaceLabels.manage.name" } else { "workspaceLabels.name" }));
        self.color = if color.is_empty() { "violet".to_string() } else { color };
        if matches!(page, Page::Create | Page::Edit) {
            self.name.focus(cx);
        }
    }

    fn normalized_name(&self) -> String {
        self.name.text().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn assign(&mut self, ops: &mut SheetOps, name: &str, color: &str, assigned: bool) {
        let Some(workspace) = &self.workspace else { return };
        self.pending.insert(name.to_lowercase());
        self.error = None;
        ops.request(
            &self.host_id,
            json!({
                "type": "workspace.label.assignment.set.request",
                "workspaceId": j::text(workspace, "id"),
                "label": { "name": name, "color": color },
                "assigned": assigned,
            }),
            "workspace.label.assignment.set.response",
            &format!("assign:{}", name.to_lowercase()),
        );
    }

    fn create(&mut self, ops: &mut SheetOps) {
        let name = self.normalized_name();
        if name.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        let color = self.color.clone();
        self.assign(ops, &name, &color, true);
    }

    fn save(&mut self, ops: &mut SheetOps) {
        let Some(label) = self.editing.clone() else { return };
        let name = self.normalized_name();
        if name.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        ops.request(
            &self.host_id,
            json!({ "type": "workspace.label.update.request", "name": j::text(&label, "name"), "newName": name, "color": self.color }),
            "workspace.label.update.response",
            "update",
        );
    }

    fn submit(&mut self, ops: &mut SheetOps) {
        if self.offline {
            return;
        }
        match self.page {
            Page::Create => self.create(ops),
            Page::Edit => self.save(ops),
            _ => {}
        }
    }

    fn after_change(&mut self, ops: &mut SheetOps) {
        self.load(ops);
        ops.core.refresh_host(&self.host_id);
    }
}

impl Sheet for LabelsSheet {
    fn title(&self) -> String {
        match self.page {
            Page::Pick => tr("workspaceLabels.title"),
            Page::Create => tr("workspaceLabels.create"),
            Page::Manage => tr("workspaceLabels.manage.title"),
            Page::Edit => tr_args("workspaceLabels.manage.editLabel", &[("name", self.editing.as_ref().map_or("", |l| j::text(l, "name")))]),
        }
    }

    fn max_width(&self) -> f64 {
        400.0
    }

    fn can_go_back(&self) -> bool {
        match self.page {
            Page::Pick => false,
            // The standalone manager has no pick page to return to.
            Page::Manage => self.workspace.is_some(),
            Page::Create | Page::Edit => true,
        }
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        self.offline = hosts::host(ui_state(scope).as_ref(), &self.host_id).is_none_or(|h| !h.connected());
        let start = y;
        let mut y = y;
        let act = |id: &str, enabled: bool| (enabled && interactive).then(|| id.to_string());
        if self.page == Page::Pick {
            if let Some(name) = self.workspace.as_ref().map(|w| j::text(w, "name")).filter(|n| !n.is_empty()) {
                let spec = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(cx, rect(x + 12.0, y - 12.0, w - 24.0, 16.0), spec, name);
                y += 8.0;
            }
            if self.busy && self.labels.is_empty() {
                // LinearProgressIndicator(minHeight: 2).
                let t = (cx.seconds_since_app_start() % 1.5) / 1.5;
                ui.fill(cx, rect(x, y, w, 2.0), p.surface2, 0.0);
                ui.fill(cx, rect(x + w * t * 0.7, y, w * 0.3, 2.0), p.accent, 0.0);
                crate::shell::sheets::SPINNING.with(|s| s.set(true));
                y += 2.0;
            }
        }
        match self.page {
            Page::Pick => {
                for (i, label) in self.labels.clone().iter().enumerate() {
                    let name = j::text(label, "name");
                    let id = act(&format!("pick:{i}"), !self.offline);
                    let (r, h) = row(ui, cx, x, y, w, id.as_deref(), name, None);
                    ui.dot(cx, row_leading(r).center(), 10.0, label_color(&p, j::text(label, "color")));
                    if self.pending.contains(&name.to_lowercase()) {
                        spinner(ui, cx, crate::common::paint::inset(row_trailing(r), 1.0, 1.0), p.muted);
                    } else if self.assigned.contains(name) {
                        ui.icon(cx, row_trailing(r), "check", 16.0, p.foreground);
                    }
                    y += h;
                }
                if !self.labels.is_empty() {
                    y += separator(ui, cx, x, y, w);
                }
                let id = act("create", !self.offline);
                let (r, h) = row(ui, cx, x, y, w, id.as_deref(), &tr("workspaceLabels.create"), None);
                ui.icon(cx, row_leading(r), "plus", 16.0, p.muted);
                ui.icon(cx, row_trailing(r), "chevron-right", 16.0, p.muted);
                y += h;
                let id = act("manage", true);
                let (r, h) = row(ui, cx, x, y, w, id.as_deref(), &tr("workspaceLabels.manage.open"), None);
                ui.icon(cx, row_leading(r), "sliders-horizontal", 16.0, p.muted);
                y += h;
            }
            Page::Create | Page::Edit => {
                let edit = self.page == Page::Edit;
                self.name.set_read_only(cx, self.offline || self.busy);
                self.name.draw(cx, scope, rect(x + 4.0, y + 4.0, w - 8.0, if ui.compact { 44.0 } else { 32.0 }));
                y += 8.0 + if ui.compact { 44.0 } else { 32.0 };
                // Swatches: 20px circles, 12 apart, wrapping; 2px foreground ring when selected.
                let mut sx = x + 12.0;
                let mut sy = y + 8.0;
                for color in LABEL_COLORS {
                    if sx + 20.0 > x + w - 12.0 {
                        sx = x + 12.0;
                        sy += 32.0;
                    }
                    let r = rect(sx, sy, 20.0, 20.0);
                    if self.color == color {
                        ui.dot(cx, r.center(), 20.0, p.foreground);
                        ui.dot(cx, r.center(), 16.0, label_color(&p, color));
                    } else {
                        ui.dot(cx, r.center(), 20.0, label_color(&p, color));
                    }
                    if !self.offline && !self.busy && interactive {
                        ui.hit_tip(r, &format!("color:{color}"), &tr(&format!("workspaceLabels.colors.{color}")));
                    }
                    sx += 32.0;
                }
                y = sy + 20.0 + 8.0;
                y += separator(ui, cx, x, y, w);
                let label = if self.busy {
                    tr("workspaceLabels.creating")
                } else {
                    tr(if edit { "workspaceLabels.manage.save" } else { "workspaceLabels.createConfirm" })
                };
                let id = act("submit", !self.offline && !self.busy && !self.normalized_name().is_empty());
                let (r, h) = row(ui, cx, x, y, w, id.as_deref(), &label, None);
                if self.busy {
                    spinner(ui, cx, crate::common::paint::inset(row_trailing(r), 1.0, 1.0), p.muted);
                }
                y += h;
                if edit {
                    let id = act("delete", !self.offline && !self.busy);
                    let (_, h) = row(ui, cx, x, y, w, id.as_deref(), &tr("workspaceLabels.manage.delete"), Some(p.destructive));
                    y += h;
                }
            }
            Page::Manage => {
                let fh = if ui.compact { 44.0 } else { 32.0 };
                self.search.draw(cx, scope, rect(x + 4.0, y + 4.0, w - 8.0, fh));
                y += 8.0 + fh;
                if self.labels.is_empty() {
                    y += hint(ui, cx, x, y, w, &tr("workspaceLabels.manage.empty"), false);
                }
                for (i, label) in self.labels.clone().iter().enumerate() {
                    let name = j::text(label, "name");
                    if !name.to_lowercase().contains(&self.query) {
                        continue;
                    }
                    let id = act(&format!("edit:{i}"), !self.offline);
                    let (r, h) = row(ui, cx, x, y, w, id.as_deref(), name, None);
                    ui.dot(cx, row_leading(r).center(), 10.0, label_color(&p, j::text(label, "color")));
                    ui.icon(cx, crate::common::paint::inset(row_trailing(r), 1.0, 1.0), "pencil", 14.0, p.muted);
                    y += h;
                }
            }
        }
        if self.offline {
            y += hint(ui, cx, x, y, w, &tr("workspaceLabels.manage.offline"), false);
        }
        if let Some(error) = self.error.clone() {
            y += hint(ui, cx, x, y, w, &error, true);
        }
        y - start
    }

    fn click(&mut self, cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        match id {
            "sheet:back" => match self.page {
                Page::Edit => self.open(cx, Page::Manage, None),
                Page::Create | Page::Manage => self.open(cx, Page::Pick, None),
                Page::Pick => {}
            },
            "create" => self.open(cx, Page::Create, None),
            "manage" => self.open(cx, Page::Manage, None),
            "submit" => self.submit(ops),
            "delete" => {
                if let Some(label) = self.editing.clone() {
                    self.busy = true;
                    self.error = None;
                    let name = j::text(&label, "name").to_string();
                    ops.request(
                        &self.host_id,
                        json!({ "type": "workspace.label.delete.inspect.request", "name": name }),
                        "workspace.label.delete.inspect.response",
                        "inspect",
                    );
                }
            }
            _ => {
                if let Some(color) = id.strip_prefix("color:") {
                    self.color = color.to_string();
                } else if let Some(label) = id.strip_prefix("pick:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| self.labels.get(i).cloned()) {
                    let name = j::text(&label, "name").to_string();
                    let assigned = !self.assigned.contains(&name);
                    self.assign(ops, &name, j::text(&label, "color"), assigned);
                } else if let Some(label) = id.strip_prefix("edit:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| self.labels.get(i).cloned()) {
                    self.open(cx, Page::Edit, Some(label));
                }
            }
        }
        SheetFlow::Stay
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> SheetFlow {
        match self.page {
            Page::Create | Page::Edit => {
                if let Some(FieldEvent::Returned(_)) = self.name.handle_event(cx, event, scope) {
                    self.submit_pending = true;
                }
            }
            Page::Manage => {
                if let Some(FieldEvent::Changed(text)) = self.search.handle_event(cx, event, scope) {
                    self.query = text.trim().to_lowercase();
                }
            }
            Page::Pick => {}
        }
        SheetFlow::Stay
    }

    fn after_event(&mut self, _cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        if !self.loaded {
            self.loaded = true;
            self.load(ops);
        }
        if std::mem::take(&mut self.submit_pending) {
            self.submit(ops);
        }
        SheetFlow::Stay
    }

    fn on_response(&mut self, cx: &mut Cx, tag: &str, result: &Result<Value, CoreError>, ops: &mut SheetOps) -> SheetFlow {
        let failure = response_message(result);
        let payload = result.as_ref().ok().cloned().unwrap_or(Value::Null);
        match tag {
            "list" => {
                self.busy = false;
                match failure {
                    Some(error) => self.error = Some(error),
                    None => self.labels = j::array(&payload, "labels").to_vec(),
                }
            }
            "update" => {
                self.busy = false;
                match failure {
                    Some(error) => self.error = Some(error),
                    None => {
                        let old = self.editing.as_ref().map(|l| j::text(l, "name").to_string()).unwrap_or_default();
                        if self.assigned.remove(&old) {
                            self.assigned.insert(self.normalized_name());
                        }
                        self.after_change(ops);
                        self.open(cx, Page::Manage, None);
                    }
                }
            }
            "inspect" => match failure {
                Some(error) => {
                    self.busy = false;
                    self.error = Some(error);
                }
                None => {
                    self.busy = false;
                    let name = self.editing.as_ref().map(|l| j::text(l, "name").to_string()).unwrap_or_default();
                    let count = j::int(&payload, "affectedWorkspaceCount", 0);
                    let key = if count == 1 { "workspaceLabels.manage.deleteMessage_one" } else { "workspaceLabels.manage.deleteMessage_other" };
                    let confirm = ConfirmSheet::new(
                        ops,
                        &tr_args("workspaceLabels.manage.deleteTitle", &[("name", &name)]),
                        &tr_args(key, &[("count", &count.to_string())]),
                        &tr("workspaceLabels.manage.delete"),
                        true,
                        "delete",
                    );
                    return SheetFlow::Push(Box::new(confirm));
                }
            },
            "delete" => {
                self.busy = false;
                match failure {
                    Some(error) => self.error = Some(error),
                    None => {
                        let name = self.editing.as_ref().map(|l| j::text(l, "name").to_string()).unwrap_or_default();
                        self.assigned.remove(&name);
                        self.after_change(ops);
                        self.open(cx, Page::Manage, None);
                    }
                }
            }
            _ => {
                if let Some(key) = tag.strip_prefix("assign:") {
                    self.pending.remove(key);
                    let creating = self.page == Page::Create && self.busy;
                    self.busy = false;
                    match failure {
                        Some(_) => self.error = Some(tr("workspaceLabels.errors.update")),
                        None => {
                            self.assigned = j::array(&payload, "workspaceLabels")
                                .iter()
                                .map(|l| l.as_str().map_or_else(|| l.to_string(), str::to_string))
                                .collect();
                            self.after_change(ops);
                            if creating {
                                self.open(cx, Page::Pick, None);
                            }
                        }
                    }
                }
            }
        }
        SheetFlow::Stay
    }

    fn on_deliver(&mut self, _cx: &mut Cx, tag: &str, value: &Value, ops: &mut SheetOps) -> SheetFlow {
        if tag == "delete"
            && value.as_bool() == Some(true)
            && let Some(label) = &self.editing
        {
            self.busy = true;
            ops.request(
                &self.host_id,
                json!({ "type": "workspace.label.delete.request", "name": j::text(label, "name") }),
                "workspace.label.delete.response",
                "delete",
            );
        }
        SheetFlow::Stay
    }
}

/// The core state as seen by sheets during draw (the dispatch scope carries `UiContext`).
fn ui_state(scope: &mut Scope) -> std::sync::Arc<Value> {
    scope.data.get::<crate::common::UiContext>().map(|ui| ui.state.clone()).unwrap_or_default()
}
