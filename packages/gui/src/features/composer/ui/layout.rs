//! Inline composer rendering (Flutter `_composer`, `_composerTextSurface`, `_attachButton`,
//! `AgentControls`, `ContextWindowMeter`, `_sendControl`, `ComposerQueueTrack`,
//! `_deliveryPending`, `_failedDelivery`, `AttachmentPill`). Geometry follows the original:
//! 16px outer insets, 12px gaps, a surface1 card (borderAccent, 16 radius) with 8/12/8 insets
//! below 720px and 16/16/12 above, 28px toolbar glyph buttons on a 6px bleed.

use super::look::Look;
use super::panel::{ComposerPanel, Item};
use crate::common::paint::{rect, Font, TextSpec};
use crate::common::UiContext;
use crate::features::composer::attachments::{describe, file_type_label, AttachmentGlyph};
use crate::features::composer::controls_model::*;
use crate::features::composer::i18n::{t, t_args};
use crate::features::composer::json::*;
use crate::features::composer::service;
use crate::theme::fade;
use makepad_widgets::*;
use serde_json::Value;

pub const OVERHANG: f64 = 8.0;

/// One block of the column above the card.
enum Block {
    Queue(Vec<Value>),
    Error(String, bool),
    Muted(String),
    Pending(Value),
    Failed(Value),
}

impl ComposerPanel {
    fn tip(&mut self, r: Rect, text: String) {
        if !text.is_empty() {
            self.st.tips.push((r, text));
        }
    }

    /// Isolate the card, tooltip or sheet from earlier blocks.
    pub(super) fn layer(&mut self, cx: &mut Cx2d) {
        self.painter.new_layer(cx);
    }

    pub(super) fn spinner(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4) {
        let rotation = (self.st.spin * 1.2).fract();
        self.painter.ring(cx, r, color, 2.0, 0.7, rotation);
        self.st.next_frame = cx.new_next_frame();
    }

    pub(super) fn draw_composer(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look, walk: Walk) {
        self.st.hits.clear();
        self.st.tips.clear();
        self.st.mode_shortcut = crate::common::shortcut_label(ui, "cycle-agent-mode");
        self.st.interrupt_shortcut = crate::common::shortcut_label(ui, "agent-interrupt");
        let ctl_sheet = self.ctl_ref().is_sheet();
        let subagent = self.ctl_ref().is_subagent();
        let probe = cx.peek_walk_turtle(Walk { height: Size::Fixed(0.0), ..walk });
        if ctl_sheet || subagent {
            // pane-context host "sheet" and provider subagents show no composer.
            let r = cx.walk_turtle(Walk { height: Size::Fixed(0.0), ..walk });
            self.draw_bg.draw_abs(cx, r);
            return;
        }
        let full_width = probe.size.x.max(0.0);
        let content_width = (full_width - 32.0).min(look.t.content_max_width).max(0.0);
        let left = probe.pos.x + (full_width - content_width) * 0.5;

        let draft = self.ctl_ref().draft();
        let blocks = self.blocks(&draft);
        let block_heights: Vec<f64> = blocks.iter().map(|b| self.block_height(cx, look, b, content_width)).collect();
        self.st.input_cap = None;
        let (mut card_height, input_visible) = self.card_height(cx, look, &draft, content_width);
        let gaps = blocks.len() as f64 * 12.0;
        let mut total = block_heights.iter().sum::<f64>() + gaps + card_height + 16.0;
        // dock/index(.native).tsx ComposerViewportContent bounds the complete composer,
        // controls included, by the slot capacity. The text input is its one shrinking
        // child (input.tsx textInput flexShrink 1 down to its minHeight); its text scrolls.
        if let Some(capacity) = self.slot_capacity(look) {
            let (min, _, _) = self.input_bounds(cx, look);
            let cap = (input_visible - (total - capacity)).max(min);
            if cap < input_visible {
                self.st.input_cap = Some(cap);
                card_height -= input_visible - cap;
                total -= input_visible - cap;
            }
        }
        let r = cx.walk_turtle(Walk { height: Size::Fixed(total), ..walk });
        self.draw_bg.draw_abs(cx, r);

        let mut y = r.pos.y;
        for (block, height) in blocks.iter().zip(block_heights) {
            self.draw_block(cx, look, block, rect(left, y, content_width, height));
            y += height + 12.0;
        }
        self.draw_card(cx, ui, look, &draft, rect(left, y, content_width, card_height));
    }

    fn blocks(&self, draft: &Value) -> Vec<Block> {
        let ctl = self.ctl_ref();
        let mut blocks = Vec::new();
        // A dequeued (sending) message leaves the queue track; the stream shows it.
        let queue: Vec<Value> = maps(get(draft, "queue")).into_iter().filter(|q| s(get(q, "state")) != "sending").collect();
        if !queue.is_empty() {
            blocks.push(Block::Queue(queue.clone()));
        }
        if let Some(error) = get(draft, "sendError").as_str().filter(|e| !e.is_empty()) {
            blocks.push(Block::Error(error.to_owned(), false));
        }
        if let Some(error) = ctl.error.as_ref().filter(|e| !e.is_empty()) {
            blocks.push(Block::Error(error.clone(), true));
        }
        if let Some(error) = service::with(|svc| svc.persistence_error()) {
            blocks.push(Block::Error(format!("Draft persistence failed: {error}"), false));
        }
        if ctl.not_ready_notice && ctl.execution_blocked() {
            blocks.push(Block::Muted(t("composer.delivery.agentNotReady")));
        }
        let pending = get(draft, "pending");
        if non_empty_obj(pending) && !ctl.is_draft() {
            blocks.push(Block::Pending(pending.clone()));
        }
        let failed = get(draft, "failedMessage");
        if non_empty_obj(failed) {
            blocks.push(Block::Failed(failed.clone()));
        }
        for queued in queue.iter().filter(|q| s(get(q, "state")) == "unacknowledged") {
            blocks.push(Block::Pending(queued.clone()));
        }
        blocks
    }

