//! `WaTerminalKeyBar`: the mobile/compact virtual key bar of the original
//! `terminal-virtual-keyboard.ts` and `terminal-pane.tsx` key buttons: Esc, Tab, sticky
//! Ctrl/Alt/Shift, arrows, Paste, the soft-keyboard toggle and Enter, in two rows, one row
//! when wide (≥660px outside compact layout) or four rows of three below 300px. Keys are
//! 34px surface1 cells with a border and a 6px radius in a surface0 band with a top border,
//! centred at most 880px wide; an active modifier takes the primary border.

use makepad_widgets::*;
use paseo_terminal::Key;

use crate::common::paint::{Font, HitMap, Painter, TextSpec, rect};
use crate::features::workspace::i18n::t;
use crate::theme;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::paint::PaseoPainter;

    pub WaTerminalKeyBar = {{WaTerminalKeyBar}} {
        width: Fill, height: Fit
        painter: <PaseoPainter> {}
    }
}

const KEY_HEIGHT: f64 = 34.0;
const GAP: f64 = 4.0;
const PADDING: f64 = 8.0;
const MAX_WIDTH: f64 = 880.0;
const SINGLE_ROW_MIN_WIDTH: f64 = 660.0;
const THREE_COLUMN_MAX_WIDTH: f64 = 300.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Key(Key),
    Modifier(Modifier),
    Paste,
    KeyboardToggle,
}

use Control::{Key as K, KeyboardToggle as Toggle, Modifier as M, Paste};

const ROWS: &[&[Control]] = &[
    &[K(Key::Escape), K(Key::Tab), M(Modifier::Ctrl), K(Key::Up), M(Modifier::Shift), Toggle],
    &[M(Modifier::Alt), Paste, K(Key::Left), K(Key::Down), K(Key::Right), K(Key::Enter)],
];

const WIDE_ROWS: &[&[Control]] = &[&[
    K(Key::Escape),
    K(Key::Tab),
    M(Modifier::Ctrl),
    M(Modifier::Alt),
    M(Modifier::Shift),
    K(Key::Left),
    K(Key::Down),
    K(Key::Up),
    K(Key::Right),
    Paste,
    Toggle,
    K(Key::Enter),
]];

const THREE_COLUMN_ROWS: &[&[Control]] = &[
    &[K(Key::Escape), K(Key::Tab), M(Modifier::Ctrl)],
    &[M(Modifier::Alt), M(Modifier::Shift), Toggle],
    &[Paste, K(Key::Up), K(Key::Enter)],
    &[K(Key::Left), K(Key::Down), K(Key::Right)],
];

/// `resolveTerminalVirtualKeyboardRows`.
pub fn rows_for(compact: bool, width: f64) -> &'static [&'static [Control]] {
    if width > 0.0 && width < THREE_COLUMN_MAX_WIDTH {
        THREE_COLUMN_ROWS
    } else if !compact && width >= SINGLE_ROW_MIN_WIDTH {
        WIDE_ROWS
    } else {
        ROWS
    }
}

fn label(control: Control) -> &'static str {
    match control {
        K(Key::Escape) => "Esc",
        K(Key::Tab) => "Tab",
        K(Key::Up) => "↑",
        K(Key::Down) => "↓",
        K(Key::Left) => "←",
        K(Key::Right) => "→",
        K(Key::Enter) => "Enter",
        K(_) => "",
        M(Modifier::Ctrl) => "Ctrl",
        M(Modifier::Alt) => "Alt",
        M(Modifier::Shift) => "Shift",
        Paste | Toggle => "",
    }
}

#[derive(Clone, Debug, DefaultNone)]
pub enum KeyBarAction {
    None,
    Pressed(Control),
}

#[derive(Live, LiveHook, Widget)]
pub struct WaTerminalKeyBar {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[rust]
    area: Area,
    #[rust]
    hits: HitMap<usize>,
    #[rust]
    controls: Vec<Control>,
    #[rust]
    hovered: Option<usize>,
    #[rust]
    pressed: Option<usize>,
    #[rust]
    pub ctrl: bool,
    #[rust]
    pub alt: bool,
    #[rust]
    pub shift: bool,
    #[rust]
    pub keyboard_visible: bool,
    #[rust]
    pub compact: bool,
}

impl WaTerminalKeyBar {
    fn active(&self, control: Control) -> bool {
        match control {
            M(Modifier::Ctrl) => self.ctrl,
            M(Modifier::Alt) => self.alt,
            M(Modifier::Shift) => self.shift,
            _ => false,
        }
    }

    /// The key under `abs`. Keys are stored relative to the bar's drawn origin: the
    /// parent can move the bar after it draws (the panel lays the output out above it),
    /// so draw-time absolute rects would miss the pointer.
    fn key_at(&self, cx: &Cx, abs: DVec2) -> Option<usize> {
        self.hits.at(abs - self.area.rect(cx).pos)
    }
}

