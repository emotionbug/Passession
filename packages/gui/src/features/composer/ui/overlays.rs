//! Composer overlays drawn above the window in their own draw list: the slash/@ autocomplete
//! popover, tooltips, the desktop context-window hover card, and the modal surfaces (model
//! sheet, forge picker, image preview, fullscreen editor, compact context sheet).

use super::layout::{format_session_cost, format_token_count};
use super::look::Look;
use super::panel::{ComposerPanel, Item, OItem, Overlay};
use super::sheet::{Leading, Page, RowAction};
use crate::common::paint::{rect, Font, TextSpec};
use crate::common::{Route, UiContext, UiEffect};
use crate::features::settings::SettingsPanelWidgetRefExt;
use crate::features::composer::autocomplete::clean_label;
use crate::features::composer::controller::Outputs;
use crate::features::composer::controls_model::ControlsModel;
use crate::features::composer::i18n::{t, t_args};
use crate::features::composer::json::*;
use crate::features::composer::service;
use crate::theme::fade;
use makepad_widgets::*;
use serde_json::Value;

const SUGGEST_MAX: f64 = 220.0;

/// Context window details presentation (`ContextWindowDetails` showTitle/gap/refreshable).
#[derive(Clone, Copy)]
struct Details {
    show_title: bool,
    gap: f64,
    refreshable: bool,
}

impl Details {
    /// Desktop hover card: titled, 12px gaps.
    const CARD: Details = Details { show_title: true, gap: 12.0, refreshable: true };
    /// Compact sheet: the sheet title replaces the details title, 16px gaps.
    const SHEET: Details = Details { show_title: false, gap: 16.0, refreshable: true };
}

/// A sheet header: title, optional back arrow, leading provider glyph and settings gear.
struct Header<'a> {
    title: &'a str,
    back: bool,
    leading: Option<(String, String)>,
    gear: bool,
}

impl<'a> Header<'a> {
    fn plain(title: &'a str) -> Self {
        Self { title, back: false, leading: None, gear: false }
    }
}

impl ComposerPanel {
    pub(super) fn suggestions_rect(&self) -> Option<Rect> {
        self.st.overlay_rect
    }

    fn suggestions_visible(&self) -> bool {
        let ctl = self.ctl_ref();
        ctl.autocomplete.visible() && !matches!(self.st.overlay(), Overlay::Fullscreen) && !ctl.is_subagent() && !ctl.is_sheet()
    }

    pub(super) fn draw_overlays(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look) {
        self.st.ohits.clear();
        self.st.overlay_rect = None;
        self.overlay_list.begin_overlay_reuse(cx);
        let size = cx.current_pass_size();
        cx.begin_sized_turtle(size, Layout::flow_down());
        match self.st.overlay().clone() {
            Overlay::None => {
                if self.suggestions_visible() {
                    self.draw_suggestions(cx, look);
                }
                // One popover at a time owns the overlay hit area.
                if self.st.meter_hover && !look.compact && self.st.meter_rect.size.x > 0.0 && self.st.overlay_rect.is_none() {
                    self.draw_context_card(cx, look);
                }
                if let Some((text, anchor)) = self.st.tooltip.clone() {
                    self.layer(cx);
                    self.draw_tooltip(cx, look, &text, anchor);
                }
            }
            Overlay::Sheet(_) => self.draw_sheet(cx, ui, look),
            Overlay::Forge => self.draw_forge(cx, look, size),
            Overlay::Preview(image) => self.draw_preview(cx, look, size, &image),
            Overlay::Fullscreen => self.draw_fullscreen(cx, look, size),
            Overlay::Context => self.draw_context_sheet(cx, look, size),
        }
        cx.end_pass_sized_turtle();
        self.overlay_list.end(cx);
    }

    fn card(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, fill: Vec4) {
        let shadow = rect(r.pos.x, r.pos.y + 4.0, r.size.x, r.size.y);
        self.painter.fill(cx, shadow, look.shadow(), 8.0);
        self.painter.outline(cx, r, fill, look.p.border_accent, 1.0, 8.0);
    }

