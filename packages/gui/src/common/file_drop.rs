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

use makepad_widgets::makepad_draw::makepad_platform::makepad_shader_compiler::ShaderTy;
use makepad_widgets::*;

use crate::common::clock;
use crate::common::paint::{Font, Painter, TextSpec, rect as rect_of};
use crate::common::tr;
use crate::common::transition::{Intent, Transition};
use crate::theme;

live_design! {
    use link::theme::*;
    use crate::common::paint::PaseoPainter;

    pub FileDropZoneBase = {{FileDropZone}} {}
    pub FileDropZone = <FileDropZoneBase> {
        painter: <PaseoPainter> {}
    }
}

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
                    *drag.response.lock().unwrap() = if accepts {
                        DragResponse::Copy
                    } else {
                        DragResponse::None
                    };
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
    fn claim(
        &self,
        abs: DVec2,
        items: &[DragItem],
        handled: &std::sync::Mutex<bool>,
        take: bool,
    ) -> bool {
        let mut handled = handled.lock().unwrap();
        let over = !*handled
            && self.rect.contains(abs)
            && items
                .iter()
                .any(|item| matches!(item, DragItem::FilePath { .. }));
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
    #[rust]
    visibility: Transition,
    #[rust]
    backdrop: Area,
}

impl FileDropZone {
    /// Routes `event`; returns the files dropped on the zone while `accepts`.
    pub fn handle_event(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        accepts: bool,
    ) -> Option<Vec<PathBuf>> {
        if self.visibility.advance(cx, event) && !self.backdrop.is_empty() {
            self.update_opacity(cx, self.opacity());
            if !self.visibility.is_moving() && !self.state.dragging {
                // Stop rasterizing the pane-sized backdrop. The transparent
                // text and rect are discarded on the next ordinary draw.
                if let Some(size) = self.backdrop.get_write_ref(
                    cx,
                    live_id!(rect_size),
                    ShaderTy::Vec2,
                    "drop zone",
                ) {
                    size.buffer[..2].fill(0.0);
                }
                self.backdrop = Area::Empty;
            }
        }
        let was = self.state.dragging;
        let files = self.state.route(event, accepts);
        if was != self.state.dragging {
            let target = if self.state.dragging { 1.0 } else { 0.0 };
            self.visibility
                .retarget(cx, target, self.intent().resolve().spec);
            if self.state.dragging && !self.backdrop.is_attached(cx) {
                // Paint the backdrop once when it first becomes visible.
                self.list.redraw(cx);
            }
        }
        files
    }

    fn intent(&self) -> Intent {
        if self.state.dragging {
            Intent::Appear
        } else {
            Intent::Disappear
        }
    }

    fn opacity(&self) -> f32 {
        self.intent()
            .resolve()
            .sample(self.visibility.value(), self.state.rect.size)
            .incoming
            .opacity as f32
    }

    fn update_opacity(&self, cx: &mut Cx, opacity: f32) {
        clock::write_instance(cx, self.backdrop, live_id!(opacity), opacity);
        for text in [&self.painter.text_icon, &self.painter.text_medium] {
            for area in text.draw_areas() {
                if let Some(color) =
                    area.get_write_ref(cx, live_id!(color), ShaderTy::Vec4, "drop label")
                {
                    for i in 0..color.repeat {
                        color.buffer[i * color.stride + 3] = text.color.w * opacity;
                    }
                }
            }
        }
    }

