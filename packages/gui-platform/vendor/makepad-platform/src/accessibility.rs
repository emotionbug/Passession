//! Platform-neutral window semantics. The producer submits nodes in traversal order;
//! native consumers observe the revision after drawing and own screen-reader focus.

use std::hash::{DefaultHasher, Hash, Hasher};

use crate::{Cx, Event, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AccessibilityRole {
    Button,
    TextField,
    Text,
    ListItem,
    Header,
    Checkbox,
    Switch,
    Slider,
    ScrollArea,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessibilityAction {
    Activate,
    Focus,
    ScrollForward,
    ScrollBackward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct AccessibilityActions(u8);

impl AccessibilityActions {
    pub const NONE: Self = Self(0);
    pub const ACTIVATE: Self = Self(1 << AccessibilityAction::Activate as u8);
    pub const FOCUS: Self = Self(1 << AccessibilityAction::Focus as u8);
    pub const SCROLL_FORWARD: Self = Self(1 << AccessibilityAction::ScrollForward as u8);
    pub const SCROLL_BACKWARD: Self = Self(1 << AccessibilityAction::ScrollBackward as u8);

    pub const fn contains(self, action: AccessibilityAction) -> bool {
        self.0 & (1 << action as u8) != 0
    }
}

impl std::ops::BitOr for AccessibilityActions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccessibilityState {
    pub enabled: bool,
    pub selected: bool,
    pub checked: bool,
    pub focused: bool,
}

impl Default for AccessibilityState {
    fn default() -> Self {
        Self { enabled: true, selected: false, checked: false, focused: false }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccessibilityNode<Label = String> {
    /// Nonzero identity derived from the control or model identity, not its frame position.
    pub id: u64,
    /// `None` means a child of the window. Parents must precede their children.
    pub parent: Option<u64>,
    pub role: AccessibilityRole,
    pub label: Label,
    /// Visible bounds in logical window coordinates.
    pub bounds: Rect,
    pub state: AccessibilityState,
    pub actions: AccessibilityActions,
}

#[derive(Clone, Copy, Debug)]
pub struct AccessibilityActionEvent {
    pub id: u64,
    pub action: AccessibilityAction,
}

#[derive(Default)]
pub struct AccessibilityTree {
    enabled: bool,
    revision: u64,
    hash: u64,
    nodes: Vec<AccessibilityNode>,
}

impl AccessibilityTree {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn nodes(&self) -> &[AccessibilityNode] {
        &self.nodes
    }

    pub fn node(&self, id: u64) -> Option<&AccessibilityNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    fn set_enabled(&mut self, enabled: bool) -> bool {
        if self.enabled == enabled {
            return false;
        }
        self.enabled = enabled;
        if !enabled && !self.nodes.is_empty() {
            self.nodes.clear();
            self.hash = 0;
            self.revision += 1;
        }
        true
    }

    fn update(&mut self, build: impl FnOnce(&mut AccessibilityWriter<'_>)) {
        if !self.enabled {
            return;
        }
        let mut writer = AccessibilityWriter {
            nodes: &mut self.nodes,
            len: 0,
            hash: DefaultHasher::new(),
            changed: false,
        };
        build(&mut writer);
        let hash = if writer.len > 0 { writer.hash.finish() } else { 0 };
        // Exact comparison catches even a hash collision, without copying unchanged labels.
        if hash != self.hash || writer.changed || writer.len != writer.nodes.len() {
            writer.nodes.truncate(writer.len);
            self.hash = hash;
            self.revision += 1;
        }
    }
}

/// Accepts borrowed labels and reuses the tree's storage on unchanged frames.
pub struct AccessibilityWriter<'a> {
    nodes: &'a mut Vec<AccessibilityNode>,
    len: usize,
    hash: DefaultHasher,
    changed: bool,
}

impl AccessibilityWriter<'_> {
    pub fn push(&mut self, node: AccessibilityNode<&str>) {
        let AccessibilityNode { id, parent, role, label, bounds, state, actions } = node;
        if id == 0 || bounds.size.x <= 0.0 || bounds.size.y <= 0.0 || label.is_empty() {
            return;
        }
        let edges = [bounds.pos.x, bounds.pos.y, bounds.size.x, bounds.size.y];
        if !edges.iter().all(|edge| edge.is_finite()) {
            return;
        }
        (id, parent, role, label, state, actions).hash(&mut self.hash);
        for edge in edges {
            (if edge == 0.0 { 0 } else { edge.to_bits() }).hash(&mut self.hash);
        }
        match self.nodes.get_mut(self.len) {
            Some(old) => {
                if old.id != id || old.parent != parent || old.role != role || old.bounds != bounds
                    || old.state != state || old.actions != actions || old.label != label
                {
                    self.changed = true;
                    old.id = id;
                    old.parent = parent;
                    old.role = role;
                    old.bounds = bounds;
                    old.state = state;
                    old.actions = actions;
                    if old.label != label {
                        old.label.clear();
                        old.label.push_str(label);
                    }
                }
            }
            None => {
                self.changed = true;
                self.nodes.push(AccessibilityNode {
                    id, parent, role, label: label.to_owned(), bounds, state, actions,
                });
            }
        }
        self.len += 1;
    }
}

impl Cx {
    pub fn accessibility_enabled(&self) -> bool {
        self.accessibility.enabled
    }

    pub fn set_accessibility_enabled(&mut self, enabled: bool) {
        if self.accessibility.set_enabled(enabled) {
            self.redraw_all();
        }
    }

    pub fn accessibility_tree(&self) -> &AccessibilityTree {
        &self.accessibility
    }

    pub fn update_accessibility(&mut self, build: impl FnOnce(&mut AccessibilityWriter<'_>)) {
        self.accessibility.update(build);
    }

    /// Validates against the latest published semantics before dispatching to the app.
    pub fn perform_accessibility_action(&mut self, id: u64, action: AccessibilityAction) -> bool {
        if !self.accessibility.enabled || !self.accessibility.node(id).is_some_and(|node| {
            node.state.enabled && node.actions.contains(action)
        }) {
            return false;
        }
        self.call_event_handler(&Event::AccessibilityAction(AccessibilityActionEvent { id, action }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dvec2;

    fn node(id: u64, role: AccessibilityRole, label: &str) -> AccessibilityNode<&str> {
        AccessibilityNode {
            id, parent: None, role, label,
            bounds: Rect { pos: dvec2(4.0, 3.0), size: dvec2(50.0, 30.0) },
            state: AccessibilityState::default(),
            actions: AccessibilityActions::NONE,
        }
    }

    #[test]
    fn identical_semantics_keep_revision_and_storage() {
        let mut tree = AccessibilityTree::default();
        tree.set_enabled(true);
        let build = |writer: &mut AccessibilityWriter<'_>| {
            writer.push(node(3, AccessibilityRole::Header, "Projects"));
            writer.push(AccessibilityNode {
                parent: Some(3),
                ..node(8, AccessibilityRole::ListItem, "Paseo")
            });
        };
        tree.update(build);
        let before = tree.revision();
        let label = tree.nodes()[1].label.as_ptr();
        tree.update(build);
        assert_eq!(tree.revision(), before);
        assert_eq!(tree.nodes()[1].label.as_ptr(), label);
        assert_eq!(tree.nodes()[0].role, AccessibilityRole::Header);
        assert_eq!(tree.node(8).unwrap().parent, Some(3));
        tree.update(|writer| writer.push(node(8, AccessibilityRole::ListItem, "Passession")));
        assert_eq!(tree.revision(), before + 1);
        assert_eq!(tree.nodes()[0].id, 8);
        assert_eq!(tree.nodes()[0].label, "Passession");
        assert!(tree.node(3).is_none());
    }

    #[test]
    fn disabled_semantics_do_not_run_producer_and_clear_stale_nodes() {
        let mut tree = AccessibilityTree::default();
        tree.update(|_| panic!("inactive producer ran"));
        assert_eq!(tree.revision(), 0);
        tree.set_enabled(true);
        tree.update(|writer| writer.push(node(1, AccessibilityRole::Button, "Open")));
        assert!(tree.node(1).is_some());
        tree.set_enabled(false);
        assert!(tree.node(1).is_none());
        assert_eq!(tree.revision(), 2);
        tree.update(|_| panic!("inactive producer ran"));
        assert_eq!(tree.revision(), 2);
    }

    #[test]
    fn native_action_uses_current_enabled_nodes_and_advertised_actions() {
        use std::{cell::RefCell, rc::Rc};
        let received = Rc::new(RefCell::new(Vec::new()));
        let handler = Rc::clone(&received);
        let mut cx = Cx::new(Box::new(move |_, event| {
            if let Event::AccessibilityAction(event) = event {
                handler.borrow_mut().push((event.id, event.action));
            }
        }));
        cx.set_accessibility_enabled(true);
        cx.update_accessibility(|writer| {
            writer.push(AccessibilityNode {
                actions: AccessibilityActions::ACTIVATE,
                ..node(1, AccessibilityRole::Button, "Open")
            });
            writer.push(AccessibilityNode {
                actions: AccessibilityActions::ACTIVATE,
                state: AccessibilityState { enabled: false, ..AccessibilityState::default() },
                ..node(2, AccessibilityRole::Button, "Remove")
            });
        });
        assert!(!cx.perform_accessibility_action(1, AccessibilityAction::Focus));
        assert!(!cx.perform_accessibility_action(2, AccessibilityAction::Activate));
        assert!(!cx.perform_accessibility_action(3, AccessibilityAction::Activate));
        assert!(cx.perform_accessibility_action(1, AccessibilityAction::Activate));
        cx.update_accessibility(|_| ());
        assert!(!cx.perform_accessibility_action(1, AccessibilityAction::Activate));
        assert_eq!(&*received.borrow(), &[(1, AccessibilityAction::Activate)]);
    }
}
