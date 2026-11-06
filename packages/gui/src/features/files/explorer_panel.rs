//! The workspace file explorer (Flutter `files_panel.dart` FilesPanel): the tree on the sidebar
//! surface, new file/folder (fsEntryOps), the grouped entry context menu, background menu,
//! rename/delete/duplicate (fsEntryDuplicate), copy paths, add to chat, download/share,
//! reveal of an assistant folder link, and the busy progress line.

use std::collections::HashMap;

use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::overlay::{self, MenuEntry, MenuItem};
use crate::common::paint::HitMap;
use crate::common::{UiContext, clock};
use crate::theme::{self, Typography};

use super::explorer::Entry;
use super::explorer_view::{ExplorerEvent, ExplorerLook, ExplorerView, ToolbarAction};
use super::host;
use super::i18n::{l, lf, t, ta};
use super::kit::{self, Kit, rect};
use super::path::{parent_explorer_path, remote_absolute, remote_relative};
use super::store::{self, EntryAction};
use super::transfer::TransferMode;

#[derive(Clone, Debug)]
enum Pending {
    EntryMenu(Entry),
    BackgroundMenu,
    CreateName { parent: String, kind: &'static str },
    RenameName(Entry),
    Delete(Entry),
}

#[derive(Clone, Debug, PartialEq)]
enum Hot {
    Block,
    Cancel(u64),
}

/// The explorer's target.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplorerTarget {
    pub host_id: String,
    pub workspace_id: String,
    pub cwd: String,
    pub reveal: Option<String>,
}

pub struct ExplorerPanel {
    pub owner: String,
    /// Screen position minus drawn position of the root widget (see `kit::unshift_hit`).
    pub hit_shift: DVec2,
    target: Option<ExplorerTarget>,
    view: ExplorerView,
    overlays: HashMap<u64, Pending>,
    reveal_pending: Option<String>,
    transfer_hits: HitMap<Hot>,
    transfer_hover: Option<Hot>,
    transfer_pressed: Option<Hot>,
    /// The cwd the panel was showing when a menu/prompt opened: actions run there or not at all.
    opened_in: HashMap<u64, String>,
}

impl ExplorerPanel {
    pub fn new(owner: &str) -> ExplorerPanel {
        ExplorerPanel {
            owner: owner.to_owned(),
            hit_shift: DVec2::default(),
            target: None,
            view: ExplorerView::default(),
            overlays: HashMap::new(),
            reveal_pending: None,
            transfer_hits: HitMap::default(),
            transfer_hover: None,
            transfer_pressed: None,
            opened_in: HashMap::new(),
        }
    }

    pub fn set_target(&mut self, target: ExplorerTarget) {
        // An assistant folder link reveals its folder once the tree lists the target's cwd.
        let reveal_changed = self.target.as_ref().map(|old| (&old.reveal, &old.cwd)) != Some((&target.reveal, &target.cwd));
        if reveal_changed {
            self.reveal_pending = target.reveal.clone().filter(|path| !path.is_empty() && path != ".");
        }
        self.target = Some(target);
    }

    fn workspace_key(target: &ExplorerTarget) -> String {
        let id = target.workspace_id.trim();
        if id.is_empty() { format!("root:{}", target.cwd.trim()) } else { format!("workspace:{id}") }
    }

    fn entry_ops(ui: &UiContext, host_id: &str) -> bool {
        // COMPAT(fsEntryOps): create, rename and delete need a host that has them.
        host::feature(ui, host_id, "fsEntryOps")
    }

    fn duplicate_op(ui: &UiContext, host_id: &str) -> bool {
        // COMPAT(fsEntryDuplicate)
        host::feature(ui, host_id, "fsEntryDuplicate")
    }

    /// Whether the panel still shows `cwd` (the tab can be reopened under another cwd).
    fn showing(&self, cwd: &str) -> bool {
        self.target.as_ref().is_some_and(|target| target.cwd == cwd)
    }

