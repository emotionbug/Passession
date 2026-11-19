//! Semantics follow retained draw lists: a cached pane reuses its nodes, while a
//! detached or repainted list cannot leave stale controls in the window's tree.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use makepad_widgets::*;

use super::paint::{intersect, rect};

#[derive(Clone, Copy)]
enum Bounds {
    Drawn(Rect),
    Area(Area),
}

struct Entry {
    node: AccessibilityNode,
    bounds: Bounds,
}

#[derive(Default)]
struct Paint {
    epoch: u64,
    used: usize,
    entries: Vec<Entry>,
}

#[derive(Default)]
struct Registry {
    lists: HashMap<DrawListId, Paint>,
    visible: Vec<Visible>,
    seen_lists: HashSet<DrawListId>,
    seen_ids: HashSet<u64>,
    scroll_emitted: Vec<bool>,
}

type Shared = Rc<RefCell<Registry>>;

#[derive(Clone, Copy)]
struct Visible {
    list: DrawListId,
    index: usize,
    bounds: Rect,
    order: usize,
}

pub(crate) fn id(key: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish().max(1)
}

pub(crate) fn widget_id(uid: WidgetUid) -> u64 {
    id(("widget", uid.0))
}

pub(crate) fn widget_action(event: &Event, uid: WidgetUid, action: AccessibilityAction) -> bool {
    matches!(event, Event::AccessibilityAction(e) if e.action == action && e.id == widget_id(uid))
}

fn record(cx: &mut Cx2d, node: AccessibilityNode<&str>, bounds: Bounds) {
    if !cx.accessibility_enabled() || node.label.is_empty() {
        return;
    }
    let Some(list) = cx.get_current_draw_list_id() else {
        return;
    };
    let epoch = cx.draw_lists[list].redraw_id;
    let mut registry = cx.global::<Shared>().borrow_mut();
    let paint = registry.lists.entry(list).or_default();
    if paint.epoch != epoch {
        paint.epoch = epoch;
        paint.used = 0;
    }
    if let Some(entry) = paint.entries.get_mut(paint.used) {
        if entry.node.label != node.label {
            entry.node.label.clear();
            entry.node.label.push_str(node.label);
        }
        entry.node.id = node.id;
        entry.node.role = node.role;
        entry.node.actions = node.actions;
        entry.node.state = node.state;
        entry.bounds = bounds;
    } else {
        paint.entries.push(Entry {
            node: AccessibilityNode {
                label: node.label.to_owned(),
                id: node.id,
                role: node.role,
                bounds: node.bounds,
                state: node.state,
                actions: node.actions,
                parent: None,
            },
            bounds,
        });
    }
    paint.used += 1;
}

pub(crate) fn record_rect(cx: &mut Cx2d, node: AccessibilityNode<&str>) {
    let bounds = Bounds::Drawn(node.bounds);
    record(cx, node, bounds);
}

pub(crate) fn record_widget(
    cx: &mut Cx2d,
    uid: WidgetUid,
    area: Area,
    role: AccessibilityRole,
    label: &str,
    state: AccessibilityState,
    actions: AccessibilityActions,
) {
    if !cx.accessibility_enabled() {
        return;
    }
    record(
        cx,
        AccessibilityNode {
            id: widget_id(uid),
            parent: None,
            role,
            label,
            bounds: Rect::default(),
            state,
            actions,
        },
        Bounds::Area(area),
    );
}

pub(crate) fn icon_label(icon: &str) -> &'static str {
    match icon {
        "x" => "common.actions.close",
        "chevron-left" | "arrow-left" => "common.actions.back",
        "search" => "sessionNavigation.search",
        "settings" => "sidebar.actions.settings",
        "settings-2" | "filter" | "list-filter" => "accessibility.filter",
        "plus" => "workspace.tabs.actions.newTab",
        "menu" => "shell.menu.open",
        _ => "accessibility.options",
    }
}

pub(crate) fn activate_if(enabled: bool) -> AccessibilityActions {
    if enabled {
        AccessibilityActions::ACTIVATE
    } else {
        AccessibilityActions::NONE
    }
}

pub(crate) fn record_button(
    cx: &mut Cx2d,
    uid: WidgetUid,
    area: Area,
    label: &str,
    tooltip: &str,
    icon: &str,
    state: AccessibilityState,
) {
    if !cx.accessibility_enabled() {
        return;
    }
    let mut record = |label: &str| {
        record_widget(
            cx,
            uid,
            area,
            AccessibilityRole::Button,
            label,
            state,
            activate_if(state.enabled),
        )
    };
    if !label.is_empty() {
        record(label);
    } else if !tooltip.is_empty() {
        record(tooltip);
    } else {
        super::i18n::with_translation(icon_label(icon), record);
    }
}

