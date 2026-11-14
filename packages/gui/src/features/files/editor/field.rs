//! A single-line text field drawn inside a custom view (find, replace, remote location),
//! Flutter `TextField` with `paseoInputDecoration(size: sm)`: surface2 fill, 6px radius, 8px
//! inset, optional trailing status text.

use makepad_widgets::*;
use makepad_widgets::makepad_draw::event::DigitId;

use crate::common::paint::Font;
use crate::theme::{Palette, with_alpha};

use super::buffer::{EditKind, Selection, TextBuffer};
use crate::features::files::kit::{Kit, rect, spec};

#[derive(Clone, Debug, PartialEq)]
pub enum FieldEvent {
    None,
    Changed,
    /// Enter (with Shift held?).
    Submitted { shift: bool },
    Escaped,
}

pub struct TextField {
    pub buffer: TextBuffer,
    pub focused: bool,
    pub blink: super::Blink,
    scroll_x: f64,
    text_rect: Rect,
    font_size: f64,
    drag_anchor: Option<usize>,
    touch_focus: Option<(DigitId, u64)>,
    frame: Rect,
    ime_id: u64,
    ime_area: Area,
    ime_caret_area: Area,
    show_actions: bool,
    selection_invalidated: bool,
    owns_selection: bool,
    handles: Option<TextSelectionHandles>,
    handles_revision: u64,
    touch_handles: bool,
    owner_draw_pos: DVec2,
    max_scroll_x: f64,
    scroll_follows_anchor: bool,
}