impl Widget for WaTerminalKeyBar {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(fe) | Hit::FingerHoverOver(fe) => {
                let hovered = self.key_at(cx, fe.abs);
                if hovered != self.hovered {
                    self.hovered = hovered;
                    cx.set_cursor(if hovered.is_some() { MouseCursor::Hand } else { MouseCursor::Default });
                    self.redraw(cx);
                }
            }
            Hit::FingerHoverOut(_) => {
                self.hovered = None;
                self.redraw(cx);
            }
            Hit::FingerDown(fe) if fe.is_primary_hit() => {
                self.pressed = self.key_at(cx, fe.abs);
                self.redraw(cx);
            }
            Hit::FingerUp(fe) => {
                let pressed = self.pressed.take();
                if let Some(index) = pressed
                    && fe.is_over
                    && self.key_at(cx, fe.abs) == Some(index)
                    && (!fe.device.is_touch() || fe.was_tap())
                    && let Some(control) = self.controls.get(index).copied()
                {
                    cx.widget_action(self.widget_uid(), &scope.path, KeyBarAction::Pressed(control));
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let ty = theme::current_typography();
        let available = cx.turtle().width_left();
        let inner = (available - PADDING * 2.0).max(0.0);
        let rows = rows_for(self.compact, inner);
        let height = PADDING * 2.0 + 1.0 + rows.len() as f64 * KEY_HEIGHT + (rows.len() as f64 - 1.0).max(0.0) * GAP;
        let r = cx.walk_turtle(Walk { height: Size::Fixed(height), ..walk });
        self.area = self.painter.capture_box(cx, r, p.surface0, Vec4::default(), 0.0, 0.0);
        self.painter.hline(cx, r.pos.x, r.pos.y, r.size.x, p.border);
        let width = inner.min(MAX_WIDTH);
        let left = r.pos.x + (r.size.x - width) * 0.5;
        self.hits.clear();
        self.controls.clear();
        let spec = TextSpec::new(Font::Medium, ty.ui_size(14.0), p.muted);
        for (row_index, row) in rows.iter().enumerate() {
            let count = row.len() as f64;
            let key_width = ((width - GAP * (count - 1.0)) / count).max(0.0);
            let y = r.pos.y + 1.0 + PADDING + row_index as f64 * (KEY_HEIGHT + GAP);
            for (column, control) in row.iter().enumerate() {
                let index = self.controls.len();
                self.controls.push(*control);
                let key = rect(left + column as f64 * (key_width + GAP), y, key_width, KEY_HEIGHT);
                let active = self.active(*control);
                let lit = self.hovered == Some(index) || self.pressed == Some(index);
                let fill = if active || lit { p.surface2 } else { p.surface1 };
                let border = if active { p.primary } else { p.border };
                self.painter.outline(cx, key, fill, border, 1.0, 6.0);
                let color = if active { p.foreground } else { p.muted };
                match control {
                    Toggle => {
                        let icon = if self.keyboard_visible { "keyboard-off" } else { "keyboard" };
                        self.painter.icon(cx, key, icon, 16.0, p.muted);
                    }
                    Paste => {
                        self.painter.text_in(cx, key, TextSpec { color, ..spec }, &t("workspace.terminal.keys.paste"), 0.5);
                    }
                    _ => {
                        self.painter.text_in(cx, key, TextSpec { color, ..spec }, label(*control), 0.5);
                    }
                }
                self.hits.push(rect(key.pos.x - r.pos.x, key.pos.y - r.pos.y, key.size.x, key.size.y), index);
            }
        }
        DrawStep::done()
    }
}

impl WaTerminalKeyBarRef {
    pub fn pressed(&self, actions: &Actions) -> Option<Control> {
        let bar = self.borrow()?;
        match actions.find_widget_action(bar.widget_uid()).cast_ref() {
            KeyBarAction::Pressed(control) => Some(*control),
            KeyBarAction::None => None,
        }
    }

    /// Sticky modifier, soft-keyboard and layout state shown by the keys.
    pub fn set_state(&self, cx: &mut Cx, modifiers: paseo_terminal::Modifiers, keyboard_visible: bool, compact: bool) {
        let Some(mut bar) = self.borrow_mut() else { return };
        let next = (modifiers.control, modifiers.alt, modifiers.shift, keyboard_visible, compact);
        if (bar.ctrl, bar.alt, bar.shift, bar.keyboard_visible, bar.compact) != next {
            bar.ctrl = modifiers.control;
            bar.alt = modifiers.alt;
            bar.shift = modifiers.shift;
            bar.keyboard_visible = keyboard_visible;
            bar.compact = compact;
            bar.redraw(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_follow_the_original_breakpoints() {
        assert_eq!(rows_for(false, 700.0).len(), 1);
        assert_eq!(rows_for(true, 700.0).len(), 2);
        assert_eq!(rows_for(false, 400.0).len(), 2);
        assert_eq!(rows_for(true, 280.0).len(), 4);
        assert!(rows_for(false, 0.0).iter().all(|row| row.len() == 6));
    }
}
