//! One virtualized stream row (PortalList item): paints its `RowItem` with
//! the immediate-mode painters into its own draw list (backgrounds under
//! text), keeps its selectable text and hit regions, and handles hover,
//! selection and copy, inline toggles, code/detail scrolling and the question
//! answer field. Everything that needs the panel (links, sheets, menus,
//! permission responses) is posted as a [`ChatRowEvent`].

use super::canvas::*;
use super::paint::{Draw, RowState, now_seconds};
use super::permission;
use super::rows::{RowContext, RowItem, RowKind, draw_row};
use crate::common::widgets::PTextFieldWidgetRefExt;
use crate::common::{UiContext, UiEffect};
use makepad_widgets::*;
use makepad_widgets::makepad_draw::event::{DigitDevice, TouchState};
use makepad_widgets::scroll_bars::TouchScroll;
use std::cell::RefCell;
use std::rc::Rc;

live_design! {
    use link::theme::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::features::timeline::canvas::*;

    pub ChatRow = {{ChatRow}} {
        width: Fill, height: Fit,
        kit: <ChatKit> {}
        answer: <PTextField> { width: Fill, height: Fit }
    }
}

/// What a row asks of its panel.
#[derive(Clone, Debug)]
pub struct ChatRowEvent {
    pub panel: u64,
    pub row: String,
    pub kind: RowEventKind,
}

#[derive(Clone, Debug)]
pub enum RowEventKind {
    /// A tap on a hit target; `rect` is its absolute rect (menu anchors).
    Tap { target: HitTarget, rect: Rect },
    /// The question card's primary button on its last question / dismiss.
    Answer { key: String, dismiss: bool },
}

/// The model a row paints, shared with the panel.
#[derive(Clone)]
pub struct RowModel {
    pub panel: u64,
    pub item: RowItem,
    pub ctx: Rc<RowContext>,
    pub state: Rc<RefCell<RowState>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Select,
    /// Touch drag of a horizontal scroll region.
    ScrollX,
}

#[derive(Live, LiveHook, Widget)]
pub struct ChatRow {
    #[redraw]
    #[rust]
    area: Area,
    #[walk]
    walk: Walk,
    #[live]
    kit: ChatKit,
    #[live]
    answer: WidgetRef,
    #[rust(DrawList2d::new(cx))]
    draw_list: DrawList2d,
    #[rust]
    canvas: Canvas,
    #[rust]
    model: Option<RowModel>,
    #[rust]
    answer_field: Option<permission::AnswerField>,
    #[rust]
    pressed: Option<(HitTarget, Rect)>,
    #[rust(Drag::None)]
    drag: Drag,
    #[rust]
    drag_key: String,
    #[rust]
    touch_scroll: TouchScroll,
    #[rust]
    next_frame: NextFrame,
    #[rust]
    last_width: f64,
    #[rust]
    show_actions: bool,
    #[rust]
    selection_source: Option<u64>,
}

/// The key painters compare `RowState.hover_hit` against.
pub fn hover_key(target: &HitTarget) -> String {
    match target {
        HitTarget::Copy { key, .. } => key.clone(),
        HitTarget::Toggle(key) if key.starts_with("stamp:") => key.clone(),
        HitTarget::RewindMenu => "rewind".into(),
        HitTarget::Chrome(id) => id.clone(),
        other => permission::hit_key(other),
    }
}

impl ChatRow {
    pub fn set_model(&mut self, cx: &mut Cx, model: RowModel) {
        if self.model.as_ref().is_none_or(|old| old.panel != model.panel || old.item.id != model.item.id) {
            self.clear_selection(cx);
            model.state.borrow_mut().selection = None;
        }
        self.model = Some(model);
    }

    fn pan_horizontal(&mut self, cx: &mut Cx, delta: f64) {
        let Some(model) = &self.model else { return };
        let mut state = model.state.borrow_mut();
        let max = state.extent_x.get(&self.drag_key).copied().unwrap_or(0.0);
        let mut position = state.scroll_x.get(&self.drag_key).copied().unwrap_or(0.0);
        if self.touch_scroll.apply_delta(&mut position, delta, max) {
            if let Some(offset) = state.scroll_x.get_mut(&self.drag_key) {
                *offset = position;
            } else {
                state.scroll_x.insert(self.drag_key.clone(), position);
            }
            self.area.redraw(cx);
        }
    }

