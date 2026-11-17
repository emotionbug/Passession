//! State notifications can arrive synchronously while a native callback is running.
//! Keep them until it returns; requests needing an immediate answer do not belong here.

use crate::{event::WindowGeomChangeEvent, window::WindowId};
use std::collections::VecDeque;

pub(crate) enum WindowStateChange {
    Focus(WindowId, bool),
    Geometry(WindowGeomChangeEvent),
}

#[derive(Default)]
pub(crate) struct PendingWindowState {
    changes: VecDeque<WindowStateChange>,
}

impl PendingWindowState {
    pub(crate) fn focus(&mut self, window_id: WindowId, focused: bool) {
        self.changes.push_back(WindowStateChange::Focus(window_id, focused));
    }

    pub(crate) fn geometry(&mut self, change: WindowGeomChangeEvent) {
        let queued = self.changes.iter_mut().find_map(|queued| match queued {
            WindowStateChange::Geometry(queued) if queued.window_id == change.window_id => Some(queued),
            _ => None,
        });
        match queued {
            Some(queued) => queued.new_geom = change.new_geom,
            None => self.changes.push_back(WindowStateChange::Geometry(change)),
        }
    }

    pub(crate) fn pop_front(&mut self) -> Option<WindowStateChange> {
        self.changes.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{event::WindowGeom, makepad_math::dvec2};

    fn resized(window: usize, from: f64, to: f64) -> WindowGeomChangeEvent {
        let geom = |width| WindowGeom { inner_size: dvec2(width, 820.0), ..Default::default() };
        WindowGeomChangeEvent {
            window_id: WindowId(window, 0),
            old_geom: geom(from),
            new_geom: geom(to),
        }
    }

    fn next_geometry(pending: &mut PendingWindowState) -> WindowGeomChangeEvent {
        let Some(WindowStateChange::Geometry(change)) = pending.pop_front() else {
            panic!("expected pending geometry");
        };
        change
    }

    #[test]
    fn resize_during_callback_is_kept() {
        let mut pending = PendingWindowState::default();
        pending.geometry(resized(0, 1280.0, 1700.0));
        let change = next_geometry(&mut pending);
        assert_eq!(change.window_id, WindowId(0, 0));
        assert_eq!(change.old_geom.inner_size.x, 1280.0);
        assert_eq!(change.new_geom.inner_size.x, 1700.0);
        assert!(pending.pop_front().is_none());
    }

    #[test]
    fn repeated_resize_keeps_original_geometry_and_latest_scale() {
        let mut pending = PendingWindowState::default();
        pending.geometry(resized(0, 1280.0, 1700.0));
        let mut latest = resized(0, 1700.0, 1500.0);
        latest.new_geom.dpi_factor = 2.0;
        latest.new_geom.position = dvec2(1920.0, 20.0);
        pending.geometry(latest);
        let change = next_geometry(&mut pending);
        assert_eq!(change.old_geom.inner_size.x, 1280.0);
        assert_eq!(change.new_geom.inner_size.x, 1500.0);
        assert_eq!(change.new_geom.dpi_factor, 2.0);
        assert_eq!(change.new_geom.position, dvec2(1920.0, 20.0));
        assert!(pending.pop_front().is_none());
    }

    #[test]
    fn resizes_of_different_windows_are_both_kept() {
        let mut pending = PendingWindowState::default();
        pending.geometry(resized(0, 1280.0, 1700.0));
        pending.geometry(resized(1, 800.0, 900.0));
        let first = next_geometry(&mut pending);
        let second = next_geometry(&mut pending);
        assert_eq!((first.window_id, first.new_geom.inner_size.x), (WindowId(0, 0), 1700.0));
        assert_eq!((second.window_id, second.new_geom.inner_size.x), (WindowId(1, 0), 900.0));
    }

    #[test]
    fn notifications_arriving_while_draining_preserve_focus_order() {
        let mut pending = PendingWindowState::default();
        pending.focus(WindowId(0, 0), false);
        pending.geometry(resized(1, 800.0, 900.0));
        pending.focus(WindowId(1, 0), true);
        assert!(matches!(pending.pop_front(), Some(WindowStateChange::Focus(WindowId(0, 0), false))));
        pending.geometry(resized(1, 900.0, 1000.0));
        pending.focus(WindowId(1, 0), false);
        let change = next_geometry(&mut pending);
        assert_eq!((change.old_geom.inner_size.x, change.new_geom.inner_size.x), (800.0, 1000.0));
        assert!(matches!(pending.pop_front(), Some(WindowStateChange::Focus(WindowId(1, 0), true))));
        assert!(matches!(pending.pop_front(), Some(WindowStateChange::Focus(WindowId(1, 0), false))));
        assert!(pending.pop_front().is_none());
    }
}