    /// components/ui/autocomplete-popover.tsx: 12px above the input card, as wide as it,
    /// ≤220 tall, best match nearest the input; a detail card above for a described command.
    fn draw_suggestions(&mut self, cx: &mut Cx2d, look: &Look) {
        let p = look.p;
        let wrapper = self.st.wrapper;
        let room = wrapper.pos.y - 24.0;
        if room <= 0.0 || wrapper.size.x <= 0.0 {
            return;
        }
        let body = TextSpec::new(Font::Regular, look.body(), p.foreground);
        let muted = TextSpec::new(Font::Regular, look.body(), p.muted);
        let small = TextSpec::new(Font::Regular, look.small(), p.muted);
        let ctl = self.ctl_ref();
        let options = ctl.autocomplete.options.clone();
        let selected = ctl.autocomplete.selected_index;
        let message = if ctl.autocomplete.is_loading() {
            Some(t(ctl.autocomplete.loading_key()))
        } else if let Some(error) = ctl.autocomplete.error_message() {
            Some(format!("Error: {error}"))
        } else if ctl.autocomplete.options.is_empty() {
            Some(t(ctl.autocomplete.empty_key()))
        } else {
            None
        };
        let max_h = SUGGEST_MAX.min(room);
        if let Some(message) = message {
            self.st.suggest_scroll_max = 0.0;
            self.st.suggest_scroll = 0.0;
            let h = (self.painter.paragraph_height(cx, wrapper.size.x - 24.0, muted, &message) + 24.0).min(max_h);
            let r = rect(wrapper.pos.x, wrapper.pos.y - 12.0 - h, wrapper.size.x, h);
            self.card(cx, look, r, p.surface1);
            self.painter.paragraph(cx, dvec2(r.pos.x + 12.0, r.pos.y + 12.0), r.size.x - 24.0, muted, &message);
            self.overlay_bg.draw_abs(cx, r);
            self.st.overlay_rect = Some(r);
            return;
        }
        let describe = |option: &crate::features::composer::autocomplete::AutocompleteOption| {
            let raw = option.description.clone().unwrap_or_default();
            clean_label(&if option.client_command { t(&raw) } else { raw })
        };
        let line = self.painter.line_height(cx, body);
        let row_h = (line + 16.0).max(36.0);
        let list_h = (options.len() as f64 * row_h + 8.0).min(max_h);
        let list = rect(wrapper.pos.x, wrapper.pos.y - 12.0 - list_h, wrapper.size.x, list_h);
        // Pin to the bottom; keep the keyboard selection visible.
        let content = options.len() as f64 * row_h + 8.0;
        let max_scroll = (content - list_h).max(0.0);
        self.st.suggest_scroll_max = max_scroll;
        let mut scroll = (max_scroll - self.st.suggest_scroll).clamp(0.0, max_scroll);
        if let Some(index) = selected {
            let top = 4.0 + index as f64 * row_h;
            if top < scroll {
                scroll = top - 4.0;
            } else if top + row_h > scroll + list_h {
                scroll = top + row_h + 4.0 - list_h;
            }
            self.st.suggest_scroll = max_scroll - scroll.clamp(0.0, max_scroll);
        }
        let mut whole = list;
        // Detail card for a selected command with a description.
        if let Some(active) = selected.and_then(|i| options.get(i)) {
            let description = describe(active);
            if active.kind == "command" && !description.is_empty() {
                let w = list.size.x - 24.0;
                let mut h = 24.0 + line + 4.0 + self.painter.paragraph_height(cx, w, small, &description);
                if let Some(detail) = &active.detail {
                    h += 4.0 + self.painter.paragraph_height(cx, w, small, &clean_label(detail));
                }
                let card = rect(list.pos.x, list.pos.y - 4.0 - h, list.size.x, h);
                if card.pos.y > 0.0 {
                    self.card(cx, look, card, p.surface1);
                    let mut y = card.pos.y + 12.0;
                    self.painter.text(cx, dvec2(card.pos.x + 12.0, y), body, &clean_label(&active.label));
                    y += line + 4.0;
                    y += self.painter.paragraph(cx, dvec2(card.pos.x + 12.0, y), w, small, &description).y;
                    if let Some(detail) = &active.detail {
                        self.painter.paragraph(cx, dvec2(card.pos.x + 12.0, y + 4.0), w, small, &clean_label(detail));
                    }
                    whole = rect(card.pos.x, card.pos.y, card.size.x, list.pos.y + list.size.y - card.pos.y);
                }
            }
        }
        self.card(cx, look, list, p.surface1);
        cx.begin_turtle(Walk::abs_rect(list), Layout::flow_down().with_clip(true, true));
        for (index, option) in options.iter().enumerate() {
            let y = list.pos.y + 4.0 + index as f64 * row_h - scroll;
            let row = rect(list.pos.x, y, list.size.x, row_h);
            if row.pos.y + row_h < list.pos.y || row.pos.y > list.pos.y + list.size.y {
                continue;
            }
            let item = Item::Suggest(index);
            if Some(index) == selected || self.st.hover.as_ref() == Some(&item) {
                self.painter.fill(cx, row, p.surface2, 0.0);
            }
            let label = clean_label(&option.label);
            let description = describe(option);
            let mut x = row.pos.x + 12.0;
            if option.kind == "file" || option.kind == "directory" {
                let icon = if option.kind == "directory" { "folder" } else { "file" };
                self.painter.icon(cx, rect(x, row.pos.y, 18.0, row_h), icon, 14.0, p.muted);
                x += 22.0;
            }
            let right = row.pos.x + row.size.x - 12.0;
            let drawn = self.painter.text_in(cx, rect(x, row.pos.y, right - x, row_h), body, &label, 0.0);
            let mut dx = drawn.pos.x + drawn.size.x + 8.0;
            if let Some(detail) = &option.detail {
                let d = self.painter.text_in(cx, rect(dx, row.pos.y, (right - dx).max(0.0), row_h), small, detail, 0.0);
                dx = d.pos.x + d.size.x + 8.0;
            }
            if !description.is_empty() && dx < right {
                self.painter.text_in(cx, rect(dx, row.pos.y, right - dx, row_h), small, &description, 0.0);
            }
            let visible = crate::common::paint::intersect(row, list);
            self.st.hits.push(visible, item);
        }
        cx.end_turtle();
        self.overlay_bg.draw_abs(cx, whole);
        self.st.overlay_rect = Some(whole);
    }

    fn draw_tooltip(&mut self, cx: &mut Cx2d, look: &Look, text: &str, anchor: Rect) {
        let spec = TextSpec::new(Font::Regular, look.small(), look.p.foreground);
        let size = self.painter.measure(cx, spec, text);
        let w = size.x + 16.0;
        let h = size.y + 8.0;
        let x = (anchor.pos.x + anchor.size.x * 0.5 - w * 0.5).clamp(4.0, (self.st.window.x - w - 4.0).max(4.0));
        let mut y = anchor.pos.y - h - 6.0;
        if y < 4.0 {
            y = anchor.pos.y + anchor.size.y + 6.0;
        }
        let r = rect(x, y, w, h);
        self.painter.outline(cx, r, look.p.surface2, look.p.border, 1.0, 6.0);
        self.painter.text_in(cx, r, spec, text, 0.5);
    }

