//! Compact navigation presentation. Layout owns the destination; this deck retains only the
//! screen paints that are still on their way in/out. No retiring root receives input.

use makepad_widgets::*;

use crate::common::layer::Layer;
use crate::common::overlay::{
    DrawLayerTexture, LayerTexture, layer_rect, place_layer, release_layer_quad,
};
use crate::common::transition::{Intent, Pattern, Transition, Visual};
use crate::shell::layout::{CompactDestination, Layout};
use crate::shell::{PaseoShell, chrome, panels::Panel};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Screen {
    Sidebar,
    Tab(Option<String>),
}

impl Screen {
    fn matches_tab(&self, key: &str) -> bool {
        matches!(self, Self::Tab(Some(tab)) if tab == key)
    }
}

struct Scene {
    screen: Screen,
    depth: usize,
    list: Option<DrawList2d>,
    chrome: Layer,
    /// A closed detail loses its live registration immediately, but its paint and child
    /// list handles must survive until the outgoing scene is no longer composited.
    closed_root: Option<Panel>,
    texture: Option<LayerTexture>,
    composite: Area,
    dim: Area,
    shadow: Area,
    from: Visual,
    to: Visual,
}

impl Scene {
    fn new(screen: Screen, depth: usize) -> Self {
        Self {
            screen,
            depth,
            list: None,
            chrome: Layer::default(),
            closed_root: None,
            texture: None,
            composite: Area::Empty,
            dim: Area::Empty,
            shadow: Area::Empty,
            from: Visual::default(),
            to: Visual::default(),
        }
    }

    fn pose(&self, t: f64) -> Visual {
        let a = self.from;
        let b = self.to;
        Visual {
            offset: a.offset + (b.offset - a.offset) * t,
            opacity: a.opacity + (b.opacity - a.opacity) * t,
            scale: a.scale + (b.scale - a.scale) * t,
            edge_shadow: a.edge_shadow + (b.edge_shadow - a.edge_shadow) * t,
            dim: a.dim + (b.dim - a.dim) * t,
        }
    }

    fn clear_capture(&mut self, cx: &mut Cx) {
        release_layer_quad(cx, self.composite);
        if let Some(texture) = self.texture.take() {
            texture.detach(cx);
        }
        self.composite = Area::Empty;
    }
}

#[derive(Default)]
pub(super) struct Navigation {
    scenes: Vec<Scene>,
    target: usize,
    observed: Option<(u64, CompactDestination)>,
    bounds: Rect,
    transition: Transition,
    composition: Option<DrawList2d>,
    textured: bool,
    ready: bool,
}

impl Navigation {
    pub(super) fn moving(&self) -> bool {
        self.transition.is_moving()
    }

    pub(super) fn invalidate(&self, cx: &mut Cx) {
        if let Some(scene) = self.scenes.get(self.target) {
            scene.chrome.invalidate(cx);
        }
        self.redraw(cx);
    }

    pub(super) fn owns_hit(&self, id: &str) -> bool {
        !self.moving()
            && self
                .scenes
                .get(self.target)
                .is_some_and(|scene| scene.chrome.owns_hit(id))
    }

    pub(super) fn keep_closed(&mut self, key: &str, panel: Panel) {
        if let Some(scene) = self
            .scenes
            .iter_mut()
            .find(|scene| scene.screen.matches_tab(key))
        {
            scene.closed_root = Some(panel);
        }
    }

    fn redraw(&self, cx: &mut Cx) {
        if let Some(composition) = &self.composition {
            composition.redraw(cx);
        }
    }

    pub(super) fn clear(&mut self, cx: &mut Cx) {
        self.settle(cx);
        self.scenes.clear();
        self.observed = None;
    }

