//! `FileDropZone` (original `components/file-drop`): the area of a pane that takes files
//! dragged in from the desktop, and the backdrop it shows while they hover.
//!
//! The zone is the whole surface, not the composer card: `file-drop-zone.tsx` wraps the
//! agent pane (`panels/agent-panel.tsx` ChatSurface), the draft tab, the New session screen
//! and the terminal output. Whoever consumes the files (a composer, the terminal) says per
//! event whether it can take them now (`useFileDrop` `disabled`: no consumer, or one that
//! is sending or uploading). A zone that cannot take them still claims the drag and answers
//! "no drop" (`dropEffect = "none"`), so the desktop shows its refusal cursor instead of
//! accepting a drop that would then do nothing.
//!
//! The owner draws the zone last, over its own content, and routes every event through
//! [`FileDropZone::handle_event`].

use std::path::PathBuf;

use makepad_widgets::*;

use crate::common::clock;
use crate::common::paint::{Font, Painter, TextSpec, rect as rect_of};
use crate::common::tr;
use crate::theme;

live_design! {
    use link::theme::*;
    use crate::common::paint::PaseoPainter;

    pub FileDropZoneBase = {{FileDropZone}} {}
    pub FileDropZone = <FileDropZoneBase> {
        painter: <PaseoPainter> {}
    }
}

/// `file-drop-backdrop.tsx` `withTiming(…, { duration: 150 })`.
const FADE_SECONDS: f64 = 0.15;

/// Where the zone is and whether accepted files hover over it
/// (`use-drop-listeners.ts` `isDragging`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ZoneState {
    rect: Rect,
    dragging: bool,
}

impl ZoneState {
    /// Routes one native drag event; returns the dropped files for the consumer.
    ///
    /// A file drag anywhere inside the zone belongs to it (the first zone to claim an event
    /// keeps it). `accepts` is the consumer's answer for this moment: without it the drag
    /// is refused and a drop is left unhandled, so the platform reports it as not taken.
    fn route(&mut self, event: &Event, accepts: bool) -> Option<Vec<PathBuf>> {
        match event {
            Event::Drag(drag) => {
                let over = self.claim(drag.abs, &drag.items, &drag.handled, true);
                if over {
                    *drag.response.lock().unwrap() = if accepts { DragResponse::Copy } else { DragResponse::None };
                }
                self.dragging = over && accepts;
                None
            }
            Event::Drop(drop) => {
                self.dragging = false;
                self.claim(drop.abs, &drop.items, &drop.handled, accepts)
                    .then(|| paseo_gui_platform::dropped_file_paths(drop))
                    .filter(|_| accepts)
            }
            Event::DragEnd => {
                self.dragging = false;
                None
            }
            _ => None,
        }
    }

    /// Whether an unclaimed file drag is over the zone; `take` marks it claimed.
    fn claim(&self, abs: DVec2, items: &[DragItem], handled: &std::sync::Mutex<bool>, take: bool) -> bool {
        let mut handled = handled.lock().unwrap();
        let over = !*handled && self.rect.contains(abs) && items.iter().any(|item| matches!(item, DragItem::FilePath { .. }));
        if over && take {
            *handled = true;
        }
        over
    }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct FileDropZone {
    #[live]
    painter: Painter,
    #[rust(DrawList2d::new(cx))]
    list: DrawList2d,
    #[rust]
    state: ZoneState,
    /// The backdrop's opacity as last drawn, and when.
    #[rust]
    shown: f64,
    #[rust]
    shown_at: f64,
    #[rust]
    next_frame: NextFrame,
}

impl FileDropZone {
    /// Routes `event`; returns the files dropped on the zone while `accepts`.
    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, accepts: bool) -> Option<Vec<PathBuf>> {
        if self.next_frame.is_event(event).is_some() {
            self.list.redraw(cx);
        }
        let was = self.state.dragging;
        let files = self.state.route(event, accepts);
        if was != self.state.dragging {
            self.shown_at = now();
            self.list.redraw(cx);
        }
        files
    }

    /// The zone left the screen (its owner is hidden or gone): no drag belongs to it.
    pub fn clear(&mut self) {
        self.state = ZoneState::default();
        self.shown = 0.0;
    }

    /// Sets the zone to `rect` and paints the backdrop over it: `surface0` at 70%, a 32px
    /// primary Upload glyph over "Drop files here" (`fontSize.base`, medium, foreground),
    /// 8px apart, centred. Call after the owner's own content.
    pub fn draw(&mut self, cx: &mut Cx2d, rect: Rect) {
        self.state.rect = rect;
        let target = if self.state.dragging { 1.0 } else { 0.0 };
        let now = now();
        let step = ((now - self.shown_at) / FADE_SECONDS).clamp(0.0, 1.0);
        self.shown_at = now;
        self.shown = if target > self.shown { (self.shown + step).min(target) } else { (self.shown - step).max(target) };
        // The list is begun every frame so a drag can redraw it before it ever showed.
        self.list.begin_always(cx);
        if self.shown > 0.0 {
            let palette = theme::current_palette();
            let typography = theme::current_typography();
            self.painter.new_layer(cx);
            self.painter.set_alpha(self.shown as f32);
            self.painter.fill(cx, rect, Vec4 { w: 0.7, ..palette.surface0 }, 0.0);
            let spec = TextSpec::new(Font::Medium, typography.ui_size(14.0), palette.foreground);
            let label = tr("composer.attachments.dropFilesHere");
            let label_height = self.painter.line_height(cx, spec);
            let top = rect.pos.y + (rect.size.y - (32.0 + 8.0 + label_height)) * 0.5;
            self.painter.icon(cx, rect_of(rect.pos.x, top, rect.size.x, 32.0), "upload", 32.0, palette.primary);
            self.painter.text_in(cx, rect_of(rect.pos.x, top + 40.0, rect.size.x, label_height), spec, &label, 0.5);
            self.painter.set_alpha(1.0);
        }
        self.list.end(cx);
        if self.shown != target {
            self.next_frame = cx.new_next_frame();
        }
    }
}