pub(crate) fn record_field(cx: &mut Cx2d, uid: WidgetUid, area: Area, label: &str, enabled: bool) {
    let state = AccessibilityState {
        enabled,
        focused: cx.has_key_focus(area),
        ..AccessibilityState::default()
    };
    let actions = if enabled {
        AccessibilityActions::FOCUS | AccessibilityActions::ACTIVATE
    } else {
        AccessibilityActions::NONE
    };
    let mut record = |label: &str| {
        record_widget(
            cx,
            uid,
            area,
            AccessibilityRole::TextField,
            label,
            state,
            actions,
        )
    };
    if label.is_empty() {
        super::i18n::with_translation("accessibility.textField", record);
    } else {
        record(label);
    }
}

pub(crate) fn focus_requested(event: &Event, uid: WidgetUid) -> bool {
    matches!(event, Event::AccessibilityAction(e)
        if matches!(e.action, AccessibilityAction::Activate | AccessibilityAction::Focus)
            && e.id == widget_id(uid))
}

impl Registry {
    fn collect(&mut self, cx: &Cx, id: DrawListId, window: Rect) {
        let Some(list) = cx.draw_lists.checked_index(id) else {
            return;
        };
        if !self.seen_lists.insert(id) {
            return;
        }
        let viewport = if list.draw_list_has_clip {
            let clip = list.draw_list_uniforms.view_clip;
            rect(
                clip.x as f64,
                clip.y as f64,
                (clip.z - clip.x) as f64,
                (clip.w - clip.y) as f64,
            )
        } else {
            window
        };
        if let Some(paint) = self
            .lists
            .get_mut(&id)
            .filter(|p| p.epoch == list.redraw_id)
        {
            paint.entries.truncate(paint.used);
            for (index, entry) in paint.entries.iter().enumerate() {
                let bounds = match entry.bounds {
                    Bounds::Drawn(bounds) if list.draw_list_has_clip => {
                        let shift = list.draw_list_uniforms.view_shift;
                        intersect(
                            bounds.translate(dvec2(shift.x as f64, shift.y as f64)),
                            viewport,
                        )
                    }
                    Bounds::Drawn(bounds) => bounds,
                    Bounds::Area(area) if area.is_valid(cx) => area.clipped_rect(cx),
                    Bounds::Area(_) => continue,
                };
                let bounds = intersect(bounds, window);
                if bounds.size.x > 0.0 && bounds.size.y > 0.0 && self.seen_ids.insert(entry.node.id)
                {
                    self.visible.push(Visible {
                        list: id,
                        index,
                        bounds,
                        order: self.visible.len(),
                    });
                }
            }
        }
        for index in 0..list.draw_items.len() {
            if let Some(child) = list.draw_items[index].sub_list() {
                self.collect(cx, child, window);
            }
        }
    }
}

fn visual_order(a: &Visible, b: &Visible) -> std::cmp::Ordering {
    a.bounds
        .pos
        .y
        .total_cmp(&b.bounds.pos.y)
        .then_with(|| a.bounds.pos.x.total_cmp(&b.bounds.pos.x))
        .then_with(|| a.order.cmp(&b.order))
}

/// The shell chooses which draw roots are interactive (for example, only the top modal).
pub(crate) fn publish(
    cx: &mut Cx,
    window: Rect,
    roots: &[DrawListId],
    scrolls: &[AccessibilityNode<&str>],
) {
    if !cx.accessibility_enabled() {
        return;
    }
    let shared = Rc::clone(cx.global::<Shared>());
    let mut registry = shared.borrow_mut();
    registry.visible.clear();
    registry.seen_lists.clear();
    registry.seen_ids.clear();
    registry.lists.retain(|&id, paint| {
        cx.draw_lists
            .checked_index(id)
            .is_some_and(|list| list.redraw_id == paint.epoch)
    });
    for id in roots {
        registry.collect(cx, *id, window);
    }
    registry.visible.sort_unstable_by(visual_order);
    let Registry {
        visible,
        lists,
        scroll_emitted,
        ..
    } = &mut *registry;
    scroll_emitted.clear();
    scroll_emitted.resize(scrolls.len(), false);
    cx.update_accessibility(|tree| {
        for visible in visible {
            let entry = &lists[&visible.list].entries[visible.index];
            let parent = scrolls
                .iter()
                .enumerate()
                .find(|(_, node)| encloses(node.bounds, visible.bounds));
            if let Some((index, node)) = parent
                && !std::mem::replace(&mut scroll_emitted[index], true)
            {
                tree.push((*node).clone());
            }
            tree.push(AccessibilityNode {
                id: entry.node.id,
                parent: parent.map(|(_, node)| node.id),
                role: entry.node.role,
                label: entry.node.label.as_str(),
                bounds: visible.bounds,
                state: entry.node.state,
                actions: entry.node.actions,
            });
        }
        for (node, emitted) in scrolls.iter().zip(scroll_emitted) {
            if !*emitted {
                tree.push((*node).clone());
            }
        }
    });
}