    fn settle(&mut self, cx: &mut Cx) {
        self.transition.snap(1.0);
        for scene in &mut self.scenes {
            scene.clear_capture(cx);
            if let Some(list) = &scene.list {
                list.set_view_transform(cx, &Mat4::identity());
            }
        }
        if self.scenes.len() > 1 {
            let mut selected = self.scenes.swap_remove(self.target);
            selected.from = Visual::default();
            selected.to = Visual::default();
            selected.chrome.invalidate(cx);
            self.scenes.clear();
            self.scenes.push(selected);
        }
        self.target = 0;
    }

    fn sync(&mut self, cx: &mut Cx, layout: &Layout, animate: bool) {
        let observed = (layout.revision, layout.compact_destination);
        if self.observed == Some(observed) {
            return;
        }
        self.observed = Some(observed);
        let (screen, depth) = if observed.1 == CompactDestination::Sidebar {
            (Screen::Sidebar, 0)
        } else {
            let selected = layout.focused().selected.as_deref();
            (
                Screen::Tab(selected.map(str::to_owned)),
                layout.compact_depth(selected),
            )
        };
        if self
            .scenes
            .get(self.target)
            .is_some_and(|scene| scene.screen == screen)
        {
            self.scenes[self.target].depth = depth;
            return;
        }
        if !animate
            || self.scenes.get(self.target).is_none_or(|scene| {
                scene.list.is_none() || (!self.moving() && scene.depth == depth)
            })
        {
            self.clear(cx);
            self.observed = Some(observed);
            self.scenes.push(Scene::new(screen, depth));
            self.redraw(cx);
            return;
        }
        let back = depth < self.scenes[self.target].depth;
        let style = if back {
            Intent::NavigateBack
        } else {
            Intent::NavigateForward
        }
        .resolve();
        let size = self.bounds.size;
        let start = style.sample(0.0, size);
        let end = style.sample(1.0, size);
        let old_target = self.target;
        let t = self.transition.value();
        for (index, scene) in self.scenes.iter_mut().enumerate() {
            scene.from = scene.pose(t);
            scene.to = if index == old_target {
                end.outgoing
            } else {
                Visual {
                    opacity: 0.0,
                    ..scene.from
                }
            };
        }
        self.target = self
            .scenes
            .iter()
            .position(|scene| scene.screen == screen)
            .unwrap_or_else(|| {
                let mut scene = Scene::new(screen, depth);
                scene.from = start.incoming;
                // In Cupertino, Back reveals a screen beneath the departing one.
                if back && matches!(style.pattern, Pattern::CupertinoPush { .. }) {
                    self.scenes.insert(0, scene);
                    0
                } else {
                    self.scenes.push(scene);
                    self.scenes.len() - 1
                }
            });
        self.scenes[self.target].to = Visual::default();
        self.textured =
            !matches!(style.pattern, Pattern::CupertinoPush { .. }) || self.scenes.len() > 2;
        self.transition.snap(0.0);
        self.ready = false;
        self.transition.retarget(cx, 1.0, style.spec);
        self.redraw(cx);
    }

    fn place(&self, cx: &mut Cx) {
        let t = self.transition.value();
        for scene in &self.scenes {
            let pose = scene.pose(t);
            let rect = layer_rect(self.bounds, pose, dvec2(0.5, 0.5));
            if self.textured {
                DrawLayerTexture::update(cx, scene.composite, rect, pose.opacity as f32);
            } else if let Some(list) = &scene.list {
                // Geometry still starts at its native window position.
                place_layer(
                    cx,
                    list,
                    Rect {
                        pos: rect.pos - self.bounds.pos,
                        size: rect.size,
                    },
                    pose.scale,
                );
            }
            update_scrim(cx, scene.dim, rect, pose.dim);
            update_scrim(cx, scene.shadow, edge_rect(rect), pose.edge_shadow * 0.14);
        }
    }
}

fn edge_rect(rect: Rect) -> Rect {
    crate::common::paint::rect(rect.pos.x - 6.0, rect.pos.y, 6.0, rect.size.y)
}