    /// context-window-details.tsx (+ AgentUsageSection); measures when `draw` is false.
    fn context_details(&mut self, cx: &mut Cx2d, look: &Look, area: Rect, details: Details, draw: bool) -> f64 {
        let (pos, width) = (area.pos, area.size.x);
        let Details { show_title, gap, refreshable } = details;
        let p = look.p;
        let base = TextSpec::new(Font::Regular, look.body(), p.foreground);
        let detail = TextSpec::new(Font::Regular, look.small(), p.muted);
        let usage = get(&self.ctl_ref().agent.agent, "lastUsage").clone();
        let mut lines: Vec<(TextSpec, String)> = Vec::new();
        if show_title {
            lines.push((base, t("contextWindow.title")));
        }
        let used = get(&usage, "contextWindowUsedTokens").as_f64();
        let max = get(&usage, "contextWindowMaxTokens").as_f64();
        match (used, max) {
            (Some(used), Some(max)) if max > 0.0 && used >= 0.0 => {
                let percentage = format!("{}", (used / max * 100.0).round());
                lines.push((base, t_args("contextWindow.used", &[("percentage", percentage.as_str())])));
                let (u, m) = (format_token_count(used), format_token_count(max));
                lines.push((detail, t_args("contextWindow.tokens", &[("used", u.as_str()), ("max", m.as_str())])));
            }
            _ => lines.push((detail, t("contextWindow.noData"))),
        }
        if let Some(cost) = format_session_cost(get(&usage, "totalCostUsd")) {
            lines.push((detail, t_args("contextWindow.sessionCost", &[("cost", cost.as_str())])));
        }
        let mut y = pos.y;
        for (index, (spec, text)) in lines.iter().enumerate() {
            if index > 0 {
                y += 6.0;
            }
            y += if draw {
                self.painter.paragraph(cx, dvec2(pos.x, y), width, *spec, text).y
            } else {
                self.painter.paragraph_height(cx, width, *spec, text)
            };
        }
        // Agent usage reports of the account the agent runs under.
        if !(self.ctl_ref().connected() && self.ctl_ref().supports("usageSources")) {
            return y - pos.y;
        }
        let ctl = self.ctl_ref();
        let reports = ctl.usage_reports.clone();
        let error = ctl.usage_error.clone();
        let settled = ctl.usage_settled;
        let refreshing = ctl.usage_refreshing.clone();
        let failed = ctl.usage_refresh_failed.clone();
        let message = if let Some(error) = &error {
            Some(t_args("providerUsage.agentError", &[("reason", error.as_str())]))
        } else if !settled && reports.is_empty() {
            Some(t("providerUsage.loading"))
        } else {
            None
        };
        if message.is_none() && reports.is_empty() {
            return y - pos.y;
        }
        y += gap;
        if draw {
            self.painter.hline(cx, pos.x, y, width, p.border_accent);
        }
        y += 1.0 + gap;
        if let Some(message) = message {
            y += if draw {
                self.painter.paragraph(cx, dvec2(pos.x, y), width, detail, &message).y
            } else {
                self.painter.paragraph_height(cx, width, detail, &message)
            };
            return y - pos.y;
        }
        for (index, entry) in reports.iter().enumerate() {
            if index > 0 {
                y += gap;
            }
            let report = get(entry, "report");
            let last = get(entry, "lastSuccessful");
            let retained = s(get(report, "status")) != "available" && non_empty_obj(get(last, "report"));
            let shown = if retained { get(last, "report") } else { report };
            let title = s_or(get(entry, "sourceLabel"), &display(get(entry, "id"))).to_owned();
            let plan = s_or(get(shown, "planLabel"), s(get(report, "planLabel"))).to_owned();
            let medium = TextSpec::new(Font::Medium, look.body(), p.foreground);
            let line = self.painter.line_height(cx, medium);
            if draw {
                self.painter.text_in(cx, rect(pos.x, y, width - 70.0, line), medium, &title, 0.0);
                if refreshable {
                    let id = get(entry, "id").clone();
                    let busy = refreshing.contains(&id);
                    let label = if busy { t("providerUsage.refreshing") } else { t("providerUsage.refresh") };
                    let b = rect(pos.x + width - 64.0, y - 4.0, 64.0, line + 8.0);
                    self.painter.text_in(cx, b, TextSpec::new(Font::Regular, look.small(), if busy { p.muted } else { p.foreground }), &label, 1.0);
                    if !busy {
                        self.st.ohits.push(b, OItem::UsageRefresh(index));
                    }
                }
            }
            y += line;
            if !plan.is_empty() {
                let h = self.painter.line_height(cx, detail);
                if draw {
                    self.painter.text_in(cx, rect(pos.x, y, width, h), detail, &plan, 0.0);
                }
                y += h;
            }
            let remaining_mode = look.usage_remaining;
            for window in list_ref(get(shown, "windows")) {
                let label = s_or(get(window, "label"), s(get(window, "id"))).to_owned();
                let used = window.get("usedPct").and_then(Value::as_f64);
                let left = window.get("remainingPct").and_then(Value::as_f64);
                let used = used.or(left.map(|l| 100.0 - l));
                let shown_pct = if remaining_mode { left.or(used.map(|u| 100.0 - u)) } else { used };
                let h = self.painter.line_height(cx, detail);
                y += 4.0;
                if draw {
                    self.painter.text_in(cx, rect(pos.x, y, width - 48.0, h), detail, &label, 0.0);
                    if let Some(pct) = shown_pct {
                        self.painter.text_in(cx, rect(pos.x + width - 48.0, y, 48.0, h), detail, &format!("{}%", pct.round()), 1.0);
                    }
                }
                y += h + 2.0;
                if draw {
                    self.painter.fill(cx, rect(pos.x, y, width, 4.0), p.surface3, 2.0);
                    if let Some(pct) = used {
                        let fraction = (pct / 100.0).clamp(0.0, 1.0);
                        let color = if pct > 90.0 { p.destructive } else if pct >= 70.0 { look.amber500() } else { p.accent };
                        self.painter.fill(cx, rect(pos.x, y, width * fraction, 4.0), color, 2.0);
                    }
                }
                y += 4.0;
            }
            let mut messages = Vec::new();
            if !get(report, "error").is_null() {
                messages.push(display(get(report, "error")));
            }
            if failed.contains(&get(entry, "id").clone()) {
                messages.push(t("providerUsage.refreshFailed"));
            }
            for message in messages {
                y += 4.0;
                y += if draw {
                    self.painter.paragraph(cx, dvec2(pos.x, y), width, TextSpec::new(Font::Regular, look.small(), p.destructive), &message).y
                } else {
                    self.painter.paragraph_height(cx, width, detail, &message)
                };
            }
        }
        y - pos.y
    }

    /// Desktop hover card above the meter (300 wide when the host reports usage).
    fn draw_context_card(&mut self, cx: &mut Cx2d, look: &Look) {
        let shows_usage = self.ctl_ref().supports("usageSources");
        let width = if shows_usage { 300.0f64.min(self.st.window.x - 24.0) } else { 220.0 };
        let pad = if shows_usage { 12.0 } else { 8.0 };
        let inner = width - pad * 2.0;
        let h = self.context_details(cx, look, rect(0.0, 0.0, inner, 0.0), Details::CARD, false) + pad * 2.0;
        let anchor = self.st.meter_rect;
        let x = (anchor.pos.x + anchor.size.x - width).clamp(8.0, (self.st.window.x - width - 8.0).max(8.0));
        let r = rect(x, anchor.pos.y - 8.0 - h, width, h);
        self.card(cx, look, r, look.p.surface1);
        self.context_details(cx, look, rect(r.pos.x + pad, r.pos.y + pad, inner, 0.0), Details::CARD, true);
        self.overlay_bg.draw_abs(cx, r);
        self.st.overlay_rect = Some(r);
    }

    fn scrim(&mut self, cx: &mut Cx2d, look: &Look, size: DVec2) {
        let full = rect(0.0, 0.0, size.x, size.y);
        self.painter.fill(cx, full, look.p.scrim, 0.0);
        self.overlay_bg.draw_abs(cx, full);
        self.st.ohits.push(full, OItem::Scrim);
    }

    /// Sheet frame: centred dialog on wide layouts, bottom sheet at 80% on compact ones.
    fn sheet_frame(&mut self, cx: &mut Cx2d, look: &Look, size: DVec2, content_h: f64) -> Rect {
        let r = if look.compact {
            let h = (size.y * 0.8).max(content_h.min(size.y * 0.9));
            rect(0.0, size.y - h, size.x, h)
        } else {
            let w = 480.0f64.min(size.x - 32.0);
            let h = content_h.min(size.y * 0.8).max(120.0);
            rect((size.x - w) * 0.5, (size.y - h) * 0.5, w, h)
        };
        self.painter.outline(cx, r, look.p.surface0, look.p.border, 1.0, 16.0);
        self.st.ohits.push(r, OItem::Panel);
        self.st.overlay_rect = Some(r);
        r
    }

    fn sheet_header_height(&mut self, cx: &mut Cx2d, look: &Look) -> f64 {
        let spec = TextSpec::new(Font::Medium, look.t.ui_size(16.0), look.p.foreground);
        20.0 + self.painter.line_height(cx, spec).max(28.0)
    }

    fn sheet_header(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, header: Header) {
        let Header { title, back, leading, gear } = header;
        let p = look.p;
        let mut x = r.pos.x + 12.0;
        let y = r.pos.y + 10.0;
        if back {
            let b = rect(x, y, 28.0, 28.0);
            self.painter.icon(cx, b, "chevron-left", 18.0, p.foreground);
            self.st.ohits.push(b, OItem::Back);
            x += 32.0;
        }
        if let Some((provider, svg)) = leading {
            let g = rect(x, y + 4.0, 20.0, 20.0);
            crate::common::provider_icon::ProviderIcon::paint(&mut self.painter, &mut self.draw_icon, cx, g, &provider, (!svg.is_empty()).then_some(svg.as_str()), p.foreground);
            x += 26.0;
        }
        let close = rect(r.pos.x + r.size.x - 40.0, y, 28.0, 28.0);
        self.painter.icon(cx, close, "x", 16.0, p.muted);
        self.st.ohits.push(close, OItem::Close);
        let mut end = close.pos.x - 4.0;
        if gear {
            let g = rect(end - 28.0, y, 28.0, 28.0);
            self.painter.icon(cx, g, "settings", 16.0, p.muted);
            self.st.ohits.push(g, OItem::Gear);
            end = g.pos.x - 4.0;
        }
        let spec = TextSpec::new(Font::Medium, look.t.ui_size(16.0), p.foreground);
        let height = self.sheet_header_height(cx, look) - 20.0;
        self.painter.text_in(cx, rect(x, y, (end - x).max(0.0), height), spec, title, 0.0);
    }

