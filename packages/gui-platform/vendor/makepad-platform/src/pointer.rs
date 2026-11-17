//! Paseo: the pointer shows the shape of what is under it.
//!
//! Widgets ask for a shape with `Cx::set_cursor`, mostly when the pointer enters them, and
//! upstream Makepad keeps whatever was asked last. Leaving a link for a region that asks
//! for nothing kept the hand, and content that changed under a still pointer kept the old
//! shape until the next move. Two rules make a missing request harmless:
//!
//! - A mouse move that hands the hover to another area (or to none) shows the default
//!   arrow unless something asked for a shape while it was dispatched. While the same area
//!   stays hovered, its shape stays, so a widget may set it on `FingerHoverIn` alone.
//! - After a button is released, a key changes the view or the view scrolls, the frames
//!   drawn next re-test the hover at the pointer's last position as if it had moved.
//!   `Cx::refresh_hover` asks for the same after a change outside the input event loop.
//!
//! While a button is held, the pressed widget owns the shape and nothing is resolved.

use {
    std::time::{Duration, Instant},
    crate::{
        cx::Cx,
        cursor::MouseCursor,
        event::{Event, KeyModifiers, MouseMoveEvent},
        makepad_live_id::{live_id, LiveId},
        makepad_math::DVec2,
        window::WindowId,
    },
};

/// How long after a release or scroll every drawn frame re-tests the hover; covers
/// closing popups and scroll animations that settle over several frames.
const REHOVER_WINDOW: Duration = Duration::from_millis(300);

#[derive(Default)]
pub(crate) struct CxPointer {
    /// `Cx::set_cursor` was called while the current pointer event was dispatched.
    pub(crate) requested: bool,
    /// The mouse's last position inside a window, until it leaves.
    last: Option<LastPointer>,
    /// Re-test the hover after the next drawn frame.
    rehover: bool,
    /// Frames that start before this instant re-test the hover too.
    rehover_until: Option<Instant>,
}

#[derive(Clone, Copy)]
struct LastPointer {
    window_id: WindowId,
    abs: DVec2,
    modifiers: KeyModifiers,
    time: f64,
}

impl Cx {
    /// Asks the frames drawn next to re-test the hover under a still pointer.
    pub fn refresh_hover(&mut self) {
        self.pointer.rehover = true;
    }

    pub(crate) fn pointer_event_starting(&mut self, event: &Event) {
        let last = match event {
            Event::MouseMove(e) => {
                self.pointer.requested = false;
                // A real move re-tests the hover itself.
                self.pointer.rehover = false;
                Some((e.window_id, e.abs, e.modifiers, e.time))
            }
            Event::MouseDown(e) => Some((e.window_id, e.abs, e.modifiers, e.time)),
            Event::MouseUp(e) => Some((e.window_id, e.abs, e.modifiers, e.time)),
            Event::Scroll(e) => Some((e.window_id, e.abs, e.modifiers, e.time)),
            Event::MouseLeave(_) => {
                self.pointer.last = None;
                return;
            }
            _ => return,
        };
        if let Some((window_id, abs, modifiers, time)) = last {
            self.pointer.last = Some(LastPointer { window_id, abs, modifiers, time });
        }
    }

    pub(crate) fn pointer_event_finished(&mut self, event: &Event) {
        match event {
            Event::MouseMove(e) if self.fingers.first_mouse_button.is_none() => {
                // `handled` holds the area that claimed this move's hover; the hover
                // area itself still names the previous one until the platform cycles it.
                let hovered = e.handled.get();
                let previous = self.fingers.find_hover_area(live_id!(mouse).into());
                if !self.pointer.requested && hovered != previous {
                    self.set_cursor(MouseCursor::Default);
                }
            }
            Event::MouseUp(_) | Event::Scroll(_) | Event::KeyDown(_) | Event::KeyUp(_) => {
                self.pointer.rehover = true;
                self.pointer.rehover_until = Some(Instant::now() + REHOVER_WINDOW);
            }
            _ => {}
        }
    }

    /// Called once a frame is drawn; `started` is when drawing began.
    pub(crate) fn rehover_after_draw(&mut self, started: Instant) {
        let due = self.pointer.rehover
            || self.pointer.rehover_until.is_some_and(|until| started < until);
        if !due || self.fingers.first_mouse_button.is_some() {
            return;
        }
        self.pointer.rehover = false;
        let Some(last) = self.pointer.last else { return };
        // A release outside the window (the press grabbed the pointer) has nothing to hover.
        let size = self.windows.is_created(last.window_id)
            .then(|| self.windows[last.window_id].window_geom.inner_size);
        if !size.is_some_and(|size| {
            last.abs.x >= 0.0 && last.abs.y >= 0.0 && last.abs.x < size.x && last.abs.y < size.y
        }) {
            return;
        }
        self.call_event_handler(&Event::MouseMove(MouseMoveEvent {
            abs: last.abs,
            window_id: last.window_id,
            modifiers: last.modifiers,
            time: last.time,
            handled: Default::default(),
        }));
        let mouse = live_id!(mouse).into();
        self.fingers.cycle_hover_area(mouse);
        self.fingers.switch_captures();
    }
}

#[cfg(test)]
mod tests {
    use {
        std::{cell::RefCell, rc::Rc},
        crate::{
            area::{Area, RectArea},
            cx_api::CxOsOp,
            draw_list::{CxRectArea, DrawList},
            event::{Hit, MouseButton, MouseUpEvent},
            makepad_math::{dvec2, Rect},
            live_traits::LiveNew,
            window::WindowHandle,
        },
        super::*,
    };

    /// The areas under the pointer, each with the shape it asks for on hover-in.
    type Scene = Rc<RefCell<Vec<(Area, Option<MouseCursor>)>>>;

