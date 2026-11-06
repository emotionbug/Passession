//! Native editable fields: one Makepad `TextInput` per field id, created from
//! the panel's `field_template`, kept while the field is drawn. Text lives in
//! the widget (IME composition, selection and undo stay native); settings
//! reads it back by id when a form saves.

use std::collections::{HashMap, HashSet};

use makepad_widgets::makepad_draw::text::selection::Cursor;
use makepad_widgets::*;

use super::draw::Font;
use super::theme::Palette;
use crate::features::settings::rpc::Handler;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    Page,
    Overlay,
}

/// What a field needs on the frame it is drawn.
pub struct FieldSpec {
    pub id: String,
    /// Text set when the widget is created (or reset with [`FieldHost::set_text`]).
    pub initial: String,
    pub placeholder: String,
    pub password: bool,
    pub numeric: bool,
    pub multiline: bool,
    pub enabled: bool,
    pub autofocus: bool,
    pub font_px: f64,
    pub on_submit: Option<Handler>,
    pub on_blur: Option<Handler>,
    /// Read-only selectable text (Flutter `SelectableText`) instead of an input.
    pub plain: Option<Plain>,
    /// `InputDecoration(isCollapsed, filled: false, border: none)`: no box, no padding.
    pub bare: bool,
}

/// How a [`FieldSpec::plain`] text is drawn: no box, no padding, the text
/// in this font, size and colour; selectable and copyable, never editable.
#[derive(Clone, Copy)]
pub struct Plain {
    pub font: Font,
    pub color: Vec4,
}

impl FieldSpec {
    pub fn new(id: impl Into<String>, initial: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            initial: initial.into(),
            placeholder: String::new(),
            password: false,
            numeric: false,
            multiline: false,
            enabled: true,
            autofocus: false,
            font_px: 14.0,
            on_submit: None,
            on_blur: None,
            plain: None,
            bare: false,
        }
    }

    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }

    /// Selectable read-only text of `text` (see [`Plain`]); `size` in px.
    pub fn selectable(id: impl Into<String>, text: impl Into<String>, font: Font, size: f64, color: Vec4) -> Self {
        let mut spec = Self::new(id, text);
        spec.font_px = size;
        spec.enabled = false;
        spec.plain = Some(Plain { font, color });
        spec
    }
    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }
    pub fn password(mut self, on: bool) -> Self {
        self.password = on;
        self
    }
    pub fn numeric(mut self) -> Self {
        self.numeric = true;
        self
    }
    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn autofocus(mut self) -> Self {
        self.autofocus = true;
        self
    }
    pub fn on_submit(mut self, handler: Handler) -> Self {
        self.on_submit = Some(handler);
        self
    }
    pub fn on_blur(mut self, handler: Handler) -> Self {
        self.on_blur = Some(handler);
        self
    }
}

struct Entry {
    widget: WidgetRef,
    layer: Layer,
    /// Overlay stacking depth it was drawn at (0 page, 1.. overlay layers).
    depth: usize,
    multiline: bool,
    enabled: bool,
    placeholder: String,
    /// Theme epoch, font and enabled state the widget was last styled for.
    epoch: Option<u64>,
    styled_enabled: bool,
    /// Plain text colour the widget was last styled with.
    styled_color: Option<Vec4>,
    font_px: f64,
    on_submit: Option<Handler>,
    on_blur: Option<Handler>,
    focus_pending: bool,
}

impl Entry {
    fn release_editor(&self, cx: &mut Cx) {
        self.widget.as_text_input().release_focus(cx);
    }
}

/// What happened to fields during one event.
#[derive(Default)]
pub struct FieldOutcome {
    pub changed: bool,
    pub run: Vec<Handler>,
}

#[derive(Default)]
pub struct FieldHost {
    template: Option<LivePtr>,
    /// Selectable-text templates: regular, medium and code font.
    plain_templates: [Option<LivePtr>; 3],
    entries: HashMap<String, Entry>,
    drawn: HashSet<String>,
    pending_text: HashMap<String, String>,
}

impl FieldHost {
    pub fn set_templates(&mut self, template: Option<LivePtr>, plain: [Option<LivePtr>; 3]) {
        self.template = template;
        self.plain_templates = plain;
    }

    /// The field's current text ("" when it was never drawn).
    pub fn text(&self, id: &str) -> String {
        if let Some(text) = self.pending_text.get(id) {
            return text.clone();
        }
        self.entries.get(id).map(|entry| entry.widget.text()).unwrap_or_default()
    }