    fn rel(&self, cx: &Cx, abs: DVec2) -> DVec2 {
        abs - self.area.rect(cx).pos
    }

    fn post(&self, cx: &mut Cx, kind: RowEventKind) {
        if let Some(model) = &self.model {
            cx.action(ChatRowEvent { panel: model.panel, row: model.item.id.clone(), kind });
        }
    }

    /// Wheel over an inner scroll region that can still move in that
    /// direction: scrolls it and reports true (the panel then keeps the
    /// event from the list).
    pub fn scroll_inner(&mut self, cx: &mut Cx, abs: DVec2, delta: DVec2) -> bool {
        let Some(model) = &self.model else { return false };
        let rect = self.area.rect(cx);
        if !rect.contains(abs) {
            return false;
        }
        self.touch_scroll.reset();
        let rel = abs - rect.pos;
        let mut state = model.state.borrow_mut();
        let horizontal = delta.x.abs() > delta.y.abs();
        if horizontal {
            if let Some(key) = self.canvas.scroll_region_at(rel, false) {
                let max = state.extent_x.get(key).copied().unwrap_or(0.0);
                let current = state.scroll_x.get(key).copied().unwrap_or(0.0);
                let next = (current + delta.x).clamp(0.0, max);
                if (next - current).abs() > 0.01 {
                    state.scroll_x.insert(key.to_owned(), next);
                    drop(state);
                    self.area.redraw(cx);
                    return true;
                }
            }
            return false;
        }
        if let Some(key) = self.canvas.scroll_region_at(rel, true) {
            let max = state.extent_y.get(key).copied().unwrap_or(0.0);
            let current = state.scroll_y.get(key).copied().unwrap_or(0.0);
            let next = (current + delta.y).clamp(0.0, max);
            if (next - current).abs() > 0.01 {
                state.scroll_y.insert(key.to_owned(), next);
                drop(state);
                self.area.redraw(cx);
                return true;
            }
        }
        false
    }

    /// Row-local taps: expansion toggles, copy feedback, question form
    /// navigation. Returns false when the panel must handle the tap.
    fn tap_local(&mut self, cx: &mut Cx, scope: &mut Scope, target: &HitTarget) -> bool {
        let Some(model) = self.model.clone() else { return true };
        let now = now_seconds();
        match target {
            HitTarget::Toggle(key) => {
                let mut state = model.state.borrow_mut();
                if key.starts_with("stamp:") {
                    // The elapsed label shows its end time while revealed.
                    state.copied.insert(key.clone(), now);
                } else {
                    state.toggle(key);
                }
                true
            }
            HitTarget::Copy { key, text } => {
                cx.copy_to_clipboard(text);
                model.state.borrow_mut().copied.insert(key.clone(), now);
                if let Some(ui) = scope.data.get_mut::<UiContext>() {
                    ui.push(UiEffect::Redraw);
                }
                true
            }
            HitTarget::QuestionTab { index, .. } => {
                if let Some(card) = model.state.borrow_mut().permission.as_mut() {
                    card.active_question = *index;
                }
                true
            }
            HitTarget::QuestionOption { index, .. } => {
                let RowKind::Permission { request, .. } = &model.item.kind else { return true };
                let Some(qs) = permission::questions(request) else { return true };
                let mut state = model.state.borrow_mut();
                if let Some(card) = state.permission.as_mut() {
                    let active = card.active_question.min(qs.len().saturating_sub(1));
                    if let Some(q) = qs.get(active)
                        && card.toggle(q, active, *index, qs.len())
                    {
                        // The custom answer was replaced by the option.
                        self.answer.set_text(cx, "");
                    }
                }
                true
            }
            HitTarget::QuestionPrimary { key } => {
                let RowKind::Permission { request, .. } = &model.item.kind else { return true };
                let Some(qs) = permission::questions(request) else { return true };
                let mut state = model.state.borrow_mut();
                let Some(card) = state.permission.as_mut() else { return true };
                if card.pending {
                    return true;
                }
                let index = card.active_question.min(qs.len().saturating_sub(1));
                let last = index + 1 >= qs.len();
                if !last {
                    if card.is_answered(&qs[index], index) {
                        card.active_question = index + 1;
                    }
                    return true;
                }
                if !qs.iter().enumerate().all(|(i, q)| card.is_answered(q, i)) {
                    return true;
                }
                drop(state);
                self.post(cx, RowEventKind::Answer { key: key.clone(), dismiss: false });
                true
            }
            HitTarget::QuestionDismiss { key } => {
                if model.state.borrow().permission.as_ref().is_some_and(|c| c.pending) {
                    return true;
                }
                self.post(cx, RowEventKind::Answer { key: key.clone(), dismiss: true });
                true
            }
            HitTarget::ScrollX(_) | HitTarget::ScrollY(_) => true,
            _ => false,
        }
    }