    fn ensure(&mut self, ui: &mut UiContext) -> Option<ExplorerTarget> {
        let target = self.target.clone()?;
        if target.cwd.is_empty() {
            return None;
        }
        let owner = self.owner.clone();
        store::register_owner(&owner, &target.host_id, &target.workspace_id);
        let key = Self::workspace_key(&target);
        let reveal = self.reveal_pending.take();
        store::with(|s| {
            let state = s.explorer(ui, &owner, &target.cwd, ".", false, Some(key));
            if let Some(path) = reveal {
                state.reveal(&path);
            }
        });
        Some(target)
    }

    pub fn handle_event(&mut self, cx: &mut Cx, kit: &mut Kit, event: &Event, ui: &mut UiContext, area: Area) -> bool {
        let Some(target) = self.ensure(ui) else { return false };
        let mut changed = self.handle_commands(cx, ui, &target);
        changed |= self.view.location.blink.handle_event(event);
        let scroll_enabled = area.is_valid(cx) && !self.transfer_hits_active();
        changed |= store::with(|s| {
            s.explorer_mut(&self.owner).is_some_and(|state| self.view.animate_scroll(cx, event, state, scroll_enabled))
        });
        let hit = kit::unshift_hit(event.hits(cx, area), self.hit_shift);
        if matches!(hit, Hit::Nothing) {
            // Caret blinking is local paint only. Initialization, reveal and
            // command results still dispatch any work queued by the explorer.
            store::with(|s| {
                if !s.pending_cmds_empty() {
                    s.pump(cx, ui);
                }
            });
            return changed;
        }
        if let Hit::FingerDown(_) = &hit {
            cx.set_key_focus(area);
        }
        // The download dialog takes input first.
        if self.transfer_hits_active() {
            changed |= self.transfer_hit(cx, ui, &hit);
            return changed;
        }
        let actions = self.actions(ui, &target);
        let busy = store::with(|s| s.is_busy(&self.owner));
        let look = ExplorerLook {
            palette: theme::current_palette(),
            typography: theme::current_typography(),
            compact: ui.compact,
            actions: &actions,
            navigation_menu: false,
            picker: false,
            busy,
        };
        let owner = self.owner.clone();
        let mut view_changed = false;
        let event = store::with(|s| {
            let state = s.explorer_mut(&owner)?;
            Some(self.view.handle_hit(cx, kit, &hit, state, &look, &mut view_changed))
        });
        changed |= view_changed;
        if let Some(event) = event {
            changed |= self.on_event(ui, &target, event, busy);
        }
        store::with(|s| {
            if !s.pending_cmds_empty() {
                s.pump(cx, ui);
            }
        });
        changed
    }

    fn actions(&self, ui: &UiContext, target: &ExplorerTarget) -> Vec<ToolbarAction> {
        if !Self::entry_ops(ui, &target.host_id) {
            return Vec::new();
        }
        vec![
            ToolbarAction { id: "new_file", icon: "file-plus", tooltip: t("workspace.fileActions.newFile"), enabled: true },
            ToolbarAction { id: "new_directory", icon: "folder-plus", tooltip: t("workspace.fileActions.newFolder"), enabled: true },
        ]
    }

    fn on_event(&mut self, ui: &mut UiContext, target: &ExplorerTarget, event: ExplorerEvent, busy: bool) -> bool {
        match event {
            ExplorerEvent::None => false,
            ExplorerEvent::Action(id) => {
                self.create(ui, target, ".", if id == "new_file" { "file" } else { "directory" });
                true
            }
            ExplorerEvent::FileSelected(entry) => {
                let beside = store::open_beside_preference(ui);
                host::open_file(ui, &target.host_id, &target.workspace_id, &entry.path, beside);
                true
            }
            ExplorerEvent::EntryMenu { entry, at, row } => {
                if busy {
                    return false;
                }
                let anchor = match at {
                    Some(at) => rect(at.x, at.y, 0.0, 0.0),
                    None => row,
                };
                self.entry_menu(ui, target, entry, anchor);
                true
            }
            ExplorerEvent::BackgroundMenu { anchor, .. } => {
                if busy || !Self::entry_ops(ui, &target.host_id) {
                    return false;
                }
                let entries = vec![
                    MenuItem::new("file", t("workspace.fileActions.newFile")).icon("file-plus").entry(),
                    MenuItem::new("directory", t("workspace.fileActions.newFolder")).icon("folder-plus").entry(),
                ];
                let id = overlay::open_menu(ui, kit::shifted(anchor, self.hit_shift), None, entries);
                self.overlays.insert(id, Pending::BackgroundMenu);
                self.opened_in.insert(id, target.cwd.clone());
                true
            }
            ExplorerEvent::DirectorySelected(_) => false,
        }
    }