impl TextField {
    pub fn new(text: &str) -> TextField {
        let mut buffer = TextBuffer::new(text.replace('\n', " "));
        let len = buffer.len();
        buffer.set_selection(Selection::caret(len));
        TextField { buffer, focused: false, blink: super::Blink::default(), scroll_x: 0.0, text_rect: Rect::default(), font_size: 14.0, drag_anchor: None, touch_focus: None, frame: Rect::default(), ime_id: LiveId::unique().0, ime_area: Area::Empty, ime_caret_area: Area::Empty, show_actions: false, selection_invalidated: false, owns_selection: false, handles: None, handles_revision: 0, touch_handles: false, owner_draw_pos: DVec2::default(), max_scroll_x: 0.0, scroll_follows_anchor: false }
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn set_text(&mut self, text: &str) {
        self.buffer.set_text(text.replace('\n', " "));
        let len = self.buffer.len();
        self.buffer.set_selection(Selection::caret(len));
        self.drag_anchor = None;
        self.touch_focus = None;
        self.show_actions = false;
        self.selection_invalidated = true;
        self.touch_handles = false;
    }

    pub fn select_all(&mut self) {
        self.touch_focus = None;
        let len = self.buffer.len();
        self.buffer.set_selection(Selection { anchor: 0, head: len });
    }

    pub fn clear_selection(&mut self, cx: &mut Cx) {
        self.cancel_selection_handles(cx);
        if self.owns_selection {
            cx.clear_text_selection(self.ime_area);
        }
        self.owns_selection = false;
        self.buffer.set_selection(Selection::caret(self.buffer.selection.head));
        self.drag_anchor = None;
        self.touch_focus = None;
        self.show_actions = false;
        self.selection_invalidated = false;
    }

    /// Draws the field in `frame` with `placeholder` and an optional trailing status.
    /// `ime_area` owns keyboard focus; the caret has separate IME avoidance geometry.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        cx: &mut Cx2d,
        kit: &mut Kit,
        frame: Rect,
        placeholder: &str,
        trailing: Option<&str>,
        font_size: f64,
        p: &Palette,
        ime_area: Area,
    ) {
        if self.selection_invalidated {
            self.cancel_selection_handles(cx);
            if self.owns_selection {
                cx.clear_text_selection(self.ime_area);
            }
            self.owns_selection = false;
            self.selection_invalidated = false;
        }
        self.font_size = font_size;
        self.frame = frame;
        self.ime_area = ime_area;
        self.owner_draw_pos = ime_area.rect(cx).pos;
        if self.handles_revision != self.buffer.revision || !self.focused {
            if let Some(handles) = self.handles.as_mut() {
                handles.cancel(cx);
            }
        }
        self.handles_revision = self.buffer.revision;
        // Focus shows on the surface2 fill (border_accent is ~1.1:1 against it).
        let border = if self.focused { p.muted } else { p.surface2 };
        kit.outline(cx, frame, p.surface2, border, 1.0, 6.0);
        let trailing_w = match trailing {
            Some(text) if !text.is_empty() => kit.measure(cx, spec(Font::Regular, font_size - 2.0, p.muted), text).x + 8.0,
            _ => 0.0,
        };
        let text_rect = rect(frame.pos.x + 8.0, frame.pos.y, frame.size.x - 16.0 - trailing_w, frame.size.y);
        self.text_rect = text_rect;
        if let Some(text) = trailing.filter(|t| !t.is_empty()) {
            let r = rect(frame.pos.x + frame.size.x - 8.0 - trailing_w + 8.0, frame.pos.y, trailing_w - 8.0, frame.size.y);
            kit.text_in(cx, r, spec(Font::Regular, font_size - 2.0, p.muted), text, 1.0);
        }
        cx.begin_turtle(Walk::abs_rect(text_rect), Layout::default());
        let s = spec(Font::Regular, font_size, p.foreground);
        if self.buffer.is_empty() {
            kit.text_in(cx, text_rect, spec(Font::Regular, font_size, p.muted), placeholder, 0.0);
        }
        let laid = kit.layout(cx, s, self.buffer.text(), None);
        let x_of = |index: usize| laid.rows.first().map(|row| row.index_to_x_in_lpxs(index) as f64).unwrap_or(0.0);
        let sel = self.buffer.selection;
        let caret_x = x_of(sel.head);
        self.max_scroll_x = (laid.size_in_lpxs.width as f64 - text_rect.size.x + 2.0).max(0.0);
        if !self.handles.as_ref().is_some_and(TextSelectionHandles::is_dragging) {
            let reveal_x = if self.scroll_follows_anchor { x_of(sel.anchor) } else { caret_x };
            if reveal_x - self.scroll_x > text_rect.size.x - 2.0 {
                self.scroll_x = reveal_x - text_rect.size.x + 2.0;
            } else if reveal_x < self.scroll_x {
                self.scroll_x = reveal_x;
            }
        }
        self.scroll_x = self.scroll_x.clamp(0.0, self.max_scroll_x);
        let x0 = text_rect.pos.x - self.scroll_x;
        let h = laid.size_in_lpxs.height as f64;
        let y = text_rect.pos.y + (text_rect.size.y - h.max(font_size * 1.2)) / 2.0;
        if !sel.is_empty() {
            let (a, b) = (x_of(sel.start()), x_of(sel.end()));
            kit.fill(cx, rect(x0 + a, y, b - a, h.max(font_size * 1.2)), with_alpha(p.accent, 0.35), 0.0);
        }
        if !self.buffer.is_empty() {
            kit.draw_laidout(cx, s, dvec2(x0, y), &laid);
        }
        if self.focused {
            self.blink.arm(cx);
            let caret = rect(x0 + caret_x, y, 1.5, h.max(font_size * 1.2));
            if self.blink.visible() {
                kit.fill(cx, caret, p.foreground, 0.0);
            }
            // KeyboardView avoids the supplied area's bottom, not the relative
            // caret position. Using the owner area would pan the whole file pane.
            cx.add_aligned_rect_area(&mut self.ime_caret_area, caret);
            cx.update_text_input(self.ime_area, self.ime_id, self.buffer.text(),
                sel.anchor, sel.head, false, false, true);
            cx.show_text_ime_caret(self.ime_caret_area, Rect { pos: dvec2(0.0, 0.0), size: caret.size });
        } else {
            self.blink.disarm(cx);
        }
        cx.end_turtle();
        if self.focused && self.touch_handles {
            self.handles.get_or_insert_with(|| TextSelectionHandles::new(cx)).draw(
                cx, self.ime_area, text_rect,
                rect(x0 + x_of(sel.anchor), y, 1.5, h.max(font_size * 1.2)),
                rect(x0 + caret_x, y, 1.5, h.max(font_size * 1.2)), sel.is_empty(), true,
            );
        }
        if self.focused && !self.handles.as_ref().is_some_and(TextSelectionHandles::is_dragging)
            && (self.show_actions || (self.owns_selection && cx.text_actions_active(self.ime_area))) {
            self.show_actions = false;
            let a = x_of(sel.start());
            let b = x_of(sel.end());
            let anchor = rect(x0 + a, y, (b - a).max(1.5), h.max(font_size * 1.2))
                .clip((text_rect.pos, text_rect.pos + text_rect.size));
            if anchor.size.x > 0.0 && anchor.size.y > 0.0 {
                cx.show_text_actions(self.ime_area, anchor, TextActionCapabilities {
                    copy: !sel.is_empty(), cut: !sel.is_empty(), paste: true, select_all: !self.buffer.is_empty(),
                });
            } else {
                cx.clear_text_selection(self.ime_area);
                cx.claim_text_selection(self.ime_area);
            }
        }
    }