    fn block_height(&mut self, cx: &mut Cx2d, look: &Look, block: &Block, width: f64) -> f64 {
        let body = TextSpec::new(Font::Regular, look.body(), look.p.foreground);
        let small = TextSpec::new(Font::Regular, look.small(), look.p.muted);
        match block {
            Block::Queue(items) => {
                let mut h = 0.0;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        h += 8.0;
                    }
                    h += self.queue_row_height(cx, look, item, width);
                }
                h
            }
            Block::Error(text, _) | Block::Muted(text) => self.painter.paragraph_height(cx, width - 24.0, body, text),
            Block::Pending(_) => self.painter.line_height(cx, small) + 4.0 + 32.0,
            Block::Failed(message) => {
                let text = t_args("composer.delivery.notSent", &[("preview", s(get(message, "text")))]);
                self.painter.paragraph_height(cx, width, body, &text) + 4.0 + 32.0
            }
        }
    }

    fn queue_row_height(&mut self, cx: &mut Cx2d, look: &Look, item: &Value, width: f64) -> f64 {
        let body = TextSpec::new(Font::Regular, look.body(), look.p.foreground);
        let small = TextSpec::new(Font::Regular, look.small(), look.p.muted);
        let text_width = (width - 24.0 - 8.0 - 72.0).max(40.0);
        let line = self.painter.line_height(cx, body);
        let text = s(get(item, "text"));
        let lines = self.painter.paragraph_height(cx, text_width, body, text).min(line * 2.0).max(line);
        let status = if s_or(get(item, "state"), "queued") != "queued" { self.painter.line_height(cx, small) } else { 0.0 };
        (lines + status + 16.0).max(48.0)
    }

    fn draw_block(&mut self, cx: &mut Cx2d, look: &Look, block: &Block, r: Rect) {
        let p = look.p;
        match block {
            Block::Queue(items) => {
                let mut y = r.pos.y;
                for item in items {
                    let h = self.queue_row_height(cx, look, item, r.size.x);
                    self.draw_queue_row(cx, look, item, rect(r.pos.x, y, r.size.x, h));
                    y += h + 8.0;
                }
            }
            Block::Error(text, dismissable) => {
                // index.tsx sendErrorText: red-500, base size.
                let spec = TextSpec::new(Font::Regular, look.body(), look.red500());
                self.painter.paragraph(cx, r.pos, r.size.x - 24.0, spec, text);
                if *dismissable {
                    let x = rect(r.pos.x + r.size.x - 20.0, r.pos.y, 20.0, 20.0);
                    self.painter.icon(cx, x, "x", 14.0, p.muted);
                    self.st.hits.push(x, Item::DismissError);
                }
            }
            Block::Muted(text) => {
                let spec = TextSpec::new(Font::Regular, look.body(), p.muted);
                self.painter.paragraph(cx, r.pos, r.size.x, spec, text);
            }
            Block::Pending(message) => self.draw_pending(cx, look, message, r),
            Block::Failed(message) => self.draw_failed(cx, look, message, r),
        }
    }

    /// composer/index.tsx QueuedMessageRow.
    fn draw_queue_row(&mut self, cx: &mut Cx2d, look: &Look, item: &Value, r: Rect) {
        let p = look.p;
        self.painter.outline(cx, r, p.surface1, p.border, 1.0, 8.0);
        let id = owned(get(item, "messageId"));
        let state = s_or(get(item, "state"), "queued").to_owned();
        let editable = matches!(state.as_str(), "queued" | "blocked" | "rejected");
        let sending = self.ctl_ref().sending();
        let body = TextSpec::new(Font::Regular, look.body(), p.foreground);
        let text_width = (r.size.x - 24.0 - 8.0 - 72.0).max(40.0);
        let line = self.painter.line_height(cx, body);
        let text = s(get(item, "text"));
        let full = self.painter.paragraph_height(cx, text_width, body, text);
        let mut y = r.pos.y + 8.0;
        if full > line * 2.0 + 0.5 {
            // Two lines, the second ellipsized.
            let first_end = self.fit_prefix(cx, body, text, text_width);
            self.painter.text(cx, dvec2(r.pos.x + 12.0, y), body, &text[..first_end]);
            let rest = text[first_end..].trim_start();
            self.painter.text_in(cx, rect(r.pos.x + 12.0, y + line, text_width, line), body, rest, 0.0);
            y += line * 2.0;
        } else {
            y += self.painter.paragraph(cx, dvec2(r.pos.x + 12.0, y), text_width, body, text).y.max(line);
        }
        if state != "queued" {
            let error = get(item, "error");
            let status = if error.is_null() { state.clone() } else { format!("{state}: {}", display(error)) };
            let color = if state == "rejected" { p.destructive } else { p.muted };
            let small = TextSpec::new(Font::Regular, look.small(), color);
            let h = self.painter.line_height(cx, small);
            self.painter.text_in(cx, rect(r.pos.x + 12.0, y, text_width, h), small, &status, 0.0);
        }
        let cy = r.pos.y + r.size.y * 0.5;
        let send = rect(r.pos.x + r.size.x - 12.0 - 32.0, cy - 16.0, 32.0, 32.0);
        let first = rect(send.pos.x - 8.0 - 32.0, cy - 16.0, 32.0, 32.0);
        if state == "unacknowledged" {
            self.circle_button(cx, first, p.surface2, "x", p.foreground, true);
            self.st.hits.push(first, Item::QueueRemove(id.clone()));
            self.tip(first, t("common.actions.dismiss"));
        } else {
            self.circle_button(cx, first, p.surface2, "pencil", p.foreground, editable);
            if editable {
                self.st.hits.push(first, Item::QueueEdit(id.clone()));
            }
            self.tip(first, t("composer.attachments.editQueuedMessage"));
        }
        let can_send = !sending && state != "sending";
        self.circle_button(cx, send, p.accent, "arrow-up", p.on_tertiary, can_send);
        if can_send {
            self.st.hits.push(send, Item::QueueSend(id));
        }
        self.tip(send, t("composer.attachments.sendQueuedMessageNow"));
    }

    fn circle_button(&mut self, cx: &mut Cx2d, r: Rect, fill: Vec4, icon: &str, color: Vec4, enabled: bool) {
        let alpha = if enabled { 1.0 } else { 0.5 };
        self.painter.fill(cx, r, fade(fill, alpha), r.size.x * 0.5);
        self.painter.icon(cx, r, icon, 14.0, fade(color, alpha));
    }

    pub(super) fn fit_prefix(&mut self, cx: &mut Cx2d, spec: TextSpec, text: &str, width: f64) -> usize {
        let laid = self.painter.layout(cx, spec, text, Some(width));
        let end = laid
            .rows
            .first()
            .map(|row| row.text.len())
            .unwrap_or(text.len());
        let mut end = end.min(text.len()).max(1).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        end
    }

    /// Flutter `_deliveryPending`.
    fn draw_pending(&mut self, cx: &mut Cx2d, look: &Look, message: &Value, r: Rect) {
        let ctl = self.ctl_ref();
        let id = owned(get(message, "messageId"));
        let remote = !ctl.is_draft() && ctl.supports("agentMessageReceipts");
        let draft = ctl.draft();
        let receipt = get(get(&draft, "receipts"), &id).clone();
        let phase = s(get(&receipt, "phase"));
        let active = ctl.active_send().as_deref() == Some(id.as_str());
        let status = if !ctl.connected() {
            "offline"
        } else if !remote {
            "legacyUnknown"
        } else if phase == "dispatching" {
            "dispatching"
        } else if phase == "submitted" {
            "awaitingAck"
        } else if matches!(phase, "preparing" | "queued") || (ctl.sending() && active) {
            "preparing"
        } else {
            "unknown"
        };
        let checking = ctl.checking;
        let sending = ctl.sending();
        let draft_kind = ctl.is_draft();
        let can_retry = if remote { crate::features::composer::drafts::receipt_can_retry(&receipt) } else { !sending };
        let small = TextSpec::new(Font::Regular, look.small(), look.p.muted);
        let h = self.painter.line_height(cx, small);
        self.painter.paragraph(cx, r.pos, r.size.x, small, &t(&format!("composer.delivery.{status}")));
        let mut x = r.pos.x;
        let y = r.pos.y + h + 4.0;
        if remote {
            x = self.text_button(cx, look, dvec2(x, y), &t("composer.delivery.checkAgain"), !checking, Item::Check(id.clone()));
        }
        if can_retry {
            x = self.text_button(cx, look, dvec2(x, y), &t("composer.delivery.retrySame"), !sending && !checking, Item::Retry(id.clone()));
        }
        if !draft_kind {
            self.text_button(cx, look, dvec2(x, y), &t("composer.delivery.stopWaiting"), !checking, Item::StopWaiting(id));
        }
    }

    /// Flutter `_failedDelivery`.
    fn draw_failed(&mut self, cx: &mut Cx2d, look: &Look, message: &Value, r: Rect) {
        let ctl = self.ctl_ref();
        let draft = ctl.draft();
        let can_restore = ctl.editor_text.is_empty()
            && !non_empty_list(get(&draft, "attachments"))
            && !non_empty_list(get(&draft, "images"));
        let receipt = get(get(&draft, "receipts"), s(get(message, "messageId"))).clone();
        let sending = ctl.sending();
        let body = TextSpec::new(Font::Regular, look.body(), look.p.foreground);
        let text = t_args("composer.delivery.notSent", &[("preview", s(get(message, "text")))]);
        let h = self.painter.paragraph(cx, r.pos, r.size.x, body, &text).y;
        let y = r.pos.y + h + 4.0;
        let mut x = r.pos.x;
        x = self.text_button(cx, look, dvec2(x, y), &t("composer.delivery.restore"), can_restore && !sending, Item::Restore);
        if crate::features::composer::drafts::receipt_can_retry(&receipt) {
            x = self.text_button(cx, look, dvec2(x, y), &t("composer.delivery.retrySame"), !sending, Item::RetryFailed);
        }
        self.text_button(cx, look, dvec2(x, y), &t("composer.delivery.discard"), !sending, Item::Discard);
    }

    /// Material TextButton: primary label, 12px insets, hover fill.
    fn text_button(&mut self, cx: &mut Cx2d, look: &Look, at: DVec2, label: &str, enabled: bool, item: Item) -> f64 {
        let (x, y) = (at.x, at.y);
        let spec = TextSpec::new(Font::Medium, look.body(), if enabled { look.p.primary } else { fade(look.p.primary, 0.38) });
        let w = self.painter.measure(cx, spec, label).x + 24.0;
        let r = rect(x, y, w, 32.0);
        if enabled && self.st.hover.as_ref() == Some(&item) {
            self.painter.fill(cx, r, look.p.surface2, 16.0);
        }
        self.painter.text_in(cx, r, spec, label, 0.5);
        if enabled {
            self.st.hits.push(r, item);
        }
        x + w + 8.0
    }

    fn insets(look: &Look) -> (f64, f64, f64) {
        // (top, bottom, side)
        if look.narrow { (8.0, 8.0, 12.0) } else { (16.0, 12.0, 16.0) }
    }

    fn has_tray(&self, draft: &Value) -> bool {
        non_empty_list(get(draft, "images")) || non_empty_list(get(draft, "attachments")) || self.ctl_ref().uploading()
    }

    /// The tray's wrapped height at `width`.
    fn tray_height(&mut self, cx: &mut Cx2d, look: &Look, draft: &Value, width: f64) -> f64 {
        let widths = self.pill_widths(cx, look, draft);
        let mut x = 0.0;
        let mut rows = 1.0;
        for w in widths {
            let w = w + OVERHANG;
            if x > 0.0 && x + w > width {
                rows += 1.0;
                x = 0.0;
            }
            x += w;
        }
        rows * (48.0 + OVERHANG)
    }

    fn pill_widths(&mut self, cx: &mut Cx2d, look: &Look, draft: &Value) -> Vec<f64> {
        let mut out = Vec::new();
        out.extend(std::iter::repeat_n(48.0, maps(get(draft, "images")).len()));
        for attachment in maps(get(draft, "attachments")) {
            let (title, subtitle, _) = describe(&attachment);
            out.push(self.label_pill_width(cx, look, &title, subtitle.as_deref()));
        }
        for name in self.ctl_ref().uploading.clone() {
            out.push(self.label_pill_width(cx, look, &name, file_type_label(&name).as_deref()));
        }
        out
    }

    fn label_pill_width(&mut self, cx: &mut Cx2d, look: &Look, title: &str, subtitle: Option<&str>) -> f64 {
        let body = TextSpec::new(Font::Regular, look.body(), look.p.foreground);
        let small = TextSpec::new(Font::Regular, look.small(), look.p.muted);
        let tw = self.painter.measure(cx, body, title).x;
        let sw = self.painter.measure(cx, small, subtitle.unwrap_or("File")).x;
        (12.0 + 18.0 + 8.0 + tw.max(sw) + 12.0).min(260.0)
    }

    /// One editor line (the editor's own font and 1.4 line spacing).
    fn line(&mut self, cx: &mut Cx2d, look: &Look) -> f64 {
        self.caret_text.text_style.font_size = crate::theme::Typography::pt(look.content());
        let laid = self.caret_text.layout(cx, 0.0, 0.0, None, false, Align::default(), "Ag");
        laid.size_in_lpxs.height as f64 * self.caret_text.font_scale as f64
    }

    fn input_bounds(&mut self, cx: &mut Cx2d, look: &Look) -> (f64, f64, f64) {
        let line = self.line(cx, look);
        let vertical = if look.native { 6.0 } else { 4.0 };
        let min_lines = if look.compact { 1.0 } else { 2.0 };
        let mut min = line * min_lines + vertical * 2.0;
        if look.native {
            min = min.max(30.0);
        }
        let mut max = if look.compact { 160.0 } else { (look.window.y * 0.5).floor().max(160.0) };
        if let Some(cap) = self.st.input_cap {
            // The slot capacity shrinks the input, never below its minimum.
            max = max.min(cap).max(min);
        }
        (min, max, vertical)
    }

    /// capacity.ts resolveComposerSlotCapacity: the host's stationary viewport (keyboard
    /// coverage already removed by the shell) less 5, 60% of it on compact so conversation
    /// stays visible above; fullscreen takes the whole viewport.
    fn slot_capacity(&self, look: &Look) -> Option<f64> {
        let viewport = self.st.viewport_height.filter(|h| h.is_finite() && *h > 0.0)?;
        if matches!(self.st.overlay(), super::panel::Overlay::Fullscreen) {
            return Some(viewport);
        }
        let capacity = (viewport - 5.0).max(0.0);
        Some(if look.compact { capacity * 0.6 } else { capacity })
    }

    /// The card height and the text input's visible height within it.
    fn card_height(&mut self, cx: &mut Cx2d, look: &Look, draft: &Value, width: f64) -> (f64, f64) {
        let (top, bottom, side) = Self::insets(look);
        let mut h = top;
        if self.has_tray(draft) {
            h += self.tray_height(cx, look, draft, width - side * 2.0 + OVERHANG) - OVERHANG + 12.0;
            h -= OVERHANG;
        }
        let (min, max, vertical) = self.input_bounds(cx, look);
        let reserve = if look.compact { 36.0 } else { 0.0 };
        let natural = self.editor_natural(cx, look, width - side * 2.0 - reserve, vertical, min);
        let visible = natural.clamp(min, max);
        h += visible;
        h += 12.0 + 28.0 + bottom;
        (h, visible)
    }

    fn draw_card(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look, draft: &Value, r: Rect) {
        let p = look.p;
        let (top, bottom, side) = Self::insets(look);
        self.st.wrapper = r;
        // The card and its contents paint above the blocks drawn before it.
        self.layer(cx);
        if look.compact {
            let shadow = rect(r.pos.x, r.pos.y + 2.0, r.size.x, r.size.y);
            self.painter.fill(cx, shadow, crate::theme::argb(0x40000000), 16.0);
        }
        self.painter.outline(cx, r, p.surface1, p.border_accent, 1.0, 16.0);
        let mut y = r.pos.y + top;
        if self.has_tray(draft) {
            let tray_top = y - OVERHANG;
            let h = self.draw_tray(cx, look, draft, rect(r.pos.x + side - OVERHANG, tray_top, r.size.x - side * 2.0 + OVERHANG, 0.0));
            y = tray_top + h + 12.0;
        }
        // Text surface.
        let compact = look.compact;
        let reserve = if compact { 36.0 } else { 0.0 };
        let (min, max, vertical) = self.input_bounds(cx, look);
        let natural = self.editor_natural(cx, look, r.size.x - side * 2.0 - reserve, vertical, min);
        let visible = natural.clamp(min, max);
        let text_rect = rect(r.pos.x + side, y, r.size.x - side * 2.0, visible);
        let fullscreen = matches!(self.st.overlay(), super::panel::Overlay::Fullscreen);
        if !fullscreen {
            self.draw_input(cx, look, text_rect, reserve, vertical, min);
        }
        self.draw_focus_hint(cx, ui, look, text_rect);
        let line = self.line(cx, look);
        if compact && self.st.input_natural > line * 2.0 + vertical * 2.0 + 0.5 {
            // presentation.tsx Maximize2 on the send rail, after more than two lines.
            let glyph = look.glyph();
            let rail = (28.0 - glyph) / 2.0 + 5.0 * glyph / 24.0 - ((28.0 - 16.0) / 2.0 + 3.0 * 16.0 / 24.0);
            let b = rect(text_rect.pos.x + text_rect.size.x - 28.0 + 6.0 - rail, text_rect.pos.y, 28.0, 28.0);
            self.painter.icon(cx, b, "maximize-2", 16.0, p.muted);
            self.st.hits.push(b, Item::Expand);
            self.tip(b, t("composer.input.fullscreen"));
        }
        y += visible + 12.0;
        self.draw_toolbar(cx, ui, look, rect(r.pos.x + side - 6.0, y, r.size.x - (side - 6.0) * 2.0, 28.0));
        let _ = bottom;
    }

    /// The editor inside a clipped, scrollable viewport (TextInput has no own scrolling).
    pub(super) fn draw_input(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, reserve: f64, vertical: f64, min: f64) {
        let viewport = rect(r.pos.x, r.pos.y, (r.size.x - reserve).max(10.0), r.size.y);
        self.st.input_clip = viewport;
        let natural = self.editor_natural(cx, look, viewport.size.x, vertical, min);
        let max_scroll = (natural - viewport.size.y).max(0.0);
        let text = self.input.text();
        if self.st.input_scroll == f64::MAX {
            self.st.input_scroll = max_scroll;
        } else if max_scroll > 0.0 && !self.st.input_manual_scroll {
            // Keep the caret row visible: lay the text out exactly as the editor does (same
            // font, line spacing, width and wrapping) and take the caret row's extent, the
            // rect TextInput draws its cursor in.
            self.caret_text.text_style.font_size = crate::theme::Typography::pt(look.content());
            let laid = self.caret_text.layout(cx, 0.0, 0.0, Some(viewport.size.x as f32), true, Align::default(), &text);
            let cursor = self.input.cursor();
            let position = laid.cursor_to_position(cursor);
            if let Some(row) = laid.rows.get(position.row_index) {
                let scale = self.caret_text.font_scale as f64;
                let caret_top = (row.origin_in_lpxs.y - row.ascender_in_lpxs) as f64 * scale + vertical;
                let caret_bottom = caret_top + (row.ascender_in_lpxs - row.descender_in_lpxs) as f64 * scale;
                if caret_bottom > self.st.input_scroll + viewport.size.y {
                    self.st.input_scroll = caret_bottom - viewport.size.y + vertical;
                } else if caret_top < self.st.input_scroll {
                    self.st.input_scroll = (caret_top - vertical).max(0.0);
                }
            }
        }
        self.st.input_scroll = self.st.input_scroll.clamp(0.0, max_scroll);
        cx.begin_turtle(
            Walk::abs_rect(viewport),
            Layout::flow_down()
                .with_clip(true, true)
                .with_scroll(dvec2(0.0, self.st.input_scroll))
                .with_padding_top(vertical)
                .with_padding_bottom(vertical),
        );
        let walk = Walk { abs_pos: None, margin: Margin::default(), width: Size::Fill, height: Size::Fit };
        let _ = self.input.draw_walk(cx, &mut Scope::empty(), walk);
        cx.end_turtle();
    }

    /// The editor's content height at `width` before it is drawn (same font, spacing and
    /// wrapping as the TextInput), so the card grows in the same frame as the text.
    fn editor_natural(&mut self, cx: &mut Cx2d, look: &Look, width: f64, vertical: f64, min: f64) -> f64 {
        let mut text = self.input.text();
        if text.is_empty() {
            // The hint wraps like text (input.tsx placeholder).
            text = self.input.empty_text().to_owned();
        }
        let line = self.line(cx, look);
        let height = if text.is_empty() {
            line
        } else {
            self.caret_text.text_style.font_size = crate::theme::Typography::pt(look.content());
            let laid = self.caret_text.layout(cx, 0.0, 0.0, Some(width.max(1.0) as f32), true, Align::default(), &text);
            laid.size_in_lpxs.height as f64 * self.caret_text.font_scale as f64
        };
        let natural = (height + vertical * 2.0).max(min);
        self.st.input_natural = natural;
        natural
    }

    /// input.tsx FocusHint: "⌘L to focus" while the empty wide desktop input is unfocused.
    fn draw_focus_hint(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look, r: Rect) {
        if look.compact || look.native || self.st.input_focused || !self.ctl_ref().editor_text.is_empty() {
            return;
        }
        let Some(combo) = crate::common::shortcut_label(ui, "focus-message-input") else { return };
        let first = combo.split(' ').next().unwrap_or("").to_owned();
        let text = t_args("composer.input.focusHint", &[("shortcut", first.as_str())]);
        let spec = TextSpec::new(Font::Regular, look.small(), fade(look.p.muted, 0.5));
        let w = self.painter.measure(cx, spec, &text).x;
        self.painter.text(cx, dvec2(r.pos.x + r.size.x - w, r.pos.y), spec, &text);
    }

    /// index.tsx attachmentTray: removable pills carry their 8px disc overhang.
    fn draw_tray(&mut self, cx: &mut Cx2d, look: &Look, draft: &Value, r: Rect) -> f64 {
        let p = look.p;
        let sending = self.ctl_ref().sending();
        let show_remove_always = look.native || look.compact;
        let widths = self.pill_widths(cx, look, draft);
        let images = maps(get(draft, "images"));
        let attachments = maps(get(draft, "attachments"));
        let uploading = self.ctl_ref().uploading.clone();
        let mut x = r.pos.x;
        let mut y = r.pos.y;
        let mut index = 0usize;
        let total = widths.len();
        // Remove discs overhang the frames (and thumbnails): paint them afterwards, above.
        let mut discs: Vec<Rect> = Vec::new();
        while index < total {
            let w = widths[index] + OVERHANG;
            if x > r.pos.x && x + w > r.pos.x + r.size.x {
                x = r.pos.x;
                y += 48.0 + OVERHANG;
            }
            let frame = rect(x + OVERHANG, y + OVERHANG, widths[index], 48.0);
            let disc = rect(x, y, 24.0, 24.0);
            let (open, remove, removable) = if index < images.len() {
                (Item::Image(index), Item::ImageRemove(index), true)
            } else if index < images.len() + attachments.len() {
                let i = index - images.len();
                (Item::Attachment(i), Item::AttachmentRemove(i), true)
            } else {
                (Item::Send, Item::Send, false)
            };
            if index < images.len() {
                let image = images[index].clone();
                self.painter.outline(cx, frame, p.surface1, p.border_accent, 1.0, 6.0);
                match service::with(|svc| svc.blobs.texture(&image, true)) {
                    Ok(Some((texture, size))) => {
                        let inner = rect(frame.pos.x + 1.0, frame.pos.y + 1.0, frame.size.x - 2.0, frame.size.y - 2.0);
                        self.draw_image.paint(cx, inner, &texture, size, 5.0, false);
                    }
                    Ok(None) => self.spinner(cx, rect(frame.pos.x + 15.0, frame.pos.y + 15.0, 18.0, 18.0), p.muted),
                    Err(_) => self.painter.icon(cx, frame, "image", 16.0, p.muted),
                }
                self.tip(frame, t("composer.attachments.openImage"));
            } else if index < images.len() + attachments.len() {
                let attachment = attachments[index - images.len()].clone();
                let (title, subtitle, glyph) = describe(&attachment);
                let icon = match glyph {
                    AttachmentGlyph::FileText => "file-text",
                    AttachmentGlyph::CircleDot => "circle-dot",
                    AttachmentGlyph::GitPullRequest => "git-pull-request",
                };
                self.label_pill(cx, look, frame, Some(icon), &title, &subtitle.unwrap_or_else(|| t("message.attachments.file")));
            } else {
                let name = uploading[index - images.len() - attachments.len()].clone();
                self.painter.outline(cx, frame, p.surface1, p.border_accent, 1.0, 6.0);
                self.spinner(cx, rect(frame.pos.x + 12.0, frame.pos.y + 15.0, 18.0, 18.0), p.muted);
                self.label_pill(cx, look, frame, None, &name, &file_type_label(&name).unwrap_or_default());
            }
            if removable {
                if !sending {
                    self.st.hits.push(frame, open.clone());
                }
                let hovered = self.st.hover.as_ref().is_some_and(|h| *h == open || *h == remove);
                if show_remove_always || hovered {
                    discs.push(disc);
                }
                if !sending {
                    self.st.hits.push(disc, remove.clone());
                }
                let label = match remove {
                    Item::ImageRemove(_) => t("composer.attachments.removeImage"),
                    _ => t("composer.attachments.removeFile"),
                };
                self.tip(disc, label);
            }
            x += w;
            index += 1;
        }
        if !discs.is_empty() {
            self.layer(cx);
            for disc in discs {
                self.painter.outline(cx, disc, p.surface2, p.border, 1.0, 12.0);
                self.painter.icon(cx, disc, "x", 12.0, p.muted);
            }
            self.layer(cx);
        }
        y + 48.0 + OVERHANG - r.pos.y
    }

    /// AttachmentLabel: 18px glyph slot, title over the small muted subtitle.
    fn label_pill(&mut self, cx: &mut Cx2d, look: &Look, frame: Rect, icon: Option<&str>, title: &str, subtitle: &str) {
        let p = look.p;
        if icon.is_some() {
            self.painter.outline(cx, frame, p.surface1, p.border_accent, 1.0, 6.0);
            self.painter.icon(cx, rect(frame.pos.x + 12.0, frame.pos.y + 15.0, 18.0, 18.0), icon.unwrap_or("file"), 14.0, p.muted);
        }
        let body = TextSpec::new(Font::Regular, look.body(), p.foreground);
        let small = TextSpec::new(Font::Regular, look.small(), p.muted);
        let tx = frame.pos.x + 12.0 + 18.0 + 8.0;
        let tw = frame.size.x - (tx - frame.pos.x) - 12.0;
        let bh = self.painter.line_height(cx, body);
        let sh = self.painter.line_height(cx, small);
        let top = frame.pos.y + (48.0 - bh - sh) * 0.5;
        self.painter.text_in(cx, rect(tx, top, tw, bh), body, title, 0.0);
        self.painter.text_in(cx, rect(tx, top + bh, tw, sh), small, subtitle, 0.0);
    }

    /// input.tsx buttonRow: attach, agent controls; context meter and the primary action.
    fn draw_toolbar(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look, r: Rect) {
        let p = look.p;
        let glyph = look.glyph();
        // Attach.
        let attach = rect(r.pos.x, r.pos.y, 28.0, 28.0);
        self.st.attach_rect = attach;
        let connected = self.ctl_ref().connected();
        let hovered = self.st.hover == Some(Item::Attach);
        if hovered && connected {
            self.painter.fill(cx, attach, p.surface2, 14.0);
        }
        let color = if hovered { p.foreground } else { p.muted };
        self.painter.icon(cx, attach, "plus", glyph, fade(color, if connected { 1.0 } else { 0.5 }));
        if connected {
            self.st.hits.push(attach, Item::Attach);
        }
        self.tip(attach, t("composer.input.addAttachment"));

        // Right side: context meter + send control.
        let mut right = r.pos.x + r.size.x;
        let send_width = self.draw_send_control(cx, ui, look, right, r.pos.y);
        right -= send_width;
        let has_agent = !self.ctl_ref().is_draft() && !get(&self.ctl_ref().agent.agent, "status").is_null();
        if has_agent {
            let meter = rect(right - 4.0 - 28.0, r.pos.y, 28.0, 28.0);
            self.draw_meter(cx, look, meter);
            right = meter.pos.x;
        } else {
            self.st.meter_rect = Rect::default();
        }
        let controls = rect(attach.pos.x + 28.0, r.pos.y, (right - attach.pos.x - 28.0 - 4.0).max(0.0), 28.0);
        self.draw_agent_controls(cx, look, controls);
    }

    /// context-window-meter.tsx ring.
    fn draw_meter(&mut self, cx: &mut Cx2d, look: &Look, r: Rect) {
        let p = look.p;
        self.st.meter_rect = r;
        let usage = get(&self.ctl_ref().agent.agent, "lastUsage").clone();
        let percentage = meter_percentage(&usage);
        let value = percentage.unwrap_or(0.0);
        let progress = if value > 90.0 { p.destructive } else if value >= 70.0 { look.amber500() } else { p.muted };
        let glyph = if look.compact { 16.0 } else { look.glyph() };
        let ring = rect(r.pos.x + (28.0 - glyph) * 0.5, r.pos.y + (28.0 - glyph) * 0.5, glyph, glyph);
        self.painter.ring(cx, ring, p.surface3, 2.0, 1.0, 0.0);
        if let Some(percentage) = percentage {
            let fraction = (percentage / 100.0).clamp(0.0, 1.0);
            if fraction > 0.0 {
                self.painter.ring(cx, ring, progress, 2.0, fraction.min(0.9999), -0.25);
            }
        }
        self.st.hits.push(r, Item::Meter);
    }

    /// input.tsx PrimaryAction. Returns the width used (with its 4px leading gap).
    fn draw_send_control(&mut self, cx: &mut Cx2d, ui: &mut UiContext, look: &Look, right: f64, y: f64) -> f64 {
        let p = look.p;
        let glyph = look.glyph();
        let (running, has_message, loading, connected, cancelling, behavior, blocked) = {
            let ctl = self.ctl_ref();
            let draft = ctl.draft();
            let blocked = !ctl.initialized
                || ctl.execution_blocked()
                || non_empty_obj(get(&draft, "pending"))
                || non_empty_obj(get(&draft, "failedMessage"));
            (
                ctl.running(),
                ctl.has_message(),
                ctl.sending() || ctl.uploading(),
                ctl.connected(),
                ctl.cancelling,
                ctl.behavior(ui),
                blocked,
            )
        };
        let r = rect(right - 28.0, y, 28.0, 28.0);
        if !has_message && running {
            let enabled = connected && !cancelling;
            self.painter.fill(cx, r, fade(look.red600(), if enabled { 1.0 } else { 0.5 }), 14.0);
            if cancelling {
                self.spinner(cx, rect(r.pos.x + 7.0, r.pos.y + 7.0, 14.0, 14.0), look.white());
            } else {
                let square = glyph * 0.6;
                let inner = rect(r.pos.x + (28.0 - square) * 0.5, r.pos.y + (28.0 - square) * 0.5, square, square);
                self.painter.fill(cx, inner, look.white(), 2.0);
            }
            if enabled {
                self.st.hits.push(r, Item::Stop);
            }
            let tip = with_combo(t("composer.cancel.interrupt"), self.st.interrupt_shortcut.clone());
            self.tip(r, tip);
            return 32.0;
        }
        if !has_message && !loading {
            return 0.0;
        }
        let can_press_loading = loading && running;
        let queues = behavior == "queue" && running;
        let disabled = !can_press_loading && (loading || !connected || blocked);
        self.painter.fill(cx, r, fade(p.accent, if disabled { 0.5 } else { 1.0 }), 14.0);
        if loading {
            self.spinner(cx, rect(r.pos.x + 7.0, r.pos.y + 7.0, 14.0, 14.0), p.on_tertiary);
        } else {
            self.painter.icon(cx, r, "arrow-up", glyph, fade(p.on_tertiary, if disabled { 0.5 } else { 1.0 }));
        }
        if !disabled {
            self.st.hits.push(r, Item::Send);
        }
        let label = if can_press_loading {
            t("composer.input.interruptAgent")
        } else if queues {
            t("composer.input.queue")
        } else {
            t("composer.input.send")
        };
        // input.tsx DEFAULT_SEND_KEYS: the send tooltip shows Enter.
        self.tip(r, with_combo(label, Some("Enter".into())));
        32.0
    }

    /// agent-controls/index.tsx: model trigger, thinking, mode and features.
    fn draw_agent_controls(&mut self, cx: &mut Cx2d, look: &Look, r: Rect) {
        let p = look.p;
        let disabled = self.controls_disabled();
        let alpha = if disabled { 0.5 } else { 1.0 };
        let glyph = look.glyph();
        let config = self.ctl_ref().config();
        let (providers, modes, features) = {
            let ctl = self.ctl_ref();
            (ctl.providers.clone(), ctl.modes(), ctl.features())
        };
        let model = ControlsModel { providers: &providers, config: &config, modes: &modes, features: &features };
        let (select_model, loading, error, unknown, extra) = (
            t("modelSelector.selectModel"),
            t("modelSelector.loading"),
            t("modelSelector.error"),
            t("agentControls.thinking.unknown"),
            t("agentControls.thinking.extraHigh"),
        );
        let labels = ModelLabels { select_model: &select_model, loading: &loading, error: &error, thinking_unknown: &unknown, extra_high: &extra };
        let body = TextSpec::new(Font::Regular, look.body(), fade(p.muted, alpha));
        let thinking = model.thinking_options();
        let provider = owned(get(&config, "provider"));
        let provider_svg = owned(get(&model.provider(), "iconSvg"));
        let model_text = short_model_label(&model.selected_model_label(&labels));
        let thinking_text = model.thinking_label(&labels);
        let mode_text = format_agent_mode_label(model.selected_mode().unwrap_or(&Value::Null));
        let presentation_density = if look.compact {
            Density::Tight
        } else {
            let font_scale = look.t.ui_base_font_size / 14.0;
            resolve_density(r.size.x, self.st.density, true, !thinking.is_empty(), !modes.is_empty(), &features, font_scale)
        };
        self.st.density = presentation_density;
        let presentation = presentation(presentation_density);
        let mut x = r.pos.x;
        let max_x = r.pos.x + r.size.x;
        let caret = presentation.show_carets && !look.compact;
        // Model trigger.
        let icon_w = if provider.trim().is_empty() { 0.0 } else { glyph + 4.0 };
        let thinking_suffix = look.compact && !thinking.is_empty();
        let suffix_w = if thinking_suffix { self.painter.measure(cx, body, &thinking_text).x + 4.0 } else { 0.0 };
        let caret_w = if caret { 18.0 } else { 0.0 };
        let text_w = self.painter.measure(cx, body, &model_text).x;
        let mut reserved_right = 0.0;
        if !look.compact {
            if !thinking.is_empty() {
                reserved_right += 4.0 + self.trigger_width(cx, body, presentation.show_thinking_label.then_some(thinking_text.as_str()), glyph, caret);
            }
            if !modes.is_empty() {
                reserved_right += 4.0 + self.trigger_width(cx, body, presentation.show_mode_label.then_some(mode_text.as_str()), glyph, caret);
            }
            if !features.is_empty() {
                reserved_right += if presentation.aggregate_features {
                    4.0 + 28.0
                } else {
                    features.iter().map(|f| 4.0 + self.feature_width(cx, body, &model, f, glyph)).sum::<f64>()
                };
            }
        }
        let model_w = (8.0 + icon_w + text_w + suffix_w + caret_w + 8.0).min((max_x - x - reserved_right).max(40.0));
        let trigger = rect(x, r.pos.y, model_w, 28.0);
        self.trigger_bg(cx, look, trigger, Item::Model, disabled);
        let mut tx = trigger.pos.x + 8.0;
        if icon_w > 0.0 {
            let icon = rect(tx, trigger.pos.y + (28.0 - glyph) * 0.5, glyph, glyph);
            crate::common::provider_icon::ProviderIcon::paint(
                &mut self.painter,
                &mut self.draw_icon,
                cx,
                icon,
                &provider,
                (!provider_svg.is_empty()).then_some(provider_svg.as_str()),
                fade(p.muted, alpha),
            );
            tx += icon_w;
        }
        let avail = (trigger.pos.x + model_w - 8.0 - caret_w - suffix_w - tx).max(0.0);
        let drawn = self.painter.text_in(cx, rect(tx, trigger.pos.y, avail, 28.0), body, &model_text, 0.0);
        tx = drawn.pos.x + drawn.size.x + 4.0;
        if thinking_suffix {
            let extra_spec = TextSpec::new(Font::Regular, look.body(), fade(p.extra_muted, alpha));
            self.painter.text_in(cx, rect(tx, trigger.pos.y, suffix_w, 28.0), extra_spec, &thinking_text, 0.0);
        }
        if caret {
            self.painter.icon(cx, rect(trigger.pos.x + model_w - 8.0 - 14.0, trigger.pos.y + 7.0, 14.0, 14.0), "chevron-down", 14.0, fade(p.muted, alpha));
        }
        let model_label = model.selected_model_label(&labels);
        self.tip(trigger, t_args("modelSelector.selectedModel", &[("model", model_label.as_str())]));
        x += model_w + 4.0;
        if look.compact {
            return;
        }
        if !thinking.is_empty() {
            let label = presentation.show_thinking_label.then_some(thinking_text.as_str());
            let w = self.trigger_width(cx, body, label, glyph, caret);
            let tr = rect(x, r.pos.y, w, 28.0);
            self.icon_trigger(cx, look, tr, Trigger { icon: "brain", tint: None, label, caret }, Item::Thinking, disabled);
            self.tip(tr, t("agentControls.hints.thinking"));
            x += w + 4.0;
        }
        if !modes.is_empty() {
            let label = presentation.show_mode_label.then_some(mode_text.as_str());
            let w = self.trigger_width(cx, body, label, glyph, caret);
            let mr = rect(x, r.pos.y, w, 28.0);
            self.icon_trigger(cx, look, mr, Trigger { icon: mode_icon(model.selected_mode()), tint: None, label, caret }, Item::Mode, disabled);
            let tip = with_combo(t("agentControls.hints.mode"), self.st.mode_shortcut.clone());
            self.tip(mr, tip);
            x += w + 4.0;
        }
        if features.is_empty() {
            return;
        }
        if presentation.aggregate_features {
            let fr = rect(x, r.pos.y, 28.0, 28.0);
            self.icon_trigger(cx, look, fr, Trigger { icon: "settings-2", tint: None, label: None, caret: false }, Item::Features, disabled);
            self.tip(fr, t("agentControls.features.open"));
            return;
        }
        for (index, feature) in features.iter().enumerate() {
            let w = self.feature_width(cx, body, &model, feature, glyph);
            if x + w > max_x + 0.5 {
                break;
            }
            let value = model.feature_value(feature);
            let tint = feature_icon_tint(feature, &value).map(|c| vec4(c[0], c[1], c[2], c[3]));
            let label = s_or(get(feature, "label"), s(get(feature, "id"))).to_owned();
            let fr = rect(x, r.pos.y, w, 28.0);
            if s(get(feature, "type")) == "toggle" {
                self.icon_trigger(cx, look, fr, Trigger { icon: feature_icon(feature), tint, label: None, caret: false }, Item::Feature(index), disabled);
                self.tip(fr, s_or(get(feature, "tooltip"), &label).to_owned());
            } else {
                let options = maps(get(feature, "options"));
                let selected = options.iter().find(|o| get(o, "id") == &value).map(|o| s_or(get(o, "label"), &label).to_owned()).unwrap_or(label.clone());
                let icon_only = s(get(feature, "desktopTrigger")) == "icon";
                let shown = (!icon_only).then_some(selected.as_str());
                self.icon_trigger(cx, look, fr, Trigger { icon: feature_icon(feature), tint, label: shown, caret: false }, Item::Feature(index), disabled);
                let hint = if icon_only { format!("{label}: {selected}") } else { s_or(get(feature, "tooltip"), &label).to_owned() };
                self.tip(fr, hint);
            }
            x += w + 4.0;
        }
    }

    fn trigger_width(&mut self, cx: &mut Cx2d, body: TextSpec, label: Option<&str>, glyph: f64, caret: bool) -> f64 {
        match label {
            None => 28.0,
            Some(label) => 8.0 + glyph + 4.0 + self.painter.measure(cx, body, label).x + if caret { 18.0 } else { 0.0 } + 8.0,
        }
    }

    fn feature_width(&mut self, cx: &mut Cx2d, body: TextSpec, model: &ControlsModel, feature: &Value, glyph: f64) -> f64 {
        if s(get(feature, "type")) == "toggle" || s(get(feature, "desktopTrigger")) == "icon" {
            return 28.0;
        }
        let value = model.feature_value(feature);
        let label = maps(get(feature, "options"))
            .iter()
            .find(|o| get(o, "id") == &value)
            .map(|o| owned(get(o, "label")))
            .unwrap_or_else(|| owned(get(feature, "label")));
        self.trigger_width(cx, body, Some(&label), glyph, false)
    }

    fn trigger_bg(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, item: Item, disabled: bool) {
        if !disabled {
            if self.st.hover.as_ref() == Some(&item) {
                self.painter.fill(cx, r, look.p.surface2, 14.0);
            }
            self.st.hits.push(r, item);
        }
    }

    fn icon_trigger(&mut self, cx: &mut Cx2d, look: &Look, r: Rect, trigger: Trigger, item: Item, disabled: bool) {
        let Trigger { icon, tint, label, caret } = trigger;
        let alpha = if disabled { 0.5 } else { 1.0 };
        let glyph = look.glyph();
        self.trigger_bg(cx, look, r, item, disabled);
        let color = fade(tint.unwrap_or(look.p.muted), alpha);
        match label {
            None => self.painter.icon(cx, r, icon, glyph, color),
            Some(label) => {
                let icon_rect = rect(r.pos.x + 8.0, r.pos.y + (28.0 - glyph) * 0.5, glyph, glyph);
                self.painter.icon(cx, icon_rect, icon, glyph, color);
                let body = TextSpec::new(Font::Regular, look.body(), fade(look.p.muted, alpha));
                let end = r.pos.x + r.size.x - 8.0 - if caret { 18.0 } else { 0.0 };
                let tx = icon_rect.pos.x + glyph + 4.0;
                self.painter.text_in(cx, rect(tx, r.pos.y, (end - tx).max(0.0), 28.0), body, label, 0.0);
                if caret {
                    self.painter.icon(cx, rect(end + 4.0, r.pos.y + 7.0, 14.0, 14.0), "chevron-down", 14.0, fade(look.p.muted, alpha));
                }
            }
        }
    }
}

