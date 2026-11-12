//! Port of the `AppStore` layout/tab model (`lib/core/app_store.dart`): tabs, the split tree
//! of panes, the Explorer column, sidebar/explorer geometry and the compact destination,
//! persisted as layout v2 JSON (byte-compatible with the Flutter client).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::common::{LayoutSnapshot, PaneSnapshot, PanelTarget, json as j};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactDestination {
    Agent,
    Sidebar,
    Explorer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Side by side (Flutter `Axis.horizontal`, persisted "horizontal").
    Horizontal,
    /// Stacked (persisted "vertical").
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub id: String,
    pub tabs: Vec<String>,
    pub selected: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Pane(Pane),
    Split { id: String, axis: Axis, ratio: f64, first: Box<Node>, second: Box<Node> },
}

impl Node {
    pub fn panes(&self) -> Vec<&Pane> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Pane>) {
        match self {
            Node::Pane(pane) => out.push(pane),
            Node::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    fn panes_mut(&mut self) -> Vec<&mut Pane> {
        let mut out = Vec::new();
        self.collect_mut(&mut out);
        out
    }

    fn collect_mut<'a>(&'a mut self, out: &mut Vec<&'a mut Pane>) {
        match self {
            Node::Pane(pane) => out.push(pane),
            Node::Split { first, second, .. } => {
                first.collect_mut(out);
                second.collect_mut(out);
            }
        }
    }

    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.panes().into_iter().find(|pane| pane.id == id)
    }

    fn pane_mut(&mut self, id: &str) -> Option<&mut Pane> {
        self.panes_mut().into_iter().find(|pane| pane.id == id)
    }

    pub fn contains_pane(&self, id: &str) -> bool {
        self.pane(id).is_some()
    }

    fn first_pane_id(&self) -> String {
        self.panes().first().map(|p| p.id.clone()).unwrap_or_else(|| MAIN_PANE.to_string())
    }

    fn last_pane_id(&self) -> String {
        self.panes().last().map(|p| p.id.clone()).unwrap_or_else(|| MAIN_PANE.to_string())
    }

    pub fn to_json(&self) -> Value {
        match self {
            Node::Pane(pane) => json!({
                "kind": "pane",
                "id": pane.id,
                "tabs": pane.tabs,
                "selected": pane.selected,
            }),
            Node::Split { id, axis, ratio, first, second } => json!({
                "kind": "split",
                "id": id,
                "axis": match axis { Axis::Horizontal => "horizontal", Axis::Vertical => "vertical" },
                "ratio": ratio,
                "first": first.to_json(),
                "second": second.to_json(),
            }),
        }
    }
}

pub const MAIN_PANE: &str = "main";
pub const DEFAULT_SIDEBAR_WIDTH: f64 = 320.0;
pub const DEFAULT_EXPLORER_WIDTH: f64 = 340.0;

/// Removed surfaces (browser, executable plugin) that are dropped when a saved layout is restored.
const REMOVED_KINDS: [&str; 2] = ["browser", "plugin"];