fn draw_scrim(shell: &mut PaseoShell, cx: &mut Cx2d, rect: Rect, opacity: f64) -> Area {
    let scrim = &mut shell.motion_scrim;
    scrim.new_draw_call(cx);
    scrim.color = vec4(0.0, 0.0, 0.0, opacity as f32);
    scrim.draw_abs(cx, rect);
    scrim.area()
}

fn update_scrim(cx: &mut Cx, area: Area, rect: Rect, opacity: f64) {
    if let Some(write) =
        area.get_write_ref(cx, live_id!(rect_pos), ShaderTy::Vec2, "navigation shade")
    {
        write.buffer[..2].copy_from_slice(&[rect.pos.x as f32, rect.pos.y as f32]);
    }
    crate::common::overlay::update_scrim(cx, area, opacity as f32);
}

pub(super) fn animate(shell: &mut PaseoShell, cx: &mut Cx, event: &Event) {
    if !shell.data.compact || shell.data.page.is_some() {
        shell.data.compact_navigation.clear(cx);
        return;
    }
    let nav = &mut shell.data.compact_navigation;
    if nav.scenes.is_empty() {
        return;
    }
    nav.sync(cx, &shell.data.layout, true);
    if nav.transition.advance_when_ready(cx, event, nav.ready) {
        if nav.moving() {
            nav.place(cx);
        } else {
            nav.settle(cx);
            nav.redraw(cx);
        }
    }
}

fn paint_scene(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    bounds: Rect,
    scene: &mut Scene,
    retire_hidden: bool,
) {
    let list = scene.list.get_or_insert_with(|| DrawList2d::new(cx));
    list.begin_always(cx);
    std::mem::swap(&mut scene.chrome, &mut shell.layers.chrome);
    chrome::draw_live(shell, cx, scope, bounds, retire_hidden);
    std::mem::swap(&mut scene.chrome, &mut shell.layers.chrome);
    list.end(cx);
    scene.chrome.finish_frame(cx);
}

pub(super) fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, r: Rect) {
    let mut nav = std::mem::take(&mut shell.data.compact_navigation);
    if nav.bounds != r {
        nav.settle(cx);
        nav.bounds = r;
    }
    nav.sync(cx, &shell.data.layout, false);
    let mut composition = nav
        .composition
        .take()
        .unwrap_or_else(|| DrawList2d::new(cx));
    composition.begin_always(cx);
    if nav.moving() {
        let mut backdrop = theme_background();
        backdrop.w = 1.0;
        shell.painter.fill(cx, r, backdrop, 0.0);
        let t = nav.transition.value();
        for scene in &mut nav.scenes {
            let pose = scene.pose(t);
            let rect = layer_rect(r, pose, dvec2(0.5, 0.5));
            if scene.from.edge_shadow != 0.0 || scene.to.edge_shadow != 0.0 {
                scene.shadow = draw_scrim(shell, cx, edge_rect(rect), pose.edge_shadow * 0.14);
            }
            if nav.textured {
                if scene.texture.is_none() {
                    if scene.list.is_none() {
                        paint_scene(shell, cx, scope, r, scene, false);
                        let parent = composition.draw_list_id();
                        let source = scene.list.as_ref().unwrap().draw_list_id();
                        cx.draw_lists[parent].detach_sub_list(source);
                    }
                    // Both surfaces already have retained window-coordinate draw calls. Replay
                    // them into the offscreen target without remeasuring the widget tree.
                    let list = scene.list.as_ref().unwrap();
                    list.set_view_transform(cx, &Mat4::identity());
                    scene.texture = Some(LayerTexture::capture_bounds(cx, list.draw_list_id(), r));
                }
                let texture = scene.texture.as_ref().unwrap();
                scene.composite = shell.overlay_composite.draw_texture(
                    cx,
                    rect,
                    &texture.texture,
                    pose.opacity as f32,
                );
            } else if let Some(list) = &scene.list {
                cx.append_sub_draw_list(list);
            } else {
                paint_scene(shell, cx, scope, r, scene, false);
            }
            if scene.from.dim != 0.0 || scene.to.dim != 0.0 {
                scene.dim = draw_scrim(shell, cx, rect, pose.dim);
            }
            if let Some(list) = &scene.list {
                // The old pane may still have a cached external overlay. It is not part of either
                // snapshot and must not stay attached above the actual destination.
                cx.draw_lists[list.draw_list_id()].codeflow_parent_id = None;
            }
        }
        nav.place(cx);
        nav.ready = true;
        shell.data.hits.clear();
        shell.data.tooltips.clear();
    } else if let Some(scene) = nav.scenes.get_mut(nav.target) {
        paint_scene(shell, cx, scope, r, scene, true);
    }
    composition.end(cx);
    nav.composition = Some(composition);
    shell.data.compact_navigation = nav;
}