    fn index_at(&self, cx: &mut Cx, kit: &mut Kit, abs: DVec2) -> usize {
        let s = spec(Font::Regular, self.font_size, Vec4::default());
        let laid = kit.layout(cx, s, self.buffer.text(), None);
        let x = (abs.x - self.text_rect.pos.x + self.scroll_x).max(0.0);
        laid.rows.first().map(|row| row.x_in_lpxs_to_index(x as f32)).unwrap_or(0).min(self.buffer.len())
    }

    pub fn cancel_selection_handles(&mut self, cx: &mut Cx) {
        if let Some(handles) = self.handles.as_mut() {
            handles.cancel(cx);
        }
        self.touch_handles = false;
    }

    /// Handle events are routed before ordinary owner hits, including frame-driven
    /// scrolling with a stationary pointer outside the field.
    pub fn handle_selection_handles(&mut self, cx: &mut Cx, kit: &mut Kit, event: &Event) -> bool {
        if self.selection_invalidated || !self.focused || self.handles_revision != self.buffer.revision {
            self.cancel_selection_handles(cx);
            return false;
        }
        let Some(action) = self.handles.as_mut().and_then(|handles| handles.handle_event(cx, event, self.ime_area)) else {
            return false;
        };
        self.touch_focus = None;
        self.drag_anchor = None;
        self.show_actions = false;
        if action.phase != TextHandlePhase::Cancel {
            self.scroll_x = (self.scroll_x + action.scroll.x).clamp(0.0, self.max_scroll_x);
            let shift = self.ime_area.rect(cx).pos - self.owner_draw_pos;
            let index = self.index_at(cx, kit, action.abs - shift);
            self.buffer.move_selection_handle(action.endpoint, index);
            self.scroll_follows_anchor = action.endpoint == TextHandleEndpoint::Anchor;
            self.blink.restart(cx);
            cx.update_text_input(self.ime_area, self.ime_id, self.buffer.text(),
                self.buffer.selection.anchor, self.buffer.selection.head, false, false, true);
            self.show_actions = action.phase == TextHandlePhase::End && cfg!(target_os = "android");
        }
        true
    }

    pub fn cancel_touch_focus(&mut self) {
        self.touch_focus = None;
    }

    /// Owners route the captured gesture here even before this field has focus.
    pub fn wants_touch_focus(&self, hit: &Hit) -> bool {
        match hit {
            Hit::FingerUp(fe) => self.touch_focus == Some((fe.digit_id, self.buffer.revision))
                && fe.is_over && fe.was_tap() && self.frame.contains(fe.abs),
            Hit::FingerLongPress(fe) => self.touch_focus == Some((fe.digit_id, self.buffer.revision))
                && self.frame.contains(fe.abs),
            _ => false,
        }
    }