    pub fn exists(&self, id: &str) -> bool {
        self.entries.contains_key(id) || self.pending_text.contains_key(id)
    }

    /// Replaces the text now, or when the field is next created.
    pub fn set_text(&mut self, cx: &mut Cx, id: &str, text: &str) {
        match self.entries.get(id) {
            Some(entry) => entry.widget.set_text(cx, text),
            None => {
                self.pending_text.insert(id.to_string(), text.to_string());
            }
        }
    }

    /// Forgets every field whose id starts with `prefix` (a closed sheet).
    pub fn forget_prefix(&mut self, cx: &mut Cx, prefix: &str) {
        self.entries.retain(|id, entry| {
            let keep = !id.starts_with(prefix);
            if !keep {
                entry.release_editor(cx);
            }
            keep
        });
        self.pending_text.retain(|id, _| !id.starts_with(prefix));
    }

    pub fn focus(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.focus_pending = true;
        }
    }

    pub fn begin_frame(&mut self) {
        self.drawn.clear();
    }

    /// Drops fields no longer drawn, releasing their native editor before disposal.
    pub fn end_frame(&mut self, cx: &mut Cx) {
        let drawn = &self.drawn;
        self.entries.retain(|id, entry| {
            let keep = drawn.contains(id);
            if !keep {
                entry.release_editor(cx);
            }
            keep
        });
    }

    pub fn draw(&mut self, cx: &mut Cx2d, pal: &Palette, (layer, depth): (Layer, usize), spec: FieldSpec, rect: Rect) {
        let id = spec.id.clone();
        self.drawn.insert(id.clone());
        let template = match spec.plain.map(|p| p.font) {
            None => self.template,
            Some(Font::Regular) => self.plain_templates[0],
            Some(Font::Medium) => self.plain_templates[1],
            Some(Font::Code) => self.plain_templates[2],
        };
        let initial = self.pending_text.remove(&id).unwrap_or(spec.initial.clone());
        let created = !self.entries.contains_key(&id);
        let entry = self.entries.entry(id.clone()).or_insert_with(|| Entry {
            widget: WidgetRef::new_from_ptr(cx, template),
            layer,
            depth,
            multiline: spec.multiline,
            enabled: true,
            placeholder: String::new(),
            epoch: None,
            styled_enabled: true,
            styled_color: None,
            font_px: 0.0,
            on_submit: None,
            on_blur: None,
            focus_pending: spec.autofocus,
        });
        entry.layer = layer;
        entry.depth = depth;
        entry.multiline = spec.multiline;
        entry.on_submit = spec.on_submit;
        entry.on_blur = spec.on_blur;
        let input = entry.widget.as_text_input();
        if let Some(plain) = spec.plain {
            // Selectable text follows its source every frame.
            if created || entry.widget.text() != initial {
                entry.widget.set_text(cx, &initial);
            }
            if created {
                input.set_is_read_only(cx, true);
            }
            let epoch = crate::theme::epoch();
            if entry.epoch != Some(epoch) || entry.font_px != spec.font_px || entry.styled_color != Some(plain.color) {
                entry.epoch = Some(epoch);
                entry.font_px = spec.font_px;
                entry.styled_color = Some(plain.color);
                let font = spec.font_px * 0.75;
                let fg = plain.color;
                // Material 3 default: the primary colour at 40%.
                let selection = super::theme::alpha(pal.accent, 0.4);
                entry.widget.apply_over(
                    cx,
                    live! {
                        draw_text: {
                            color: (fg), color_hover: (fg), color_focus: (fg), color_down: (fg), color_disabled: (fg),
                            color_empty: (fg), color_empty_hover: (fg), color_empty_focus: (fg),
                            text_style: { font_size: (font) }
                        }
                        draw_selection: { color: (selection) }
                    },
                );
            }
            entry
                .widget
                .draw_walk_all(cx, &mut Scope::empty(), Walk::fixed(rect.size.x, rect.size.y).with_abs_pos(rect.pos));
            return;
        }
        if created {
            entry.widget.set_text(cx, &initial);
            input.set_is_password(cx, spec.password);
            input.set_is_numeric_only(cx, spec.numeric);
        }
        if entry.placeholder != spec.placeholder {
            entry.placeholder = spec.placeholder.clone();
            input.set_empty_text(cx, spec.placeholder);
        }
        if created || entry.enabled != spec.enabled {
            entry.enabled = spec.enabled;
            input.set_is_read_only(cx, !spec.enabled);
        }
        let epoch = crate::theme::epoch();
        if entry.epoch != Some(epoch) || entry.font_px != spec.font_px || entry.styled_enabled != spec.enabled {
            entry.epoch = Some(epoch);
            entry.font_px = spec.font_px;
            entry.styled_enabled = spec.enabled;
            let font = spec.font_px * 0.75;
            let fill = if spec.bare { pal.transparent } else if spec.enabled { pal.surface2 } else { pal.surface1 };
            let (border, focus, fg, hint) = (pal.border, pal.border_accent, pal.foreground, pal.muted);
            let (border, focus) = if spec.bare { (pal.transparent, pal.transparent) } else { (border, focus) };
            let selection = super::theme::alpha(pal.accent, 0.4);
            let (pad_x, pad_y, border_size) = if spec.bare { (0.0, 0.0, 0.0) } else { (12.0, 9.0, 1.0) };
            entry.widget.apply_over(
                cx,
                live! {
                    padding: { left: (pad_x), right: (pad_x), top: (pad_y), bottom: (pad_y) }
                    draw_bg: {
                        color: (fill), color_hover: (fill), color_focus: (fill),
                        color_down: (fill), color_empty: (fill), color_disabled: (fill),
                        // Makepad's TextInput `Sdf2d::box` draws twice the radius it is given: 8px.
                        border_radius: 4.0, border_size: (border_size),
                        border_color_1: (border), border_color_2: (border),
                        border_color_1_hover: (border), border_color_2_hover: (border),
                        border_color_1_focus: (focus), border_color_2_focus: (focus),
                        border_color_1_down: (focus), border_color_2_down: (focus),
                        border_color_1_empty: (border), border_color_2_empty: (border),
                        border_color_1_disabled: (border), border_color_2_disabled: (border),
                    }
                    draw_text: {
                        color: (fg), color_hover: (fg), color_focus: (fg), color_down: (fg), color_disabled: (hint),
                        color_empty: (hint), color_empty_hover: (hint), color_empty_focus: (hint),
                        text_style: { font_size: (font) }
                    }
                    draw_cursor: { color: (fg) }
                    draw_selection: { color: (selection) }
                },
            );
        }
        entry
            .widget
            .draw_walk_all(cx, &mut Scope::empty(), Walk::fixed(rect.size.x, rect.size.y).with_abs_pos(rect.pos));
        if entry.focus_pending {
            entry.focus_pending = false;
            entry.widget.set_key_focus(cx);
        }
    }

    /// Forwards `event` to the fields of the topmost layer (`layer` at
    /// `depth`; pointer events never reach fields under a menu or a higher
    /// sheet) and reports edits, submits and blurs.
    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, (layer, depth): (Layer, usize)) -> FieldOutcome {
        let mut outcome = FieldOutcome::default();
        for entry in self.entries.values_mut() {
            if (entry.layer != layer || entry.depth != depth) && crate::common::is_user_input(event) {
                continue;
            }
            let widget = entry.widget.clone();
            let actions = cx.capture_actions(|cx| widget.handle_event(cx, event, &mut Scope::empty()));
            if actions.is_empty() {
                continue;
            }
            let input = widget.as_text_input();
            if input.changed(&actions).is_some() {
                outcome.changed = true;
            }
            if input.returned(&actions).is_some() {
                if entry.multiline {
                    // Enter inserts a line break in multiline fields (Flutter
                    // `keyboardType: multiline`); TextInput only does so on Shift+Enter.
                    let selection = input.selection();
                    let start = selection.start().index;
                    let end = selection.end().index;
                    let mut text = widget.text();
                    if start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) {
                        text.replace_range(start..end, "\n");
                        widget.set_text(cx, &text);
                        input.set_cursor(cx, Cursor { index: start + 1, prefer_next_row: false }, false);
                        outcome.changed = true;
                    }
                } else if let Some(handler) = &entry.on_submit {
                    outcome.run.push(handler.clone());
                }
            }
            for action in actions.filter_widget_actions_cast::<TextInputAction>(widget.widget_uid()) {
                if let TextInputAction::KeyFocusLost = action
                    && let Some(handler) = &entry.on_blur
                {
                    outcome.run.push(handler.clone());
                }
            }
        }
        outcome
    }
}