/// Tabs shown on top of the tab they were opened from (the original's compact-sheet and child
/// targets): compact Back closes them, and closing one returns to its opener.
pub fn is_detail_kind(kind: &str) -> bool {
    matches!(kind, "file" | "working_diff" | "commit_diff" | "provider_subagent" | "pull_request" | "execution_log" | "project_settings")
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub tabs: Vec<PanelTarget>,
    pub tree: Node,
    pub focused_pane: String,
    pub maximized_pane: Option<String>,
    pub explorer_tabs: Vec<String>,
    pub selected_explorer: Option<String>,
    /// Per-workspace Explorer focus (`_explorerFocus`), keyed by `[serverId, workspaceId]`.
    explorer_focus: HashMap<(String, String), String>,
    pub explorer_visible: bool,
    pub explorer_width: f64,
    pub sidebar_width: f64,
    pub sidebar_visible: bool,
    pub compact_destination: CompactDestination,
    /// Bumped on every open/select (`navigationSerial`): pages shown in place of the
    /// conversation close when the user moves on.
    pub navigation_serial: u64,
    /// Detail tab key → the tab it was opened from (`parentTabIdByTabId`); never cyclic.
    openers: HashMap<String, String>,
    /// Changes since the last save request.
    pub dirty: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            tree: Node::Pane(Pane { id: MAIN_PANE.to_string(), tabs: Vec::new(), selected: None }),
            focused_pane: MAIN_PANE.to_string(),
            maximized_pane: None,
            explorer_tabs: Vec::new(),
            selected_explorer: None,
            explorer_focus: HashMap::new(),
            explorer_visible: false,
            explorer_width: DEFAULT_EXPLORER_WIDTH,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_visible: true,
            compact_destination: CompactDestination::Agent,
            navigation_serial: 0,
            openers: HashMap::new(),
            dirty: false,
        }
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl Layout {
    pub fn target(&self, key: &str) -> Option<&PanelTarget> {
        self.tabs.iter().find(|tab| tab.key() == key)
    }

    pub fn panes(&self) -> Vec<&Pane> {
        self.tree.panes()
    }

    pub fn focused(&self) -> &Pane {
        let panes = self.tree.panes();
        panes.iter().find(|pane| pane.id == self.focused_pane).copied().unwrap_or(panes[0])
    }

    fn focused_id(&self) -> String {
        self.focused().id.clone()
    }

    /// The selected tab of the focused pane (`store.selected`).
    pub fn selected(&self) -> Option<&PanelTarget> {
        self.focused().selected.as_deref().and_then(|key| self.target(key))
    }

    pub fn selected_explorer_target(&self) -> Option<&PanelTarget> {
        self.selected_explorer.as_deref().and_then(|key| self.target(key))
    }

    pub fn explorer_targets(&self) -> Vec<&PanelTarget> {
        self.explorer_tabs.iter().filter_map(|key| self.target(key)).collect()
    }

    fn explorer_workspace(target: &PanelTarget) -> (String, String) {
        (target.server_id.clone(), target.workspace_id.clone())
    }

    /// `explorerFocusFor`: the view a workspace's Explorer last focused, while still open.
    pub fn explorer_focus_for(&self, workspace: &PanelTarget) -> Option<String> {
        self.explorer_focus
            .get(&Self::explorer_workspace(workspace))
            .filter(|key| self.explorer_tabs.contains(key))
            .cloned()
    }

    fn focus_explorer(&mut self, key: &str) {
        self.selected_explorer = Some(key.to_string());
        if let Some(target) = self.target(key) {
            let workspace = Self::explorer_workspace(target);
            self.explorer_focus.insert(workspace, key.to_string());
        }
    }

    fn changed(&mut self) {
        self.dirty = true;
    }

    fn upsert(&mut self, target: PanelTarget) {
        let key = target.key();
        match self.tabs.iter_mut().find(|tab| tab.key() == key) {
            Some(existing) => *existing = target,
            None => self.tabs.push(target),
        }
    }

    /// `open(target, beside:)`.
    pub fn open(&mut self, target: PanelTarget, beside: bool) {
        self.navigation_serial += 1;
        let key = target.key();
        let opener = self.focused().selected.clone().filter(|opener| is_detail_kind(&target.kind) && *opener != key);
        self.upsert(target);
        if let Some(opener) = opener {
            self.set_opener(&key, opener);
        }
        let found = self.tree.panes().iter().find(|pane| pane.tabs.contains(&key)).map(|pane| pane.id.clone());
        if let Some(pane_id) = found {
            if let Some(pane) = self.tree.pane_mut(&pane_id) {
                pane.selected = Some(key);
            }
            self.focused_pane = pane_id;
            self.compact_destination = CompactDestination::Agent;
            self.changed();
            return;
        }
        self.explorer_tabs.retain(|k| *k != key);
        if beside {
            let focused = self.focused_id();
            self.split_pane(&focused, Axis::Horizontal);
        }
        let focused = self.focused_id();
        if let Some(pane) = self.tree.pane_mut(&focused) {
            pane.tabs.push(key.clone());
            pane.selected = Some(key);
        }
        self.compact_destination = CompactDestination::Agent;
        self.changed();
    }

    /// `attachParentTab`: `key` was opened from `opener`. Reopening a tab from one opened out of
    /// it moves it to the end of that chain, so closing still unwinds to the first opener.
    fn set_opener(&mut self, key: &str, opener: String) {
        let mut at = opener.as_str();
        let mut below = None;
        for _ in 0..self.openers.len() {
            let Some(above) = self.openers.get(at) else { break };
            if above == key {
                below = Some(at.to_string());
                break;
            }
            at = above;
        }
        if let Some(below) = below {
            match self.openers.get(key).cloned() {
                Some(origin) => self.openers.insert(below, origin),
                None => self.openers.remove(&below),
            };
        }
        self.openers.insert(key.to_string(), opener);
    }

    /// `openExplorer(target, focus:)`.
    pub fn open_explorer(&mut self, target: PanelTarget, focus: bool) {
        let key = target.key();
        self.upsert(target);
        for pane in self.tree.panes_mut() {
            remove_from_pane(pane, &key);
        }
        if !self.explorer_tabs.contains(&key) {
            self.explorer_tabs.push(key.clone());
        }
        self.focus_explorer(&key);
        self.explorer_visible = true;
        if focus {
            self.compact_destination = CompactDestination::Explorer;
        }
        self.changed();
    }

    pub fn select_explorer_tab(&mut self, key: &str) {
        if !self.explorer_tabs.iter().any(|k| k == key) {
            return;
        }
        self.focus_explorer(key);
        self.explorer_visible = true;
        self.compact_destination = CompactDestination::Explorer;
        self.changed();
    }

    pub fn show_explorer(&mut self, visible: bool) {
        self.explorer_visible = visible;
        self.compact_destination = if visible { CompactDestination::Explorer } else { CompactDestination::Agent };
        self.changed();
    }

    /// Compact overlay only; the wide column's visibility is untouched (not persisted).
    pub fn show_compact_explorer(&mut self, visible: bool) {
        self.compact_destination = if visible { CompactDestination::Explorer } else { CompactDestination::Agent };
    }

    pub fn set_explorer_width(&mut self, width: f64) {
        self.explorer_width = width.clamp(180.0, 1000.0);
        self.changed();
    }

    pub fn set_sidebar_width(&mut self, width: f64) {
        self.sidebar_width = width.clamp(200.0, 600.0);
        self.changed();
    }

    pub fn show_sidebar(&mut self, visible: bool) {
        self.sidebar_visible = visible;
        self.compact_destination = if visible { CompactDestination::Sidebar } else { CompactDestination::Agent };
        self.changed();
    }

    /// `handleBack(compact:)`: explorer → agent → sidebar; false when nothing is left.
    pub fn handle_back(&mut self, compact: bool) -> bool {
        if !compact {
            return false;
        }
        match self.compact_destination {
            CompactDestination::Explorer => {
                self.compact_destination = CompactDestination::Agent;
                true
            }
            CompactDestination::Agent => {
                self.compact_destination = CompactDestination::Sidebar;
                true
            }
            CompactDestination::Sidebar => false,
        }
    }

    /// The detail tab compact Back closes before `handle_back` leaves the conversation.
    pub fn compact_back_tab(&self, compact: bool) -> Option<String> {
        if !compact || self.compact_destination != CompactDestination::Agent {
            return None;
        }
        self.selected().filter(|tab| is_detail_kind(&tab.kind)).map(PanelTarget::key)
    }

    pub fn select_tab(&mut self, key: &str) -> bool {
        self.navigation_serial += 1;
        let found = self.tree.panes().iter().find(|pane| pane.tabs.iter().any(|k| k == key)).map(|p| p.id.clone());
        if let Some(pane_id) = found {
            if let Some(pane) = self.tree.pane_mut(&pane_id) {
                pane.selected = Some(key.to_string());
            }
            self.focused_pane = pane_id;
            self.compact_destination = CompactDestination::Agent;
            self.changed();
            return true;
        }
        if self.explorer_tabs.iter().any(|k| k == key) {
            self.select_explorer_tab(key);
        }
        false
    }

    pub fn focus_pane(&mut self, id: &str) {
        if self.focused_pane == id || !self.tree.contains_pane(id) {
            return;
        }
        self.focused_pane = id.to_string();
        self.changed();
    }

    /// Unit-square rects of every pane (`adjacentPane` geometry).
    pub fn pane_rects(&self) -> Vec<(String, [f64; 4])> {
        fn locate(node: &Node, r: [f64; 4], out: &mut Vec<(String, [f64; 4])>) {
            match node {
                Node::Pane(pane) => out.push((pane.id.clone(), r)),
                Node::Split { axis, ratio, first, second, .. } => {
                    let [l, t, rr, b] = r;
                    match axis {
                        Axis::Horizontal => {
                            let boundary = l + (rr - l) * ratio;
                            locate(first, [l, t, boundary, b], out);
                            locate(second, [boundary, t, rr, b], out);
                        }
                        Axis::Vertical => {
                            let boundary = t + (b - t) * ratio;
                            locate(first, [l, t, rr, boundary], out);
                            locate(second, [l, boundary, rr, b], out);
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        locate(&self.tree, [0.0, 0.0, 1.0, 1.0], &mut out);
        out
    }

    pub fn adjacent_pane(&self, direction: Direction) -> Option<String> {
        let rects = self.pane_rects();
        let focused = self.focused_id();
        let current = rects.iter().find(|(id, _)| *id == focused)?.1;
        let center = |r: [f64; 4]| ((r[0] + r[2]) * 0.5, (r[1] + r[3]) * 0.5);
        let (cx, cy) = center(current);
        let mut nearest = None;
        let mut distance = f64::INFINITY;
        for (id, r) in &rects {
            if *id == focused {
                continue;
            }
            let (x, y) = center(*r);
            let d = (x - cx).powi(2) + (y - cy).powi(2);
            let eligible = match direction {
                Direction::Left => r[2] <= current[0] + 0.0001,
                Direction::Right => r[0] >= current[2] - 0.0001,
                Direction::Up => r[3] <= current[1] + 0.0001,
                Direction::Down => r[1] >= current[3] - 0.0001,
            };
            if eligible && d < distance {
                nearest = Some(id.clone());
                distance = d;
            }
        }
        nearest
    }

    pub fn focus_adjacent_pane(&mut self, direction: Direction) {
        if let Some(id) = self.adjacent_pane(direction) {
            self.focus_pane(&id);
        }
    }

    pub fn move_focused_tab(&mut self, direction: Direction) {
        let key = self.focused().selected.clone();
        if let (Some(key), Some(id)) = (key, self.adjacent_pane(direction)) {
            self.move_tab(&key, &id, None);
        }
    }

    /// `splitPane`: returns the new pane id.
    pub fn split_pane(&mut self, pane_id: &str, axis: Axis) -> Option<String> {
        let pane = self.tree.pane(pane_id)?.clone();
        let next = Pane { id: new_id(), tabs: Vec::new(), selected: None };
        let next_id = next.id.clone();
        let split = Node::Split {
            id: new_id(),
            axis,
            ratio: 0.5,
            first: Box::new(Node::Pane(pane)),
            second: Box::new(Node::Pane(next)),
        };
        let tree = std::mem::replace(&mut self.tree, Node::Pane(Pane { id: String::new(), tabs: Vec::new(), selected: None }));
        self.tree = replace_node(tree, pane_id, split);
        self.focused_pane = next_id.clone();
        self.maximized_pane = None;
        self.changed();
        Some(next_id)
    }

    pub fn resize_split(&mut self, id: &str, value: f64) {
        fn visit(node: &mut Node, id: &str, value: f64) {
            if let Node::Split { id: split_id, ratio, first, second, .. } = node {
                if split_id == id {
                    *ratio = value.clamp(0.1, 0.9);
                }
                visit(first, id, value);
                visit(second, id, value);
            }
        }
        visit(&mut self.tree, id, value);
        self.changed();
    }

    pub fn maximize_pane(&mut self, id: Option<String>) {
        self.maximized_pane = id;
        self.changed();
    }

    /// `moveTab`: a source pane the move leaves empty is removed.
    pub fn move_tab(&mut self, key: &str, pane_id: &str, index: Option<usize>) {
        if self.target(key).is_none() || !self.tree.contains_pane(pane_id) {
            return;
        }
        let sources: Vec<String> = self
            .tree
            .panes()
            .iter()
            .filter(|pane| pane.id != pane_id && pane.tabs.iter().any(|k| k == key))
            .map(|pane| pane.id.clone())
            .collect();
        for pane in self.tree.panes_mut() {
            remove_from_pane(pane, key);
        }
        self.explorer_tabs.retain(|k| k != key);
        if let Some(target) = self.tree.pane_mut(pane_id) {
            let at = index.unwrap_or(target.tabs.len()).min(target.tabs.len());
            target.tabs.insert(at, key.to_string());
            target.selected = Some(key.to_string());
        }
        for source in sources {
            if self.tree.pane(&source).is_some_and(|p| p.tabs.is_empty()) {
                self.remove_pane(&source);
            }
        }
        self.focused_pane = pane_id.to_string();
        self.changed();
    }

    /// `UiEffect::ReplaceTab`: `target` takes `key`'s position (pane slot or explorer slot)
    /// and selection; the old tab leaves the layout.
    pub fn replace_tab(&mut self, key: &str, target: PanelTarget) {
        let next = target.key();
        if next == key {
            self.upsert(target);
            self.changed();
            return;
        }
        self.upsert(target);
        for pane in self.tree.panes_mut() {
            pane.tabs.retain(|k| *k != next);
            if let Some(slot) = pane.tabs.iter_mut().find(|k| *k == key) {
                *slot = next.clone();
            }
            if pane.selected.as_deref() == Some(key) {
                pane.selected = Some(next.clone());
            }
        }
        if self.explorer_tabs.iter().any(|k| k == key) {
            self.explorer_tabs.retain(|k| *k != next);
            for slot in self.explorer_tabs.iter_mut().filter(|k| *k == key) {
                *slot = next.clone();
            }
            if self.selected_explorer.as_deref() == Some(key) {
                self.focus_explorer(&next);
            }
        }
        self.tabs.retain(|tab| tab.key() != key);
        // `transferReplacedTabParent`.
        let opener = self.openers.remove(key).filter(|opener| *opener != next);
        let opened: Vec<String> = self.openers.iter().filter(|(tab, from)| *from == key && **tab != next).map(|(tab, _)| tab.clone()).collect();
        self.openers.retain(|_, from| from != key);
        if let Some(opener) = opener {
            self.set_opener(&next, opener);
        }
        for tab in opened {
            self.set_opener(&tab, next.clone());
        }
        self.navigation_serial += 1;
        self.changed();
    }

    pub fn reorder_tab(&mut self, pane_id: &str, old_index: usize, new_index: usize) {
        let Some(pane) = self.tree.pane_mut(pane_id) else { return };
        if old_index >= pane.tabs.len() {
            return;
        }
        let key = pane.tabs.remove(old_index);
        let at = new_index.min(pane.tabs.len());
        pane.tabs.insert(at, key);
        self.changed();
    }

    /// `closePane` once its guards passed: removes the pane's tabs and the pane.
    pub fn close_pane(&mut self, pane_id: &str) -> Vec<String> {
        let Some(pane) = self.tree.pane(pane_id) else { return Vec::new() };
        let keys = pane.tabs.clone();
        for key in &keys {
            self.remove_tab(key);
        }
        self.remove_pane(pane_id);
        self.changed();
        keys
    }

    /// `closeTab` once its guard passed.
    pub fn close_tab(&mut self, key: &str) {
        self.remove_tab(key);
        self.changed();
    }

    /// `_removePane`: unless it is the last pane. Focus on it moves to the nearest sibling.
    fn remove_pane(&mut self, pane_id: &str) {
        if self.tree.panes().len() <= 1 || !self.tree.contains_pane(pane_id) {
            return;
        }
        fn remove(node: Node, pane_id: &str, fallback: &mut Option<String>) -> Node {
            match node {
                Node::Pane(_) => node,
                Node::Split { id, axis, ratio, first, second } => {
                    if matches!(&*first, Node::Pane(p) if p.id == pane_id) {
                        *fallback = Some(second.first_pane_id());
                        return *second;
                    }
                    if matches!(&*second, Node::Pane(p) if p.id == pane_id) {
                        *fallback = Some(first.last_pane_id());
                        return *first;
                    }
                    Node::Split {
                        id,
                        axis,
                        ratio,
                        first: Box::new(remove(*first, pane_id, fallback)),
                        second: Box::new(remove(*second, pane_id, fallback)),
                    }
                }
            }
        }
        let mut fallback = None;
        let tree = std::mem::replace(&mut self.tree, Node::Pane(Pane { id: String::new(), tabs: Vec::new(), selected: None }));
        self.tree = remove(tree, pane_id, &mut fallback);
        if self.focused_pane == pane_id {
            self.focused_pane = fallback.unwrap_or_else(|| self.tree.first_pane_id());
        }
        if self.maximized_pane.as_deref() == Some(pane_id) {
            self.maximized_pane = None;
        }
    }

    /// `_removeTab`: an emptied pane goes away except `main` and the last pane. A pane that was
    /// showing `key` returns to the tab it was opened from (`getCloseSuccessorTabId`).
    fn remove_tab(&mut self, key: &str) {
        let shown = self.tree.panes().iter().any(|pane| pane.selected.as_deref() == Some(key));
        let opener = self.openers.remove(key);
        // Tabs opened from `key` now return to where `key` came from.
        self.openers.retain(|_, parent| match &opener {
            _ if parent != key => true,
            Some(above) => {
                parent.clone_from(above);
                true
            }
            None => false,
        });
        let containing: Vec<String> = self
            .tree
            .panes()
            .iter()
            .filter(|pane| pane.tabs.iter().any(|k| k == key))
            .map(|pane| pane.id.clone())
            .collect();
        for pane_id in containing {
            let empty = match self.tree.pane_mut(&pane_id) {
                Some(pane) => {
                    remove_from_pane(pane, key);
                    pane.tabs.is_empty()
                }
                None => false,
            };
            if empty && pane_id != MAIN_PANE {
                self.remove_pane(&pane_id);
            }
        }
        self.explorer_tabs.retain(|k| k != key);
        if self.selected_explorer.as_deref() == Some(key) {
            self.selected_explorer = self.explorer_tabs.last().cloned();
        }
        self.tabs.retain(|tab| tab.key() != key);
        if shown
            && let Some(opener) = opener
            && let Some(pane_id) = self.pane_of(&opener)
        {
            if let Some(pane) = self.tree.pane_mut(&pane_id) {
                pane.selected = Some(opener);
            }
            self.focused_pane = pane_id;
        }
    }

    /// `_remapTargets`: host identity changed or hosts merged.
    pub fn remap_server(&mut self, old_id: &str, new_id: &str) {
        let mut keys: HashMap<String, String> = HashMap::new();
        for tab in self.tabs.iter_mut() {
            if tab.server_id != old_id {
                continue;
            }
            let replacement = tab.copy_with_server(new_id);
            keys.insert(tab.key(), replacement.key());
            *tab = replacement;
        }
        if keys.is_empty() {
            return;
        }
        let map = |key: &mut String| {
            if let Some(next) = keys.get(key) {
                *key = next.clone();
            }
        };
        for pane in self.tree.panes_mut() {
            pane.tabs.iter_mut().for_each(map);
            if let Some(selected) = pane.selected.as_mut() {
                map(selected);
            }
        }
        self.explorer_tabs.iter_mut().for_each(map);
        if let Some(selected) = self.selected_explorer.as_mut() {
            map(selected);
        }
        let focus: Vec<((String, String), String)> = self.explorer_focus.drain().collect();
        for ((server, workspace), mut key) in focus {
            map(&mut key);
            let server = if server == old_id { new_id.to_string() } else { server };
            self.explorer_focus.insert((server, workspace), key);
        }
        let openers: Vec<(String, String)> = self.openers.drain().collect();
        for (mut key, mut opener) in openers {
            map(&mut key);
            map(&mut opener);
            if key != opener {
                self.set_opener(&key, opener);
            }
        }
        self.changed();
    }

    /// `_restoreLayout`.
    pub fn restore(saved: &Value) -> Layout {
        let mut layout = Layout::default();
        for raw in j::array(saved, "tabs") {
            let target = PanelTarget::from_json(raw);
            if REMOVED_KINDS.contains(&target.kind.as_str()) {
                continue;
            }
            let key = target.key();
            if !layout.tabs.iter().any(|t| t.key() == key) {
                layout.tabs.push(target);
            }
        }
        let mut seen: Vec<String> = Vec::new();
        fn restore_node(node: &Value, layout: &Layout, seen: &mut Vec<String>) -> Node {
            if j::text(node, "kind") == "split" {
                let ratio = j::number(node, "ratio").map(|r| r.clamp(0.1, 0.9)).unwrap_or(0.5);
                let id = j::text(node, "id");
                return Node::Split {
                    id: if id.is_empty() { new_id() } else { id.to_string() },
                    axis: if j::text(node, "axis") == "vertical" { Axis::Vertical } else { Axis::Horizontal },
                    ratio,
                    first: Box::new(restore_node(j::object(node, "first"), layout, seen)),
                    second: Box::new(restore_node(j::object(node, "second"), layout, seen)),
                };
            }
            let mut keys = Vec::new();
            for key in j::array(node, "tabs").iter().filter_map(Value::as_str) {
                if layout.target(key).is_some() && !seen.iter().any(|s| s == key) {
                    seen.push(key.to_string());
                    keys.push(key.to_string());
                }
            }
            let selected = j::text(node, "selected");
            let selected = if keys.iter().any(|k| k == selected) { Some(selected.to_string()) } else { keys.first().cloned() };
            let id = j::text(node, "id");
            Node::Pane(Pane { id: if id.is_empty() { MAIN_PANE.to_string() } else { id.to_string() }, tabs: keys, selected })
        }
        layout.tree = restore_node(j::object(saved, "tree"), &layout, &mut seen);
        let focused = j::text(saved, "focusedPane");
        layout.focused_pane = if focused.is_empty() { layout.tree.first_pane_id() } else { focused.to_string() };
        for key in j::array(saved, "explorerTabs").iter().filter_map(Value::as_str) {
            if layout.target(key).is_some() && !seen.iter().any(|s| s == key) {
                seen.push(key.to_string());
                layout.explorer_tabs.push(key.to_string());
            }
        }
        let selected_explorer = saved.get("selectedExplorer").and_then(Value::as_str).map(str::to_string);
        layout.selected_explorer = match selected_explorer {
            Some(key) if layout.explorer_tabs.contains(&key) => Some(key),
            _ => layout.explorer_tabs.first().cloned(),
        };
        if let Some(key) = layout.selected_explorer.clone() {
            layout.focus_explorer(&key);
        }
        let unplaced: Vec<String> = layout.tabs.iter().map(PanelTarget::key).filter(|key| !seen.contains(key)).collect();
        let focused_id = layout.focused_id();
        if let Some(pane) = layout.tree.pane_mut(&focused_id) {
            pane.tabs.extend(unplaced);
            if pane.selected.is_none() {
                pane.selected = pane.tabs.first().cloned();
            }
        }
        if let Some(openers) = j::object(saved, "tabOpeners").as_object() {
            for (key, opener) in openers {
                if let Some(opener) = opener.as_str()
                    && key != opener
                    && layout.target(key).is_some()
                    && layout.target(opener).is_some()
                {
                    layout.set_opener(key, opener.to_string());
                }
            }
        }
        layout.explorer_visible = j::boolean(saved, "explorerVisible", false);
        if let Some(width) = j::number(saved, "explorerWidth") {
            layout.explorer_width = width.clamp(180.0, 1000.0);
        }
        if let Some(width) = j::number(saved, "sidebarWidth") {
            layout.sidebar_width = width.clamp(200.0, 600.0);
        }
        layout.sidebar_visible = saved.get("sidebarVisible") != Some(&Value::Bool(false));
        // Compact starts at the active agent, never a persisted side destination.
        layout.compact_destination = CompactDestination::Agent;
        layout
    }

    /// Layout v2 JSON (`savePreferences` `layout`); `tabOpeners` only while a detail tab is open.
    pub fn to_json(&self) -> Value {
        let mut saved = json!({
            "version": 2,
            "tabs": self.tabs.iter().map(PanelTarget::to_json).collect::<Vec<_>>(),
            "tree": self.tree.to_json(),
            "focusedPane": self.focused_pane,
            "explorerTabs": self.explorer_tabs,
            "selectedExplorer": self.selected_explorer,
            "explorerVisible": self.explorer_visible,
            "explorerWidth": self.explorer_width,
            "sidebarWidth": self.sidebar_width,
            "sidebarVisible": self.sidebar_visible,
        });
        if !self.openers.is_empty() {
            saved["tabOpeners"] = json!(self.openers);
        }
        saved
    }

    pub fn snapshot(&self) -> LayoutSnapshot {
        LayoutSnapshot {
            tabs: self.tabs.clone(),
            panes: self
                .tree
                .panes()
                .into_iter()
                .map(|pane| PaneSnapshot { id: pane.id.clone(), tabs: pane.tabs.clone(), selected: pane.selected.clone() })
                .collect(),
            focused_pane: self.focused_id(),
            selected: self.focused().selected.clone(),
            explorer_tabs: self.explorer_tabs.clone(),
            selected_explorer: self.selected_explorer.clone(),
        }
    }

    /// Pane id holding `key`, if any.
    pub fn pane_of(&self, key: &str) -> Option<String> {
        self.tree.panes().iter().find(|pane| pane.tabs.iter().any(|k| k == key)).map(|pane| pane.id.clone())
    }
}

fn replace_node(node: Node, id: &str, replacement: Node) -> Node {
    if node.own_id() == id {
        return replacement;
    }
    match node {
        Node::Pane(_) => node,
        Node::Split { id: split_id, axis, ratio, first, second } => {
            // Only one subtree holds the id; the other is returned unchanged.
            if first.contains_id(id) {
                Node::Split { id: split_id, axis, ratio, first: Box::new(replace_node(*first, id, replacement)), second }
            } else {
                Node::Split { id: split_id, axis, ratio, first, second: Box::new(replace_node(*second, id, replacement)) }
            }
        }
    }
}

impl Node {
    fn own_id(&self) -> &str {
        match self {
            Node::Pane(pane) => &pane.id,
            Node::Split { id, .. } => id,
        }
    }

    fn contains_id(&self, id: &str) -> bool {
        match self {
            Node::Pane(pane) => pane.id == id,
            Node::Split { id: split_id, first, second, .. } => split_id == id || first.contains_id(id) || second.contains_id(id),
        }
    }
}

/// `_removeFromPane`: selection moves to the tab now at the removed index.
fn remove_from_pane(pane: &mut Pane, key: &str) {
    let Some(index) = pane.tabs.iter().position(|k| k == key) else { return };
    pane.tabs.remove(index);
    if pane.selected.as_deref() == Some(key) {
        pane.selected = if pane.tabs.is_empty() { None } else { Some(pane.tabs[index.min(pane.tabs.len() - 1)].clone()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> PanelTarget {
        PanelTarget::agent("srv", "ws", "/w", id, "")
    }

    fn file(path: &str) -> PanelTarget {
        PanelTarget::new("file", "srv", "ws", "/w").with_id(path)
    }

    /// Sessions `a`, `b`, `c` as tabs with `a` shown, like a phone that restored its tabs.
    fn sessions() -> Layout {
        let mut layout = Layout::default();
        for id in ["a", "b", "c"] {
            layout.open(agent(id), false);
        }
        layout.select_tab(&agent("a").key());
        layout
    }

    fn shown(layout: &Layout) -> Option<String> {
        layout.selected().map(PanelTarget::key)
    }

    /// Compact Back as `commands::back` runs it: the detail tab closes, else `handle_back`.
    fn back(layout: &mut Layout) -> bool {
        match layout.compact_back_tab(true) {
            Some(key) => {
                layout.close_tab(&key);
                true
            }
            None => layout.handle_back(true),
        }
    }

    #[test]
    fn compact_back_from_a_file_opened_in_a_chat_returns_to_that_session_then_to_the_sidebar() {
        let mut layout = sessions();
        let readme = file("docs/readme.md");
        layout.open(readme.clone(), false);
        assert_eq!(shown(&layout), Some(readme.key()));

        assert!(back(&mut layout));
        assert!(layout.target(&readme.key()).is_none());
        assert_eq!(shown(&layout), Some(agent("a").key()));
        assert_eq!(layout.compact_destination, CompactDestination::Agent);

        assert!(back(&mut layout));
        assert_eq!(layout.compact_destination, CompactDestination::Sidebar);
        assert_eq!(shown(&layout), Some(agent("a").key()));
        assert!(!back(&mut layout));
    }

    #[test]
    fn compact_back_unwinds_files_that_link_to_each_other_down_to_the_session() {
        let mut layout = sessions();
        let (readme, guide) = (file("docs/readme.md"), file("docs/guide.md"));
        layout.open(readme.clone(), false);
        layout.open(guide.clone(), false);
        // The guide links back to the readme, which is still open.
        layout.open(readme.clone(), false);
        assert_eq!(shown(&layout), Some(readme.key()));

        assert!(back(&mut layout));
        assert_eq!(shown(&layout), Some(guide.key()));
        assert!(back(&mut layout));
        assert_eq!(shown(&layout), Some(agent("a").key()));
    }

    #[test]
    fn a_file_reopened_from_another_session_returns_to_that_session_after_a_restart() {
        let mut layout = sessions();
        let readme = file("docs/readme.md");
        layout.open(readme.clone(), false);
        // Left through the sidebar instead of Back, then linked again from session `b`.
        layout.select_tab(&agent("b").key());
        layout.open(readme.clone(), false);

        let mut layout = Layout::restore(&layout.to_json());
        assert_eq!(shown(&layout), Some(readme.key()));
        assert!(back(&mut layout));
        assert_eq!(shown(&layout), Some(agent("b").key()));
        assert!(layout.to_json().get("tabOpeners").is_none());
    }

    #[test]
    fn closing_a_file_follows_its_remaining_opener_or_else_its_neighbour() {
        let mut layout = sessions();
        let (readme, guide) = (file("docs/readme.md"), file("docs/guide.md"));
        layout.open(readme.clone(), false);
        layout.open(guide.clone(), false);
        // The readme the guide came from closes first: the guide returns to the readme's session.
        layout.close_tab(&readme.key());
        assert_eq!(shown(&layout), Some(guide.key()));
        layout.close_tab(&guide.key());
        assert_eq!(shown(&layout), Some(agent("a").key()));

        // Without its opener a closed tab hands over to the tab at its index, as before.
        layout.open(readme.clone(), false);
        layout.close_tab(&agent("a").key());
        assert_eq!(shown(&layout), Some(readme.key()));
        layout.close_tab(&readme.key());
        assert_eq!(shown(&layout), Some(agent("c").key()));
    }

    #[test]
    fn closing_the_last_tab_of_a_split_pane_removes_it_and_moves_focus_to_the_sibling() {
        let mut layout = Layout::default();
        layout.open(agent("a"), false);
        layout.open(agent("b"), true);
        assert_eq!(layout.panes().len(), 2);
        let b = agent("b").key();
        layout.close_tab(&b);
        assert_eq!(layout.panes().len(), 1);
        assert_eq!(layout.focused().id, MAIN_PANE);
        assert_eq!(layout.selected().map(PanelTarget::key), Some(agent("a").key()));
    }

    #[test]
    fn main_pane_survives_becoming_empty() {
        let mut layout = Layout::default();
        layout.open(agent("a"), false);
        layout.close_tab(&agent("a").key());
        assert_eq!(layout.panes().len(), 1);
        assert!(layout.focused().tabs.is_empty());
    }

    #[test]
    fn restore_drops_removed_kinds_keeps_terminals_and_places_unplaced_tabs_in_the_focused_pane() {
        let terminal = PanelTarget::new("terminal", "srv", "ws", "/w").with_id("t");
        let browser = PanelTarget::new("browser", "srv", "ws", "/w").with_id("b");
        let saved = json!({
            "version": 2,
            "tabs": [agent("a").to_json(), agent("b").to_json(), terminal.to_json(), browser.to_json()],
            "tree": {"kind": "pane", "id": "main", "tabs": [agent("a").key(), browser.key(), terminal.key()], "selected": browser.key()},
            "sidebarWidth": 9999,
        });
        let layout = Layout::restore(&saved);
        assert_eq!(layout.tabs.len(), 3);
        assert!(layout.target(&terminal.key()).is_some());
        assert!(layout.target(&browser.key()).is_none());
        let pane = layout.focused();
        assert_eq!(pane.tabs, vec![agent("a").key(), terminal.key(), agent("b").key()]);
        assert_eq!(pane.selected, Some(agent("a").key()));
        assert_eq!(layout.sidebar_width, 600.0);
        assert_eq!(layout.compact_destination, CompactDestination::Agent);
    }

    #[test]
    fn adjacent_pane_follows_split_geometry() {
        let mut layout = Layout::default();
        layout.open(agent("a"), false);
        let right = layout.split_pane(MAIN_PANE, Axis::Horizontal).unwrap();
        layout.focus_pane(MAIN_PANE);
        assert_eq!(layout.adjacent_pane(Direction::Right), Some(right.clone()));
        assert_eq!(layout.adjacent_pane(Direction::Left), None);
        layout.move_focused_tab(Direction::Right);
        assert_eq!(layout.focused().id, right);
    }

    #[test]
    fn remap_server_rewrites_keys_everywhere() {
        let mut layout = Layout::default();
        layout.open(agent("a"), false);
        layout.remap_server("srv", "srv2");
        let key = PanelTarget::agent("srv2", "ws", "/w", "a", "").key();
        assert_eq!(layout.focused().selected, Some(key.clone()));
        assert!(layout.target(&key).is_some());
    }

    #[test]
    fn panel_target_key_matches_dart_uri_encode_component() {
        let target = PanelTarget::new("file", "s v", "w/1", "/").with_id("a(b)!");
        assert_eq!(target.key(), "s%20v/w%2F1/file/a(b)!");
    }
}