    pub fn observe_event(&mut self, event: &Event) {
        if matches!(event, Event::KeyFocus(_)) {
            self.touch_focus = None;
        }
        if matches!(event, Event::TouchUpdate(update) if update.cancelled) {
            self.touch_focus = None;
            self.drag_anchor = None;
            self.show_actions = false;
        }
    }

    fn place_pointer_cursor(&mut self, cx: &mut Cx, kit: &mut Kit, abs: DVec2, tap_count: u32, shift: bool) {
        cx.claim_text_selection(self.ime_area);
        self.owns_selection = true;
        let index = self.index_at(cx, kit, abs);
        match tap_count {
            2 => {
                let (a, b) = self.buffer.word_at(index);
                self.buffer.set_selection(Selection { anchor: a, head: b });
            }
            3 => self.select_all(),
            _ => self.buffer.move_head(index, shift),
        }
    }

    pub fn handle_hit(&mut self, cx: &mut Cx, kit: &mut Kit, hit: &Hit) -> FieldEvent {
        if self.selection_invalidated {
            self.cancel_selection_handles(cx);
            if self.owns_selection {
                cx.clear_text_selection(self.ime_area);
            }
            self.owns_selection = false;
            self.selection_invalidated = false;
        }
        let (revision, selection) = (self.buffer.revision, self.buffer.selection);
        let mut event = FieldEvent::None;
        match hit {
            Hit::FingerDown(fe) if fe.device.is_primary_hit() => {
                self.touch_focus = fe.device.is_touch().then_some((fe.digit_id, self.buffer.revision));
                if !fe.device.is_touch() {
                    self.cancel_selection_handles(cx);
                }
                if fe.device.is_touch() {
                    self.drag_anchor = None;
                    return FieldEvent::None;
                }
                self.place_pointer_cursor(cx, kit, fe.abs, fe.tap_count, fe.modifiers.shift);
                self.drag_anchor = (!fe.device.is_touch()).then_some(self.buffer.selection.anchor);
            }
            Hit::FingerLongPress(fe) if self.wants_touch_focus(hit) => {
                self.touch_focus = None;
                self.touch_handles = true;
                cx.claim_text_selection(self.ime_area);
                self.owns_selection = true;
                let index = self.index_at(cx, kit, fe.abs);
                let (a, b) = self.buffer.word_at(index);
                self.buffer.set_selection(Selection { anchor: a, head: b });
                self.drag_anchor = Some(a);
                self.show_actions = cfg!(target_os = "android");
            }
            Hit::FingerMove(fe) => {
                if fe.device.is_touch() && (!fe.is_over || !self.frame.contains(fe.abs)) {
                    self.touch_focus = None;
                }
                if let Some(anchor) = self.drag_anchor {
                    let index = self.index_at(cx, kit, fe.abs);
                    self.buffer.set_selection(Selection { anchor, head: index });
                }
            }
            Hit::FingerUp(fe) => {
                if self.wants_touch_focus(hit) {
                    self.touch_handles = true;
                    self.place_pointer_cursor(cx, kit, fe.abs, fe.tap_count, false);
                    if cfg!(target_os = "android") && !self.buffer.selection.is_empty() {
                        self.show_actions = true;
                    }
                }
                self.touch_focus = None;
                if cfg!(target_os = "android") && fe.device.is_touch() && self.drag_anchor.is_some() {
                    self.show_actions = true;
                }
                self.drag_anchor = None;
            }
            Hit::KeyDown(ke) => {
                if ke.key_code != KeyCode::KeyA || !ke.modifiers.is_primary() {
                    self.cancel_selection_handles(cx);
                }
                let m = ke.modifiers;
                let word = if cfg!(target_vendor = "apple") { m.alt } else { m.control };
                let head = self.buffer.selection.head;
                match ke.key_code {
                    KeyCode::ReturnKey | KeyCode::NumpadEnter => return FieldEvent::Submitted { shift: m.shift },
                    KeyCode::Escape => return FieldEvent::Escaped,
                    KeyCode::ArrowLeft => {
                        let target = if word { self.buffer.word_left(head) } else { self.buffer.prev(head) };
                        self.buffer.move_head(target, m.shift);
                    }
                    KeyCode::ArrowRight => {
                        let target = if word { self.buffer.word_right(head) } else { self.buffer.next(head) };
                        self.buffer.move_head(target, m.shift);
                    }
                    KeyCode::Home => self.buffer.move_head(0, m.shift),
                    KeyCode::End => {
                        let len = self.buffer.len();
                        self.buffer.move_head(len, m.shift);
                    }
                    KeyCode::Backspace => self.buffer.backspace(word),
                    KeyCode::Delete => self.buffer.delete(word),
                    KeyCode::KeyA if m.is_primary() => self.select_all(),
                    KeyCode::KeyZ if m.is_primary() => {
                        if m.shift { self.buffer.redo(); } else { self.buffer.undo(); }
                    }
                    _ => {}
                }
            }
            Hit::TextIme(edit) => self.buffer.apply_ime(edit, true),
            Hit::TextInput(te) => {
                let text = te.input.replace(['\n', '\r'], " ");
                if !text.is_empty() {
                    self.buffer.insert(&text, if te.was_paste { EditKind::Other } else { EditKind::Insert });
                }
            }
            Hit::TextCopy(ce) => {
                *ce.response.borrow_mut() = Some(self.buffer.selected_text().to_owned());
            }
            Hit::TextCut(ce) => {
                *ce.response.borrow_mut() = Some(self.buffer.selected_text().to_owned());
                if !self.buffer.selection.is_empty() {
                    self.buffer.insert("", EditKind::Other);
                }
            }
            Hit::KeyFocusLost(_) => {
                self.focused = false;
                self.clear_selection(cx);
            }
            _ => {}
        }
        if self.buffer.selection != selection && !self.buffer.selection.is_empty() {
            cx.claim_text_selection(self.ime_area);
            self.owns_selection = true;
        }
        if self.buffer.revision != revision {
            event = FieldEvent::Changed;
        }
        if self.buffer.revision != revision || self.buffer.selection != selection {
            self.scroll_follows_anchor = false;
            self.blink.restart(cx);
        }
        if self.focused {
            cx.update_text_input(self.ime_area, self.ime_id, self.buffer.text(),
                self.buffer.selection.anchor, self.buffer.selection.head, false, false, true);
        }
        event
    }
}