    fn set_hover(&mut self, cx: &mut Cx, rel: Option<DVec2>) {
        let Some(model) = &self.model else { return };
        let (hit, link) = match rel {
            Some(rel) => match self.canvas.hit_at(rel) {
                Some(HitTarget::Link { href, .. }) => (None, Some(href.clone())),
                Some(target) => (Some(hover_key(target)), None),
                None => (None, None),
            },
            None => (None, None),
        };
        let hovered = rel.is_some();
        let mut state = model.state.borrow_mut();
        let cursor_hand = hit.is_some() || link.is_some();
        if state.hover_hit != hit || state.hover_link != link || state.hovered != hovered {
            state.hover_hit = hit;
            state.hover_link = link;
            state.hovered = hovered;
            drop(state);
            self.area.redraw(cx);
        }
        if hovered {
            cx.set_cursor(if cursor_hand { MouseCursor::Hand } else { MouseCursor::Text });
        }
    }

    fn clear_selection(&mut self, cx: &mut Cx) {
        cx.clear_text_selection(self.area);
        self.show_actions = false;
        self.touch_scroll.reset();
        self.drag = Drag::None;
        self.pressed = None;
        self.set_selection(cx, None);
    }

    fn set_selection(&mut self, cx: &mut Cx, selection: Option<(usize, usize)>) {
        if let Some(model) = &self.model {
            model.state.borrow_mut().selection = selection;
        }
        self.canvas.selection = selection;
        self.selection_source = self.canvas.selection_source_hash();
        if selection.is_some() {
            cx.claim_text_selection(self.area);
        } else {
            cx.clear_text_selection(self.area);
        }
        self.area.redraw(cx);
    }
}