    fn app() -> (Cx, Scene, WindowHandle, DrawList) {
        let scene: Scene = Default::default();
        let widgets = scene.clone();
        let mut cx = Cx::new(Box::new(move |cx, event| {
            for (area, cursor) in widgets.borrow().iter() {
                if let (Hit::FingerHoverIn(_), Some(cursor)) = (event.hits(cx, *area), cursor) {
                    cx.set_cursor(*cursor);
                }
            }
        }));
        let window = WindowHandle::new(&mut cx);
        cx.windows[window.window_id()].is_created = true;
        cx.windows[window.window_id()].window_geom.inner_size = dvec2(100.0, 100.0);
        let list = cx.draw_lists.alloc();
        (cx, scene, window, list)
    }

    /// An area covering `x..x + 10` of the window's top row.
    fn area(cx: &mut Cx, list: &DrawList, x: f64) -> Area {
        let draw_list = &mut cx.draw_lists[list.id()];
        draw_list.rect_areas.push(CxRectArea {
            rect: Rect { pos: dvec2(x, 0.0), size: dvec2(10.0, 10.0) },
            draw_clip: (dvec2(-1e6, -1e6), dvec2(1e6, 1e6)),
        });
        Area::Rect(RectArea {
            draw_list_id: list.id(),
            rect_id: draw_list.rect_areas.len() - 1,
            redraw_id: draw_list.redraw_id,
        })
    }

    /// Dispatches a move the way the platform loops do.
    fn move_to(cx: &mut Cx, window: &WindowHandle, x: f64) {
        cx.call_event_handler(&Event::MouseMove(MouseMoveEvent {
            abs: dvec2(x, 5.0),
            window_id: window.window_id(),
            modifiers: Default::default(),
            time: 0.0,
            handled: Default::default(),
        }));
        cx.fingers.cycle_hover_area(live_id!(mouse).into());
    }

    /// The shape the platform is asked to show, consuming the request.
    fn shown(cx: &mut Cx) -> Option<MouseCursor> {
        let ops = std::mem::take(&mut cx.platform_ops);
        ops.into_iter().find_map(|op| match op {
            CxOsOp::SetCursor(cursor) => Some(cursor),
            _ => None,
        })
    }

    #[test]
    fn leaving_a_link_for_a_region_that_asks_nothing_shows_the_arrow() {
        let (mut cx, scene, window, list) = app();
        let link = area(&mut cx, &list, 0.0);
        let text = area(&mut cx, &list, 20.0);
        scene.borrow_mut().extend([(link, Some(MouseCursor::Hand)), (text, None)]);
        move_to(&mut cx, &window, 5.0);
        assert_eq!(shown(&mut cx), Some(MouseCursor::Hand));
        move_to(&mut cx, &window, 25.0);
        assert_eq!(shown(&mut cx), Some(MouseCursor::Default));
    }

    #[test]
    fn leaving_a_link_for_empty_space_shows_the_arrow() {
        let (mut cx, scene, window, list) = app();
        let link = area(&mut cx, &list, 0.0);
        scene.borrow_mut().push((link, Some(MouseCursor::Hand)));
        move_to(&mut cx, &window, 5.0);
        shown(&mut cx);
        move_to(&mut cx, &window, 50.0);
        assert_eq!(shown(&mut cx), Some(MouseCursor::Default));
    }

    #[test]
    fn moving_within_a_link_keeps_the_shape_it_set_on_entry() {
        let (mut cx, scene, window, list) = app();
        let link = area(&mut cx, &list, 0.0);
        scene.borrow_mut().push((link, Some(MouseCursor::Hand)));
        move_to(&mut cx, &window, 2.0);
        shown(&mut cx);
        move_to(&mut cx, &window, 8.0);
        assert_eq!(shown(&mut cx), None);
    }

    #[test]
    fn popup_closed_by_a_click_shows_the_shape_under_the_still_pointer() {
        let (mut cx, scene, window, list) = app();
        let menu_item = area(&mut cx, &list, 0.0);
        let input_below = area(&mut cx, &list, 0.0);
        scene.borrow_mut().push((menu_item, Some(MouseCursor::Hand)));
        move_to(&mut cx, &window, 5.0);
        shown(&mut cx);

        cx.call_event_handler(&Event::MouseUp(MouseUpEvent {
            abs: dvec2(5.0, 5.0),
            button: MouseButton::PRIMARY,
            window_id: window.window_id(),
            modifiers: Default::default(),
            time: 0.0,
        }));
        cx.fingers.cycle_hover_area(live_id!(mouse).into());
        // The click closed the menu; the next frame draws the text input beneath it.
        *scene.borrow_mut() = vec![(input_below, Some(MouseCursor::Text))];
        cx.call_draw_event();
        assert_eq!(shown(&mut cx), Some(MouseCursor::Text));
    }

    #[test]
    fn escape_closing_a_popup_updates_the_still_pointer() {
        let (mut cx, scene, window, list) = app();
        let menu = area(&mut cx, &list, 0.0);
        let input = area(&mut cx, &list, 0.0);
        scene.borrow_mut().push((menu, Some(MouseCursor::Hand)));
        move_to(&mut cx, &window, 5.0);
        assert_eq!(shown(&mut cx), Some(MouseCursor::Hand));

        cx.call_window_event_handler(window.window_id(), &Event::KeyDown(crate::event::KeyEvent {
            key_code: crate::event::KeyCode::Escape,
            ..Default::default()
        }));
        *scene.borrow_mut() = vec![(input, Some(MouseCursor::Text))];
        cx.call_draw_event();
        assert_eq!(shown(&mut cx), Some(MouseCursor::Text));
    }
}