#[cfg(test)]
pub(super) mod touch_tests {
    use super::*;
    use makepad_widgets::makepad_draw::event::{TextImeEvent, TouchUpdateEvent};

    pub fn down() -> Hit {
        Hit::FingerDown(FingerDownEvent {
            window_id: WindowId(0, 0), abs: dvec2(5.0, 5.0),
            digit_id: live_id_num!(touch, 1).into(), device: DigitDevice::Touch { uid: 1 },
            tap_count: 2, modifiers: KeyModifiers::default(), time: 0.0, rect: rect(0.0, 0.0, 100.0, 100.0),
        })
    }

    pub fn up() -> FingerUpEvent {
        FingerUpEvent {
            window_id: WindowId(0, 0), abs: dvec2(5.0, 5.0), abs_start: dvec2(5.0, 5.0),
            capture_time: 0.0, time: 0.1, digit_id: live_id_num!(touch, 1).into(),
            device: DigitDevice::Touch { uid: 1 }, has_long_press_occurred: false,
            tap_cancelled: false, tap_count: 2, modifiers: KeyModifiers::default(),
            rect: rect(0.0, 0.0, 100.0, 100.0), is_over: true, is_sweep: false,
        }
    }

    pub fn moved_out() -> Hit {
        Hit::FingerMove(FingerMoveEvent {
            window_id: WindowId(0, 0), abs: dvec2(-1.0, 5.0), abs_start: dvec2(5.0, 5.0),
            digit_id: live_id_num!(touch, 1).into(), device: DigitDevice::Touch { uid: 1 },
            tap_count: 2, modifiers: KeyModifiers::default(), time: 0.05,
            rect: rect(0.0, 0.0, 100.0, 100.0), is_over: true,
        })
    }

