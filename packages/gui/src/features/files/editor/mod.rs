//! The native text editing surfaces of the files feature.

pub mod buffer;
pub mod field;
pub mod view;

use makepad_widgets::{Cx, Event, Timer};

/// Flutter `EditableText` caret blink: visible, then toggled every 500 ms while the caret is
/// shown; typing or moving the caret shows it solid again. The timer is armed only by a draw
/// that shows a caret, so an unfocused or hidden editor stops blinking after one tick.
#[derive(Default)]
pub struct Blink {
    timer: Timer,
    hidden: bool,
}

/// `_kCursorBlinkHalfPeriod`.
const BLINK_HALF_PERIOD: f64 = 0.5;

impl Blink {
    /// Whether the caret is in its visible phase.
    pub fn visible(&self) -> bool {
        !self.hidden
    }

    /// A draw that shows the caret keeps the blink running.
    pub fn arm(&mut self, cx: &mut Cx) {
        if self.timer.is_empty() {
            self.timer = cx.start_timeout(BLINK_HALF_PERIOD);
        }
    }

    /// No caret on screen (unfocused, read-only): stop and reset to visible.
    pub fn disarm(&mut self, cx: &mut Cx) {
        if !self.timer.is_empty() {
            cx.stop_timer(self.timer);
            self.timer = Timer::empty();
        }
        self.hidden = false;
    }

    /// Typing or a caret move: solid again, the next toggle a full half period later.
    pub fn restart(&mut self, cx: &mut Cx) {
        self.disarm(cx);
    }

    /// The blink timer fired: toggles the phase (the redraw re-arms it). Returns whether the
    /// owner must redraw.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        if self.timer.is_event(event).is_some() {
            self.timer = Timer::empty();
            self.hidden = !self.hidden;
            return true;
        }
        false
    }
}