    /// Original `FileActionsContextMenuContent`: grouped rows, a separator between groups.
    fn entry_menu(&mut self, ui: &mut UiContext, target: &ExplorerTarget, entry: Entry, anchor: Rect) {
        let file = entry.kind == "file";
        let ops = Self::entry_ops(ui, &target.host_id);
        let duplicate = Self::duplicate_op(ui, &target.host_id);
        let expanded = !file && store::with(|s| s.explorer_mut(&self.owner).is_some_and(|state| state.is_expanded(&entry.path)));
        let mut groups: Vec<Vec<MenuEntry>> = Vec::new();
        if !file && ops {
            groups.push(vec![
                MenuItem::new("new_file", t("workspace.fileActions.newFile")).icon("file-plus").entry(),
                MenuItem::new("new_directory", t("workspace.fileActions.newFolder")).icon("folder-plus").entry(),
            ]);
        }
        if expanded {
            groups.push(vec![MenuItem::new("collapse", t("workspace.fileActions.collapseFolder")).icon("folder-minus").entry()]);
        }
        if file {
            groups.push(vec![MenuItem::new("open", t("workspace.fileActions.openToSide")).icon("arrow-right-to-line").entry()]);
        }
        let mut reference = vec![
            MenuItem::new("absolute", t("workspace.fileActions.copyPath")).icon("copy").entry(),
            MenuItem::new("relative", t("workspace.fileActions.copyRelativePath")).icon("copy").entry(),
        ];
        if file {
            reference.push(MenuItem::new("download", t("workspace.fileActions.download")).icon("download").entry());
            reference.push(MenuItem::new("share", t("downloads.shareFile")).icon("share-2").entry());
        }
        reference.push(MenuItem::new("chat", t("workspace.fileActions.addToChat")).icon("message-square-plus").entry());
        groups.push(reference);
        if ops || duplicate {
            let mut manage = Vec::new();
            if ops {
                manage.push(MenuItem::new("rename", t("workspace.fileActions.rename")).icon("pencil").entry());
            }
            if duplicate {
                manage.push(MenuItem::new("duplicate", t("workspace.fileActions.duplicate")).icon("copy-plus").entry());
            }
            groups.push(manage);
        }
        if ops {
            groups.push(vec![MenuItem::new("delete", t("workspace.fileActions.delete")).icon("trash-2").destructive(true).entry()]);
        }
        let mut entries = Vec::new();
        for (index, group) in groups.into_iter().enumerate() {
            if index > 0 {
                entries.push(MenuEntry::Separator);
            }
            entries.extend(group);
        }
        let id = overlay::open_menu(ui, kit::shifted(anchor, self.hit_shift), None, entries);
        self.overlays.insert(id, Pending::EntryMenu(entry));
        self.opened_in.insert(id, target.cwd.clone());
    }

    /// Original `handleNewEntry`: the parent expands (and lists), then the name prompt.
    fn create(&mut self, ui: &mut UiContext, target: &ExplorerTarget, parent: &str, kind: &'static str) {
        store::with(|s| {
            if let Some(state) = s.explorer_mut(&self.owner) {
                state.expand_for_create(parent);
            }
        });
        let (title, placeholder) = if kind == "directory" {
            (t("workspace.fileActions.newFolder"), t("workspace.fileExplorer.draft.folderPlaceholder"))
        } else {
            (t("workspace.fileActions.newFile"), t("workspace.fileExplorer.draft.filePlaceholder"))
        };
        let id = overlay::prompt_text(ui, &title, None, "", &placeholder, &t("common.actions.save"));
        self.overlays.insert(id, Pending::CreateName { parent: parent.to_owned(), kind });
        self.opened_in.insert(id, target.cwd.clone());
    }