    /// The search row of a sheet under another one: same look, placeholder only (the one
    /// search editor belongs to the top sheet).
    fn search_row_idle(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, placeholder: &str) {
        let p = look.p;
        self.painter.fill(cx, r, p.surface1, 0.0);
        self.painter.hline(cx, r.pos.x, r.pos.y + r.size.y - 1.0, r.size.x, p.border);
        self.painter.icon(cx, rect(r.pos.x + 12.0, r.pos.y, 16.0, r.size.y), "search", 16.0, p.muted);
        let spec = TextSpec::new(Font::Regular, look.body(), p.surface4);
        self.painter.text_in(cx, rect(r.pos.x + 36.0, r.pos.y, r.size.x - 48.0, r.size.y), spec, placeholder, 0.0);
    }

    fn search_row(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, placeholder: &str) {
        let p = look.p;
        self.painter.fill(cx, r, p.surface1, 0.0);
        self.painter.hline(cx, r.pos.x, r.pos.y + r.size.y - 1.0, r.size.x, p.border);
        self.painter.icon(cx, rect(r.pos.x + 12.0, r.pos.y, 16.0, r.size.y), "search", 16.0, p.muted);
        if self.search.empty_text() != placeholder {
            self.search.set_empty_text(cx, placeholder.to_owned());
        }
        let line = self.painter.line_height(cx, TextSpec::new(Font::Regular, look.body(), p.foreground));
        let input = rect(r.pos.x + 36.0, r.pos.y + (r.size.y - line) * 0.5, r.size.x - 48.0, line);
        let walk = Walk { abs_pos: Some(input.pos), margin: Margin::default(), width: Size::Fixed(input.size.x), height: Size::Fit };
        let _ = self.search.draw_walk(cx, &mut Scope::empty(), walk);
        // A newly opened sheet has no valid input area until this first draw.
        if std::mem::take(&mut self.st.search_focus_pending) {
            self.search.set_key_focus(cx);
        }
    }

    fn draw_sheet(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look) {
        let Some(Overlay::Sheet(mut state)) = self.st.overlay.take() else { return };
        let profiles_supported = self.profiles_supported(ui);
        let query = self.st.search_text.clone();
        let stacked = state.stacked.clone();
        // Under a stacked model sheet the controls sheet shows its unfiltered root.
        let root_query = if stacked.is_some() { String::new() } else { query.clone() };
        let view = state.build(self.ctl_ref(), look.compact, &root_query, profiles_supported);
        let rows = state.rows.clone();
        let top = stacked.map(|provider| {
            // model-sheet.tsx: the compact model browser pushed over the controls sheet,
            // drilled into the single provider, without a back arrow.
            let mut models = super::sheet::SheetState::new(state.can_switch);
            models.config = state.config.clone();
            models.drill = Some(provider);
            models.pages = vec![Page::Models];
            let mut view = models.build(self.ctl_ref(), look.compact, &query, profiles_supported);
            view.back = false;
            (view, models.rows)
        });
        if let Some((_, top_rows)) = &top {
            state.stacked_rows = top_rows.clone();
        }
        self.st.overlay = Some(Overlay::Sheet(state));
        let error = self.sheet_feature_error();
        self.paint_sheet(cx, look, &view, &rows, top.is_none(), error);
        if let Some((top_view, top_rows)) = top {
            // Only the top sheet takes pointer input.
            self.st.ohits.clear();
            self.layer(cx);
            self.paint_sheet(cx, look, &top_view, &top_rows, true, None);
        }
    }

