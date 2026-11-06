//! Port of `showShortcutHelp` (`ui/command_center.dart`, the original keyboard-shortcuts
//! dialog): a search field over the catalog sections, each a bordered list of label rows with
//! the effective combo (user overrides win) as `ShortcutBadge` step pills, or "Unassigned".

use makepad_widgets::*;
use serde_json::Value;

use crate::common::paint::{Font, rect};
use crate::common::tr;
use crate::shell::PaseoShell;
use crate::shell::sheets::{Field, FieldEvent, Sheet, SheetFlow, SheetOps};
use crate::shell::shortcuts::{format_step, help_rows};
use crate::shell::ui::Ui;
use crate::theme::with_alpha;

pub struct ShortcutHelpSheet {
    search: Field,
    query: String,
    preferences: Value,
}

impl ShortcutHelpSheet {
    pub fn new(compact: bool) -> Self {
        let search = Field::new("", &tr("settings.shortcuts.searchPlaceholder"));
        Self { search: if compact { search } else { search.autofocus() }, query: String::new(), preferences: Value::Null }
    }
}

impl Sheet for ShortcutHelpSheet {
    fn title(&self) -> String {
        tr("settings.shortcuts.dialogTitle")
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        self.preferences = state.get("preferences").cloned().unwrap_or(Value::Null);
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, _interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        // Search row: 16px muted glyph, borderless field.
        let fh = if ui.compact { 44.0 } else { 32.0 };
        ui.icon(cx, rect(x, y + (fh - 16.0) * 0.5, 16.0, 16.0), "search", 16.0, p.muted);
        self.search.draw(cx, scope, rect(x + 24.0, y, w - 24.0, fh));
        y += fh + 12.0;

        let sections = help_rows(&self.preferences, &self.query);
        if sections.is_empty() {
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            ui.text_centered(cx, rect(x, y + 24.0, w, 20.0), spec, "No results");
            return y + 24.0 + 20.0 + 24.0 - start;
        }
        let label_spec = ui.spec(Font::Regular, 14.0, p.foreground);
        let badge_spec = ui.spec(Font::Regular, 12.0, p.muted);
        let unassigned = tr("settings.shortcuts.unassigned");
        for (i, (title, rows)) in sections.iter().enumerate() {
            if i > 0 {
                y += 16.0;
            }
            let spec = ui.spec(Font::Medium, 12.0, p.muted);
            ui.text(cx, rect(x, y, w, 16.0), spec, title);
            y += 16.0 + 8.0;
            // Measure rows first: the 1px surface2 border box wraps them.
            let label_w = (w - 32.0) * 0.6;
            let heights: Vec<f64> = rows.iter().map(|(label, _)| ui.painter.paragraph_height(cx, label_w, label_spec, label).max(20.0) + 24.0).collect();
            let total: f64 = heights.iter().sum();
            ui.painter.outline(cx, rect(x, y, w, total), Vec4::default(), p.surface2, 1.0, 8.0);
            for (index, ((label, combo), h)) in rows.iter().zip(&heights).enumerate() {
                ui.painter.paragraph(cx, dvec2(x + 16.0, y + 12.0), label_w, label_spec, label);
                let right = x + w - 16.0;
                if combo.is_empty() {
                    let spec = ui.spec(Font::Regular, 14.0, p.muted);
                    let tw = ui.measure(cx, spec, &unassigned);
                    ui.text(cx, rect(right - tw, y + 12.0, tw, 20.0), spec, &unassigned);
                } else {
                    // Step pills right-aligned, 4 apart: surface3 @ 50%, 6px radius, 4/2 padding.
                    let steps: Vec<String> = combo.split_whitespace().map(format_step).collect();
                    let widths: Vec<f64> = steps.iter().map(|s| ui.measure(cx, badge_spec, s) + 8.0).collect();
                    let mut px = right - widths.iter().sum::<f64>() - 4.0 * (steps.len().saturating_sub(1)) as f64;
                    for (step, sw) in steps.iter().zip(widths) {
                        let pill = rect(px, y + 12.0 + 1.0, sw, 18.0);
                        ui.fill(cx, pill, with_alpha(p.surface3, 0.5), 6.0);
                        ui.text_centered(cx, pill, badge_spec, step);
                        px += sw + 4.0;
                    }
                }
                if index + 1 < rows.len() {
                    ui.painter.hline(cx, x, y + h - 1.0, w, p.surface2);
                }
                y += h;
            }
        }
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, _id: &str, _ops: &mut SheetOps) -> SheetFlow {
        SheetFlow::Stay
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> SheetFlow {
        if let Some(FieldEvent::Changed(text) | FieldEvent::Returned(text)) = self.search.handle_event(cx, event, scope) {
            self.query = text.trim().to_lowercase();
        }
        SheetFlow::Stay
    }
}