    fn handle_commands(&mut self, cx: &mut Cx, ui: &mut UiContext, target: &ExplorerTarget) -> bool {
        let mut answers = Vec::new();
        for command in ui.commands.iter_mut() {
            if command.handled || command.tab_key != self.owner {
                continue;
            }
            if let Some((id, args)) = overlay::result_of(command)
                && let Some(pending) = self.overlays.remove(&id) {
                    let args = args.clone();
                    let cwd = self.opened_in.remove(&id).unwrap_or_default();
                    answers.push((pending, args, cwd));
                    command.handled = true;
                }
        }
        let any = !answers.is_empty();
        for (pending, args, cwd) in answers {
            // The entry was listed in `cwd`; the action runs there or not at all.
            if args.get("cancelled") == Some(&Value::Bool(true)) || !self.showing(&cwd) {
                continue;
            }
            match pending {
                Pending::EntryMenu(entry) => {
                    if let Some(value) = args.get("value").and_then(Value::as_str) {
                        self.run_action(cx, ui, target, value, entry);
                    }
                }
                Pending::BackgroundMenu => {
                    if let Some(value) = args.get("value").and_then(Value::as_str) {
                        let kind = if value == "directory" { "directory" } else { "file" };
                        self.create(ui, target, ".", kind);
                    }
                }
                Pending::CreateName { parent, kind } => {
                    let name = args.get("text").and_then(Value::as_str).unwrap_or("").trim().to_owned();
                    if name.is_empty() {
                        continue;
                    }
                    let owner = self.owner.clone();
                    store::with(|s| {
                        s.entry_request(
                            ui,
                            &owner,
                            &target.host_id,
                            &target.cwd,
                            EntryAction::Create { parent: parent.clone(), kind: kind.to_owned() },
                            json!({ "parentPath": parent, "name": name, "kind": kind }),
                        )
                    });
                }
                Pending::RenameName(entry) => {
                    let name = args.get("text").and_then(Value::as_str).unwrap_or("").trim().to_owned();
                    if name.is_empty() || name == entry.name {
                        continue;
                    }
                    let parent = parent_explorer_path(&entry.path);
                    let owner = self.owner.clone();
                    let fields = json!({ "path": entry.path, "name": name });
                    store::with(|s| s.entry_request(ui, &owner, &target.host_id, &target.cwd, EntryAction::Rename { entry, parent }, fields));
                }
                Pending::Delete(entry) => {
                    if args.get("confirmed") != Some(&Value::Bool(true)) {
                        continue;
                    }
                    let parent = parent_explorer_path(&entry.path);
                    let owner = self.owner.clone();
                    let fields = json!({ "path": entry.path });
                    store::with(|s| s.entry_request(ui, &owner, &target.host_id, &target.cwd, EntryAction::Delete { parent }, fields));
                }
            }
        }
        any
    }

    fn run_action(&mut self, cx: &mut Cx, ui: &mut UiContext, target: &ExplorerTarget, action: &str, entry: Entry) {
        let path = entry.path.clone();
        match action {
            "open" => host::open_file(ui, &target.host_id, &target.workspace_id, &path, true),
            "collapse" => store::with(|s| {
                if let Some(state) = s.explorer_mut(&self.owner) {
                    state.collapse(&path);
                }
            }),
            "absolute" => host::copy_text(ui, remote_absolute(&target.cwd, &path), Some(t("common.states.copied"))),
            "relative" => host::copy_text(ui, remote_relative(&target.cwd, &path), Some(t("common.states.copied"))),
            "chat" => {
                host::add_to_chat(
                    &target.host_id,
                    &target.workspace_id,
                    vec![json!({ "kind": "workspace_file", "path": remote_absolute(&target.cwd, &path), "selection": { "kind": "whole_file" } })],
                );
                host::toast(ui, l("Added to chat draft"), false);
            }
            "new_file" | "new_directory" => {
                let kind = if action == "new_file" { "file" } else { "directory" };
                self.create(ui, target, &path, kind);
            }
            "download" | "share" => {
                let mode = if action == "share" { TransferMode::Share } else { TransferMode::Save };
                let owner = self.owner.clone();
                store::with(|s| s.start_transfer(ui, &owner, &target.host_id, &target.cwd, &path, mode, None, None));
            }
            "rename" => {
                let id = overlay::prompt_text(ui, &lf("Rename {name}", &[("name", &entry.name)]), None, &entry.name, "", &t("common.actions.save"));
                self.overlays.insert(id, Pending::RenameName(entry));
                self.opened_in.insert(id, target.cwd.clone());
            }
            "delete" => {
                let title = t(if entry.is_directory() {
                    "workspace.fileActions.confirmDelete.folderTitle"
                } else {
                    "workspace.fileActions.confirmDelete.fileTitle"
                });
                let message = ta("workspace.fileActions.confirmDelete.message", &[("name", &entry.name)]);
                let id = overlay::confirm(ui, &title, &message, &t("workspace.fileActions.confirmDelete.confirm"), true);
                self.overlays.insert(id, Pending::Delete(entry));
                self.opened_in.insert(id, target.cwd.clone());
            }
            "duplicate" => {
                let parent = parent_explorer_path(&path);
                let owner = self.owner.clone();
                store::with(|s| s.entry_request(ui, &owner, &target.host_id, &target.cwd, EntryAction::Duplicate { parent }, json!({ "path": path })));
            }
            _ => {}
        }
        let _ = cx;
    }