    /// Sets the zone to `rect` and paints the backdrop over it: `surface0` at 70%, a 32px
    /// primary Upload glyph over "Drop files here" (`fontSize.base`, medium, foreground),
    /// 8px apart, centred. Call after the owner's own content.
    pub fn draw(&mut self, cx: &mut Cx2d, rect: Rect) {
        self.state.rect = rect;
        // The list is registered even while hidden, so drag entry can redraw
        // it before it has any instances.
        self.list.begin_always(cx);
        self.backdrop = Area::Empty;
        if self.state.dragging || self.visibility.is_moving() {
            let palette = theme::current_palette();
            let typography = theme::current_typography();
            self.painter.new_layer(cx);
            self.painter.fill(
                cx,
                rect,
                Vec4 {
                    w: 0.7,
                    ..palette.surface0
                },
                0.0,
            );
            self.backdrop = self.painter.draw_box.area();
            let spec = TextSpec::new(Font::Medium, typography.ui_size(14.0), palette.foreground);
            let label = tr("composer.attachments.dropFilesHere");
            let label_height = self.painter.line_height(cx, spec);
            let top = rect.pos.y + (rect.size.y - (32.0 + 8.0 + label_height)) * 0.5;
            self.painter.icon(
                cx,
                rect_of(rect.pos.x, top, rect.size.x, 32.0),
                "upload",
                32.0,
                palette.primary,
            );
            self.painter.text_in(
                cx,
                rect_of(rect.pos.x, top + 40.0, rect.size.x, label_height),
                spec,
                &label,
                0.5,
            );
        }
        self.list.end(cx);
        if !self.backdrop.is_empty() {
            self.update_opacity(cx, self.opacity());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn file(path: &str) -> DragItem {
        DragItem::FilePath {
            path: path.to_owned(),
            internal_id: None,
        }
    }

    fn drag(
        abs: DVec2,
        items: Vec<DragItem>,
    ) -> (Event, Arc<Mutex<DragResponse>>, Arc<Mutex<bool>>) {
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
        let event = Event::Drop(DropEvent {
            modifiers: KeyModifiers::default(),
            handled: handled.clone(),
            abs,
            items: Arc::new(items),
        });
        (event, handled)
    }

    /// A chat pane: the conversation fills it, the composer card sits at its bottom.
    fn pane() -> ZoneState {
        ZoneState {
            rect: rect_of(300.0, 40.0, 900.0, 800.0),
            dragging: false,
        }
    }

    #[test]
    fn file_drop_motion_updates_retained_opacity_without_redrawing() {
        use makepad_widgets::makepad_draw::CxDraw;

        use crate::common::transition::{Spec, begin_event};

        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::live_design(&mut cx);
        crate::theme::live_design(&mut cx);
        crate::common::paint::live_design(&mut cx);
        super::live_design(&mut cx);
        FileDropZone::register_main_module(&mut cx);
        cx.live_expand();
        cx.live_scan_dependencies();
        cx.native_load_dependencies();
        let mut zone = FileDropZone::new_local(&mut cx);
        let bounds = pane().rect;
        let pass = Pass::new(&mut cx);
        pass.set_size(&mut cx, dvec2(1600.0, 1000.0));
        let mut root = DrawList2d::new(&mut cx);
        let draw_event = DrawEvent::default();
        let mut paint = |cx: &mut Cx, zone: &mut FileDropZone| {
            let mut draw = CxDraw::new(cx, &draw_event);
            draw.begin_pass(&pass, Some(1.0));
            root.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            cx.begin_pass_sized_turtle(Layout::default());
            zone.draw(&mut cx, bounds);
            cx.end_pass_sized_turtle();
            root.end(&mut cx);
            cx.end_pass(&pass);
        };
        paint(&mut cx, &mut zone);
        let (event, ..) = drag(bounds.center(), vec![file("/tmp/a.txt")]);
        zone.handle_event(&mut cx, &event, true);
        assert!(matches!(zone.intent(), Intent::Appear));
        paint(&mut cx, &mut zone);
        let backdrop = zone.backdrop;
        let opacity = |cx: &Cx| {
            backdrop
                .get_read_ref(cx, live_id!(opacity), ShaderTy::Float)
                .unwrap()
                .buffer[0]
        };
        let duration = |intent: Intent| match intent.resolve().spec {
            Spec::Tween(duration, _) => duration.as_secs_f64(),
            Spec::Spring { .. } => panic!("file-drop fades are timed"),
        };
        let tick = |zone: &mut FileDropZone, cx: &mut Cx, time| {
            cx.new_draw_event = DrawEvent::default();
            let event = Event::NextFrame(NextFrameEvent {
                frame: 0,
                time,
                set: [zone.visibility.scheduled_frame().unwrap()].into(),
            });
            begin_event(&event);
            zone.handle_event(cx, &event, true);
            assert!(!cx.new_draw_event.will_redraw());
        };
        let appear = duration(Intent::Appear);
        assert_eq!(opacity(&cx), 0.0);
        tick(&mut zone, &mut cx, 1.0);
        tick(&mut zone, &mut cx, 1.0 + appear * 0.5);
        assert!((0.0..1.0).contains(&opacity(&cx)) && opacity(&cx) > 0.0);
        tick(&mut zone, &mut cx, 1.0 + appear * 1.01);
        assert_eq!(opacity(&cx), 1.0);

        zone.handle_event(&mut cx, &Event::DragEnd, true);
        assert!(matches!(zone.intent(), Intent::Disappear));
        let disappear = duration(Intent::Disappear);
        tick(&mut zone, &mut cx, 2.0);
        tick(&mut zone, &mut cx, 2.0 + disappear * 0.5);
        assert!((0.0..1.0).contains(&opacity(&cx)) && opacity(&cx) > 0.0);
        tick(&mut zone, &mut cx, 2.0 + disappear * 1.01);
        assert_eq!(opacity(&cx), 0.0);
        assert!(zone.visibility.scheduled_frame().is_none());
        assert!(zone.backdrop.is_empty());
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

        let files = vec![
            file("/tmp/a.txt"),
            file("/home/u/한글 파일 이름.txt"),
            file("C:\\Users\\u\\그림 one.png"),
        ];
        let (event, handled) = drop_at(dvec2(700.0, 120.0), files);
        assert_eq!(
            zone.route(&event, true),
            Some(vec![
                PathBuf::from("/tmp/a.txt"),
                PathBuf::from("/home/u/한글 파일 이름.txt"),
                PathBuf::from("C:\\Users\\u\\그림 one.png")
            ])
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
        let text = DragItem::String {
            value: "hello".into(),
            internal_id: None,
        };
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

    #[test]
    fn admission_is_rechecked_at_drop_and_only_one_zone_consumes_it() {
        let mut zone = pane();
        let mut other = pane();
        let (event, ..) = drag(dvec2(700.0, 400.0), vec![file("/tmp/a.txt")]);
        zone.route(&event, true);
        assert!(zone.dragging);
        let (event, handled) = drop_at(dvec2(700.0, 400.0), vec![file("/tmp/a.txt")]);
        assert_eq!(zone.route(&event, false), None);
        assert!(!*handled.lock().unwrap());
        assert!(!zone.dragging);

        let (event, handled) = drop_at(dvec2(700.0, 400.0), vec![file("/tmp/b.txt")]);
        assert_eq!(
            zone.route(&event, true),
            Some(vec![PathBuf::from("/tmp/b.txt")])
        );
        assert!(*handled.lock().unwrap());
        assert_eq!(other.route(&event, true), None);
        assert_eq!(zone.route(&event, true), None);
    }
}