fn now() -> f64 {
    clock::now_ms() as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn file(path: &str) -> DragItem {
        DragItem::FilePath { path: path.to_owned(), internal_id: None }
    }

    fn drag(abs: DVec2, items: Vec<DragItem>) -> (Event, Arc<Mutex<DragResponse>>, Arc<Mutex<bool>>) {
        let response = Arc::new(Mutex::new(DragResponse::None));
        let handled = Arc::new(Mutex::new(false));
        let event = Event::Drag(DragEvent {
            modifiers: KeyModifiers::default(),
            handled: handled.clone(),
            abs,
            items: Arc::new(items),
            response: response.clone(),
        });
        (event, response, handled)
    }

    fn drop_at(abs: DVec2, items: Vec<DragItem>) -> (Event, Arc<Mutex<bool>>) {
        let handled = Arc::new(Mutex::new(false));
        let event = Event::Drop(DropEvent { modifiers: KeyModifiers::default(), handled: handled.clone(), abs, items: Arc::new(items) });
        (event, handled)
    }

    /// A chat pane: the conversation fills it, the composer card sits at its bottom.
    fn pane() -> ZoneState {
        ZoneState { rect: rect_of(300.0, 40.0, 900.0, 800.0), dragging: false }
    }

    #[test]
    fn files_are_accepted_anywhere_in_the_pane_and_dropped_in_order() {
        let mut zone = pane();
        // Over the conversation, far above the composer card.
        let (event, response, handled) = drag(dvec2(700.0, 120.0), vec![file("/tmp/a.txt")]);
        assert_eq!(zone.route(&event, true), None);
        assert_eq!(*response.lock().unwrap(), DragResponse::Copy);
        assert!(*handled.lock().unwrap());
        assert!(zone.dragging);

        let files = vec![file("/tmp/a.txt"), file("/home/u/한글 파일 이름.txt"), file("C:\\Users\\u\\그림 one.png")];
        let (event, handled) = drop_at(dvec2(700.0, 120.0), files);
        assert_eq!(
            zone.route(&event, true),
            Some(vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/home/u/한글 파일 이름.txt"), PathBuf::from("C:\\Users\\u\\그림 one.png")])
        );
        assert!(*handled.lock().unwrap());
        assert!(!zone.dragging);
    }

    #[test]
    fn a_busy_consumer_refuses_the_drag_instead_of_swallowing_the_drop() {
        let mut zone = pane();
        let (event, response, handled) = drag(dvec2(700.0, 700.0), vec![file("/tmp/a.txt")]);
        *response.lock().unwrap() = DragResponse::Copy;
        assert_eq!(zone.route(&event, false), None);
        // The desktop is told "no drop" and no backdrop invites one.
        assert_eq!(*response.lock().unwrap(), DragResponse::None);
        assert!(*handled.lock().unwrap());
        assert!(!zone.dragging);

        let (event, handled) = drop_at(dvec2(700.0, 700.0), vec![file("/tmp/a.txt")]);
        assert_eq!(zone.route(&event, false), None);
        assert!(!*handled.lock().unwrap());
    }

    #[test]
    fn drags_outside_the_zone_or_without_files_are_left_alone() {
        let mut zone = pane();
        // Over the sidebar, left of the pane.
        let (event, response, handled) = drag(dvec2(100.0, 400.0), vec![file("/tmp/a.txt")]);
        zone.dragging = true;
        assert_eq!(zone.route(&event, true), None);
        assert_eq!(*response.lock().unwrap(), DragResponse::None);
        assert!(!*handled.lock().unwrap());
        assert!(!zone.dragging);

        // Dragged text is not a file.
        let text = DragItem::String { value: "hello".into(), internal_id: None };
        let (event, response, handled) = drag(dvec2(700.0, 400.0), vec![text.clone()]);
        assert_eq!(zone.route(&event, true), None);
        assert_eq!(*response.lock().unwrap(), DragResponse::None);
        assert!(!*handled.lock().unwrap());
        let (event, handled) = drop_at(dvec2(700.0, 400.0), vec![text]);
        assert_eq!(zone.route(&event, true), None);
        assert!(!*handled.lock().unwrap());

        // Another zone already claimed the event (two panes side by side).
        let (event, response, handled) = drag(dvec2(700.0, 400.0), vec![file("/tmp/a.txt")]);
        *handled.lock().unwrap() = true;
        assert_eq!(zone.route(&event, true), None);
        assert_eq!(*response.lock().unwrap(), DragResponse::None);
        assert!(!zone.dragging);
    }

    #[test]
    fn the_backdrop_state_ends_with_the_drag() {
        let mut zone = pane();
        let (event, ..) = drag(dvec2(700.0, 400.0), vec![file("/tmp/a.txt")]);
        zone.route(&event, true);
        assert!(zone.dragging);
        // The pointer left the window (XdndLeave, DragLeave).
        assert_eq!(zone.route(&Event::DragEnd, true), None);
        assert!(!zone.dragging);
    }
}