impl Widget for ChatRow {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.next_frame.is_event(event).is_some() {
            self.area.redraw(cx);
        }
        if !self.area.is_valid(cx) {
            self.touch_scroll.reset();
        }
        if let Some(delta) = self.touch_scroll.frame(cx, event) {
            self.pan_horizontal(cx, delta);
        }
        // The question answer field (a native text field).
        if self.answer_field.is_some() {
            self.answer.handle_event(cx, event, scope);
            if let Event::Actions(actions) = event {
                if let Some(text) = self.answer.as_ptext_field().changed(actions)
                    && let Some(model) = self.model.clone()
                {
                    if let RowKind::Permission { request, .. } = &model.item.kind
                        && let Some(qs) = permission::questions(request)
                    {
                        let mut state = model.state.borrow_mut();
                        if let Some(card) = state.permission.as_mut() {
                            let active = card.active_question.min(qs.len().saturating_sub(1));
                            card.set_other(&qs[active], active, &text);
                        }
                    }
                    self.area.redraw(cx);
                }
                // Enter on the answer acts as the primary button.
                if self.answer.as_ptext_field().returned(actions).is_some()
                    && let Some(model) = self.model.clone()
                    && let RowKind::Permission { key, .. } = &model.item.kind
                {
                    let target = HitTarget::QuestionPrimary { key: key.clone() };
                    self.tap_local(cx, scope, &target);
                }
            }
        }
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let rel = self.rel(cx, e.abs);
                self.set_hover(cx, Some(rel));
            }
            Hit::FingerHoverOut(_) => self.set_hover(cx, None),
            Hit::FingerDown(e) => {
                self.touch_scroll.reset();
                let rel = self.rel(cx, e.abs);
                self.drag = Drag::None;
                self.pressed = None;
                if let Some(target) = self.canvas.hit_at(rel).cloned() {
                    let rect = self
                        .canvas
                        .hits
                        .iter()
                        .rev()
                        .find(|(r, t)| *t == target && r.contains(rel))
                        .map(|(r, _)| r.translate(self.area.rect(cx).pos))
                        .unwrap_or_default();
                    self.pressed = Some((target, rect));
                    return;
                }
                if e.device.is_touch() {
                    if let Some(key) = self.canvas.scroll_region_at(rel, false) {
                        self.drag = Drag::ScrollX;
                        self.drag_key = key.to_owned();
                        self.touch_scroll.begin(dvec2(e.abs.y, e.abs.x), e.time, true);
                    }
                    return;
                }
                cx.claim_text_selection(self.area);
                if let Some(offset) = self.canvas.offset_at(rel) {
                    let selection = if e.tap_count >= 3 {
                        Some((0, self.canvas.plain.len()))
                    } else if e.tap_count == 2 {
                        Some(self.canvas.word_at(offset))
                    } else {
                        Some((offset, offset))
                    };
                    self.drag = Drag::Select;
                    self.set_selection(cx, selection);
                } else {
                    self.set_selection(cx, None);
                }
            }
            Hit::FingerLongPress(e) => {
                self.touch_scroll.reset();
                // Touch selection: the word under the finger, extended by moving.
                let rel = self.rel(cx, e.abs);
                if let Some(offset) = self.canvas.offset_at(rel) {
                    self.pressed = None;
                    self.drag = Drag::Select;
                    let word = self.canvas.word_at(offset);
                    self.set_selection(cx, Some(word));
                }
            }
            Hit::FingerMove(e) => match self.drag {
                Drag::Select => {
                    if let (DigitDevice::Touch { uid }, Event::TouchUpdate(update)) = (e.device, event)
                        && let Some(touch) = update.touches.iter().find(|touch| touch.uid == uid)
                    {
                        touch.handled.set(self.area);
                    }
                    let rel = self.rel(cx, e.abs);
                    if let (Some(offset), Some((anchor, _))) = (self.canvas.offset_at(rel), self.canvas.selection) {
                        self.set_selection(cx, Some((anchor, offset)));
                    }
                }
                Drag::ScrollX => {
                    if let Some(delta) = self.touch_scroll.move_to(dvec2(e.abs.y, e.abs.x), e.time) {
                        self.pressed = None;
                        if let Event::TouchUpdate(update) = event {
                            for touch in &update.touches {
                                if matches!(touch.state, TouchState::Move) {
                                    touch.handled.set(self.area);
                                }
                            }
                        }
                        self.pan_horizontal(cx, delta);
                    }
                }
                Drag::None => {}
            },
            Hit::FingerUp(e) => {
                self.touch_scroll.release(e.time);
                self.touch_scroll.schedule(cx);
                if let Some((target, rect)) = self.pressed.take() {
                    let rel = self.rel(cx, e.abs);
                    if e.is_over && (!e.device.is_touch() || e.was_tap()) && self.canvas.hit_at(rel) == Some(&target) {
                        self.clear_selection(cx);
                        if !self.tap_local(cx, scope, &target) {
                            self.post(cx, RowEventKind::Tap { target, rect });
                        }
                    }
                    self.area.redraw(cx);
                } else if self.drag == Drag::Select && e.device.is_touch() {
                    if cfg!(target_os = "android") {
                        self.show_actions = self.canvas.selected_range().is_some();
                        self.area.redraw(cx);
                    } else if let Some(text) = self.canvas.selected_text()
                        && let Some(ui) = scope.data.get_mut::<UiContext>()
                    {
                        ui.push(UiEffect::CopyText { text, toast: Some(super::i18n::t("common.actions.copied")) });
                    }
                } else if e.device.is_touch() && e.was_tap() {
                    self.clear_selection(cx);
                }
                self.drag = Drag::None;
            }
            Hit::FingerScroll(_) => self.touch_scroll.reset(),
            Hit::TextCopy(ce) => {
                if let Some(text) = self.canvas.selected_text() {
                    *ce.response.borrow_mut() = Some(text);
                }
            }
            Hit::KeyDown(ke) if ke.key_code == KeyCode::KeyA && (ke.modifiers.control || ke.modifiers.logo) => {
                self.set_selection(cx, Some((0, self.canvas.plain.len())));
            }
            Hit::KeyFocusLost(_) => self.clear_selection(cx),
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let Some(model) = self.model.clone() else { return DrawStep::done() };
        self.draw_list.begin_always(cx);
        cx.begin_turtle(walk, Layout::default());
        let origin = cx.turtle().pos();
        let width = cx.turtle().rect().size.x;
        if width.is_finite() && width > 0.0 {
            self.last_width = width;
        }
        let width = self.last_width.max(1.0);
        self.kit.begin_layer(cx);
        let selection_color = with_alpha(model.ctx.look.palette.accent, 0.3);
        let selection = model.state.borrow().selection;
        self.canvas.begin(origin);
        self.canvas.selection = selection;
        let (height, field, animating) = {
            let mut state = model.state.borrow_mut();
            state.find_y = None;
            let mut d = Draw {
                kit: &mut self.kit,
                canvas: &mut self.canvas,
                look: model.ctx.look,
                state: &mut state,
                now: now_seconds(),
                area: self.area,
                animating: false,
            };
            let (h, field) = draw_row(cx, &mut d, &model.ctx, &model.item, origin.x, origin.y, width);
            let animating = d.animating;
            state.height = h;
            if let Some(y) = state.find_y {
                state.find_y = Some(y - origin.y);
            }
            (h, field, animating)
        };
        if self.selection_source != self.canvas.selection_source_hash() {
            self.clear_selection(cx);
        }
        // Selection from this frame's runs. Appended after the text, it still
        // joins this row's box batch, which `begin_layer` opened before any
        // text batch, so it renders beneath the glyphs.
        let selection_rects = self.canvas.selection_rects();
        let selection_area = if selection_rects.is_empty() {
            Area::Empty
        } else {
            self.kit.rect.begin_many_instances(cx);
            for &r in &selection_rects {
                self.kit.rect(cx, r, selection_color, 2.0);
            }
            self.kit.rect.end_many_instances(cx);
            self.kit.rect.area()
        };
        if let Some(field) = &field {
            let text = self.answer.text();
            if text != field.text {
                self.answer.set_text(cx, &field.text);
            }
            self.answer.as_ptext_field().set_placeholder(cx, &field.placeholder);
            self.answer.as_ptext_field().set_read_only(cx, !field.enabled);
            let inner = Rect { pos: field.rect.pos + dvec2(1.0, 1.0), size: field.rect.size - dvec2(2.0, 2.0) };
            let _ = self.answer.draw_walk(
                cx,
                scope,
                Walk { abs_pos: Some(inner.pos), margin: Margin::default(), width: Size::Fixed(inner.size.x), height: Size::Fixed(inner.size.y) },
            );
        }
        self.answer_field = field;
        cx.walk_turtle(Walk { abs_pos: None, margin: Margin::default(), width: Size::Fill, height: Size::Fixed(height) });
        cx.end_turtle_with_area(&mut self.area);
        self.draw_list.end(cx);
        if self.show_actions || cx.text_actions_active(self.area) {
            self.show_actions = false;
            cx.show_text_actions_for_area(self.area, selection_area, TextActionCapabilities {
                copy: true, select_all: true, ..Default::default()
            });
        }
        if animating {
            self.next_frame = cx.new_next_frame();
        }
        DrawStep::done()
    }
}

impl ChatRowRef {
    pub fn set_model(&self, cx: &mut Cx, model: RowModel) {
        if let Some(mut row) = self.borrow_mut() {
            row.set_model(cx, model);
        }
    }

    pub fn scroll_inner(&self, cx: &mut Cx, abs: DVec2, delta: DVec2) -> bool {
        self.borrow_mut().is_some_and(|mut row| row.scroll_inner(cx, abs, delta))
    }

    /// The current find match's y inside this row, after its last draw.
    pub fn find_y(&self) -> Option<f64> {
        self.borrow().and_then(|row| row.model.as_ref().and_then(|m| m.state.borrow().find_y))
    }

    pub fn rect(&self, cx: &Cx) -> Rect {
        self.borrow().map(|row| row.area.rect(cx)).unwrap_or_default()
    }
}