    pub fn cancel() -> Event {
        Event::TouchUpdate(TouchUpdateEvent {
            window_id: WindowId(0, 0), time: 0.05, modifiers: KeyModifiers::default(),
            touches: Vec::new(), cancelled: true,
        })
    }
    #[test]
    fn single_line_ime_composition_submits_exact_path_and_paste_stays_text() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut kit = Kit::new(&mut cx);
        let mut field = TextField::new("/home/");
        for (revision, text, composing) in [
            (1, "/home/ㅎ", Some((6, 7))),
            (2, "/home/한", Some((6, 7))),
            (3, "/home/한", None),
        ] {
            let event = field.handle_hit(&mut cx, &mut kit, &Hit::TextIme(TextImeEvent {
                session: 1, revision, text: text.into(),
                selection_start: 7, selection_end: 7, composing,
            }));
            assert_ne!(event, FieldEvent::Submitted { shift: false });
        }
        // Android editor actions are translated to the same Return as hardware.
        for key_code in [KeyCode::ReturnKey, KeyCode::NumpadEnter] {
            assert_eq!(field.handle_hit(&mut cx, &mut kit, &Hit::KeyDown(KeyEvent {
                key_code, ..Default::default()
            })), FieldEvent::Submitted { shift: false });
            assert_eq!(field.text(), "/home/한");
        }
        assert_eq!(field.handle_hit(&mut cx, &mut kit, &Hit::TextInput(TextInputEvent {
            input: "\nnext\r\nline".into(), was_paste: true, replace_last: false,
        })), FieldEvent::Changed);
        assert_eq!(field.text(), "/home/한 next  line");
        assert_eq!(field.handle_hit(&mut cx, &mut kit, &Hit::TextIme(TextImeEvent {
            session: 1, revision: 4, text: "a\nb".into(),
            selection_start: 3, selection_end: 3, composing: None,
        })), FieldEvent::Changed);
        assert_eq!(field.text(), "a b");
    }

    #[test]
    fn manual_field_defers_even_double_tap_selection_until_inside_release() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut kit = Kit::new(&mut cx);
        let mut field = TextField::new("한글 text");
        field.frame = rect(0.0, 0.0, 100.0, 100.0);
        let selection = field.buffer.selection;
        assert!(!field.wants_touch_focus(&Hit::FingerUp(up())));
        field.handle_hit(&mut cx, &mut kit, &down());
        assert!(!field.focused);
        assert_eq!(field.buffer.selection, selection);
        assert!(field.wants_touch_focus(&Hit::FingerUp(up())));
        let mut outside = up();
        // The owner's pane still contains the release, but the field does not.
        outside.abs.x = -1.0;
        field.handle_hit(&mut cx, &mut kit, &Hit::FingerUp(outside));
        assert!(!field.focused);
        assert_eq!(field.buffer.selection, selection);
        assert!(!field.wants_touch_focus(&Hit::FingerUp(up())));
    }

    #[test]
    fn manual_field_rejects_stale_cancelled_and_returned_gestures() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut kit = Kit::new(&mut cx);
        for transition in 0..7 {
            let mut field = TextField::new("한글 text");
            field.frame = rect(0.0, 0.0, 100.0, 100.0);
            field.handle_hit(&mut cx, &mut kit, &down());
            let mut release = up();
            match transition {
                0 => { field.handle_hit(&mut cx, &mut kit, &moved_out()); }
                1 => field.observe_event(&cancel()),
                2 => field.set_text("new model"),
                3 => field.observe_event(&Event::KeyFocus(KeyFocusEvent { prev: Area::Empty, focus: Area::Empty })),
                4 => field.frame.pos.x = 20.0,
                5 => release.tap_cancelled = true,
                _ => field.buffer.insert("model edit", EditKind::Other),
            }
            let selection = field.buffer.selection;
            assert!(!field.wants_touch_focus(&Hit::FingerUp(release.clone())));
            field.handle_hit(&mut cx, &mut kit, &Hit::FingerUp(release));
            assert!(!field.focused);
            assert_eq!(field.buffer.selection, selection);
        }
    }
}