    /// One sheet: scrim, frame, header, optional search row and the rows list.
    /// `top`: the sheet that owns the search editor and the list scroll (a sheet under a
    /// stacked one shows its placeholder and unscrolled list).
    fn paint_sheet(&mut self, cx: &mut Cx2d, look: &Look, view: &super::sheet::SheetView, rows: &[super::sheet::Row], top: bool, error: Option<String>) {
        let p = look.p;
        let size = cx.current_pass_size();
        self.scrim(cx, look, size);
        let body = TextSpec::new(Font::Regular, look.body(), p.foreground);
        let small = TextSpec::new(Font::Regular, look.small(), p.muted);
        let body_h = self.painter.line_height(cx, body);
        let small_h = self.painter.line_height(cx, small);
        let row_base = (if look.compact { 44.0_f64 } else { 36.0 }).max(body_h + 12.0);
        let heights: Vec<f64> = rows
            .iter()
            .map(|row| {
                if row.divider {
                    9.0
                } else if row.heading {
                    28.0_f64.max(small_h + 12.0)
                } else if !row.description.is_empty() {
                    (row_base + 16.0).max(body_h + small_h + 12.0)
                } else {
                    row_base
                }
            })
            .collect();
        let header_h = self.sheet_header_height(cx, look);
        let search_h = if view.searchable { 44.0_f64.max(body_h + 16.0) } else { 0.0 };
        let empty_h = if view.empty.is_some() || view.loading { 64.0_f64.max(44.0 + body_h) } else { 0.0 };
        let sheet_width = if look.compact { size.x } else { 480.0_f64.min(size.x - 32.0) };
        let error_h = error.as_ref().map(|text| self.painter.paragraph_height(cx, (sheet_width - 32.0).max(1.0), small, text) + 16.0).unwrap_or(0.0);
        let content_h = heights.iter().sum::<f64>() + 32.0 + error_h + empty_h;
        let frame = self.sheet_frame(cx, look, size, header_h + search_h + content_h);
        self.sheet_header(cx, look, frame, Header { title: &view.title, back: view.back, leading: view.leading_provider.clone(), gear: view.gear.is_some() });
        let mut y = frame.pos.y + header_h;
        if view.searchable {
            let row = rect(frame.pos.x, y, frame.size.x, search_h);
            if top {
                self.search_row(cx, look, row, &view.placeholder);
            } else {
                self.search_row_idle(cx, look, row, &view.placeholder);
            }
            y += search_h;
        }
        let list = rect(frame.pos.x, y, frame.size.x, frame.pos.y + frame.size.y - y);
        let max_scroll = (content_h - list.size.y).max(0.0);
        let scroll = if top { self.st.sheet_scroll.clamp(0.0, max_scroll) } else { 0.0 };
        if top {
            self.st.sheet_scroll = scroll;
            self.st.sheet_scroll_max = max_scroll;
        }
        cx.begin_turtle(Walk::abs_rect(list), Layout::flow_down().with_clip(true, true));
        self.st.ohits.set_clip(Some(list));
        let mut ry = list.pos.y - scroll;
        let inset = if look.compact { 16.0 } else { 12.0 };
        if view.loading {
            self.spinner(cx, rect(list.pos.x + list.size.x * 0.5 - 8.0, ry + 24.0, 16.0, 16.0), p.muted);
            ry += empty_h;
        }
        if let Some((icon, text)) = &view.empty {
            self.painter.icon(cx, rect(list.pos.x, ry + 16.0, list.size.x, 16.0), icon, 16.0, p.muted);
            self.painter.text_in(cx, rect(list.pos.x + 24.0, ry + 36.0, list.size.x - 48.0, body_h.max(20.0)), TextSpec::new(Font::Regular, look.body(), p.muted), text, 0.5);
            ry += empty_h;
        }
        for (index, (row, h)) in rows.iter().zip(heights.iter()).enumerate() {
            let r = rect(list.pos.x, ry, list.size.x, *h);
            ry += h;
            if r.pos.y + r.size.y < list.pos.y || r.pos.y > list.pos.y + list.size.y {
                continue;
            }
            if row.divider {
                self.painter.hline(cx, r.pos.x, r.pos.y + 4.0, r.size.x, p.border);
                continue;
            }
            if row.heading {
                self.painter.text_in(cx, rect(r.pos.x + inset, r.pos.y + 8.0, r.size.x - inset * 2.0, small_h.max(16.0)), small, &row.label, 0.0);
                if let Some((icon, _, label)) = &row.secondary {
                    let b = rect(r.pos.x + r.size.x - inset - 24.0, r.pos.y + 2.0, 24.0, 24.0);
                    self.painter.icon(cx, b, icon, 12.0, p.extra_muted);
                    self.st.ohits.push(b, OItem::RowAction(index));
                    let _ = label;
                }
                continue;
            }
            let item = OItem::Row(index);
            // Before the trailing action so the action's hit wins (HitMap takes the last match).
            if row.action.is_some() {
                self.st.ohits.push(r, item.clone());
            }
            if row.selected || self.st.ohover.as_ref() == Some(&item) {
                self.painter.fill(cx, rect(r.pos.x + 4.0, r.pos.y + 1.0, r.size.x - 8.0, r.size.y - 2.0), p.surface2, 6.0);
            }
            let mut x = r.pos.x + inset;
            let mid = r.pos.y + r.size.y * 0.5;
            match &row.leading {
                Leading::None => {}
                Leading::Icon(icon, tint) => {
                    let color = tint.map(|c| vec4(c[0], c[1], c[2], c[3])).unwrap_or(p.muted);
                    self.painter.icon(cx, rect(x, mid - 8.0, 16.0, 16.0), icon, 16.0, color);
                    x += 24.0;
                }
                Leading::Provider(provider, svg) => {
                    crate::common::provider_icon::ProviderIcon::paint(&mut self.painter, &mut self.draw_icon, cx, rect(x, mid - 8.0, 16.0, 16.0), provider, (!svg.is_empty()).then_some(svg.as_str()), p.muted);
                    x += 24.0;
                }
            }
            let mut right = r.pos.x + r.size.x - inset;
            if row.chevron {
                self.painter.icon(cx, rect(right - 14.0, mid - 7.0, 14.0, 14.0), "chevron-right", 14.0, p.muted);
                right -= 18.0;
            }
            if row.selected {
                self.painter.icon(cx, rect(right - 16.0, mid - 8.0, 16.0, 16.0), "check", 16.0, p.foreground);
                right -= 20.0;
            }
            if let Some((icon, _, _)) = &row.secondary {
                let b = rect(right - 28.0, mid - 14.0, 28.0, 28.0);
                self.painter.icon(cx, b, icon, 12.0, p.muted);
                self.st.ohits.push(b, OItem::RowAction(index));
                right -= 32.0;
            }
            if !row.value.is_empty() {
                let w = self.painter.measure(cx, small, &row.value).x.min((right - x) * 0.5);
                self.painter.text_in(cx, rect(right - w, r.pos.y, w, r.size.y), small, &row.value, 1.0);
                right -= w + 4.0;
                if row.loading {
                    self.spinner(cx, rect(right - 12.0, mid - 6.0, 12.0, 12.0), p.muted);
                    right -= 16.0;
                }
                if row.warning {
                    self.painter.icon(cx, rect(right - 14.0, mid - 7.0, 14.0, 14.0), "triangle-alert", 14.0, p.muted);
                    right -= 18.0;
                }
            }
            let label_spec = if row.label_muted { TextSpec::new(Font::Regular, look.body(), p.muted) } else { body };
            if row.description.is_empty() {
                self.painter.text_in(cx, rect(x, r.pos.y, (right - x).max(0.0), r.size.y), label_spec, &row.label, 0.0);
            } else {
                let lh = self.painter.line_height(cx, body);
                let sh = self.painter.line_height(cx, small);
                let top = r.pos.y + (r.size.y - lh - sh) * 0.5;
                self.painter.text_in(cx, rect(x, top, (right - x).max(0.0), lh), label_spec, &row.label, 0.0);
                self.painter.text_in(cx, rect(x, top + lh, (right - x).max(0.0), sh), small, &row.description, 0.0);
            }
        }
        if let Some(error) = error {
            let spec = TextSpec::new(Font::Regular, look.small(), p.destructive);
            self.painter.paragraph(cx, dvec2(list.pos.x + 16.0, ry + 8.0), list.size.x - 32.0, spec, &error);
        }
        self.st.ohits.set_clip(None);
        cx.end_turtle();
    }

    fn sheet_feature_error(&self) -> Option<String> {
        match self.st.overlay() {
            Overlay::Sheet(state) if state.page() == Page::Root => self.ctl_ref().error.clone().filter(|_| self.ctl_ref().is_draft()),
            _ => None,
        }
    }

    pub(super) fn profiles_supported(&self, ui: &UiContext) -> bool {
        let info = self.ctl_ref().server_info();
        let info = if info.is_null() { ui.server_infos.get(self.ctl_ref().host()).cloned().unwrap_or_default() } else { info };
        get(get(&info, "features"), "agentProfiles") == &Value::Bool(true)
    }