    fn transfer_hits_active(&self) -> bool {
        !self.transfer_hits.is_empty()
    }

    fn transfer_hit(&mut self, cx: &mut Cx, ui: &mut UiContext, hit: &Hit) -> bool {
        match hit {
            Hit::FingerHoverIn(fe) | Hit::FingerHoverOver(fe) => {
                let hot = self.transfer_hits.at(fe.abs);
                let changed = hot != self.transfer_hover;
                self.transfer_hover = hot;
                changed
            }
            Hit::FingerDown(fe) => {
                self.transfer_pressed = self.transfer_hits.at(fe.abs);
                true
            }
            Hit::FingerUp(fe) => {
                let pressed = self.transfer_pressed.take();
                if fe.is_over && fe.was_tap()
                    && let (Some(Hot::Cancel(id)), Some(Hot::Cancel(at))) = (pressed, self.transfer_hits.at(fe.abs))
                        && id == at {
                            store::with(|s| s.cancel_transfer(ui, id));
                        }
                let _ = cx;
                true
            }
            _ => false,
        }
    }

    pub fn draw(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, ui: &UiContext, widget_area: Area) {
        let p = theme::current_palette();
        let ty: Typography = theme::current_typography();
        let Some(target) = self.target.clone() else {
            kit.fill(cx, area, p.sidebar, 0.0);
            return;
        };
        let actions = self.actions(ui, &target);
        let owner = self.owner.clone();
        let busy = store::with(|s| s.is_busy(&owner));
        let look = ExplorerLook { palette: p, typography: ty, compact: ui.compact, actions: &actions, navigation_menu: false, picker: false, busy };
        // IME positions are relative to the drawn origin; the area applies any later shift.
        let origin = area.pos;
        let drawn = store::with(|s| {
            if let Some(state) = s.explorer_mut(&owner) {
                self.view.sync_location(state);
                self.view.draw(cx, kit, area, state, &look, (widget_area, origin));
                true
            } else {
                false
            }
        });
        if !drawn {
            kit.fill(cx, area, p.sidebar, 0.0);
        }
        if busy {
            // The 2px accent progress line at the bottom.
            let phase = clock::phase(1200);
            let y = area.pos.y + area.size.y - 2.0;
            kit.fill(cx, rect(area.pos.x + area.size.x * phase * 0.7, y, area.size.x * 0.3, 2.0), p.accent, 0.0);
            clock::register(cx, widget_area);
        }
        self.transfer_hits.clear();
        let transfers: Vec<(u64, String, u64, u64)> =
            store::with(|s| s.transfers_of(&owner).iter().map(|t| (t.id, t.name.clone(), t.received, t.total)).collect());
        super::file_view::draw_transfer_cards(cx, kit, area, &transfers, &mut self.transfer_hits, Hot::Block, Hot::Cancel, &self.transfer_hover, &p, &ty);
    }

    pub fn dispose(&mut self) {
        store::unregister_owner(&self.owner);
    }
}