fn encloses(parent: Rect, child: Rect) -> bool {
    let parent_end = parent.pos + parent.size;
    let child_end = child.pos + child.size;
    parent.pos.x <= child.pos.x
        && parent.pos.y <= child.pos.y
        && parent_end.x >= child_end.x
        && parent_end.y >= child_end.y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_reading_order_is_top_then_left_with_stable_ties() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let list = DrawList2d::new(&mut cx).id();
        let entry = |x, y, order| Visible {
            list,
            index: order,
            order,
            bounds: rect(x, y, 8.0, 8.0),
        };
        let mut entries = [
            entry(0.0, 20.0, 0),
            entry(10.0, 0.0, 1),
            entry(0.0, 0.0, 2),
            entry(0.0, 0.0, 3),
        ];
        entries.sort_unstable_by(visual_order);
        assert_eq!(entries.map(|entry| entry.index), [2, 3, 1, 0]);
    }

    #[test]
    fn retained_list_reuses_identity_and_updates_only_visible_bounds() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::makepad_draw::live_design(&mut cx);
        makepad_widgets::makepad_draw::shader::draw_trapezoid::DrawTrapezoidVector::register_main_module(
            &mut cx,
        );
        cx.live_expand();
        let mut root = DrawList2d::new(&mut cx);
        let mut list = DrawList2d::new(&mut cx);
        let pass = Pass::new(&mut cx);
        let window = rect(0.0, 0.0, 100.0, 100.0);
        let viewport = rect(0.0, 20.0, 100.0, 70.0);
        let node = |id, role, label, bounds| AccessibilityNode {
            id,
            parent: None,
            role,
            label,
            bounds,
            state: AccessibilityState::default(),
            actions: AccessibilityActions::NONE,
        };
        let scrolls = [node(3, AccessibilityRole::ScrollArea, "Sessions", viewport)];
        let mut paint = |cx: &mut Cx| {
            let event = DrawEvent::default();
            let mut draw = CxDraw::new(cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            let mut cx = Cx2d::new(&mut draw);
            root.begin_always(&mut cx);
            list.begin_always(&mut cx);
            record_rect(
                &mut cx,
                node(
                    8,
                    AccessibilityRole::ListItem,
                    "Paseo",
                    rect(0.0, 65.0, 100.0, 30.0),
                ),
            );
            record_rect(
                &mut cx,
                node(
                    4,
                    AccessibilityRole::Header,
                    "Projects",
                    rect(0.0, 25.0, 100.0, 20.0),
                ),
            );
            list.end(&mut cx);
            root.end(&mut cx);
            cx.end_pass(&pass);
        };
        paint(&mut cx);
        assert!(
            !cx.has_global::<Shared>(),
            "inactive paint should not allocate a collector"
        );
        cx.set_accessibility_enabled(true);
        paint(&mut cx);
        crate::common::layer::place_viewport(&mut cx, list.id(), viewport, dvec2(0.0, 0.0));
        publish(&mut cx, window, &[root.id()], &scrolls);
        let nodes = cx.accessibility_tree().nodes();
        assert_eq!(
            nodes.iter().map(|n| (n.id, n.parent)).collect::<Vec<_>>(),
            [(3, None), (4, Some(3)), (8, Some(3)),]
        );
        assert_eq!(nodes[1].role, AccessibilityRole::Header);
        assert_eq!(nodes[2].label, "Paseo");
        assert_eq!(nodes[2].bounds.size.y, 25.0);
        let label = nodes[2].label.as_ptr();
        let revision = cx.accessibility_tree().revision();
        publish(&mut cx, window, &[root.id()], &scrolls);
        assert_eq!(cx.accessibility_tree().revision(), revision);
        assert_eq!(
            cx.accessibility_tree().node(8).unwrap().label.as_ptr(),
            label
        );
        crate::common::layer::place_viewport(&mut cx, list.id(), viewport, dvec2(0.0, -20.0));
        publish(&mut cx, window, &[root.id()], &scrolls);
        assert_eq!(cx.accessibility_tree().revision(), revision + 1);
        assert_eq!(cx.accessibility_tree().node(8).unwrap().bounds.pos.y, 45.0);
        cx.draw_lists[root.id()].detach_sub_list(list.id());
        publish(&mut cx, window, &[root.id()], &scrolls);
        assert!(cx.accessibility_tree().node(8).is_none());
        cx.set_accessibility_enabled(false);
        publish(&mut cx, window, &[root.id()], &scrolls);
        assert!(cx.accessibility_tree().nodes().is_empty());
    }
}
