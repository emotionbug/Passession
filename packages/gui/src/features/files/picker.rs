//! The remote folder picker (Flutter `RemoteDirectoryBrowser(directoriesOnly: true,
//! onDirectorySelected:)`): lists directories of the chosen host — never the client's file
//! system — with history navigation, an editable remote location and "Use this directory".
//! Mounted inside the caller's own modal (WorkspaceAutomation New Tab / Add Project).

use makepad_widgets::*;

use crate::common::UiContext;

use super::explorer_view::{ExplorerEvent, ExplorerLook, ExplorerView};
use super::kit::Kit;
use super::store;

live_design! {
    use link::theme::*;
    use link::widgets::*;
    use crate::features::files::kit::FilesKit;

    pub RemoteFolderPickerBase = {{RemoteFolderPicker}} {}
    pub RemoteFolderPicker = <RemoteFolderPickerBase> {
        width: Fill, height: Fill
        kit: <FilesKit> {}
    }
}

/// Flutter `RemoteDirectoryBrowser(directoriesOnly: true, onDirectorySelected:)`: lists the
/// chosen host's directories (never the client's file system).
#[derive(Clone, Debug, DefaultNone)]
pub enum RemoteFolderPickerAction {
    None,
    /// The absolute remote directory the user chose (`remoteAbsolute(cwd, path)`).
    Selected(String),
    /// Escape / Back inside the picker.
    Cancelled,
}

#[derive(Live, LiveHook, Widget)]
pub struct RemoteFolderPicker {
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    kit: Kit,
    #[rust]
    scope: Option<(String, String)>,
    #[rust]
    owner: String,
    #[rust]
    view: ExplorerView,
    #[rust]
    seen_epoch: u64,
    /// Top-left of the last draw (before the hosting modal centred it).
    #[rust]
    drawn_at: DVec2,
    #[rust]
    next_frame: NextFrame,
}

impl Drop for RemoteFolderPicker {
    fn drop(&mut self) {
        if !self.owner.is_empty() {
            store::release(&self.owner);
            store::unregister_owner(&self.owner);
        }
    }
}

impl RemoteFolderPicker {
    fn owner(&mut self) -> String {
        if self.owner.is_empty() {
            self.owner = format!("picker:{:?}", self.widget_uid());
        }
        self.owner.clone()
    }

    fn look(compact: bool) -> ExplorerLook<'static> {
        ExplorerLook {
            palette: crate::theme::current_palette(),
            typography: crate::theme::current_typography(),
            compact,
            actions: &[],
            navigation_menu: true,
            picker: true,
            busy: false,
        }
    }
}

impl Widget for RemoteFolderPicker {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.location.observe_event(event);
        let Some((host_id, cwd)) = self.scope.clone() else { return };
        let owner = self.owner();
        let uid = self.widget_uid();
        let mut selected = None;
        let mut changed = self.view.location.blink.handle_event(event);
        if let Some(ui) = scope.data.get_mut::<UiContext>() {
            store::register_picker(&owner, &host_id);
            store::with(|s| {
                s.explorer(ui, &owner, &cwd, ".", true, None);
            });
            let scroll_enabled = self.area.is_valid(cx);
            changed |= store::with(|s| {
                s.explorer_mut(&owner).is_some_and(|state| self.view.animate_scroll(cx, event, state, scroll_enabled))
            });
            let shift = if self.area.is_valid(cx) { self.area.rect(cx).pos - self.drawn_at } else { DVec2::default() };
            let hit = super::kit::unshift_hit(event.hits(cx, self.area), shift);
            if !matches!(hit, Hit::Nothing) {
                if matches!(&hit, Hit::FingerDown(fe) if !fe.device.is_touch())
                    || self.view.wants_touch_focus(&hit) {
                    cx.set_key_focus(self.area);
                }
                let look = Self::look(ui.compact);
                let view = &mut self.view;
                let kit = &mut self.kit;
                let result = store::with(|s| {
                    let state = s.explorer_mut(&owner)?;
                    Some(view.handle_hit(cx, kit, &hit, state, &look, &mut changed))
                });
                match result {
                    Some(ExplorerEvent::DirectorySelected(path)) => selected = Some(RemoteFolderPickerAction::Selected(path)),
                    _ => {
                        if let Hit::KeyDown(KeyEvent { key_code: KeyCode::Escape, .. }) = hit
                            && !self.view.location_focused {
                                selected = Some(RemoteFolderPickerAction::Cancelled);
                            }
                    }
                }
            }
            store::with(|s| s.pump(cx, ui));
        }
        if let Some(action) = selected {
            cx.widget_action(uid, &scope.path, action);
        }
        let epoch = store::with(|s| s.epoch);
        if changed || epoch != self.seen_epoch {
            self.seen_epoch = epoch;
            self.area.redraw(cx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        let rect = cx.turtle().rect();
        cx.add_aligned_rect_area(&mut self.area, rect);
        let compact = scope.data.get::<UiContext>().is_some_and(|ui| ui.compact);
        let owner = self.owner();
        let area = self.area;
        // The IME position is relative to the drawn origin; the area applies any shift.
        let origin = rect.pos;
        self.drawn_at = rect.pos;
        let look = Self::look(compact);
        let view = &mut self.view;
        let kit = &mut self.kit;
        view.sidebar = false;
        let drawn = store::with(|s| {
            let state = s.explorer_mut(&owner)?;
            view.sync_location(state);
            view.draw(cx, kit, rect, state, &look, (area, origin));
            Some(())
        });
        if drawn.is_none() {
            self.kit.fill(cx, rect, crate::theme::current_palette().surface0, 0.0);
        }
        cx.end_turtle();
        DrawStep::done()
    }
}

impl RemoteFolderPickerRef {
    /// Lists `cwd` (remote path, `~` for the host home, also when empty) on `host_id`,
    /// directories only. Every open starts a fresh browser (history, expansion, location).
    pub fn open(&self, cx: &mut Cx, host_id: &str, cwd: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            let cwd = if cwd.trim().is_empty() { "~".to_owned() } else { cwd.trim().to_owned() };
            let owner = inner.owner();
            store::release(&owner);
            inner.view = ExplorerView::default();
            inner.scope = Some((host_id.to_owned(), cwd));
            inner.next_frame = cx.new_next_frame();
            inner.area.redraw(cx);
        }
    }

    pub fn selected(&self, actions: &Actions) -> Option<String> {
        let uid = self.widget_uid();
        actions.filter_widget_actions_cast::<RemoteFolderPickerAction>(uid).find_map(|action| match action {
            RemoteFolderPickerAction::Selected(path) => Some(path),
            _ => None,
        })
    }
}