    /// composer/index.tsx forge picker: popover above the attach button (desktop), sheet on
    /// compact layouts.
    fn draw_forge(&mut self, cx: &mut Cx2d, look: &Look, size: DVec2) {
        let p = look.p;
        self.scrim_transparent(cx, size);
        let ctl = self.ctl_ref();
        let suffix = if ctl.forge == "gitlab" { "_mr" } else { "" };
        let items = ctl.forge_picker_items.clone();
        let error = ctl.forge_picker_error.clone();
        let fetching = ctl.forge_picker_fetching;
        let attached = ctl.attached_forge_keys();
        let query = self.st.search_text.trim().to_owned();
        let body = TextSpec::new(Font::Regular, look.body(), p.foreground);
        let small = TextSpec::new(Font::Regular, look.small(), p.muted);
        let body_h = self.painter.line_height(cx, body);
        let small_h = self.painter.line_height(cx, small);
        let row_h = if query.is_empty() { 36.0_f64.max(body_h + 12.0) } else { 44.0_f64.max(body_h + small_h + 12.0) };
        let header_h = self.sheet_header_height(cx, look);
        let search_h = 44.0_f64.max(body_h + 16.0);
        let list_h = if items.is_empty() || error.is_some() { 40.0_f64.max(body_h + 16.0) } else { items.len() as f64 * row_h + 8.0 };
        let anchor = self.st.attach_rect;
        let frame = if look.compact {
            let h = (header_h + search_h + list_h).min(size.y * 0.8);
            let r = rect(0.0, size.y - h, size.x, h);
            self.painter.fill(cx, rect(0.0, 0.0, size.x, size.y), p.scrim, 0.0);
            self.painter.outline(cx, r, p.surface0, p.border, 1.0, 16.0);
            self.sheet_header(cx, look, r, Header::plain(&t(&format!("composer.github.title{suffix}"))));
            rect(r.pos.x, r.pos.y + header_h, r.size.x, h - header_h)
        } else {
            let w = 400.0f64.min(size.x - 16.0);
            let max_h = (anchor.pos.y - 16.0).clamp(120.0, 400.0);
            let h = (search_h + list_h).min(max_h);
            let x = anchor.pos.x.clamp(8.0, (size.x - w - 8.0).max(8.0));
            let r = rect(x, anchor.pos.y - 8.0 - h, w, h);
            self.card(cx, look, r, p.surface0);
            r
        };
        self.st.ohits.push(frame, OItem::Panel);
        self.search_row(cx, look, rect(frame.pos.x, frame.pos.y, frame.size.x, search_h), &t(&format!("composer.github.searchPlaceholder{suffix}")));
        let list = rect(frame.pos.x, frame.pos.y + search_h, frame.size.x, frame.size.y - search_h);
        let message = if let Some(error) = error {
            Some(error)
        } else if items.is_empty() {
            Some(t(if fetching { "composer.github.searching" } else { "composer.github.noResults" }))
        } else {
            None
        };
        if let Some(message) = message {
            self.st.sheet_scroll_max = 0.0;
            self.st.sheet_scroll = 0.0;
            self.painter.text_in(cx, rect(list.pos.x + 12.0, list.pos.y + 8.0, list.size.x - 24.0, body_h.max(24.0)), TextSpec::new(Font::Regular, look.body(), p.muted), &message, 0.0);
            return;
        }
        let content = items.len() as f64 * row_h + 8.0;
        self.st.sheet_scroll_max = (content - list.size.y).max(0.0);
        self.st.sheet_scroll = self.st.sheet_scroll.clamp(0.0, (content - list.size.y).max(0.0));
        cx.begin_turtle(Walk::abs_rect(list), Layout::flow_down().with_clip(true, true));
        self.st.ohits.set_clip(Some(list));
        for (index, item) in items.iter().enumerate() {
            let r = rect(list.pos.x, list.pos.y + 4.0 + index as f64 * row_h - self.st.sheet_scroll, list.size.x, row_h);
            let key = crate::features::composer::attachments::forge_item_key(item);
            let change = key.starts_with("change_request");
            let prefix = if change && s(get(item, "forge")) == "gitlab" { "!" } else { "#" };
            let o = OItem::Forge(index);
            if self.st.ohover.as_ref() == Some(&o) {
                self.painter.fill(cx, r, p.surface1, 0.0);
            }
            let mid = r.pos.y + r.size.y * 0.5;
            self.painter.icon(cx, rect(r.pos.x + 12.0, mid - 8.0, 16.0, 16.0), if change { "git-pull-request" } else { "circle-dot" }, 14.0, p.muted);
            let label = format!("{prefix}{} {}", display(get(item, "number")), s(get(item, "title")));
            let right = r.pos.x + r.size.x - 12.0 - 24.0;
            if query.is_empty() {
                self.painter.text_in(cx, rect(r.pos.x + 36.0, r.pos.y, right - r.pos.x - 36.0, r.size.y), body, &label, 0.0);
            } else {
                let lh = self.painter.line_height(cx, body);
                let sh = self.painter.line_height(cx, small);
                let top = r.pos.y + (r.size.y - lh - sh) * 0.5;
                self.painter.text_in(cx, rect(r.pos.x + 36.0, top, right - r.pos.x - 36.0, lh), body, &label, 0.0);
                self.painter.text_in(cx, rect(r.pos.x + 36.0, top + lh, right - r.pos.x - 36.0, sh), small, &query, 0.0);
            }
            if attached.contains(&key) {
                self.painter.icon(cx, rect(right + 4.0, mid - 8.0, 16.0, 16.0), "check", 16.0, p.muted);
            }
            self.st.ohits.push(r, o);
        }
        self.st.ohits.set_clip(None);
        cx.end_turtle();
    }

    /// A transparent full-window catcher: outside taps close the popover.
    fn scrim_transparent(&mut self, cx: &mut Cx2d, size: DVec2) {
        let full = rect(0.0, 0.0, size.x, size.y);
        self.overlay_bg.draw_abs(cx, full);
        self.st.ohits.push(full, OItem::Scrim);
    }

    fn draw_preview(&mut self, cx: &mut Cx2d, look: &Look, size: DVec2, image: &Value) {
        let p = look.p;
        let full = rect(0.0, 0.0, size.x, size.y);
        self.painter.fill(cx, full, p.background, 0.0);
        self.overlay_bg.draw_abs(cx, full);
        self.st.ohits.push(full, OItem::Panel);
        match service::with(|svc| svc.blobs.texture(image, false)) {
            Ok(Some((texture, px))) => self.draw_image.paint(cx, rect(16.0, 16.0, size.x - 32.0, size.y - 32.0), &texture, px, 0.0, true),
            Ok(None) => self.spinner(cx, rect(size.x * 0.5 - 12.0, size.y * 0.5 - 12.0, 24.0, 24.0), p.muted),
            Err(error) => {
                self.painter.text_in(cx, rect(24.0, size.y * 0.5 - 12.0, size.x - 48.0, 24.0), TextSpec::new(Font::Regular, look.body(), p.muted), &error, 0.5);
            }
        }
        let close = rect(size.x - 12.0 - 40.0, 12.0, 40.0, 40.0);
        self.painter.fill(cx, close, fade(p.surface1, 0.85), 20.0);
        self.painter.icon(cx, close, "x", 18.0, p.foreground);
        self.st.ohits.push(close, OItem::Close);
    }

