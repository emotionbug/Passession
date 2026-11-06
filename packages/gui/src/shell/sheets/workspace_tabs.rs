//! Port of the compact workspace tab switcher (`PaseoShell._showTabs`, original
//! `MobileWorkspaceTabSwitcher`): the workspace's tabs (open tabs and not-yet-open
//! sessions), each with its kind icon, title, modified dot, selected check and a row menu
//! (`_workspaceTabMenu`); the footer holds the New agent button.

use std::sync::Arc;

use makepad_widgets::*;
use serde_json::Value;

use crate::common::paint::{Font, rect};
use crate::common::{PanelTarget, TabPlacement, tr, tr_args};
use crate::shell::sheets::{Sheet, SheetFlow, SheetOps, SheetStyle};
use crate::shell::ui::{Ui, Variant};
use crate::shell::{PaseoShell, chrome, commands};
use crate::theme::ControlSize;

struct Row {
    target: PanelTarget,
    title: String,
    kind_label: String,
    modified: bool,
}

pub struct WorkspaceTabsSheet {
    workspace: PanelTarget,
    rows: Vec<Row>,
    active: Option<String>,
    state: Arc<Value>,
    /// Row menu trigger rects from the last draw (window coordinates), by row index.
    menu_rects: Vec<Rect>,
}

impl WorkspaceTabsSheet {
    pub fn new(workspace: PanelTarget) -> Self {
        Self { workspace, rows: Vec::new(), active: None, state: Arc::new(Value::Null), menu_rects: Vec::new() }
    }
}

impl Sheet for WorkspaceTabsSheet {
    fn title(&self) -> String {
        tr("workspace.tabs.switcher.title")
    }

    /// Original `MobileWorkspaceTabSwitcher`: a 60%/90% draggable combobox sheet with its own
    /// surface2 handle and the New agent footer.
    fn style(&self) -> SheetStyle {
        SheetStyle::Draggable { material_handle: false }
    }

    fn footer_height(&self) -> f64 {
        40.0
    }

    fn draw_footer(&mut self, cx: &mut Cx2d, ui: &mut Ui, x: f64, y: f64, w: f64, interactive: bool) {
        let label = tr("workspace.header.actions.newAgent");
        ui.button(cx, rect(x, y, w, 40.0), "new", &label, Some("square-pen"), Variant::Primary, ControlSize::Md, interactive);
    }

    fn sync(&mut self, shell: &PaseoShell, state: &Value) {
        self.rows = commands::workspace_tabs(shell, state, &self.workspace)
            .into_iter()
            .map(|target| Row {
                title: chrome::title(state, &target),
                kind_label: chrome::kind_label(&target),
                modified: shell.data.dirty.contains(&target.key()) && target.kind == "file",
                target,
            })
            .collect();
        self.active = shell.data.layout.selected().map(PanelTarget::key);
        self.state = Arc::new(state.clone());
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        self.menu_rects.clear();
        for (i, row) in self.rows.iter().enumerate() {
            // `_TabOption`: (4, 0, 4, 4) margin, 36 min height, 4px padding; body 8px padding
            // with a 16px icon slot.
            let outer = rect(x + 4.0, y, w - 8.0, 36.0 + 8.0);
            let menu = rect(outer.pos.x + outer.size.x - 4.0 - 28.0, outer.pos.y + (outer.size.y - 28.0) * 0.5, 28.0, 28.0);
            let selected = self.active.as_deref() == Some(row.target.key().as_str());
            let mut right = menu.pos.x - 8.0;
            if selected {
                ui.icon(cx, rect(right - 16.0, outer.pos.y + (outer.size.y - 16.0) * 0.5, 16.0, 16.0), "check", 16.0, p.muted);
                right -= 16.0 + 8.0;
            }
            if row.modified {
                ui.dot(cx, dvec2(right - 4.0, outer.pos.y + outer.size.y * 0.5), 8.0, p.muted);
                right -= 8.0 + 8.0;
            }
            let body = rect(outer.pos.x + 4.0, outer.pos.y + 4.0, (right - outer.pos.x - 4.0).max(0.0), outer.size.y - 8.0);
            let id = format!("tab:{i}");
            if interactive && (ui.is_hover(&id) || ui.is_pressed(&id)) {
                ui.fill(cx, body, p.surface1, 0.0);
            }
            let icon_color = if selected { p.foreground } else { p.muted };
            chrome::tab_icon(ui, cx, &self.state, &row.target, rect(body.pos.x + 8.0 + 1.0, body.pos.y + (body.size.y - 14.0) * 0.5, 14.0, 14.0), icon_color);
            let spec = ui.spec(Font::Regular, 14.0, p.foreground);
            ui.text(cx, rect(body.pos.x + 8.0 + 16.0 + 8.0, body.pos.y, (body.size.x - 40.0).max(0.0), body.size.y), spec, &row.title);
            if interactive {
                ui.hit(body, id);
            }
            let menu_id = format!("tabmenu:{i}");
            if interactive && ui.is_pressed(&menu_id) {
                ui.fill(cx, menu, p.surface2, 6.0);
            }
            ui.icon(cx, menu, "ellipsis", 14.0, p.muted);
            if interactive {
                ui.hit_tip(menu, &menu_id, &tr_args("workspace.tabs.menu.openFor", &[("label", &row.kind_label)]));
            }
            self.menu_rects.push(menu);
            y += outer.size.y + 4.0;
        }
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        if id == "new" {
            ops.run(|shell, cx, ui| commands::new_session(shell, cx, ui, false, false));
            return SheetFlow::Close;
        }
        if let Some(row) = id.strip_prefix("tab:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| self.rows.get(i)) {
            let target = row.target.clone();
            ops.run(move |shell, cx, ui| {
                let key = target.key();
                if shell.data.layout.target(&key).is_none() {
                    commands::open_tab(shell, cx, ui, target, TabPlacement::Main);
                } else {
                    shell.data.layout.select_tab(&key);
                }
            });
            return SheetFlow::Close;
        }
        if let Some(index) = id.strip_prefix("tabmenu:").and_then(|i| i.parse::<usize>().ok())
            && let (Some(row), Some(anchor)) = (self.rows.get(index), self.menu_rects.get(index).copied())
        {
            let (tab, workspace) = (row.target.clone(), self.workspace.clone());
            ops.run(move |shell, cx, ui| {
                let state = ui.state.clone();
                commands::workspace_tab_menu(shell, cx, &state, tab, workspace, anchor);
            });
        }
        SheetFlow::Stay
    }
}
