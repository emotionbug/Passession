use makepad_widgets::*;

use crate::common::{accessibility, i18n::with_translation, paint::HitMap};

use super::{PaseoShell, chrome, overlays, ui::HitScope};

pub(super) fn scroll_actions(offset: f64, max: f64) -> AccessibilityActions {
    let mut actions = AccessibilityActions::NONE;
    if offset < max {
        actions = actions | AccessibilityActions::SCROLL_FORWARD;
    }
    if offset > 0.0 {
        actions = actions | AccessibilityActions::SCROLL_BACKWARD;
    }
    actions
}

pub(super) fn page_delta(action: AccessibilityAction, bounds: Rect) -> Option<f64> {
    let page = bounds.size.y * 0.8;
    match action {
        AccessibilityAction::ScrollForward => Some(page),
        AccessibilityAction::ScrollBackward => Some(-page),
        _ => None,
    }
}

pub(super) fn sidebar_id() -> u64 {
    accessibility::id("sidebar-scroll")
}

pub(super) fn history_id() -> u64 {
    accessibility::id("history-scroll")
}

pub(super) fn hit(hits: &HitMap<String>, scope: HitScope, id: u64) -> Option<String> {
    let item = &hits
        .since(0)
        .iter()
        .rev()
        .find(|hit| scope.id(&hit.item) == id)?
        .item;
    hits.rect_of(item).map(|_| item.clone())
}

pub(super) fn publish(shell: &PaseoShell, cx: &mut Cx2d, window: Rect) {
    let base = cx.get_current_draw_list_id();
    if !cx.accessibility_enabled() {
        return;
    }
    if shell.data.overlays.is_modal() {
        let root = shell.data.overlays.accessibility_root();
        let scroll = overlays::accessibility_scroll(shell)
            .filter(|(_, _, actions)| *actions != AccessibilityActions::NONE);
        with_translation("accessibility.options", |label| {
            let scroll =
                scroll.map(|(id, bounds, actions)| scroll_node(id, bounds, label, actions));
            accessibility::publish(cx, window, root.as_slice(), scroll.as_slice());
        });
    } else if shell.data.command_center_open {
        let root = shell
            .data
            .command_center
            .as_ref()
            .and_then(|widget| widget.area().draw_list_id());
        accessibility::publish(cx, window, root.as_slice(), &[]);
    } else if crate::common::overlay::window_modal_open(&shell.data.window_id) {
        let root = crate::common::overlay::accessibility_modal_root(cx, &shell.data.window_id);
        accessibility::publish(cx, window, root.as_slice(), &[]);
    } else if shell.data.compact && shell.data.chrome.explorer_visible() {
        let root = chrome::accessibility_explorer_root(shell);
        accessibility::publish(cx, window, root.as_slice(), &[]);
    } else if shell.data.compact_navigation.moving() {
        accessibility::publish(cx, window, &[], &[]);
    } else {
        let sidebar = &shell.data.sidebar;
        let actions = scroll_actions(
            sidebar.scroll,
            sidebar.content_height - sidebar.viewport.size.y,
        );
        with_translation("accessibility.sidebarSessions", |sidebar_label| {
            let sidebar = (chrome::sidebar_visible(shell) && actions != AccessibilityActions::NONE)
                .then(|| scroll_node(sidebar_id(), sidebar.viewport, sidebar_label, actions));
            with_translation("sessionNavigation.sessions", |history_label| {
                let history = shell.data.history.as_ref().and_then(|history| {
                    let (bounds, actions) = history.accessibility_scroll();
                    (actions != AccessibilityActions::NONE)
                        .then(|| scroll_node(history_id(), bounds, history_label, actions))
                });
                let scrolls: SmallVec<[_; 2]> = [sidebar, history].into_iter().flatten().collect();
                accessibility::publish(cx, window, base.as_slice(), &scrolls);
            });
        });
    }
}

fn scroll_node(
    id: u64,
    bounds: Rect,
    label: &str,
    actions: AccessibilityActions,
) -> AccessibilityNode<&str> {
    AccessibilityNode {
        id,
        parent: None,
        role: AccessibilityRole::ScrollArea,
        label,
        bounds,
        state: AccessibilityState::default(),
        actions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_actions_respect_direction_and_extent() {
        use AccessibilityAction::*;
        let at_start = scroll_actions(0.0, 20.0);
        assert!(at_start.contains(ScrollForward));
        assert!(!at_start.contains(ScrollBackward));
        let at_end = scroll_actions(20.0, 20.0);
        assert!(!at_end.contains(ScrollForward));
        assert!(at_end.contains(ScrollBackward));
        assert_eq!(scroll_actions(0.0, 0.0), AccessibilityActions::NONE);
    }

    #[test]
    fn shell_and_modal_actions_distinguish_the_same_local_id() {
        let mut hits = HitMap::default();
        hits.push(
            crate::common::paint::rect(5.0, 10.0, 42.0, 30.0),
            "close".into(),
        );
        let first_modal = HitScope::Overlay(1);
        let second_modal = HitScope::Overlay(2);
        let id = first_modal.id("close");
        assert_eq!(hit(&hits, first_modal, id).as_deref(), Some("close"));
        assert_eq!(hit(&hits, second_modal, id), None);
        assert_eq!(hit(&hits, HitScope::Shell, id), None);
    }
}