    /// input/presentation.tsx fullscreen editor: surface1, 16px insets, 12px gaps.
    fn draw_fullscreen(&mut self, cx: &mut Cx2d, look: &Look, size: DVec2) {
        let p = look.p;
        let full = rect(0.0, 0.0, size.x, size.y);
        self.painter.fill(cx, full, p.surface1, 0.0);
        self.overlay_bg.draw_abs(cx, full);
        self.st.ohits.push(full, OItem::Panel);
        let glyph = look.glyph();
        let rail = (28.0 - glyph) / 2.0 + 5.0 * glyph / 24.0 - ((28.0 - 16.0) / 2.0 + 3.0 * 16.0 / 24.0);
        let minimize = rect(size.x - 16.0 - 28.0 - rail, 16.0, 28.0, 28.0);
        if self.st.ohover == Some(OItem::Close) {
            self.painter.fill(cx, minimize, p.surface2, 14.0);
        }
        self.painter.icon(cx, minimize, "minimize-2", 16.0, p.muted);
        self.st.ohits.push(minimize, OItem::Close);
        let error = get(&self.ctl_ref().draft(), "sendError").as_str().map(str::to_owned);
        let error_h = match &error {
            Some(text) => self.painter.paragraph_height(cx, size.x - 32.0, TextSpec::new(Font::Regular, look.body(), look.red500()), text) + 12.0,
            None => 0.0,
        };
        let bottom = 16.0 + 28.0 + 12.0 + error_h;
        let editor = rect(16.0, 16.0 + 28.0 + 12.0, size.x - 32.0, (size.y - 16.0 - 28.0 - 12.0 - bottom).max(40.0));
        self.draw_input(cx, look, editor, 0.0, 0.0, editor.size.y);
        if let Some(text) = error {
            self.painter.paragraph(cx, dvec2(16.0, editor.pos.y + editor.size.y + 12.0), size.x - 32.0, TextSpec::new(Font::Regular, look.body(), look.red500()), &text);
        }
        let send = rect(size.x - 16.0 - 28.0, size.y - 16.0 - 28.0, 28.0, 28.0);
        let ctl = self.ctl_ref();
        let empty = ctl.editor_text.trim().is_empty()
            && !non_empty_list(get(&ctl.draft(), "attachments"))
            && !non_empty_list(get(&ctl.draft(), "images"));
        let enabled = !empty && !ctl.sending() && !ctl.uploading();
        self.painter.fill(cx, send, fade(p.accent, if enabled { 1.0 } else { 0.5 }), 14.0);
        self.painter.icon(cx, send, "arrow-up", glyph, fade(p.on_tertiary, if enabled { 1.0 } else { 0.5 }));
        if enabled {
            self.st.ohits.push(send, OItem::FullscreenSend);
        }
    }

    fn draw_context_sheet(&mut self, cx: &mut Cx2d, look: &Look, size: DVec2) {
        self.scrim(cx, look, size);
        let width = size.x - 32.0;
        let content = self.context_details(cx, look, rect(0.0, 0.0, width, 0.0), Details::SHEET, false);
        let header_h = self.sheet_header_height(cx, look);
        let h = (header_h + content + 32.0).min(size.y * 0.9);
        let r = rect(0.0, size.y - h, size.x, h);
        self.painter.outline(cx, r, look.p.surface0, look.p.border, 1.0, 16.0);
        self.st.ohits.push(r, OItem::Panel);
        self.sheet_header(cx, look, r, Header::plain(&t("contextWindow.title")));
        self.context_details(cx, look, rect(16.0, r.pos.y + header_h, width, 0.0), Details::SHEET, true);
    }

    /// Consume dismissal before dispatching to the editor/search.
    pub(super) fn dismiss_overlay_event(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid) -> bool {
        if event.back_pressed()
            || (matches!(event, Event::KeyDown(ke) if ke.key_code == KeyCode::Escape)
                && !matches!(self.st.overlay(), Overlay::Fullscreen))
        {
            self.overlay_click(cx, ui, path, uid, OItem::Close);
            return true;
        }
        false
    }

