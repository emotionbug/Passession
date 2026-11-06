//! A single-line text field drawn inside a custom view (find, replace, remote location),
//! Flutter `TextField` with `paseoInputDecoration(size: sm)`: surface2 fill, 6px radius, 8px
//! inset, optional trailing status text.

use makepad_widgets::*;

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
    ime_id: u64,
    ime_area: Area,
    ime_caret_area: Area,
    show_actions: bool,
    selection_invalidated: bool,
    owns_selection: bool,
}

impl TextField {
    pub fn new(text: &str) -> TextField {
        let mut buffer = TextBuffer::new(text.replace('\n', " "));
        let len = buffer.len();
        buffer.set_selection(Selection::caret(len));
        TextField { buffer, focused: false, blink: super::Blink::default(), scroll_x: 0.0, text_rect: Rect::default(), font_size: 14.0, drag_anchor: None, ime_id: LiveId::unique().0, ime_area: Area::Empty, ime_caret_area: Area::Empty, show_actions: false, selection_invalidated: false, owns_selection: false }
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn set_text(&mut self, text: &str) {
        self.buffer.set_text(text.replace('\n', " "));
        let len = self.buffer.len();
        self.buffer.set_selection(Selection::caret(len));
        self.drag_anchor = None;
        self.show_actions = false;
        self.selection_invalidated = true;
    }

    pub fn select_all(&mut self) {
        let len = self.buffer.len();
        self.buffer.set_selection(Selection { anchor: 0, head: len });
    }

    pub fn clear_selection(&mut self, cx: &mut Cx) {
        if self.owns_selection {
            cx.clear_text_selection(self.ime_area);
        }
        self.owns_selection = false;
        self.buffer.set_selection(Selection::caret(self.buffer.selection.head));
        self.drag_anchor = None;
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
            if self.owns_selection {
                cx.clear_text_selection(self.ime_area);
            }
            self.owns_selection = false;
            self.selection_invalidated = false;
        }
        self.font_size = font_size;
        self.ime_area = ime_area;
        let border = if self.focused { p.border_accent } else { p.surface2 };
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
        if caret_x - self.scroll_x > text_rect.size.x - 2.0 {
            self.scroll_x = caret_x - text_rect.size.x + 2.0;
        } else if caret_x < self.scroll_x {
            self.scroll_x = caret_x;
        }
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
                sel.anchor, sel.head, false, false);
            cx.show_text_ime(self.ime_caret_area, dvec2(0.0, caret.size.y));
        } else {
            self.blink.disarm(cx);
        }
        cx.end_turtle();
        if self.focused && (self.show_actions || (self.owns_selection && cx.text_actions_active(self.ime_area))) {
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

    pub fn handle_hit(&mut self, cx: &mut Cx, kit: &mut Kit, hit: &Hit) -> FieldEvent {
        if self.selection_invalidated {
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
                cx.claim_text_selection(self.ime_area);
                self.owns_selection = true;
                let index = self.index_at(cx, kit, fe.abs);
                match fe.tap_count {
                    2 => {
                        let (a, b) = self.buffer.word_at(index);
                        self.buffer.set_selection(Selection { anchor: a, head: b });
                    }
                    3 => self.select_all(),
                    _ => self.buffer.move_head(index, fe.modifiers.shift),
                }
                self.drag_anchor = (!fe.device.is_touch()).then_some(self.buffer.selection.anchor);
            }
            Hit::FingerLongPress(fe) => {
                cx.claim_text_selection(self.ime_area);
                self.owns_selection = true;
                let index = self.index_at(cx, kit, fe.abs);
                let (a, b) = self.buffer.word_at(index);
                self.buffer.set_selection(Selection { anchor: a, head: b });
                self.drag_anchor = Some(a);
            }
            Hit::FingerMove(fe) => {
                if let Some(anchor) = self.drag_anchor {
                    let index = self.index_at(cx, kit, fe.abs);
                    self.buffer.set_selection(Selection { anchor, head: index });
                }
            }
            Hit::FingerUp(fe) => {
                if cfg!(target_os = "android") && fe.device.is_touch() && self.drag_anchor.is_some() {
                    self.show_actions = true;
                }
                self.drag_anchor = None;
            }
            Hit::KeyDown(ke) => {
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
            self.blink.restart(cx);
        }
        if self.focused {
            cx.update_text_input(self.ime_area, self.ime_id, self.buffer.text(),
                self.buffer.selection.anchor, self.buffer.selection.head, false, false);
        }
        event
    }
}
