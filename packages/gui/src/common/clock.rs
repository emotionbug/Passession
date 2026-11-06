//! Port of `lib/ui/indicator_clock.dart`: one wall-clock cadence for indicators that stay
//! animated while agents run (status ring, spinners, elapsed time). 10 Hz while the app is
//! focused, 4 Hz while visible behind another window, stopped while hidden. Nothing ticks
//! unless an indicator was drawn since the last tick.

use std::cell::RefCell;

use makepad_widgets::*;

pub const FOCUSED_INTERVAL: f64 = 0.1;
pub const UNFOCUSED_INTERVAL: f64 = 0.25;

#[derive(Default)]
struct Clock {
    areas: Vec<Area>,
    timer: Timer,
    running: bool,
    focused: bool,
    hidden: bool,
}

thread_local! {
    static CLOCK: RefCell<Clock> = RefCell::new(Clock { focused: true, ..Default::default() });
}

/// Call while drawing an animated indicator: its area is redrawn on the next tick.
pub fn register(cx: &mut Cx, area: Area) {
    CLOCK.with(|clock| {
        let mut clock = clock.borrow_mut();
        if !clock.areas.contains(&area) {
            clock.areas.push(area);
        }
        if !clock.running && !clock.hidden {
            let interval = if clock.focused { FOCUSED_INTERVAL } else { UNFOCUSED_INTERVAL };
            clock.timer = cx.start_timeout(interval);
            clock.running = true;
        }
    });
}

/// Seconds since the Unix epoch; every indicator reads this one clock so they stay in phase.
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Phase in `0..1` of a `period_ms` cycle.
pub fn phase(period_ms: u128) -> f64 {
    (now_ms() % period_ms) as f64 / period_ms as f64
}

/// Shell: forwards timer and lifecycle events.
pub fn handle_event(cx: &mut Cx, event: &Event) {
    match event {
        Event::AppGotFocus => CLOCK.with(|c| c.borrow_mut().focused = true),
        Event::AppLostFocus => CLOCK.with(|c| c.borrow_mut().focused = false),
        Event::Background | Event::Pause => CLOCK.with(|c| c.borrow_mut().hidden = true),
        Event::Foreground | Event::Resume => {
            let areas = CLOCK.with(|c| {
                let mut c = c.borrow_mut();
                c.hidden = false;
                std::mem::take(&mut c.areas)
            });
            for area in areas {
                cx.redraw_area(area);
            }
        }
        _ => {}
    }
    let fired = CLOCK.with(|clock| {
        let clock = clock.borrow();
        clock.running && clock.timer.is_event(event).is_some()
    });
    if fired {
        let areas = CLOCK.with(|clock| {
            let mut clock = clock.borrow_mut();
            clock.running = false;
            std::mem::take(&mut clock.areas)
        });
        for area in areas {
            cx.redraw_area(area);
        }
    }
}