    pub(super) fn overlay_event(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid) {
        if matches!(event, Event::Scroll(_)) {
            self.st.touch_scroll.reset();
        }
        if let Event::Scroll(scroll) = event
            && matches!(self.st.overlay(), Overlay::Sheet(_) | Overlay::Forge)
        {
            self.st.sheet_scroll = (self.st.sheet_scroll + scroll.scroll.y).max(0.0);
            self.draw_bg.redraw(cx);
        }
        if let Event::Scroll(scroll) = event
            && matches!(self.st.overlay(), Overlay::Fullscreen)
            && self.st.input_clip.contains(scroll.abs)
        {
            self.st.input_manual_scroll = true;
            self.st.input_scroll = (self.st.input_scroll + scroll.scroll.y)
                .clamp(0.0, (self.st.input_natural - self.st.input_clip.size.y).max(0.0));
            self.draw_bg.redraw(cx);
        }
        // Fullscreen editor and sheet search inputs take their own pointer hits first.
        let hit = event.hits_with_capture_overload(cx, self.overlay_bg.area(), true);
        if let Some(dy) = self.st.touch_scroll.handle_event(cx, event, &hit) {
            let origin = self.st.touch_scroll.origin();
            if matches!(self.st.overlay(), Overlay::Sheet(_) | Overlay::Forge)
                && self.st.overlay_rect.is_some_and(|r| r.contains(origin))
            {
                self.st.touch_scroll.apply_delta(&mut self.st.sheet_scroll, dy, self.st.sheet_scroll_max);
                self.draw_bg.redraw(cx);
            } else if matches!(self.st.overlay(), Overlay::Fullscreen) && self.st.input_clip.contains(origin) {
                self.st.input_manual_scroll = true;
                let max = (self.st.input_natural - self.st.input_clip.size.y).max(0.0);
                self.st.touch_scroll.apply_delta(&mut self.st.input_scroll, dy, max);
                self.draw_bg.redraw(cx);
            } else {
                self.st.touch_scroll.reset();
            }
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let item = self.st.ohits.at(e.abs);
                if item != self.st.ohover {
                    self.st.ohover = item;
                    self.draw_bg.redraw(cx);
                }
            }
            Hit::FingerHoverOut(_) => {
                self.st.ohover = None;
                self.draw_bg.redraw(cx);
            }
            Hit::FingerUp(e) if e.is_over && e.was_tap() => {
                if let Some(item) = self.st.ohits.at(e.abs) {
                    self.overlay_click(cx, ui, path, uid, item);
                }
            }
            _ => {}
        }
    }

    fn overlay_click(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, item: OItem) {
        match item {
            OItem::Scrim | OItem::Close => {
                // A stacked model sheet closes back to the controls sheet.
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut()
                    && state.stacked.is_some()
                {
                    state.stacked = None;
                    self.clear_search(cx);
                } else {
                    self.close_overlay(cx, ui);
                }
            }
            OItem::Panel => {}
            OItem::Back => {
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                    state.pop();
                }
                self.clear_search(cx);
            }
            OItem::Gear => {
                // model-browser.tsx ProviderSettingsAction → the provider's settings sheet.
                let provider = match self.st.overlay() {
                    Overlay::Sheet(state) => state.stacked.clone().or_else(|| state.drill.clone()).unwrap_or_default(),
                    _ => String::new(),
                };
                self.close_overlay(cx, ui);
                let host = self.ctl_ref().host().to_owned();
                crate::features::settings::open_provider_settings(ui, &host, &provider);
            }
            OItem::Row(index) | OItem::RowAction(index) => {
                let secondary = matches!(item, OItem::RowAction(_));
                let action = match self.st.overlay() {
                    Overlay::Sheet(state) => if state.stacked.is_some() { &state.stacked_rows } else { &state.rows }.get(index).and_then(|row| {
                        if secondary { row.secondary.as_ref().map(|(_, a, _)| a.clone()) } else { row.action.clone() }
                    }),
                    _ => None,
                };
                if let Some(action) = action {
                    self.sheet_action(cx, ui, path, uid, action);
                }
            }
            OItem::Forge(index) => {
                if let Some(item) = self.ctl_ref().forge_picker_items.get(index).cloned() {
                    let out = self.ctl().toggle_forge_item(&item);
                    self.close_overlay(cx, ui);
                    self.apply(cx, ui, path, uid, out);
                }
            }
            OItem::FullscreenSend => {
                self.close_overlay(cx, ui);
                let out = self.ctl().submit(ui, None, None);
                self.apply(cx, ui, path, uid, out);
            }
            OItem::UsageRefresh(index) => {
                if let Some(id) = self.ctl_ref().usage_reports.get(index).map(|r| get(r, "id").clone()) {
                    self.ctl().refresh_usage(ui, &id);
                }
            }
        }
        self.draw_bg.redraw(cx);
    }

    fn clear_search(&mut self, cx: &mut Cx) {
        self.st.touch_scroll.reset();
        self.st.search_text.clear();
        self.st.sheet_scroll = 0.0;
        self.search.set_text(cx, "");
    }

    fn sheet_action(&mut self, cx: &mut Cx, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid, action: RowAction) {
        let compact = ui.compact;
        let host = self.ctl_ref().host().to_owned();
        let (page, can_switch, config, feature) = match self.st.overlay() {
            Overlay::Sheet(state) => (state.page(), state.can_switch, state.config(self.ctl_ref()), state.feature.clone()),
            _ => return,
        };
        let (providers, features) = (self.ctl_ref().providers.clone(), self.ctl_ref().features());
        let modes = self.ctl_ref().modes();
        let model = ControlsModel { providers: &providers, config: &config, modes: &modes, features: &features };
        let mut out = Outputs::default();
        match action {
            RowAction::ApplyProfile(profile) => {
                out = self.ctl().apply_profile(ui, &profile);
                self.close_overlay(cx, ui);
            }
            RowAction::OpenProfiles => {
                // index.tsx useEditAgentProfilesNavigation: settings › host › agents.
                self.close_overlay(cx, ui);
                ui.push(UiEffect::Navigate(Route::Settings { host_id: Some(host), page: "host/agents".into() }));
            }
            RowAction::CreateProfileFromModel(seed) => {
                // agent-profile-editor.tsx openCreateFromModel: the editor sheet, seeded with
                // the row, over the composer.
                self.close_overlay(cx, ui);
                self.profile_editor.as_settings_panel().open_profile_editor(cx, ui, &host, Some(seed), None);
            }
            RowAction::EditProfile(profile) => {
                self.close_overlay(cx, ui);
                self.profile_editor.as_settings_panel().open_profile_editor(cx, ui, &host, None, Some(profile));
            }
            RowAction::Drill(provider) => {
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                    state.drill = Some(provider);
                    state.push(Page::Models);
                }
                self.clear_search(cx);
            }
            RowAction::OpenModels => {
                let available: Vec<Value> = if can_switch {
                    providers.clone()
                } else {
                    providers.iter().find(|p| get(p, "provider") == get(&config, "provider")).cloned().into_iter().collect()
                };
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                    let single = !can_switch || available.len() == 1;
                    let provider = available.first().map(|p| owned(get(p, "provider")));
                    if single && compact {
                        state.stacked = provider;
                    } else if single {
                        state.drill = provider;
                        state.push(Page::Models);
                    } else {
                        state.push(Page::Providers);
                    }
                }
                self.clear_search(cx);
            }
            RowAction::SelectModel { provider, model: model_id } => {
                let next = model.with_model(&provider, &model_id);
                out = self.sheet_apply(ui, next);
                if compact {
                    if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                        state.pages = vec![Page::Root];
                        state.drill = None;
                        state.stacked = None;
                    }
                    self.clear_search(cx);
                } else {
                    self.close_overlay(cx, ui);
                }
            }
            RowAction::Push(next_page) => {
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                    state.push(next_page);
                }
                self.clear_search(cx);
            }
            RowAction::OpenFeature(next_feature) => {
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                    state.feature = Some(next_feature);
                    state.push(Page::Feature);
                }
                self.clear_search(cx);
            }
            RowAction::Choose(id) => {
                let next = match page {
                    Page::Thinking => Some(model.with_thinking(&id)),
                    Page::Mode => Some(model.with_mode(&id)),
                    Page::Feature => feature.map(|f| {
                        let value = if s(get(&f, "type")) == "toggle" { Value::Bool(id == "true") } else { Value::String(id.clone()) };
                        model.with_feature(s(get(&f, "id")), value)
                    }),
                    _ => None,
                };
                if let Some(next) = next {
                    out = self.sheet_apply(ui, next);
                }
                if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
                    state.pop();
                }
            }
        }
        self.apply(cx, ui, path, uid, out);
    }

    /// `_apply`: show the selection at once and persist/apply it.
    fn sheet_apply(&mut self, ui: &mut UiContext, next: Value) -> Outputs {
        if let Some(Overlay::Sheet(state)) = self.st.overlay.as_mut() {
            state.config = Some(next.clone());
        }
        self.ctl().save_config(ui, next)
    }

    /// Hover/click events of the non-modal popovers (autocomplete, context card).
    pub(super) fn popup_event(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext, path: &HeapLiveIdPath, uid: WidgetUid) -> bool {
        if self.st.overlay_rect.is_none() {
            return false;
        }
        let hit = event.hits_with_capture_overload(cx, self.overlay_bg.area(), true);
        let owns_pan = self.st.overlay_rect.is_some_and(|r| r.contains(self.st.touch_scroll.origin()));
        if (owns_pan || matches!(hit, Hit::FingerDown(_)))
            && let Some(dy) = self.st.touch_scroll.handle_event(cx, event, &hit)
        {
            if self.suggestions_visible() {
                self.st.touch_scroll.apply_delta(&mut self.st.suggest_scroll, -dy, self.st.suggest_scroll_max);
                self.draw_bg.redraw(cx);
            }
            return true;
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let item = self.st.hits.at(e.abs).filter(|i| matches!(i, Item::Suggest(_)));
                if item != self.st.hover {
                    self.st.hover = item;
                    self.draw_bg.redraw(cx);
                }
                if self.st.meter_hover && !self.suggestions_visible() {
                    self.st.card_hover = true;
                }
                true
            }
            Hit::FingerHoverOut(_) => {
                if self.st.card_hover {
                    self.st.card_hover = false;
                    cx.stop_timer(self.st.meter_timer);
                    self.st.meter_timer = cx.start_timeout(0.25);
                }
                self.draw_bg.redraw(cx);
                true
            }
            Hit::FingerUp(e) if e.is_over && e.was_tap() => {
                if let Some(OItem::UsageRefresh(index)) = self.st.ohits.at(e.abs) {
                    if let Some(id) = self.ctl_ref().usage_reports.get(index).map(|r| get(r, "id").clone()) {
                        self.ctl().refresh_usage(ui, &id);
                    }
                } else if let Some(Item::Suggest(index)) = self.st.hits.at(e.abs)
                    && let Some(option) = self.ctl_ref().autocomplete.options.get(index).cloned()
                {
                    let out = self.ctl().apply_autocomplete(ui, &option);
                    self.apply(cx, ui, path, uid, out);
                }
                true
            }
            Hit::FingerDown(_) => true,
            _ => false,
        }
    }
}