fn theme_background() -> Vec4 {
    crate::theme::current_palette().background
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{PanelTarget, transition};

    fn step(nav: &mut Navigation, cx: &mut Cx, time: f64) {
        let event = Event::NextFrame(NextFrameEvent {
            frame: 0,
            time,
            set: [nav.transition.scheduled_frame().unwrap()].into(),
        });
        transition::begin_event(&event);
        nav.transition.advance_when_ready(cx, &event, nav.ready);
    }

    fn mark_painted(nav: &mut Navigation, cx: &mut Cx) {
        nav.scenes[nav.target]
            .list
            .get_or_insert_with(|| DrawList2d::new(cx));
        nav.ready = true;
    }

    #[test]
    fn compact_push_pop_and_interruption_settle_to_exactly_one_screen() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut nav = Navigation {
            bounds: Rect {
                pos: DVec2::default(),
                size: dvec2(420.0, 760.0),
            },
            ..Navigation::default()
        };
        let mut layout = Layout::default();
        layout.show_sidebar(true, true);
        nav.sync(&mut cx, &layout, false);
        mark_painted(&mut nav, &mut cx);
        let agent = PanelTarget::new("agent", "srv", "ws", "/workspace").with_id("agent");
        layout.open(agent.clone(), false);
        nav.sync(&mut cx, &layout, true);
        assert!(nav.moving());
        assert_eq!(nav.scenes.len(), 2);
        step(&mut nav, &mut cx, 1.0);
        assert_eq!(nav.transition.value(), 0.0, "capture precedes clock start");
        mark_painted(&mut nav, &mut cx);
        step(&mut nav, &mut cx, 1.1);
        step(&mut nav, &mut cx, 1.18);
        let sidebar = nav.scenes[0].pose(nav.transition.value());
        assert_ne!(sidebar, Visual::default());
        layout.show_sidebar(true, true);
        nav.sync(&mut cx, &layout, true);
        assert_eq!(nav.scenes.len(), 2);
        assert_eq!(nav.scenes[nav.target].pose(0.0), sidebar);
        assert!(!nav.owns_hit("sidebar:any"));
        layout.open(agent.clone(), false);
        nav.sync(&mut cx, &layout, true);
        assert_eq!(nav.scenes.len(), 2);
        mark_painted(&mut nav, &mut cx);
        step(&mut nav, &mut cx, 1.2);
        step(&mut nav, &mut cx, 1.51);
        assert!(!nav.moving());
        nav.settle(&mut cx);
        assert_eq!(nav.scenes.len(), 1);
        assert_eq!(nav.scenes[0].screen, Screen::Tab(Some(agent.key())));
        assert!(nav.transition.scheduled_frame().is_none());

        layout.open(
            PanelTarget::new("file", "srv", "ws", "/workspace").with_id("a.rs"),
            false,
        );
        nav.sync(&mut cx, &layout, true);
        assert_eq!(nav.scenes[nav.target].depth, 2);
        assert!(nav.moving());
        nav.settle(&mut cx); // resize
        assert_eq!(nav.scenes[0].depth, 2);
        assert_eq!(nav.scenes.len(), 1);
        assert!(nav.transition.scheduled_frame().is_none());
    }
}