/// ComposerTooltip: the label followed by its shortcut combo.
fn with_combo(label: String, combo: Option<String>) -> String {
    match combo.filter(|c| !c.is_empty()) {
        Some(combo) => format!("{label}  {combo}"),
        None => label,
    }
}

/// context-window-meter.tsx percentage.
pub fn meter_percentage(usage: &Value) -> Option<f64> {
    let used = get(usage, "contextWindowUsedTokens").as_f64()?;
    let max = get(usage, "contextWindowMaxTokens").as_f64()?;
    if !max.is_finite() || max <= 0.0 || !used.is_finite() || used < 0.0 {
        return None;
    }
    Some(used / max * 100.0)
}

/// context-window-meter.utils.ts formatTokenCount.
pub fn format_token_count(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{}m", (value / 1_000_000.0).round())
    } else if value >= 1000.0 {
        format!("{}k", (value / 1000.0).round())
    } else {
        format!("{}", value.round())
    }
}

/// context-window-meter.tsx formatSessionCost.
pub fn format_session_cost(value: &Value) -> Option<String> {
    let value = value.as_f64()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(if value < 0.01 { format!("${value:.4}") } else { format!("${value:.2}") })
}

pub fn spec_body(look: &Look) -> TextSpec {
    TextSpec::new(Font::Regular, look.body(), look.p.foreground)
}

pub fn spec_small(look: &Look) -> TextSpec {
    TextSpec::new(Font::Regular, look.small(), look.p.muted)
}

/// A toolbar trigger's glyph, optional tint, optional label and dropdown caret.
struct Trigger<'a> {
    icon: &'a str,
    tint: Option<Vec4>,
    label: Option<&'a str>,
    caret: bool,
}
