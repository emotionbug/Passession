//! Shell-drawn overlays: anchored menus / compact menu sheets (`showPaseoMenu`), confirm and
//! text prompt dialogs, adaptive sheets (`showAdaptiveSheet` + `SheetContents`), toasts
//! (`showToast`) and tooltips. Drawn into a last-overlay draw list; while a modal is open the
//! sweep lock keeps the content below from reacting.

use std::any::Any;

use makepad_widgets::makepad_draw::makepad_platform::makepad_shader_compiler::ShaderTy;
use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::overlay::{
    DrawLayerTexture, LayerTexture, MenuEntry, OverlayKind, OverlayRequest, SelectOption, Tip,
    fit_in_viewport, take_dismissal,
};
use crate::common::paint::{Font, HitMap, TextSpec, inset, rect};
use crate::common::transition::{Intent, Style, Transition};
use crate::common::widgets::PTextFieldWidgetRefExt;
use crate::common::{UiContext, tr};
use crate::shell::PaseoShell;
use crate::shell::sheets::{Sheet, SheetFlow, SheetOps, SheetStyle};
use crate::shell::ui::{Pointer, Press, RowLead, Ui, Variant};
use crate::theme::{self, ControlSize, with_alpha};

pub struct OpenMenu {
    pub id: u64,
    pub owner: String,
    pub title: Option<String>,
    pub anchor: Rect,
    /// Page stack: root entries, then opened submenus (title, entries).
    pub pages: Vec<(Option<String>, Vec<MenuEntry>)>,
    pub width: f64,
    pub align_end: bool,
    pub sheet: bool,
    pub scroll: f64,
    max_scroll: f64,
    viewport: Rect,
    /// Keyboard-highlighted entry of the current page (↑/↓, Enter).
    pub active: Option<usize>,
}

/// An open `showPaseoSelect` (original `Combobox`).
pub struct OpenSelect {
    pub id: u64,
    pub owner: String,
    pub title: String,
    pub anchor: Rect,
    pub options: Vec<SelectOption>,
    pub value: Option<String>,
    /// The search row's field (`searchable`).
    pub search: Option<WidgetRef>,
    pub empty_text: String,
    pub min_width: f64,
    /// Trimmed, lowercased search text.
    pub query: String,
    pub scroll: f64,
    max_scroll: f64,
    viewport: Rect,
    pub focused: bool,
    /// Keyboard-highlighted option index (↑/↓, Enter).
    pub active: Option<usize>,
}

impl OpenSelect {
    /// Indices of the options the query keeps, in order.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.options.len())
            .filter(|&i| self.options[i].matches(&self.query))
            .collect()
    }
}

pub enum Modal {
    Menu(OpenMenu),
    Select(OpenSelect),
    /// `ok_focused`: the button Enter activates (←/→/Tab move it). Destructive confirms start
    /// on Cancel.
    Confirm {
        id: u64,
        owner: String,
        title: String,
        message: String,
        confirm_label: String,
        destructive: bool,
        ok_focused: bool,
    },
    Prompt {
        id: u64,
        owner: String,
        title: String,
        message: Option<String>,
        field: WidgetRef,
        confirm_label: String,
        focused: bool,
        multiline: bool,
    },
    Sheet {
        id: u64,
        sheet: Box<dyn Sheet>,
        scroll: f64,
        content_height: f64,
        viewport: Rect,
        drag: SheetDrag,
    },
}

/// Draggable extent snap points (`minChildSize .3`, `snapSizes [.6]`, `maxChildSize .9`).
const DRAG_SNAPS: [f64; 3] = [0.3, 0.6, 0.9];

/// Interactive geometry, separate from the sheet's presentation transition.
#[derive(Default)]
pub struct SheetDrag {
    window_h: f64,
    frame: Rect,
    last_y: Option<f64>,
    travel: f64,
}

fn nearest_snap(extent: f64) -> f64 {
    DRAG_SNAPS.iter().copied().fold(DRAG_SNAPS[0], |best, s| {
        if (s - extent).abs() < (best - extent).abs() {
            s
        } else {
            best
        }
    })
}

#[derive(Clone, Copy, Default)]
enum ModalKind {
    #[default]
    Dialog,
    Sheet,
    Popover,
}

impl ModalKind {
    fn style(self, opening: bool) -> Style {
        match (self, opening) {
            (Self::Dialog, true) => Intent::OpenDialog,
            (Self::Dialog, false) => Intent::CloseDialog,
            (Self::Sheet, true) => Intent::OpenSheet,
            (Self::Sheet, false) => Intent::CloseSheet,
            (Self::Popover, true) => Intent::OpenPopover,
            (Self::Popover, false) => Intent::ClosePopover,
        }
        .resolve()
    }
}

/// The modal stack owns input and workflow. This separate paint order also holds
/// closing models to keep any draw lists and textures owned by their widgets alive.
#[derive(Default)]
struct ModalLayer {
    id: u64,
    list: Option<DrawList2d>,
    texture: Option<(LayerTexture, Rect)>,
    quad: Area,
    kind: ModalKind,
    scrim: Area,
    bounds: Rect,
    window: Rect,
    inset_top: f64,
    visibility: Transition,
    extent: Transition,
    layout_extent: Option<f64>,
    pending_open: bool,
    ready: bool,
    closing: Option<Modal>,
}

impl ModalLayer {
    fn set_open(&mut self, cx: &mut Cx, open: bool) {
        self.pending_open = false;
        self.visibility
            .retarget(cx, if open { 1.0 } else { 0.0 }, self.kind.style(open).spec);
    }

    fn release_texture(&mut self, cx: &mut Cx) {
        if let Some((texture, _)) = self.texture.take() {
            crate::common::overlay::release_layer_quad(cx, std::mem::take(&mut self.quad));
            texture.detach(cx);
        }
    }

    fn visible_extent(&self, extent: f64) -> f64 {
        (self.window.size.y * extent)
            .min(self.window.size.y - self.inset_top)
            .max(0.0)
    }

    fn transform(&self) -> (DVec2, f64, f64, f32) {
        let style = self.kind.style(self.closing.is_none());
        let frame = style.sample(self.visibility.value(), self.bounds.size);
        let mut offset = frame.incoming.offset;
        if let Some(extent) = self.layout_extent {
            offset.y += self.visible_extent(extent) - self.visible_extent(self.extent.value());
        }
        (
            offset,
            frame.incoming.scale,
            frame.incoming.opacity,
            frame.scrim as f32,
        )
    }

    fn quad_rect(&self, bounds: Rect, offset: DVec2, scale: f64) -> Rect {
        let center = self.bounds.pos + self.bounds.size * 0.5;
        Rect {
            pos: center + (bounds.pos - center) * scale + offset,
            size: bounds.size * scale,
        }
    }

    fn composite(&self, cx: &mut Cx) {
        let (offset, scale, opacity, scrim) = self.transform();
        if let Some((_, bounds)) = &self.texture {
            DrawLayerTexture::update(
                cx,
                self.quad,
                self.quad_rect(*bounds, offset, scale),
                opacity as f32,
            );
        } else if let Some(list) = &self.list {
            let offset = offset + (self.bounds.pos + self.bounds.size * 0.5) * (1.0 - scale);
            list.set_view_transform(
                cx,
                &Mat4::scaled_translation(
                    scale as f32,
                    vec3(offset.x as f32, offset.y as f32, 0.0),
                ),
            );
            if let Some(pass) = cx.draw_lists[list.id()].pass_id {
                cx.repaint_pass(pass);
            }
        }
        if let Some(write) =
            self.scrim
                .get_write_ref(cx, live_id!(opacity), ShaderTy::Float, "scrim")
        {
            write.buffer[0] = scrim;
        }
    }
}

impl Modal {
    fn id(&self) -> u64 {
        match self {
            Modal::Menu(m) => m.id,
            Modal::Select(s) => s.id,
            Modal::Confirm { id, .. } | Modal::Prompt { id, .. } | Modal::Sheet { id, .. } => *id,
        }
    }

    fn accessibility_scope(&self) -> u64 {
        match self {
            Self::Menu(menu) => crate::common::accessibility::id((
                menu.id,
                menu.pages.len(),
                menu.pages.last().and_then(|(label, _)| label.as_deref()),
            )),
            _ => self.id(),
        }
    }

    fn kind(&self, compact: bool) -> ModalKind {
        match self {
            Self::Menu(menu) if compact && menu.sheet => ModalKind::Sheet,
            Self::Select(_) if compact => ModalKind::Sheet,
            Self::Menu(_) | Self::Select(_) => ModalKind::Popover,
            Self::Sheet { sheet, .. } if compact || sheet.style() == SheetStyle::FullScreen => {
                ModalKind::Sheet
            }
            _ => ModalKind::Dialog,
        }
    }

    fn scrim(&self, compact: bool) -> Vec4 {
        match self {
            Self::Menu(menu) if compact && menu.sheet => theme::current_palette().scrim,
            Self::Select(_) if compact => theme::current_palette().scrim,
            Self::Menu(_) | Self::Select(_) => Vec4::default(),
            Self::Sheet { sheet, .. } if sheet.style() == SheetStyle::FullScreen => Vec4::default(),
            _ => argb_scrim(compact),
        }
    }

    fn same_presentation(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Sheet { sheet: a, .. }, Self::Sheet { sheet: b, .. }) => {
                (&**a as &dyn Any).type_id() == (&**b as &dyn Any).type_id()
                    && a.style() == b.style()
            }
            (Self::Menu(a), Self::Menu(b)) => {
                a.owner == b.owner && a.title == b.title && a.anchor == b.anchor
            }
            (Self::Select(a), Self::Select(b)) => {
                a.owner == b.owner && a.title == b.title && a.anchor == b.anchor
            }
            (
                Self::Confirm {
                    owner: a,
                    title: at,
                    ..
                },
                Self::Confirm {
                    owner: b,
                    title: bt,
                    ..
                },
            )
            | (
                Self::Prompt {
                    owner: a,
                    title: at,
                    ..
                },
                Self::Prompt {
                    owner: b,
                    title: bt,
                    ..
                },
            ) => a == b && at == bt,
            _ => false,
        }
    }
}

pub struct Toast {
    pub message: String,
    pub error: bool,
    /// A button that fixes what the message reports.
    pub action: Option<ToastAction>,
    pub timer: Timer,
    /// Seconds the toast still shows once its timer (re)started at `started`.
    remaining: f64,
    started: f64,
    /// The pointer is over it: the timer is stopped until it leaves.
    paused: bool,
    /// Where it was last drawn (dismiss and hover target).
    rect: Rect,
    /// Where its action button was last drawn.
    action_rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToastAction {
    pub label: String,
    pub fix: crate::shell::permissions::Fix,
}

/// Pointer ownership of the toast above all other shell surfaces.
enum ToastPress {
    None,
    Captured,
    Dismissed,
    Action(crate::shell::permissions::Fix),
}

pub struct Overlays {
    pub stack: Vec<Modal>,
    layers: Vec<ModalLayer>,
    pub hits: HitMap<String>,
    accessibility_owner: u64,
    pub pointer: Pointer,
    pub area: Area,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    pub toast: Option<Toast>,
    /// An info toast that arrived while an error toast was showing; it follows the error.
    queued_toast: Option<String>,
    pub tooltip: Option<Tip>,
    pub tooltip_pending: Option<Tip>,
    pub tooltip_timer: Timer,
    pub tooltips: std::collections::HashMap<String, String>,
    /// The chrome hit id the current/pending tooltip describes; it hides once that hit is no
    /// longer registered where it was (route change, resize, closed panel).
    pub tooltip_owner: Option<String>,
    pub locked: bool,
    /// The area the layer's sweep lock was last taken on.
    pub locked_area: Area,
    next_sheet: u64,
    focus: Area,
    focus_returns: Vec<(u64, Area)>,
}

impl Default for Overlays {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            layers: Vec::new(),
            hits: HitMap::default(),
            accessibility_owner: 0,
            pointer: Pointer::default(),
            area: Area::Empty,
            touch_scroll: crate::common::widgets::touch_scroll(),
            toast: None,
            queued_toast: None,
            tooltip: None,
            tooltip_pending: None,
            tooltip_timer: Timer::empty(),
            tooltips: Default::default(),
            tooltip_owner: None,
            locked: false,
            locked_area: Area::Empty,
            next_sheet: 0,
            focus: Area::Empty,
            focus_returns: Vec::new(),
        }
    }
}

impl Overlays {
    pub(crate) fn presentation_key(&self) -> (usize, Option<u64>) {
        (self.stack.len(), self.stack.last().map(Modal::id))
    }

    fn retire(&mut self, cx: &mut Cx, modal: Modal) {
        let Some(index) = self.layers.iter().position(|layer| layer.id == modal.id()) else {
            return;
        };
        let layer = &mut self.layers[index];
        let painted = layer.list.is_some() && layer.visibility.value() > 0.0;
        if painted {
            layer.closing = Some(modal);
            layer.set_open(cx, false);
        }
        if !painted || !layer.visibility.is_moving() {
            layer.release_texture(cx);
            self.layers.remove(index);
        }
        self.area.redraw(cx);
    }

    fn advance(&mut self, cx: &mut Cx, event: &Event) {
        if !matches!(event, Event::NextFrame(_)) {
            return;
        }
        let mut redraw = false;
        self.layers.retain_mut(|layer| {
            let visibility_changed = layer.visibility.advance(cx, event);
            let extent_changed = layer.extent.advance(cx, event);
            if layer.closing.is_some() && !layer.visibility.is_moving() {
                layer.release_texture(cx);
                redraw = true;
                return false;
            }
            if extent_changed && !layer.extent.is_moving() {
                layer.layout_extent = Some(layer.extent.value());
                redraw = true;
            }
            if visibility_changed && !layer.visibility.is_moving() {
                // Release the snapshot and repaint live widgets at rest.
                layer.release_texture(cx);
                redraw = true;
            }
            if (visibility_changed || extent_changed) && !redraw {
                layer.composite(cx);
            }
            true
        });
        if redraw {
            self.area.redraw(cx);
        }
    }

    fn push(&mut self, mut modal: Modal) {
        let mut layer = self
            .layers
            .iter()
            .rposition(|layer| {
                layer
                    .closing
                    .as_ref()
                    .is_some_and(|old| old.same_presentation(&modal))
            })
            .map(|index| self.layers.remove(index))
            .unwrap_or_default();
        layer.id = modal.id();
        if let (
            Some(Modal::Sheet {
                content_height: previous,
                ..
            }),
            Modal::Sheet { content_height, .. },
        ) = (&layer.closing, &mut modal)
        {
            *content_height = *previous;
        }
        layer.closing = None;
        layer.pending_open = true;
        layer.ready = false;
        self.layers.push(layer);
        self.stack.push(modal);
        self.touch_scroll.reset();
        self.pointer = Pointer::default();
        self.hits.clear();
        self.tooltip = None;
        self.tooltip_pending = None;
    }

    fn sync_motion(&mut self, cx: &mut Cx, compact: bool) {
        for modal in &self.stack {
            if let Some(layer) = self.layers.iter_mut().find(|layer| layer.id == modal.id()) {
                layer.kind = modal.kind(compact);
                if layer.pending_open {
                    layer.release_texture(cx);
                    layer.set_open(cx, true);
                }
            }
        }
    }

    fn top_layer(&self) -> Option<&ModalLayer> {
        let id = self.stack.last()?.id();
        self.layers.iter().rev().find(|layer| layer.id == id)
    }

    pub(super) fn accessibility_root(&self) -> Option<DrawListId> {
        if self.top_waits_for_paint() {
            return None;
        }
        self.top_layer()?.list.as_ref().map(|list| list.id())
    }

    fn top_waits_for_paint(&self) -> bool {
        self.top_layer().is_some_and(|layer| {
            !layer.ready || layer.visibility.is_moving() || layer.extent.is_moving()
        })
    }

    /// Reconcile stack mutations before autofocus. Never restore a removed area or
    /// override a newer focus request made by the action that closed the owner.
    pub fn sync_focus(&mut self, cx: &mut Cx) {
        let mut restore = None;
        self.focus_returns.retain_mut(|(id, focus)| {
            let live = self.stack.iter().any(|modal| modal.id() == *id);
            if live {
                // Closing a covered parent transfers its return target to the child;
                // it must not steal focus from the still-open top owner.
                if let Some(inherited) = restore.take() {
                    *focus = inherited;
                }
            } else if restore.is_none() {
                restore = Some(*focus);
            }
            live
        });
        if let Some(focus) = restore {
            if focus != self.focus && cx.clear_key_focus_if(self.focus) {
                let target = if focus.is_valid(cx) {
                    focus
                } else {
                    Area::Empty
                };
                cx.set_key_focus(target);
                self.focus = target;
            }
            self.touch_scroll.reset();
            self.pointer = Pointer::default();
            self.hits.clear();
        }
        for modal in &self.stack {
            if !self.focus_returns.iter().any(|(id, _)| *id == modal.id()) {
                self.focus_returns.push((modal.id(), self.focus));
                self.touch_scroll.reset();
                self.pointer = Pointer::default();
                self.hits.clear();
            }
        }
        if self.stack.is_empty() {
            cx.sweep_unlock(self.locked_area);
        }
    }

    pub fn open(&mut self, cx: &mut Cx, request: OverlayRequest, field_ptr: Option<LivePtr>) {
        // Build the model before attaching its presentation.
        let modal = match request.kind {
            OverlayKind::Menu {
                title,
                anchor,
                entries,
                width,
                align_end,
                sheet,
            } => Modal::Menu(OpenMenu {
                id: request.id,
                owner: request.owner,
                title: title.clone(),
                anchor,
                pages: vec![(title, entries)],
                width,
                align_end,
                sheet,
                scroll: 0.0,
                max_scroll: 0.0,
                viewport: Rect::default(),
                active: None,
            }),
            OverlayKind::Confirm {
                title,
                message,
                confirm_label,
                destructive,
            } => Modal::Confirm {
                id: request.id,
                owner: request.owner,
                title,
                message,
                confirm_label,
                ok_focused: !destructive,
                destructive,
            },
            OverlayKind::Prompt {
                title,
                message,
                initial,
                placeholder,
                confirm_label,
                multiline,
            } => {
                let field = WidgetRef::new_from_ptr(cx, field_ptr);
                let text = field.as_ptext_field();
                field.set_text(cx, &initial);
                text.set_placeholder(cx, &placeholder);
                text.set_multiline(cx, multiline);
                Modal::Prompt {
                    id: request.id,
                    owner: request.owner,
                    title,
                    message,
                    field,
                    confirm_label,
                    focused: false,
                    multiline,
                }
            }
            OverlayKind::Select {
                title,
                anchor,
                options,
                value,
                searchable,
                search_placeholder,
                empty_text,
                min_width,
            } => {
                let search = searchable.then(|| {
                    let field = WidgetRef::new_from_ptr(cx, field_ptr);
                    let text = field.as_ptext_field();
                    text.set_plain(cx);
                    text.set_placeholder(
                        cx,
                        &search_placeholder
                            .clone()
                            .unwrap_or_else(|| tr("common.actions.search")),
                    );
                    field
                });
                let active = value
                    .as_ref()
                    .and_then(|v| options.iter().position(|o| &o.value == v && o.enabled));
                Modal::Select(OpenSelect {
                    id: request.id,
                    owner: request.owner,
                    title,
                    anchor,
                    options,
                    value,
                    search,
                    empty_text,
                    min_width,
                    query: String::new(),
                    scroll: 0.0,
                    max_scroll: 0.0,
                    viewport: Rect::default(),
                    focused: false,
                    active,
                })
            }
        };
        self.push(modal);
    }

    pub fn open_sheet(&mut self, sheet: Box<dyn Sheet>) -> u64 {
        self.next_sheet += 1;
        let id = 1_000_000 + self.next_sheet;
        self.push(Modal::Sheet {
            id,
            sheet,
            scroll: 0.0,
            content_height: 0.0,
            viewport: Rect::default(),
            drag: SheetDrag::default(),
        });
        id
    }

    /// Replaces the open menu's entries (display preferences rebuilt after a toggle).
    pub fn update_menu(&mut self, id: u64, entries: Vec<MenuEntry>) {
        for modal in &mut self.stack {
            if let Modal::Menu(menu) = modal
                && menu.id == id
            {
                let depth = menu.pages.len();
                menu.pages[0].1 = entries.clone();
                // Re-resolve open sub pages by label.
                for level in 1..depth {
                    let title = menu.pages[level].0.clone();
                    let parent = menu.pages[level - 1].1.clone();
                    if let Some(MenuEntry::Submenu { entries, .. }) =
                        parent.iter().find(|e| matches!(e, MenuEntry::Submenu { label, .. } if Some(label) == title.as_ref()))
                    {
                        menu.pages[level].1 = entries.clone();
                    }
                }
            }
        }
    }

    pub fn is_modal(&self) -> bool {
        !self.stack.is_empty()
    }

    pub fn sheet_mut(&mut self, id: u64) -> Option<&mut Box<dyn Sheet>> {
        self.stack.iter_mut().find_map(|m| match m {
            Modal::Sheet { id: sid, sheet, .. } if *sid == id => Some(sheet),
            _ => None,
        })
    }

    fn remove_with<R>(
        &mut self,
        cx: &mut Cx,
        index: usize,
        close: impl FnOnce(&mut Cx, &mut Modal) -> R,
    ) -> R {
        let mut modal = self.stack.remove(index);
        self.touch_scroll.reset();
        self.pointer = Pointer::default();
        self.hits.clear();
        self.sync_focus(cx);
        match &modal {
            Modal::Prompt { field, .. } => field.as_ptext_field().release_focus(cx),
            Modal::Select(select) => {
                if let Some(search) = &select.search {
                    search.as_ptext_field().release_focus(cx);
                }
            }
            _ => {}
        }
        let result = close(cx, &mut modal);
        self.retire(cx, modal);
        result
    }

    fn remove(&mut self, cx: &mut Cx, index: usize) {
        self.remove_with(cx, index, |_, _| ());
    }

    fn pop(&mut self, cx: &mut Cx) {
        if let Some(index) = self.stack.len().checked_sub(1) {
            self.remove(cx, index);
        }
    }
}

/// How long a toast stays: about 3s plus 60ms per character, so two-line messages can be read;
/// errors start at 4s and may stay up to 10s.
pub fn toast_duration(text: &str, error: bool) -> f64 {
    let chars = text.chars().count() as f64;
    let (base, cap) = if error { (4.0, 10.0) } else { (3.0, 8.0) };
    (base + 0.06 * chars).min(cap)
}

/// Whether a new toast may take the slot: an unexpired error, or a toast offering an action,
/// is never replaced by an info toast.
fn toast_replaces(current: Option<&Toast>, error: bool) -> bool {
    error || !current.is_some_and(|t| t.error || t.action.is_some())
}

pub fn toast(overlays: &mut Overlays, cx: &mut Cx, message: String, error: bool) {
    let text = message.trim().to_string();
    if text.is_empty() {
        return;
    }
    if !toast_replaces(overlays.toast.as_ref(), error) {
        overlays.queued_toast = Some(text);
        return;
    }
    show_toast(overlays, cx, text, error, None);
}

/// A toast whose button fixes what it reports; it stays as long as an error toast.
pub fn toast_with_action(
    overlays: &mut Overlays,
    cx: &mut Cx,
    message: String,
    action: Option<ToastAction>,
) {
    let text = message.trim().to_string();
    if !text.is_empty() {
        show_toast(overlays, cx, text, false, action);
    }
}

fn show_toast(
    overlays: &mut Overlays,
    cx: &mut Cx,
    text: String,
    error: bool,
    action: Option<ToastAction>,
) {
    if let Some(previous) = overlays.toast.take() {
        cx.stop_timer(previous.timer);
    }
    let remaining = toast_duration(&text, error || action.is_some());
    let timer = cx.start_timeout(remaining);
    let started = cx.seconds_since_app_start();
    overlays.toast = Some(Toast {
        message: text,
        error,
        action,
        timer,
        remaining,
        started,
        paused: false,
        rect: Rect::default(),
        action_rect: Rect::default(),
    });
    overlays.area.redraw(cx);
}

/// Removes the toast; a queued info toast takes its place.
fn dismiss_toast(overlays: &mut Overlays, cx: &mut Cx) {
    if let Some(toast) = overlays.toast.take() {
        cx.stop_timer(toast.timer);
    }
    match overlays.queued_toast.take() {
        Some(next) => show_toast(overlays, cx, next, false, None),
        None => overlays.area.redraw(cx),
    }
}

/// The toast is drawn above tool sheets, window modals and the Command Center.
/// Dispatch it before those surfaces; only pointer events on its visible body are owned.
pub fn handle_toast(shell: &mut PaseoShell, cx: &mut Cx, event: &Event) -> bool {
    match toast_input(&mut shell.data.overlays, cx, event, shell.data.os_window) {
        ToastPress::None => false,
        ToastPress::Captured | ToastPress::Dismissed => true,
        ToastPress::Action(fix) => {
            crate::shell::permissions::fix(fix);
            true
        }
    }
}

fn toast_input(
    overlays: &mut Overlays,
    cx: &mut Cx,
    event: &Event,
    window: Option<WindowId>,
) -> ToastPress {
    let Some(toast) = overlays.toast.as_mut() else {
        return ToastPress::None;
    };
    if !overlays.area.is_valid(cx) {
        return ToastPress::None;
    }
    let (at, press) = match event {
        Event::MouseMove(e) if Some(e.window_id) == window => (Some(e.abs), false),
        Event::MouseLeave(e) if Some(e.window_id) == window => (None, false),
        Event::MouseDown(e) if Some(e.window_id) == window => (Some(e.abs), true),
        Event::Scroll(e) if Some(e.window_id) == window && toast.rect.contains(e.abs) => {
            e.handled_x.set(true);
            e.handled_y.set(true);
            return ToastPress::Captured;
        }
        Event::TouchUpdate(e) if Some(e.window_id) == window && !e.cancelled => {
            let started = e.touches.iter().find(|t| {
                matches!(t.state, makepad_widgets::event::TouchState::Start)
                    && toast.rect.contains(t.abs)
            });
            match started {
                Some(touch) => (Some(touch.abs), true),
                None => return ToastPress::None,
            }
        }
        _ => return ToastPress::None,
    };
    let inside = at.is_some_and(|at| toast.rect.contains(at));
    if press {
        if !inside {
            return ToastPress::None;
        }
        let action = toast
            .action
            .as_ref()
            .filter(|_| at.is_some_and(|at| toast.action_rect.contains(at)))
            .map(|action| action.fix);
        dismiss_toast(overlays, cx);
        return match action {
            Some(fix) => ToastPress::Action(fix),
            None => ToastPress::Dismissed,
        };
    }
    let now = cx.seconds_since_app_start();
    if inside && !toast.paused {
        cx.stop_timer(toast.timer);
        toast.remaining = (toast.remaining - (now - toast.started)).max(0.0);
        toast.paused = true;
    } else if !inside && toast.paused {
        // Leaving a hovered toast gives the reader at least another second and a half.
        toast.remaining = toast.remaining.max(1.5);
        toast.timer = cx.start_timeout(toast.remaining);
        toast.started = now;
        toast.paused = false;
    }
    if inside {
        cx.set_cursor(MouseCursor::Hand);
        ToastPress::Captured
    } else {
        ToastPress::None
    }
}

pub fn handle_timers(overlays: &mut Overlays, cx: &mut Cx, event: &Event) {
    if let Event::KeyFocus(focus) = event {
        let owner = if focus.focus == Area::Empty {
            focus.prev
        } else {
            focus.focus
        };
        if let (Some(owner), Some(overlay)) = (owner.draw_list_id(), overlays.area.draw_list_id())
            && cx.draw_lists[owner].pass_id == cx.draw_lists[overlay].pass_id
        {
            overlays.focus = focus.focus;
        }
    }
    if matches!(
        event,
        Event::TextHandle(_) | Event::Pause | Event::AppLostFocus
    ) || matches!(event, Event::TouchUpdate(update) if update.cancelled)
    {
        overlays.touch_scroll.reset();
        overlays.pointer = Pointer::default();
        if let Some(Modal::Sheet { drag, .. }) = overlays.stack.last_mut() {
            drag.last_y = None;
        }
    }
    if let Some(toast) = &overlays.toast
        && toast.timer.is_event(event).is_some()
    {
        dismiss_toast(overlays, cx);
    }
    if overlays.tooltip_timer.is_event(event).is_some() {
        overlays.tooltip = overlays.tooltip_pending.take();
        overlays.area.redraw(cx);
    }
    overlays.advance(cx, event);
}

/// Chrome hover → tooltip anchored to the hovered control after the theme's 500ms
/// (`TooltipThemeData.waitDuration`).
pub fn hover_tooltip(shell: &mut PaseoShell, cx: &mut Cx, hover: Option<String>, abs: DVec2) {
    let hover_id = hover.clone();
    let tip = hover.and_then(|id| {
        let text = shell.data.tooltips.get(&id).cloned()?;
        let anchor = shell.data.hits.rect_of(&id).unwrap_or(Rect {
            pos: abs,
            size: DVec2::default(),
        });
        Some(Tip {
            text,
            anchor,
            wait: 0.5,
        })
    });
    let owner = tip.as_ref().and(hover_id);
    show_tooltip(&mut shell.data.overlays, cx, tip);
    shell.data.overlays.tooltip_owner = owner;
}

/// Shows `tip` after its wait, or hides the tooltip (`None`). Feature roots reach this through
/// `common::overlay::{tooltip, tooltip_at}`.
pub fn show_tooltip(overlays: &mut Overlays, cx: &mut Cx, tip: Option<Tip>) {
    let tip = tip.filter(|t| !t.text.is_empty());
    if overlays.tooltip_pending.is_some()
        && overlays.tooltip_pending.as_ref().map(|t| &t.text) == tip.as_ref().map(|t| &t.text)
    {
        return;
    }
    overlays.tooltip_owner = None;
    cx.stop_timer(overlays.tooltip_timer);
    let had = overlays.tooltip.take().is_some();
    overlays.tooltip_pending = None;
    if let Some(tip) = tip {
        overlays.tooltip_timer = cx.start_timeout(tip.wait.max(0.0));
        overlays.tooltip_pending = Some(tip);
    }
    if had {
        overlays.area.redraw(cx);
    }
}

// ---------------------------------------------------------------------------------------------
// Geometry

fn menu_row_height(compact: bool) -> f64 {
    super::chrome::text_line_height(20.0, 14.0) + if compact { 20.0 } else { 8.0 }
}

fn entries_height(entries: &[MenuEntry], compact: bool) -> f64 {
    let mut h = 8.0;
    for entry in entries {
        h += match entry {
            MenuEntry::Separator => 9.0,
            MenuEntry::Label(_) => 12.0 + super::chrome::text_line_height(16.0, 12.0),
            MenuEntry::Item(item) => {
                menu_row_height(compact)
                    + if item.description.is_some() {
                        super::chrome::text_line_height(16.0, 12.0)
                    } else {
                        0.0
                    }
            }
            MenuEntry::Submenu { .. } => menu_row_height(compact),
        };
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Drawing

fn overlay_ui(shell: &mut PaseoShell) -> Ui<'_> {
    let compact = shell.data.compact;
    let overlays = &mut shell.data.overlays;
    Ui {
        painter: &mut shell.overlay_painter,
        draw_icon: &mut shell.overlay_icon,
        hits: &mut overlays.hits,
        tooltips: &mut overlays.tooltips,
        hover: overlays.pointer.hover.as_deref(),
        pressed: overlays.pointer.pressed.as_deref(),
        p: theme::current_palette(),
        t: theme::current_typography(),
        compact,
        semantics: super::ui::HitScope::Overlay(overlays.accessibility_owner),
    }
}

pub fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, window: Rect) {
    shell.data.overlays.sync_focus(cx);
    shell.data.overlays.sync_motion(cx, shell.data.compact);
    let full_window = window;
    // Overlay roots allocate their own system-bar padding. Only remove the
    // keyboard's additional coverage here; it may already be consumed natively.
    let insets = crate::shell::platform::safe_insets();
    let window = rect(
        window.pos.x,
        window.pos.y,
        window.size.x,
        (window.size.y - (insets.bottom_occlusion() - insets.bottom)).max(0.0),
    );
    for layer in &mut shell.data.overlays.layers {
        if layer.window != window {
            layer.release_texture(cx);
        }
    }
    shell
        .overlay_list
        .begin_overlay_last_ranked(cx, OverlayRank::Shell);
    cx.begin_turtle(Walk::abs_rect(full_window), Layout::default());
    shell.data.overlays.hits.clear();
    shell.data.overlays.tooltips.clear();
    // Full-window capture area for the overlay layer.
    shell.data.overlays.area = shell.overlay_painter.capture_box(
        cx,
        full_window,
        Vec4::default(),
        Vec4::default(),
        0.0,
        0.0,
    );

    crate::shell::chrome::draw_compact_explorer(shell, cx, scope, window);

    let mut layers = std::mem::take(&mut shell.data.overlays.layers);
    let top_id = shell.data.overlays.stack.last().map(Modal::id);
    for layer in &mut layers {
        let index = shell
            .data
            .overlays
            .stack
            .iter()
            .position(|modal| modal.id() == layer.id);
        let modal = match (index, &layer.closing) {
            (Some(index), _) => &shell.data.overlays.stack[index],
            (_, Some(modal)) => modal,
            _ => continue,
        };
        let scrim = modal.scrim(shell.data.compact);
        let draggable = shell.data.compact
            && matches!(
                modal,
                Modal::Sheet { sheet, .. } if matches!(sheet.style(), SheetStyle::Draggable { .. })
            );
        let top = top_id == Some(layer.id);
        let motion = layer.visibility.is_moving() || layer.extent.is_moving();
        let fading = layer.visibility.is_moving() && !matches!(layer.kind, ModalKind::Sheet);
        new_layer(shell, cx);
        layer.scrim = Area::Empty;
        if scrim.w > 0.0 {
            let draw = &mut shell.overlay_painter.draw_box;
            draw.paint(cx, window, scrim, 0.0);
            layer.scrim = draw.area();
        }
        if top {
            shell.data.overlays.hits.clear();
            shell
                .data
                .overlays
                .hits
                .push(window, "overlay:backdrop".into());
        }

        layer.window = window;
        layer.inset_top = insets.top;
        if draggable && layer.layout_extent.is_none() {
            layer.layout_extent = Some(DRAG_SNAPS[1]);
            layer.extent.snap(DRAG_SNAPS[1]);
        }

        // Only input/layout mutations paint the source; NextFrame composites in `advance`.
        let mut remeasure = false;
        let parent = shell.overlay_list.id();
        if layer.texture.is_none() {
            let list = layer.list.get_or_insert_with(|| DrawList2d::new(cx));
            if list.begin_maybe(cx, index.is_some()).is_ok() {
                list.set_view_transform(cx, &Mat4::identity());
                if let Some(index) = index {
                    new_layer(shell, cx);
                    let previous = sheet_height(&shell.data.overlays.stack[index]);
                    layer.bounds = draw_modal(
                        shell,
                        cx,
                        scope,
                        index,
                        window,
                        layer.layout_extent.unwrap_or_default(),
                        top,
                        top && !motion,
                    );
                    remeasure = sheet_height(&shell.data.overlays.stack[index]) != previous;
                }
                list.end(cx);
            }
            if remeasure {
                // One layout correction for measured content, not an animation tick.
                // Do not snapshot an adaptive dialog with its guessed initial height.
                cx.redraw_list_in_draw(parent);
                if fading {
                    cx.draw_lists[parent].detach_sub_list(list.id());
                }
            } else if fading {
                // Keep child draw calls at settled window geometry. Draw only
                // the bounded, composite quad into the window during a fade.
                list.set_view_transform(cx, &Mat4::identity());
                let bounds =
                    crate::common::paint::intersect(inset(layer.bounds, -12.0, -12.0), window);
                if bounds.size.x > 0.0 && bounds.size.y > 0.0 {
                    let texture = LayerTexture::capture_bounds(cx, list.id(), bounds);
                    layer.texture = Some((texture, bounds));
                    cx.draw_lists[parent].detach_sub_list(list.id());
                }
            }
        }
        if let Some((texture, bounds)) = &layer.texture {
            let (offset, scale, opacity, _) = layer.transform();
            layer.quad = shell.overlay_composite.draw_texture(
                cx,
                layer.quad_rect(*bounds, offset, scale),
                &texture.texture,
                opacity as f32,
            );
            shell.overlay_composite.draw_vars.empty_texture(0);
        }
        if motion {
            layer.composite(cx);
        }
        layer.ready = !motion && !remeasure;
    }
    shell.data.overlays.layers = layers;
    // Standalone tool-call sheets (timeline `showToolCallSheet`) above the modal stack.
    crate::features::timeline::draw_tool_call_sheets(cx, &shell.data.window_id, window);
    new_layer(shell, cx);
    draw_toast(shell, cx, window);
    draw_tooltip(shell, cx, window);
    cx.end_turtle();
    shell.overlay_list.end(cx);
    shell.data.overlays.locked = shell.data.overlays.is_modal()
        || (shell.data.compact && shell.data.chrome.explorer_visible());
}

fn argb_scrim(compact: bool) -> Vec4 {
    theme::argb(if compact { 0x73000000 } else { 0x8c000000 })
}

fn sheet_height(modal: &Modal) -> Option<f64> {
    match modal {
        Modal::Sheet { content_height, .. } => Some(*content_height),
        _ => None,
    }
}

fn new_layer(shell: &mut PaseoShell, cx: &mut Cx2d) {
    let group = shell.overlay_painter.new_layer(cx);
    shell.overlay_icon.draw_vars.set_draw_call_group(group);
}

#[allow(clippy::too_many_arguments)]
fn draw_modal(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    index: usize,
    window: Rect,
    extent: f64,
    top: bool,
    autofocus: bool,
) -> Rect {
    let compact = shell.data.compact;
    let mut modal = std::mem::replace(
        &mut shell.data.overlays.stack[index],
        Modal::Confirm {
            id: 0,
            owner: String::new(),
            title: String::new(),
            message: String::new(),
            confirm_label: String::new(),
            destructive: false,
            ok_focused: true,
        },
    );
    if cx.accessibility_enabled() {
        shell.data.overlays.accessibility_owner = modal.accessibility_scope();
    }
    crate::shell::sheets::set_fields_active(autofocus);
    let bounds = match &mut modal {
        Modal::Menu(menu) => draw_menu(shell, cx, menu, window, compact, top),
        Modal::Confirm {
            title,
            message,
            confirm_label,
            destructive,
            ok_focused,
            ..
        } => draw_confirm(
            shell,
            cx,
            window,
            title,
            message,
            confirm_label,
            *destructive,
            *ok_focused,
            top,
        ),
        Modal::Prompt {
            title,
            message,
            field,
            confirm_label,
            focused,
            multiline,
            ..
        } => {
            let bounds = draw_prompt(
                shell,
                cx,
                scope,
                window,
                title,
                message.as_deref(),
                field,
                confirm_label,
                *multiline,
                top,
            );
            if autofocus && !*focused {
                *focused = true;
                field.as_ptext_field().set_key_focus(cx);
                field.as_ptext_field().select_all(cx);
            }
            bounds
        }
        Modal::Select(select) => {
            let bounds = draw_select(shell, cx, scope, select, window, compact, top);
            // Original: the search field autofocuses on wide layouts only.
            if autofocus && !select.focused {
                select.focused = true;
                if let Some(search) = select.search.as_ref().filter(|_| !compact) {
                    search.as_ptext_field().set_key_focus(cx);
                }
            }
            bounds
        }
        Modal::Sheet {
            sheet,
            scroll,
            content_height,
            viewport,
            drag,
            ..
        } => {
            let state = scope
                .data
                .get::<UiContext>()
                .map(|ui| ui.state.clone())
                .unwrap_or_default();
            sheet.sync(shell, &state);
            draw_sheet(
                shell,
                cx,
                scope,
                window,
                sheet.as_mut(),
                scroll,
                content_height,
                viewport,
                drag,
                extent,
                top,
            )
        }
    };
    crate::shell::sheets::set_fields_active(true);
    shell.data.overlays.accessibility_owner = 0;
    shell.data.overlays.stack[index] = modal;
    bounds
}

fn menu_rect(menu: &OpenMenu, window: Rect, height: f64) -> Rect {
    let width = menu.width.max(0.0).min((window.size.x - 16.0).max(0.0));
    let a = menu.anchor;
    let x = if menu.align_end {
        a.pos.x + a.size.x - width
    } else {
        a.pos.x
    };
    let below = a.pos.y + a.size.y + 4.0;
    let h = height.max(0.0).min((window.size.y - 16.0).max(0.0));
    let y = if below + h <= window.pos.y + window.size.y - 8.0 {
        below
    } else {
        a.pos.y - 4.0 - h
    };
    fit_in_viewport(rect(x, y, width, h), window, 8.0)
}

fn draw_menu(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    menu: &mut OpenMenu,
    window: Rect,
    compact: bool,
    top: bool,
) -> Rect {
    let sheet = compact && menu.sheet;
    let depth = menu.pages.len();
    let (page_title, entries) = menu.pages[depth - 1].clone();
    let header = sheet || depth > 1;
    let header_h = if header {
        super::chrome::text_line_height(24.0, 14.0) + if sheet { 16.0 } else { 20.0 }
    } else {
        0.0
    };
    let content_h = entries_height(&entries, compact);
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let (outer, surface) = if sheet {
        // Keep the background edge-to-edge, but reserve the render surface's remaining
        // system-bar overlap below the menu content (already-consumed insets are zero).
        let insets = crate::shell::platform::safe_insets();
        let h = (content_h + header_h + 24.0 + insets.bottom)
            .min(window.size.y * 0.8)
            .min((window.size.y - insets.top).max(0.0));
        let r = rect(
            window.pos.x,
            window.pos.y + window.size.y - h,
            window.size.x,
            h,
        );
        ui.painter.draw_box.paint_bordered(
            cx,
            rect(r.pos.x, r.pos.y, r.size.x, r.size.y + 12.0),
            p.surface0,
            p.border,
            1.0,
            12.0,
        );
        ui.sheet_handle(cx, r.pos.x + r.size.x * 0.5, r.pos.y);
        (
            r,
            rect(
                r.pos.x,
                r.pos.y + 24.0,
                r.size.x,
                (r.size.y - 24.0 - insets.bottom).max(0.0),
            ),
        )
    } else {
        let r = menu_rect(menu, window, content_h + header_h);
        // md shadow then the surface1 popover with a borderAccent rule.
        let shadow = if p.is_dark {
            theme::argb(0x33000000)
        } else {
            theme::argb(0x0a000000)
        };
        ui.fill(
            cx,
            rect(r.pos.x - 2.0, r.pos.y + 2.0, r.size.x + 4.0, r.size.y + 6.0),
            shadow,
            10.0,
        );
        ui.painter
            .outline(cx, r, p.surface1, p.border_accent, 1.0, 8.0);
        (r, r)
    };
    // Compact sheets register their whole painted frame (handle band and bottom inset included)
    // so a tap anywhere on the sheet never falls through to the backdrop.
    if top {
        ui.hit(outer, "menu:surface");
    }
    let mut y = surface.pos.y;
    if header {
        let title = page_title
            .clone()
            .or(menu.title.clone())
            .unwrap_or_default();
        let mut x = surface.pos.x + 12.0;
        if depth > 1 {
            let back = rect(x, y + 4.0, 24.0, 24.0);
            ui.icon(cx, back, "chevron-left", 18.0, p.muted);
            if top {
                ui.hit_tip(cx, back, "menu:back", &tr("common.actions.back"));
            }
            x += 32.0;
        }
        let spec = ui.spec(Font::Medium, 14.0, p.foreground);
        ui.text(
            cx,
            rect(
                x,
                y + 4.0,
                surface.size.x - (x - surface.pos.x) - 12.0,
                header_h - if sheet { 16.0 } else { 20.0 },
            ),
            spec,
            &title,
        );
        ui.semantic_text(
            cx,
            rect(
                x,
                y + 4.0,
                surface.size.x - (x - surface.pos.x) - 12.0,
                header_h - if sheet { 16.0 } else { 20.0 },
            ),
            "menu:title",
            &title,
            AccessibilityRole::Header,
        );
        y += header_h - 8.0;
    }
    // The header stays fixed while tall menus scroll within their visible, touchable body.
    let viewport = rect(
        surface.pos.x,
        y,
        surface.size.x,
        (surface.pos.y + surface.size.y - y).max(0.0),
    );
    menu.viewport = viewport;
    menu.max_scroll = (content_h - viewport.size.y).max(0.0);
    menu.scroll = menu.scroll.clamp(0.0, menu.max_scroll);
    cx.begin_turtle(
        Walk::abs_rect(viewport),
        Layout {
            clip_x: true,
            clip_y: true,
            ..Layout::default()
        },
    );
    ui.hits.set_clip(Some(viewport));
    y += 4.0 - menu.scroll;
    let row_h = menu_row_height(compact);
    let small_h = super::chrome::text_line_height(16.0, 12.0);
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            MenuEntry::Separator => {
                ui.menu_separator(cx, surface.pos.x, y, surface.size.x);
                y += 9.0;
            }
            MenuEntry::Label(text) => {
                let spec = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(
                    cx,
                    rect(
                        surface.pos.x + 12.0,
                        y + 8.0,
                        surface.size.x - 24.0,
                        small_h,
                    ),
                    spec,
                    text,
                );
                y += small_h + 12.0;
            }
            MenuEntry::Item(item) => {
                let h = row_h
                    + if item.description.is_some() {
                        small_h
                    } else {
                        0.0
                    };
                let id = if top {
                    format!("menu:item:{i}")
                } else {
                    String::new()
                };
                let lead = match (&item.svg, &item.icon, item.dot) {
                    (_, _, Some(color)) => RowLead::Dot(color),
                    (Some(svg), _, None) => RowLead::Svg(svg),
                    (None, Some(icon), None) => RowLead::Glyph(icon),
                    (None, None, None) => RowLead::None,
                };
                ui.menu_row(
                    cx,
                    rect(surface.pos.x, y, surface.size.x, row_h),
                    &id,
                    &item.label,
                    lead,
                    item.hint.as_deref(),
                    item.enabled && top,
                    item.muted,
                    item.destructive,
                    item.checked,
                    top && menu.active == Some(i),
                );
                if let Some(description) = &item.description {
                    let spec = ui.spec(Font::Regular, 12.0, p.muted);
                    ui.text(
                        cx,
                        rect(
                            surface.pos.x + 36.0,
                            y + row_h - 4.0,
                            surface.size.x - 48.0,
                            small_h,
                        ),
                        spec,
                        description,
                    );
                    if top && item.enabled {
                        ui.hit(rect(surface.pos.x, y, surface.size.x, h), &id);
                    }
                }
                y += h;
            }
            MenuEntry::Submenu {
                label,
                icon,
                value,
                indicator,
                ..
            } => {
                let r = rect(surface.pos.x, y, surface.size.x, row_h);
                let id = format!("menu:sub:{i}");
                let lead = icon.as_deref().map_or(RowLead::None, RowLead::Glyph);
                ui.menu_row(
                    cx,
                    r,
                    &id,
                    label,
                    lead,
                    None,
                    top,
                    false,
                    false,
                    false,
                    top && menu.active == Some(i),
                );
                let mut right = r.pos.x + r.size.x - 12.0;
                ui.icon(
                    cx,
                    rect(right - 14.0, r.pos.y + (r.size.y - 14.0) * 0.5, 14.0, 14.0),
                    "chevron-right",
                    14.0,
                    p.muted,
                );
                right -= 20.0;
                if *indicator {
                    ui.dot(
                        cx,
                        dvec2(right - 3.0, r.pos.y + r.size.y * 0.5),
                        6.0,
                        p.accent_bright,
                    );
                    right -= 12.0;
                }
                if let Some(value) = value {
                    let spec = ui.spec(Font::Regular, 14.0, p.muted);
                    ui.text_right(
                        cx,
                        rect(
                            r.pos.x + r.size.x * 0.5,
                            r.pos.y,
                            right - r.pos.x - r.size.x * 0.5,
                            r.size.y,
                        ),
                        spec,
                        value,
                    );
                }
                y += row_h;
            }
        }
    }
    ui.hits.set_clip(None);
    cx.end_turtle();
    outer
}

/// `_SelectList` retains 8px padding above and below the scaled text line.
fn select_row_height() -> f64 {
    16.0 + super::chrome::text_line_height(20.0, 14.0)
}

fn select_list_height(select: &OpenSelect, visible: &[usize], sheet: bool) -> f64 {
    if visible.is_empty() {
        return 16.0 + super::chrome::text_line_height(20.0, 14.0);
    }
    let mut h = if sheet { 4.0 + 32.0 } else { 8.0 };
    for &i in visible {
        if select.options[i].separator_before && select.query.is_empty() {
            h += 9.0;
        }
        h += select_row_height() + if sheet { 4.0 } else { 0.0 };
    }
    h
}

/// Original `showPaseoSelect`: anchored surface0 popover (8px radius, divider border, md
/// shadow, 400px max) on wide layouts; a surface0 bottom sheet with a drag handle and the
/// medium title on compact ones. The optional search row filters label and description.
fn draw_select(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    select: &mut OpenSelect,
    window: Rect,
    sheet: bool,
    top: bool,
) -> Rect {
    let visible = select.visible();
    let line_h = super::chrome::text_line_height(20.0, 14.0);
    let search_h = if select.search.is_some() {
        25.0 + line_h
    } else {
        0.0
    };
    let list_h = select_list_height(select, &visible, sheet);
    let (field_rect, bounds) = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        let (surface, mut y) = if sheet {
            let title_h = line_h + 8.0;
            let h = (48.0 + title_h + search_h + list_h).min((window.size.y - 24.0).max(0.0));
            let r = rect(
                window.pos.x,
                window.pos.y + window.size.y - h,
                window.size.x,
                h,
            );
            ui.painter.draw_box.paint(
                cx,
                rect(r.pos.x, r.pos.y, r.size.x, r.size.y + 16.0),
                p.surface0,
                16.0,
            );
            // Material 3 drag handle: 32x4 in a 48px band, onSurfaceVariant at 40%.
            ui.fill(
                cx,
                rect(r.pos.x + r.size.x * 0.5 - 16.0, r.pos.y + 22.0, 32.0, 4.0),
                with_alpha(p.muted, 0.4),
                2.0,
            );
            let spec = ui.spec(Font::Medium, 14.0, p.foreground);
            ui.text(
                cx,
                rect(r.pos.x + 24.0, r.pos.y + 48.0, r.size.x - 48.0, line_h),
                spec,
                &select.title,
            );
            (r, r.pos.y + 48.0 + title_h)
        } else {
            let a = select.anchor;
            let width = a
                .size
                .x
                .max(select.min_width)
                .min((window.size.x - 16.0).max(0.0));
            let bottom = window.pos.y + window.size.y;
            let below = bottom - (a.pos.y + a.size.y) - 8.0;
            let above = a.pos.y - window.pos.y;
            let open_up = below < 240.0 && above > below;
            let left = if a.pos.x + width > window.pos.x + window.size.x - 8.0 {
                (window.pos.x + window.size.x - 8.0 - width).max(window.pos.x + 8.0)
            } else {
                a.pos.x
            };
            let max_h = (if open_up { above - 12.0 } else { below - 4.0 }).clamp(120.0, 400.0);
            let h = (search_h + list_h).min(max_h);
            let y = if open_up {
                a.pos.y - 4.0 - h
            } else {
                a.pos.y + a.size.y + 4.0
            };
            let r = fit_in_viewport(rect(left, y, width, h), window, 8.0);
            // BoxShadow(0x33000000, offset (0, 4), blur 8), approximated by two soft layers.
            ui.fill(
                cx,
                rect(r.pos.x - 2.0, r.pos.y + 2.0, r.size.x + 4.0, r.size.y + 6.0),
                theme::argb(0x14000000),
                10.0,
            );
            ui.fill(
                cx,
                rect(r.pos.x, r.pos.y + 3.0, r.size.x, r.size.y + 2.0),
                theme::argb(0x1f000000),
                9.0,
            );
            ui.painter.outline(cx, r, p.surface0, p.border, 1.0, 8.0);
            (r, r.pos.y)
        };
        if top {
            ui.hit(surface, "select:surface");
        }
        let mut field_rect = None;
        if select.search.is_some() {
            let inset_x = if sheet { 4.0 } else { 0.0 };
            let row = rect(
                surface.pos.x + inset_x,
                y,
                surface.size.x - inset_x * 2.0,
                search_h,
            );
            // The popover clips its children to the 8px corners: round the top, square the rest.
            if sheet {
                ui.fill(
                    cx,
                    rect(row.pos.x, row.pos.y, row.size.x, row.size.y - 1.0),
                    p.surface1,
                    0.0,
                );
            } else {
                let inner = rect(
                    row.pos.x + 1.0,
                    row.pos.y + 1.0,
                    row.size.x - 2.0,
                    row.size.y - 2.0,
                );
                ui.fill(
                    cx,
                    rect(inner.pos.x, inner.pos.y, inner.size.x, 16.0),
                    p.surface1,
                    7.0,
                );
                ui.fill(
                    cx,
                    rect(
                        inner.pos.x,
                        inner.pos.y + 8.0,
                        inner.size.x,
                        inner.size.y - 8.0,
                    ),
                    p.surface1,
                    0.0,
                );
            }
            ui.painter.hline(
                cx,
                row.pos.x,
                row.pos.y + row.size.y - 1.0,
                row.size.x,
                p.border,
            );
            ui.icon(
                cx,
                rect(
                    row.pos.x + 12.0,
                    row.pos.y + (search_h - 1.0 - 16.0) * 0.5,
                    16.0,
                    16.0,
                ),
                "search",
                16.0,
                p.muted,
            );
            field_rect = Some(rect(
                row.pos.x + 12.0 + 16.0 + 8.0,
                row.pos.y,
                row.size.x - 12.0 - 16.0 - 8.0 - 12.0,
                search_h - 1.0,
            ));
            y += search_h;
        }
        let viewport = rect(
            surface.pos.x,
            y,
            surface.size.x,
            (surface.pos.y + surface.size.y - y).max(0.0),
        );
        select.viewport = viewport;
        select.max_scroll = (list_h - viewport.size.y).max(0.0);
        select.scroll = select.scroll.clamp(0.0, select.max_scroll);
        cx.begin_turtle(
            Walk::abs_rect(viewport),
            Layout {
                clip_x: true,
                clip_y: true,
                ..Layout::default()
            },
        );
        let mut ry = viewport.pos.y - select.scroll;
        if visible.is_empty() {
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            ui.text(
                cx,
                rect(
                    viewport.pos.x + 12.0,
                    ry + 8.0,
                    viewport.size.x - 24.0,
                    line_h,
                ),
                spec,
                &select.empty_text,
            );
        } else {
            ry += 4.0;
            let (mx, mb) = if sheet { (8.0 + 4.0, 4.0) } else { (0.0, 0.0) };
            for &i in &visible {
                let option = &select.options[i];
                if option.separator_before && select.query.is_empty() {
                    ui.painter.hline(
                        cx,
                        viewport.pos.x + mx - if sheet { 4.0 } else { 0.0 },
                        ry + 4.0,
                        viewport.size.x - (mx - if sheet { 4.0 } else { 0.0 }) * 2.0,
                        p.border,
                    );
                    ry += 9.0;
                }
                let r = rect(
                    viewport.pos.x + mx,
                    ry,
                    viewport.size.x - mx * 2.0,
                    select_row_height(),
                );
                let id = format!("select:item:{i}");
                let selected = select.value.as_deref() == Some(option.value.as_str());
                draw_select_row(
                    &mut ui,
                    cx,
                    r,
                    &id,
                    option,
                    selected,
                    top && select.active == Some(i),
                );
                let clipped = clip_rect(r, viewport);
                ui.control(
                    cx,
                    clipped,
                    &id,
                    &option.label,
                    AccessibilityRole::ListItem,
                    AccessibilityState {
                        enabled: top && option.enabled,
                        selected,
                        checked: selected,
                        focused: top && select.active == Some(i),
                    },
                );
                ry += r.size.y + mb;
            }
        }
        cx.end_turtle();
        (field_rect, surface)
    };
    if let (Some(field), Some(r)) = (select.search.as_ref(), field_rect) {
        cx.begin_turtle(Walk::abs_rect(r), Layout::default());
        field.draw_all(cx, scope);
        cx.end_turtle();
    }
    bounds
}

/// `_SelectItem`: 12/8 padding, surface1 while hovered or pressed, 16px leading slot, 14px
/// label with a muted 12px description after it, trailing slot and a muted check when chosen,
/// 55% opacity when disabled.
fn draw_select_row(
    ui: &mut Ui,
    cx: &mut Cx2d,
    r: Rect,
    id: &str,
    option: &SelectOption,
    selected: bool,
    active: bool,
) {
    let p = ui.p;
    let alpha = if option.enabled { 1.0 } else { 0.55 };
    if option.enabled && (ui.is_hover(id) || ui.is_pressed(id) || active) {
        ui.fill(cx, r, p.surface1, 0.0);
    }
    let mut x = r.pos.x + 12.0;
    let mut right = r.pos.x + r.size.x - 12.0;
    let mid = r.pos.y + r.size.y * 0.5;
    if let Some(icon) = &option.icon {
        ui.icon(
            cx,
            rect(x, mid - 8.0, 16.0, 16.0),
            icon,
            option.icon_size,
            with_alpha(p.muted, alpha),
        );
        x += 16.0 + 8.0;
    } else if let Some(dot) = option.dot {
        ui.dot(cx, dvec2(x + 8.0, mid), 8.0, with_alpha(dot, dot.w * alpha));
        x += 16.0 + 8.0;
    }
    if selected {
        ui.icon(
            cx,
            rect(right - 16.0, mid - 8.0, 16.0, 16.0),
            "check",
            16.0,
            with_alpha(p.muted, alpha),
        );
        right -= 16.0 + 8.0;
    }
    if let Some(trailing) = &option.trailing {
        let spec = ui.spec(Font::Regular, 12.0, with_alpha(p.muted, alpha));
        let w = ui.measure(cx, spec, trailing);
        ui.text_right(cx, rect(right - w, r.pos.y, w, r.size.y), spec, trailing);
        right -= w + 8.0;
    }
    let label_spec = ui.spec(Font::Regular, 14.0, with_alpha(p.foreground, alpha));
    let avail = (right - x).max(0.0);
    let label_w = ui.measure(cx, label_spec, &option.label).min(avail);
    ui.text(
        cx,
        rect(x, r.pos.y, label_w, r.size.y),
        label_spec,
        &option.label,
    );
    if let Some(description) = &option.description {
        let dx = x + label_w + 8.0;
        if right - dx > 8.0 {
            let spec = ui.spec(Font::Regular, 12.0, with_alpha(p.muted, alpha));
            ui.text(
                cx,
                rect(dx, r.pos.y + 1.0, right - dx, r.size.y),
                spec,
                description,
            );
        }
    }
}

fn clip_rect(r: Rect, clip: Rect) -> Rect {
    let y0 = r.pos.y.max(clip.pos.y);
    let y1 = (r.pos.y + r.size.y).min(clip.pos.y + clip.size.y);
    rect(r.pos.x, y0, r.size.x, (y1 - y0).max(0.0))
}

/// `SheetContents`-style chrome around a dialog body; returns the body rect. `back` shows the
/// leading back chevron (`onBack`), whose click reaches the sheet as `sheet:back`.
#[allow(clippy::too_many_arguments)]
fn dialog_frame(
    ui: &mut Ui,
    cx: &mut Cx2d,
    window: Rect,
    width: f64,
    height: f64,
    title: &str,
    compact: bool,
    top: bool,
    back: bool,
) -> (Rect, Rect) {
    let p = ui.p;
    let title_h = super::chrome::text_line_height(24.0, 14.0);
    let header_h = title_h + 33.0;
    let (outer, frame) = if compact {
        // Like the compact menu sheet: the background runs edge to edge, but the body (and its
        // buttons) stays above the render surface's remaining system-bar overlap.
        let insets = crate::shell::platform::safe_insets();
        let h = (height + header_h + 24.0 + insets.bottom)
            .min(window.size.y * 0.9)
            .min((window.size.y - insets.top).max(0.0));
        let r = rect(
            window.pos.x,
            window.pos.y + window.size.y - h,
            window.size.x,
            h,
        );
        ui.painter.draw_box.paint(
            cx,
            rect(r.pos.x, r.pos.y, r.size.x, r.size.y + 16.0),
            p.surface0,
            16.0,
        );
        if p.surface0.w >= 1.0 {
            crate::common::clock::occlude(
                cx,
                rect(r.pos.x, r.pos.y + 16.0, r.size.x, r.size.y - 16.0),
            );
        }
        ui.sheet_handle(cx, r.pos.x + r.size.x * 0.5, r.pos.y - 4.0);
        (
            r,
            rect(
                r.pos.x,
                r.pos.y + 16.0,
                r.size.x,
                (r.size.y - 16.0 - insets.bottom).max(0.0),
            ),
        )
    } else {
        let w = width.max(0.0).min((window.size.x - 48.0).max(0.0));
        let h = (height + header_h).min(window.size.y * 0.85);
        let r = rect(
            window.pos.x + (window.size.x - w) * 0.5,
            window.pos.y + (window.size.y - h) * 0.5,
            w,
            h,
        );
        ui.painter.outline(cx, r, p.surface1, p.surface2, 1.0, 12.0);
        if p.surface1.w >= 1.0 {
            crate::common::clock::occlude(
                cx,
                rect(r.pos.x, r.pos.y + 12.0, r.size.x, r.size.y - 24.0),
            );
        }
        (r, r)
    };
    // The whole painted surface (the compact handle band and bottom inset included) is inert;
    // only the scrim around it dismisses.
    if top {
        ui.hit(outer, "dialog:surface");
    }
    let spec = ui.spec(Font::Medium, 14.0, p.foreground);
    let mut title_x = frame.pos.x + 24.0;
    if back {
        let r = rect(frame.pos.x + 12.0, frame.pos.y + 12.0, 32.0, 32.0);
        ui.icon_button(
            cx,
            r,
            if top { "sheet:back" } else { "" },
            "chevron-left",
            16.0,
            8.0,
            &if compact {
                String::new()
            } else {
                tr("common.actions.back")
            },
        );
        title_x = r.pos.x + 32.0 + 4.0;
    }
    ui.text(
        cx,
        rect(
            title_x,
            frame.pos.y + 16.0,
            frame.pos.x + frame.size.x - 56.0 - title_x,
            title_h,
        ),
        spec,
        title,
    );
    ui.semantic_text(
        cx,
        rect(
            title_x,
            frame.pos.y + 16.0,
            frame.pos.x + frame.size.x - 56.0 - title_x,
            title_h,
        ),
        "dialog:title",
        title,
        AccessibilityRole::Header,
    );
    let close = rect(
        frame.pos.x + frame.size.x - 16.0 - 32.0,
        frame.pos.y + 12.0,
        32.0,
        32.0,
    );
    ui.icon_button(
        cx,
        close,
        if top { "dialog:close" } else { "" },
        "x",
        16.0,
        8.0,
        &if compact {
            String::new()
        } else {
            tr("common.actions.close")
        },
    );
    ui.painter.hline(
        cx,
        frame.pos.x,
        frame.pos.y + header_h - 1.0,
        frame.size.x,
        p.surface2,
    );
    (
        rect(
            frame.pos.x,
            frame.pos.y + header_h,
            frame.size.x,
            (frame.size.y - header_h).max(0.0),
        ),
        outer,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_confirm(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    window: Rect,
    title: &str,
    message: &str,
    confirm_label: &str,
    destructive: bool,
    ok_focused: bool,
    top: bool,
) -> Rect {
    let compact = shell.data.compact;
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 14.0, p.muted);
    let width: f64 = 420.0;
    let text_w = (width.min(window.size.x - 48.0)) - 48.0;
    let text_h = ui.painter.paragraph_height(
        cx,
        if compact {
            window.size.x - 48.0
        } else {
            text_w
        },
        spec,
        message,
    );
    let button_h = theme::control_height(ControlSize::Sm);
    let (body, bounds) = dialog_frame(
        &mut ui,
        cx,
        window,
        width,
        text_h + 48.0 + button_h,
        title,
        compact,
        top,
        false,
    );
    ui.painter.paragraph(
        cx,
        dvec2(body.pos.x + 24.0, body.pos.y + 16.0),
        body.size.x - 48.0,
        spec,
        message,
    );
    let y = body.pos.y + body.size.y - 16.0 - button_h;
    let cancel = tr("common.actions.cancel");
    let cw = ui.button_width(cx, &cancel, None, ControlSize::Sm);
    let ow = ui.button_width(cx, confirm_label, None, ControlSize::Sm);
    let right = body.pos.x + body.size.x - 24.0;
    let ok = rect(right - ow, y, ow, button_h);
    let cancel_rect = rect(right - ow - 8.0 - cw, y, cw, button_h);
    ui.button(
        cx,
        ok,
        if top { "confirm:ok" } else { "" },
        confirm_label,
        None,
        if destructive {
            Variant::Destructive
        } else {
            Variant::Primary
        },
        ControlSize::Sm,
        top,
    );
    ui.button(
        cx,
        cancel_rect,
        if top { "confirm:cancel" } else { "" },
        &cancel,
        None,
        Variant::Ghost,
        ControlSize::Sm,
        top,
    );
    // Desktop keyboard focus (Enter activates it; ←/→/Tab move it).
    if top && !compact {
        let focused = if ok_focused { ok } else { cancel_rect };
        let ring = rect(
            focused.pos.x - 2.0,
            focused.pos.y - 2.0,
            focused.size.x + 4.0,
            focused.size.y + 4.0,
        );
        ui.painter.outline(
            cx,
            ring,
            Vec4::default(),
            p.muted,
            1.5,
            (button_h * 0.5 + 2.0).min(14.0),
        );
    }
    bounds
}

#[allow(clippy::too_many_arguments)]
fn draw_prompt(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    title: &str,
    message: Option<&str>,
    field: &WidgetRef,
    confirm_label: &str,
    multiline: bool,
    top: bool,
) -> Rect {
    let compact = shell.data.compact;
    let (field_rect, scroll, bounds) = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        let spec = ui.spec(Font::Regular, 14.0, p.muted);
        let message_w = if compact {
            window.size.x - 48.0
        } else {
            480.0f64.min(window.size.x - 48.0) - 48.0
        };
        let message_h = message.map_or(0.0, |m| {
            ui.painter.paragraph_height(cx, message_w, spec, m) + 12.0
        });
        // `minLines: 4, maxLines: 12`: the field grows with its wrapped text and scrolls past
        // twelve lines, keeping the end (where typing happens) in view.
        let (field_h, scroll) = if multiline {
            let text_spec = ui.spec(Font::Regular, 14.0, p.foreground);
            let line_h = ui.painter.line_height(cx, text_spec);
            let width = (480.0f64.min(window.size.x - 48.0) - 48.0 - 24.0).max(40.0);
            let text = field.text();
            let lines = (ui.painter.paragraph_height(
                cx,
                width,
                text_spec,
                if text.is_empty() { " " } else { &text },
            ) / line_h)
                .ceil()
                .max(1.0);
            let visible = lines.clamp(4.0, 12.0);
            (visible * line_h + 12.0, (lines - visible) * line_h)
        } else {
            (20.0 + super::chrome::text_line_height(20.0, 14.0), 0.0)
        };
        let button_h = theme::control_height(ControlSize::Sm);
        let (body, bounds) = dialog_frame(
            &mut ui,
            cx,
            window,
            480.0,
            message_h + 16.0 + field_h + 4.0 + 40.0 + button_h,
            title,
            compact,
            top,
            false,
        );
        // The action row is reserved first; the message and then the field share what is
        // left above it, so the field (which takes input before the overlay's hit map) never
        // draws or receives taps over the buttons.
        let by = body.pos.y + body.size.y - 16.0 - button_h;
        let limit = by - 12.0;
        let mut y = body.pos.y + 16.0;
        let single_h = 20.0 + super::chrome::text_line_height(20.0, 14.0);
        let message_vis = message_h.min((limit - y - field_h.min(single_h)).max(0.0));
        if let Some(message) = message {
            let clip = rect(body.pos.x + 24.0, y, body.size.x - 48.0, message_vis);
            cx.begin_turtle(
                Walk::abs_rect(clip),
                Layout {
                    clip_x: true,
                    clip_y: true,
                    ..Layout::default()
                },
            );
            ui.painter.paragraph(
                cx,
                dvec2(body.pos.x + 24.0, y),
                body.size.x - 48.0,
                spec,
                message,
            );
            cx.end_turtle();
            y += message_vis;
        }
        let field_vis = field_h.min((limit - y).max(0.0));
        // A shortened multiline field keeps its end (where typing happens) in view.
        let scroll = if multiline {
            scroll + (field_h - field_vis)
        } else {
            scroll
        };
        let field_rect = rect(body.pos.x + 24.0, y, body.size.x - 48.0, field_vis);
        let cancel = tr("common.actions.cancel");
        let cw = ui.button_width(cx, &cancel, None, ControlSize::Sm);
        let ow = ui.button_width(cx, confirm_label, None, ControlSize::Sm);
        let right = body.pos.x + body.size.x - 24.0;
        ui.button(
            cx,
            rect(right - ow, by, ow, button_h),
            if top { "prompt:ok" } else { "" },
            confirm_label,
            None,
            Variant::Primary,
            ControlSize::Sm,
            top,
        );
        ui.button(
            cx,
            rect(right - ow - 8.0 - cw, by, cw, button_h),
            if top { "prompt:cancel" } else { "" },
            &cancel,
            None,
            Variant::Ghost,
            ControlSize::Sm,
            top,
        );
        (field_rect, scroll, bounds)
    };
    cx.begin_turtle(
        Walk::abs_rect(field_rect),
        Layout {
            scroll: dvec2(0.0, scroll),
            clip_x: true,
            clip_y: true,
            ..Layout::default()
        },
    );
    field.draw_all(cx, scope);
    cx.end_turtle();
    bounds
}

#[allow(clippy::too_many_arguments)]
fn draw_sheet(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    sheet: &mut dyn Sheet,
    scroll: &mut f64,
    content_height: &mut f64,
    viewport: &mut Rect,
    drag: &mut SheetDrag,
    extent: f64,
    top: bool,
) -> Rect {
    let compact = shell.data.compact;
    let title = sheet.title();
    match sheet.style() {
        SheetStyle::FullScreen => {
            draw_full_screen_sheet(shell, cx, scope, window, sheet, &title, viewport, top);
            return window;
        }
        SheetStyle::Draggable { material_handle } if compact => {
            return draw_draggable_sheet(
                shell,
                cx,
                scope,
                window,
                sheet,
                &title,
                material_handle,
                scroll,
                content_height,
                viewport,
                drag,
                extent,
                top,
            );
        }
        _ => {}
    }
    let width = sheet.max_width();
    let back = sheet.can_go_back();
    let estimated = if *content_height > 0.0 {
        *content_height
    } else {
        240.0
    };
    // A sheet footer stays pinned below the scrolling body here too (divider, 12px padding).
    let footer_h = sheet.footer_height();
    let footer_band = if footer_h > 0.0 {
        1.0 + 12.0 + footer_h + 12.0
    } else {
        0.0
    };
    let (inner, bounds) = {
        let mut ui = overlay_ui(shell);
        let (body, bounds) = dialog_frame(
            &mut ui,
            cx,
            window,
            width,
            estimated + 48.0 + footer_band,
            &title,
            compact,
            top,
            back,
        );
        let inner = rect(
            body.pos.x,
            body.pos.y,
            body.size.x,
            (body.size.y - footer_band).max(0.0),
        );
        if footer_h > 0.0 {
            let bottom = inner.pos.y + inner.size.y;
            let p = ui.p;
            ui.painter
                .hline(cx, body.pos.x, bottom, body.size.x, p.border);
            sheet.draw_footer(
                cx,
                &mut ui,
                body.pos.x + 24.0,
                bottom + 1.0 + 12.0,
                body.size.x - 48.0,
                top,
            );
        }
        (inner, bounds)
    };
    *viewport = inner;
    let max = (*content_height + 48.0 - inner.size.y).max(0.0);
    *scroll = scroll.clamp(0.0, max);
    cx.begin_turtle(
        Walk::abs_rect(inner),
        Layout {
            clip_x: true,
            clip_y: true,
            ..Layout::default()
        },
    );
    let height = {
        let mut ui = overlay_ui(shell);
        ui.hits.set_clip(Some(inner));
        let h = sheet.draw(
            cx,
            &mut ui,
            scope,
            inner.pos.x + 24.0,
            inner.pos.y + 24.0 - *scroll,
            inner.size.x - 48.0,
            top,
        );
        ui.hits.set_clip(None);
        h
    };
    cx.end_turtle();
    if (height - *content_height).abs() > 0.5 {
        *content_height = height;
    }
    bounds
}

/// `SheetStyle::Draggable` on compact layouts.
#[allow(clippy::too_many_arguments)]
fn draw_draggable_sheet(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    sheet: &mut dyn Sheet,
    title: &str,
    material_handle: bool,
    scroll: &mut f64,
    content_height: &mut f64,
    viewport: &mut Rect,
    drag: &mut SheetDrag,
    extent: f64,
    top: bool,
) -> Rect {
    let insets = crate::shell::platform::safe_insets();
    // `useSafeArea`: never under the top inset.
    let max_h = window.size.y - insets.top;
    let h = (window.size.y * extent).min(max_h).max(0.0);
    let frame = rect(
        window.pos.x,
        window.pos.y + window.size.y - h,
        window.size.x,
        h,
    );
    drag.frame = frame;
    drag.window_h = window.size.y;
    let body = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        ui.fill(
            cx,
            rect(frame.pos.x, frame.pos.y, frame.size.x, frame.size.y + 16.0),
            p.surface0,
            16.0,
        );
        if p.surface0.w >= 1.0 {
            crate::common::clock::occlude(
                cx,
                rect(
                    frame.pos.x,
                    frame.pos.y + 16.0,
                    frame.size.x,
                    frame.size.y - 16.0,
                ),
            );
        }
        if top {
            ui.hit(frame, "dialog:surface");
        }
        let cxm = frame.pos.x + frame.size.x * 0.5;
        let band = if material_handle {
            // Material 3 drag handle: 32x4 in a 48px band, onSurfaceVariant at 40%.
            ui.fill(
                cx,
                rect(cxm - 16.0, frame.pos.y + 22.0, 32.0, 4.0),
                with_alpha(p.muted, 0.4),
                2.0,
            );
            48.0
        } else {
            // The sheet's own 36x4 surface2 handle with 10px margins.
            ui.fill(
                cx,
                rect(cxm - 18.0, frame.pos.y + 10.0, 36.0, 4.0),
                p.surface2,
                2.0,
            );
            24.0
        };
        let spec = ui.spec(Font::Medium, 14.0, p.foreground);
        let title_h = super::chrome::text_line_height(20.0, 14.0);
        ui.text(
            cx,
            rect(
                frame.pos.x + 24.0,
                frame.pos.y + band,
                frame.size.x - 48.0,
                title_h,
            ),
            spec,
            title,
        );
        let y = frame.pos.y + band + title_h + 8.0;
        let footer_h = sheet.footer_height();
        let footer_band = if footer_h > 0.0 {
            1.0 + 12.0 + footer_h + 12.0 + insets.bottom
        } else {
            insets.bottom
        };
        let bottom = frame.pos.y + frame.size.y - footer_band;
        if footer_h > 0.0 {
            ui.painter
                .hline(cx, frame.pos.x, bottom, frame.size.x, p.border);
            sheet.draw_footer(
                cx,
                &mut ui,
                frame.pos.x + 24.0,
                bottom + 1.0 + 12.0,
                frame.size.x - 48.0,
                top,
            );
        }
        rect(frame.pos.x, y, frame.size.x, (bottom - y).max(0.0))
    };
    *viewport = body;
    // List padding (8, 4, 8, 32).
    let max = (*content_height + 4.0 + 32.0 - body.size.y).max(0.0);
    *scroll = scroll.clamp(0.0, max);
    cx.begin_turtle(
        Walk::abs_rect(body),
        Layout {
            clip_x: true,
            clip_y: true,
            ..Layout::default()
        },
    );
    let height = {
        let mut ui = overlay_ui(shell);
        ui.hits.set_clip(Some(body));
        let h = sheet.draw(
            cx,
            &mut ui,
            scope,
            body.pos.x + 8.0,
            body.pos.y + 4.0 - *scroll,
            body.size.x - 16.0,
            top,
        );
        ui.hits.set_clip(None);
        h
    };
    cx.end_turtle();
    if (height - *content_height).abs() > 0.5 {
        *content_height = height;
    }
    frame
}

/// A pushed full-screen route (`Scaffold(backgroundColor: surface0)`): the original
/// `BackHeader` (safe area + 8px, 12px sides, 56px row, 32px round back island with a 16px
/// muted arrow, lg medium title 8px after it), then the body with 24px sides and bottom.
#[allow(clippy::too_many_arguments)]
fn draw_full_screen_sheet(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    sheet: &mut dyn Sheet,
    title: &str,
    viewport: &mut Rect,
    top: bool,
) {
    let insets = crate::shell::platform::safe_insets();
    let body = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        ui.fill(cx, window, p.surface0, 0.0);
        if p.surface0.w >= 1.0 {
            crate::common::clock::occlude(cx, window);
        }
        if top {
            ui.hit(window, "dialog:surface");
        }
        let row_y = window.pos.y + insets.top + 8.0;
        let row_h = 32.0 + super::chrome::text_line_height(24.0, 16.0);
        let island = rect(
            window.pos.x + 12.0 + insets.left,
            row_y + (row_h - 32.0) * 0.5,
            32.0,
            32.0,
        );
        let id = if top { "sheet:close" } else { "" };
        if top && (ui.is_hover(id) || ui.is_pressed(id)) {
            ui.fill(cx, island, p.interaction_highlight, 16.0);
        }
        ui.icon(cx, island, "arrow-left", 16.0, p.muted);
        if top {
            ui.hit(rect(island.pos.x, row_y + 4.0, 32.0, row_h - 8.0), id);
        }
        let spec = ui.spec(Font::Medium, 16.0, p.foreground);
        let tx = island.pos.x + 32.0 + 8.0;
        ui.text(
            cx,
            rect(
                tx,
                row_y,
                window.pos.x + window.size.x - 12.0 - insets.right - tx,
                row_h,
            ),
            spec,
            title,
        );
        let top_y = row_y + row_h;
        rect(
            window.pos.x + 24.0,
            top_y,
            window.size.x - 48.0,
            (window.pos.y + window.size.y - insets.bottom - 24.0 - top_y).max(0.0),
        )
    };
    *viewport = body;
    sheet.set_body_height(body.size.y);
    cx.begin_turtle(
        Walk::abs_rect(body),
        Layout {
            clip_x: true,
            clip_y: true,
            ..Layout::default()
        },
    );
    {
        let mut ui = overlay_ui(shell);
        ui.hits.set_clip(Some(body));
        sheet.draw(cx, &mut ui, scope, body.pos.x, body.pos.y, body.size.x, top);
        ui.hits.set_clip(None);
    }
    cx.end_turtle();
}

fn draw_toast(shell: &mut PaseoShell, cx: &mut Cx2d, window: Rect) {
    let Some((message, error, action)) = shell.data.overlays.toast.as_ref().map(|t| {
        let action = t.action.as_ref().map(|a| a.label.clone());
        (t.message.clone(), t.error, action)
    }) else {
        return;
    };
    let insets = crate::shell::platform::safe_insets();
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 14.0, p.foreground);
    let action_spec = ui.spec(Font::Medium, 14.0, p.accent);
    // Original toast-host pill: 92% wide up to 480, under the header (below the status bar
    // and notch on phones), surface0 + border, a dismiss glyph at the end. An action button
    // sits before the glyph; its press target spans the toast's height (44px at least).
    let w = (window.size.x * 0.92).min(480.0);
    let action_w = action
        .as_ref()
        .map_or(0.0, |label| ui.measure(cx, action_spec, label) + 24.0);
    let action_gap = if action.is_some() {
        action_w + 8.0
    } else {
        0.0
    };
    let text_w = (w - 36.0 - 36.0 - action_gap).max(40.0);
    let text_h = ui
        .painter
        .paragraph_height(cx, text_w, spec, &message)
        .max(20.0);
    let h = (text_h + 20.0).max(if action.is_some() { 44.0 } else { 0.0 });
    let top = window.pos.y + insets.top + if ui.compact { 72.0 } else { 44.0 };
    let r = rect(window.pos.x + (window.size.x - w) * 0.5, top, w, h);
    let border = if error { p.danger } else { p.border };
    ui.painter.outline(cx, r, p.surface0, border, 1.0, 16.0);
    let icon = if error { "circle-alert" } else { "info" };
    ui.icon(
        cx,
        rect(r.pos.x + 12.0, r.pos.y + (h - 16.0) * 0.5, 16.0, 16.0),
        icon,
        16.0,
        if error { p.danger } else { p.muted },
    );
    ui.painter.paragraph(
        cx,
        dvec2(r.pos.x + 36.0, r.pos.y + (h - text_h) * 0.5),
        text_w,
        spec,
        &message,
    );
    let action_rect = rect(r.pos.x + r.size.x - 36.0 - action_w, r.pos.y, action_w, h);
    if let Some(label) = &action {
        let pill = rect(
            action_rect.pos.x,
            r.pos.y + (h - 32.0) * 0.5,
            action_w,
            32.0,
        );
        ui.painter.outline(cx, pill, p.surface2, p.border, 1.0, 8.0);
        ui.text_centered(cx, pill, action_spec, label);
    }
    ui.icon(
        cx,
        rect(
            r.pos.x + r.size.x - 28.0,
            r.pos.y + (h - 16.0) * 0.5,
            16.0,
            16.0,
        ),
        "x",
        14.0,
        p.muted,
    );
    if let Some(toast) = shell.data.overlays.toast.as_mut() {
        toast.rect = r;
        toast.action_rect = action_rect;
    }
}

fn draw_tooltip(shell: &mut PaseoShell, cx: &mut Cx2d, window: Rect) {
    let Some(tip) = shell.data.overlays.tooltip.clone() else {
        return;
    };
    // A chrome tooltip outlives nothing: its control must still be registered at the anchor.
    if let Some(owner) = shell.data.overlays.tooltip_owner.clone() {
        let same =
            |a: Rect, b: Rect| (a.pos - b.pos).length() < 0.5 && (a.size - b.size).length() < 0.5;
        if !shell
            .data
            .hits
            .rect_of(&owner)
            .is_some_and(|r| same(r, tip.anchor))
        {
            let overlays = &mut shell.data.overlays;
            cx.stop_timer(overlays.tooltip_timer);
            overlays.tooltip = None;
            overlays.tooltip_pending = None;
            overlays.tooltip_owner = None;
            return;
        }
    }
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 12.0, p.surface0);
    let w = ui.measure(cx, spec, &tip.text) + 16.0;
    let h = 24.0;
    // Flutter `Tooltip`: centred on the target, `verticalOffset` 24 from its centre, below
    // unless that overflows the window, then above.
    let a = tip.anchor;
    let cx0 = a.pos.x + a.size.x * 0.5;
    let cy = a.pos.y + a.size.y * 0.5;
    let below = cy + 24.0;
    let y = if below + h > window.pos.y + window.size.y - 4.0 {
        cy - 24.0 - h
    } else {
        below
    };
    let x = (cx0 - w * 0.5).clamp(
        window.pos.x + 4.0,
        (window.pos.x + window.size.x - w - 4.0).max(window.pos.x + 4.0),
    );
    let r = rect(x, y, w, h);
    ui.fill(cx, r, with_alpha(p.foreground, 0.9), 4.0);
    ui.text_centered(cx, r, spec, &tip.text);
}

// ---------------------------------------------------------------------------------------------
// Events

/// Returns true while a modal overlay owns input.
pub fn handle_event(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, scope: &mut Scope) -> bool {
    let area = shell.data.overlays.area;
    // The layer's capture instance can change between frames (window resize, a new draw
    // list); a lock taken on an older area would block everything, so release it.
    let stale = std::mem::replace(&mut shell.data.overlays.locked_area, area);
    if stale != area {
        cx.sweep_unlock(stale);
    }
    let explorer_open = shell.data.compact && shell.data.chrome.explorer_visible();
    if !shell.data.overlays.is_modal() && !explorer_open {
        shell.data.overlays.touch_scroll.reset();
        // The last modal closed: release the layer's sweep lock, or every hover and click
        // below stays blocked.
        cx.sweep_unlock(area);
        return false;
    }
    // Claim before child dispatch: a child may change the stack in its handler.
    if take_dismissal(event) {
        if shell.data.overlays.is_modal() {
            dismiss_top(shell, cx, scope);
        } else if explorer_open {
            shell.data.layout.show_compact_explorer(false);
            shell.invalidate_layout(cx);
        }
        return true;
    }
    if matches!(
        event,
        Event::BackPressed { .. }
            | Event::KeyDown(KeyEvent {
                key_code: KeyCode::Escape,
                ..
            })
    ) {
        return true;
    }
    // The incoming surface uses settled geometry. Its backdrop owns input,
    // but neither its fields nor its hit map handle positions until it settles.
    if crate::common::is_user_input(event)
        && (shell.data.overlays.top_waits_for_paint()
            || (!shell.data.overlays.is_modal() && shell.data.chrome.explorer.is_moving()))
    {
        return true;
    }
    if let Event::KeyDown(key) = event
        && modal_key(shell, cx, scope, key)
    {
        return true;
    }
    if let Some(top) = shell.data.overlays.stack.len().checked_sub(1) {
        let owner = shell.data.overlays.stack[top].id();
        // Text fields and sheets receive events first (unlocked).
        cx.sweep_unlock(area);
        match &mut shell.data.overlays.stack[top] {
            Modal::Prompt {
                field, id, owner, ..
            } => {
                let field = field.clone();
                let (id, owner) = (*id, owner.clone());
                field.handle_event(cx, event, scope);
                if let Event::Actions(actions) = event
                    && let Some(text) = field.as_ptext_field().returned(actions)
                {
                    shell.data.overlays.pop(cx);
                    answer(shell, cx, scope, &owner, id, json!({ "text": text }));
                    if shell.data.overlays.is_modal() || explorer_open {
                        cx.sweep_lock(area);
                    }
                    return true;
                }
            }
            Modal::Select(select) => {
                if let Some(search) = select.search.clone() {
                    search.handle_event(cx, event, scope);
                    if let Event::Actions(actions) = event
                        && let Some(text) = search.as_ptext_field().changed(actions)
                    {
                        select.query = text.trim().to_lowercase();
                        select.scroll = 0.0;
                        shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
                    }
                }
            }
            Modal::Sheet { .. } => {
                if let Some(flow) = sheet_event(shell, cx, event, scope, top) {
                    apply_flow(shell, cx, scope, top, flow);
                }
            }
            _ => {}
        }
        if shell.data.overlays.is_modal() || explorer_open {
            cx.sweep_lock(area);
        }
        // Operations can close, replace or push a sheet. Never resolve this same
        // event against another owner's old hit map.
        if shell.data.overlays.stack.last().map(Modal::id) != Some(owner) {
            shell.data.overlays.touch_scroll.reset();
            shell.data.overlays.pointer = Pointer::default();
            return true;
        }
    }
    // While a modal is open the layer holds the sweep lock on `area`, so pointer events only
    // reach it through its own sweep area. Resolve the hit once (moves mark the event handled)
    // and share it.
    if let Event::AccessibilityAction(action) = event {
        use super::accessibility;
        let owner = shell
            .data
            .overlays
            .stack
            .last()
            .map_or(0, Modal::accessibility_scope);
        if let Some((id, bounds, actions)) = accessibility_scroll(shell)
            && action.id == id
            && actions.contains(action.action)
            && let Some(delta) = accessibility::page_delta(action.action, bounds)
        {
            scroll_top(shell, delta);
            area.redraw(cx);
        } else if action.action == AccessibilityAction::Activate
            && let Some(id) = accessibility::hit(
                &shell.data.overlays.hits,
                super::ui::HitScope::Overlay(owner),
                action.id,
            )
        {
            click(shell, cx, scope, &id);
        }
        return shell.data.overlays.is_modal();
    }
    let swept = event.hits_with_sweep_area(cx, area, area);
    if let Hit::FingerScroll(e) = &swept
        && scroll_viewport(shell).contains(e.abs)
    {
        scroll_top(shell, e.scroll.y);
        shell.data.overlays.area.redraw(cx);
    }
    // Compact draggable sheets already combine resizing and scrolling below.
    let draggable = shell.data.overlays.stack.last().is_some_and(|top| {
        matches!(top, Modal::Sheet { sheet, drag, .. }
            if shell.data.compact && matches!(sheet.style(), SheetStyle::Draggable { .. }) && drag.window_h > 0.0)
    });
    if !draggable {
        let viewport = scroll_viewport(shell);
        let eligible = !matches!(&swept, Hit::FingerDown(e) if !viewport.contains(e.abs));
        if eligible {
            if let Some(dy) = shell
                .data
                .overlays
                .touch_scroll
                .handle_event(cx, event, &swept)
            {
                shell.data.overlays.pointer.pressed = None;
                scroll_top(shell, dy);
                shell.data.overlays.area.redraw(cx);
            }
        } else {
            shell.data.overlays.touch_scroll.reset();
        }
    } else {
        shell.data.overlays.touch_scroll.reset();
    }
    if drag_sheet(shell, cx, scope, &swept) {
        return true;
    }
    let (redraw, press) = {
        let overlays = &mut shell.data.overlays;
        overlays.pointer.handle_hit(cx, swept, &overlays.hits)
    };
    if redraw {
        shell.data.overlays.area.redraw(cx);
    }
    match press {
        Some(Press::Click(id, _)) => click(shell, cx, scope, &id),
        // Only the compact Explorer rail (no shell modal open) has secondary actions here,
        // e.g. the `tab:explorer:…` tab menu.
        Some(Press::Secondary(id, abs)) => {
            if shell.data.overlays.stack.is_empty()
                && id != "compact:backdrop"
                && let Some(ui) = scope.data.get_mut::<UiContext>()
            {
                crate::shell::commands::secondary(shell, cx, ui, &id, abs);
            }
        }
        None => {}
    }
    // While open, the explorer overlay passes events to its own roots via dispatch.
    shell.data.overlays.is_modal()
}

/// `DraggableScrollableSheet` gestures on the top compact draggable sheet: dragging up grows
/// the sheet to .9 then scrolls its list; dragging down scrolls back, then shrinks it. A
/// release snaps to .3/.6/.9, or dismisses below half the minimum. A drag past 8px cancels
/// the pending tap. Returns `true` when the sheet was dismissed.
fn drag_sheet(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope, hit: &Hit) -> bool {
    let compact = shell.data.compact;
    let overlays = &mut shell.data.overlays;
    let mut cancel_tap = false;
    let mut dismiss = false;
    if matches!(hit, Hit::FingerLongPress(_)) {
        if let Some(Modal::Sheet { drag, .. }) = overlays.stack.last_mut() {
            drag.last_y = None;
        }
        overlays.pointer.pressed = None;
        return false;
    }
    {
        let Some(Modal::Sheet {
            id,
            sheet,
            drag,
            scroll,
            content_height,
            viewport,
            ..
        }) = overlays.stack.last_mut()
        else {
            return false;
        };
        if !compact
            || !matches!(sheet.style(), SheetStyle::Draggable { .. })
            || drag.window_h <= 0.0
        {
            return false;
        }
        let Some(layer) = overlays
            .layers
            .iter_mut()
            .rev()
            .find(|layer| layer.id == *id)
        else {
            return false;
        };
        match hit {
            Hit::FingerDown(fe) if drag.frame.contains(fe.abs) => {
                drag.last_y = Some(fe.abs.y);
                drag.travel = 0.0;
            }
            Hit::FingerMove(fe) => {
                let Some(last) = drag.last_y else {
                    return false;
                };
                drag.last_y = Some(fe.abs.y);
                let dy = fe.abs.y - last;
                drag.travel += dy.abs();
                cancel_tap = drag.travel > 8.0;
                let extent = layer.extent.value();
                let max_scroll = (*content_height + 4.0 + 32.0 - viewport.size.y).max(0.0);
                let extent = if dy < 0.0 {
                    let grow = (-dy / drag.window_h).min(DRAG_SNAPS[2] - extent).max(0.0);
                    *scroll = (*scroll + (-dy - grow * drag.window_h)).min(max_scroll);
                    extent + grow
                } else {
                    let back = dy.min(*scroll);
                    *scroll -= back;
                    (extent - (dy - back) / drag.window_h).max(0.0)
                };
                layer.layout_extent = Some(extent);
                layer.extent.snap(extent);
            }
            Hit::FingerUp(_) => {
                if drag.last_y.take().is_some() && drag.travel > 8.0 {
                    let extent = layer.extent.value();
                    if extent < DRAG_SNAPS[0] * 0.5 {
                        dismiss = true;
                    } else {
                        let target = nearest_snap(extent);
                        layer
                            .extent
                            .retarget(cx, target, ModalKind::Sheet.style(true).spec);
                        layer.layout_extent = Some(if layer.extent.is_moving() {
                            target.max(extent)
                        } else {
                            target
                        });
                    }
                }
            }
            _ => return false,
        }
    }
    if cancel_tap {
        shell.data.overlays.pointer.pressed = None;
    }
    if dismiss {
        dismiss_top(shell, cx, scope);
        return true;
    }
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
    false
}

/// The next keyboard target among `candidates` (in display order) from `current`: ↓/Tab
/// forward, ↑/Shift+Tab back (both wrap), Home/End to the ends. `None` when the key does not move.
fn step_active(candidates: &[usize], current: Option<usize>, key: &KeyEvent) -> Option<usize> {
    let (first, last) = (*candidates.first()?, *candidates.last()?);
    let at = current.and_then(|c| candidates.iter().position(|&i| i == c));
    let forward = match key.key_code {
        KeyCode::ArrowDown => true,
        KeyCode::ArrowUp => false,
        KeyCode::Tab => !key.modifiers.shift,
        KeyCode::Home => return Some(first),
        KeyCode::End => return Some(last),
        _ => return None,
    };
    Some(match (at, forward) {
        (None, true) => first,
        (None, false) => last,
        (Some(at), true) => candidates[(at + 1) % candidates.len()],
        (Some(at), false) => candidates[(at + candidates.len() - 1) % candidates.len()],
    })
}

/// The scroll that keeps `[top, bottom)` (content coordinates) inside a viewport of `height`.
fn scroll_into_view(scroll: f64, max: f64, height: f64, top: f64, bottom: f64) -> f64 {
    let next = if top < scroll {
        top
    } else if bottom > scroll + height {
        bottom - height
    } else {
        scroll
    };
    next.clamp(0.0, max.max(0.0))
}

fn menu_selectable(entries: &[MenuEntry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| match e {
            MenuEntry::Item(item) => item.enabled,
            MenuEntry::Submenu { .. } => true,
            _ => false,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Content offset (top, bottom) of menu entry `index`, matching [`draw_menu`].
fn menu_entry_span(entries: &[MenuEntry], index: usize, compact: bool) -> (f64, f64) {
    let top = 4.0 + entries_height(&entries[..index], compact) - 8.0;
    (
        top,
        top + entries_height(&entries[index..=index], compact) - 8.0,
    )
}

/// Content offset (top, bottom) of option `index` in a select list, matching [`draw_select`].
fn select_option_span(
    select: &OpenSelect,
    visible: &[usize],
    index: usize,
    sheet: bool,
) -> Option<(f64, f64)> {
    let mut y = 4.0;
    let gap = if sheet { 4.0 } else { 0.0 };
    for &i in visible {
        if select.options[i].separator_before && select.query.is_empty() {
            y += 9.0;
        }
        if i == index {
            return Some((y, y + select_row_height()));
        }
        y += select_row_height() + gap;
    }
    None
}

/// Keyboard for the top menu, select and confirm dialog: ↑/↓ (Tab) move the highlight, Enter
/// (Space outside a search field) picks it, →/← open and leave submenus, ←/→/Tab switch the
/// confirm buttons. Returns true when the key was used.
fn modal_key(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope, key: &KeyEvent) -> bool {
    let m = key.modifiers;
    if m.control || m.alt || m.logo {
        return false;
    }
    let compact = shell.data.compact;
    let Some(top) = shell.data.overlays.stack.last_mut() else {
        return false;
    };
    let pick = match top {
        Modal::Menu(menu) => {
            let depth = menu.pages.len();
            let entries = &menu.pages[depth - 1].1;
            let selectable = menu_selectable(entries);
            let active = menu.active.filter(|a| selectable.contains(a));
            if let Some(next) = step_active(&selectable, active, key) {
                menu.active = Some(next);
                let (top_y, bottom_y) = menu_entry_span(entries, next, compact);
                menu.scroll = scroll_into_view(
                    menu.scroll,
                    menu.max_scroll,
                    menu.viewport.size.y,
                    top_y,
                    bottom_y,
                );
                shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
                return true;
            }
            match (key.key_code, active) {
                (KeyCode::ReturnKey | KeyCode::Space, Some(i)) | (KeyCode::ArrowRight, Some(i)) => {
                    match &entries[i] {
                        MenuEntry::Submenu { .. } => format!("menu:sub:{i}"),
                        _ if key.key_code == KeyCode::ArrowRight => return false,
                        _ => format!("menu:item:{i}"),
                    }
                }
                (KeyCode::ArrowLeft | KeyCode::Backspace, _) if depth > 1 => {
                    "menu:back".to_string()
                }
                _ => return false,
            }
        }
        Modal::Select(select) => {
            let visible: Vec<usize> = select.visible();
            let enabled: Vec<usize> = visible
                .iter()
                .copied()
                .filter(|&i| select.options[i].enabled)
                .collect();
            let active = select.active.filter(|a| enabled.contains(a));
            let edits_query =
                select.search.is_some() && matches!(key.key_code, KeyCode::Home | KeyCode::End);
            if key.key_code != KeyCode::Tab
                && !edits_query
                && let Some(next) = step_active(&enabled, active, key)
            {
                select.active = Some(next);
                if let Some((top_y, bottom_y)) = select_option_span(select, &visible, next, compact)
                {
                    select.scroll = scroll_into_view(
                        select.scroll,
                        select.max_scroll,
                        select.viewport.size.y,
                        top_y,
                        bottom_y,
                    );
                }
                shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
                return true;
            }
            match (key.key_code, active) {
                (KeyCode::ReturnKey, Some(i)) => format!("select:item:{i}"),
                (KeyCode::Space, Some(i)) if select.search.is_none() => format!("select:item:{i}"),
                _ => return false,
            }
        }
        Modal::Confirm { ok_focused, .. } => match key.key_code {
            KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::Tab => {
                *ok_focused = !*ok_focused;
                shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
                return true;
            }
            KeyCode::ReturnKey | KeyCode::Space => (if *ok_focused {
                "confirm:ok"
            } else {
                "confirm:cancel"
            })
            .to_string(),
            _ => return false,
        },
        _ => return false,
    };
    let menu_depth = |shell: &PaseoShell| match shell.data.overlays.stack.last() {
        Some(Modal::Menu(menu)) => Some((menu.id, menu.pages.len())),
        _ => None,
    };
    let before = menu_depth(shell);
    click(shell, cx, scope, &pick);
    // A submenu opened or closed from the keyboard highlights its first entry.
    if let (Some((id, depth)), Some((after_id, after_depth))) = (before, menu_depth(shell))
        && id == after_id
        && depth != after_depth
        && let Some(Modal::Menu(menu)) = shell.data.overlays.stack.last_mut()
    {
        menu.active = menu_selectable(&menu.pages[after_depth - 1].1)
            .first()
            .copied();
    }
    true
}

fn scroll_viewport(shell: &PaseoShell) -> Rect {
    match shell.data.overlays.stack.last() {
        Some(Modal::Menu(menu)) => menu.viewport,
        Some(Modal::Select(select)) => select.viewport,
        Some(Modal::Sheet { viewport, .. }) => *viewport,
        _ => Rect::default(),
    }
}

pub(super) fn accessibility_scroll(
    shell: &PaseoShell,
) -> Option<(u64, Rect, AccessibilityActions)> {
    if shell.data.overlays.top_waits_for_paint() {
        return None;
    }
    let modal = shell.data.overlays.stack.last()?;
    let (offset, max) = match modal {
        Modal::Menu(menu) => (menu.scroll, menu.max_scroll),
        Modal::Select(select) => (select.scroll, select.max_scroll),
        Modal::Sheet {
            scroll,
            content_height,
            viewport,
            sheet,
            ..
        } => (
            *scroll,
            sheet_scroll_max(
                shell.data.compact,
                sheet.style(),
                *content_height,
                *viewport,
            ),
        ),
        _ => return None,
    };
    Some((
        crate::common::accessibility::id(("modal-scroll", modal.accessibility_scope())),
        scroll_viewport(shell),
        super::accessibility::scroll_actions(offset, max),
    ))
}

fn sheet_scroll_max(compact: bool, style: SheetStyle, height: f64, viewport: Rect) -> f64 {
    let padding = if compact && matches!(style, SheetStyle::Draggable { .. }) {
        36.0
    } else {
        48.0
    };
    height + padding - viewport.size.y
}

fn scroll_top(shell: &mut PaseoShell, delta: f64) {
    let overlays = &mut shell.data.overlays;
    let (scroll, max) = match overlays.stack.last_mut() {
        Some(Modal::Sheet {
            scroll,
            content_height,
            viewport,
            sheet,
            ..
        }) => (
            scroll,
            sheet_scroll_max(
                shell.data.compact,
                sheet.style(),
                *content_height,
                *viewport,
            ),
        ),
        Some(Modal::Menu(menu)) => (&mut menu.scroll, menu.max_scroll),
        Some(Modal::Select(select)) => (&mut select.scroll, select.max_scroll),
        _ => {
            overlays.touch_scroll.reset();
            return;
        }
    };
    overlays.touch_scroll.apply_delta(scroll, delta, max);
}

fn answer(
    shell: &mut PaseoShell,
    cx: &mut Cx,
    scope: &mut Scope,
    owner: &str,
    id: u64,
    args: Value,
) {
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        shell.overlay_answer(cx, ui, owner, id, args);
    }
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
}

/// Closes every shell menu, dialog and sheet as if dismissed (owners get their cancel answer;
/// sheets run `closed`). Used when a notification takes the user to a session.
pub fn dismiss_all(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext) {
    // An answer may open a follow-up (a close guard's next step); stop after a bounded number.
    for _ in 0..64 {
        if !dismiss_one(shell, cx, Some(ui)) {
            break;
        }
    }
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
}

fn dismiss_top(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope) {
    dismiss_one(shell, cx, scope.data.get_mut::<UiContext>());
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
}

fn dismiss_one(shell: &mut PaseoShell, cx: &mut Cx, ui: Option<&mut UiContext>) -> bool {
    let Some(index) = shell.data.overlays.stack.len().checked_sub(1) else {
        return false;
    };
    let mut ops = ui.as_deref().map(SheetOps::new);
    let reply = shell
        .data
        .overlays
        .remove_with(cx, index, |cx, modal| match modal {
            Modal::Menu(menu) => Some((menu.owner.clone(), menu.id, json!({ "cancelled": true }))),
            Modal::Select(select) => Some((
                select.owner.clone(),
                select.id,
                json!({ "cancelled": true }),
            )),
            Modal::Confirm { id, owner, .. } => {
                Some((owner.clone(), *id, json!({ "confirmed": false })))
            }
            Modal::Prompt { id, owner, .. } => {
                Some((owner.clone(), *id, json!({ "cancelled": true })))
            }
            Modal::Sheet { sheet, .. } => {
                if let Some(ops) = &mut ops {
                    sheet.closed(cx, ops);
                }
                None
            }
        });
    if let Some(ui) = ui {
        if let Some((owner, id, result)) = reply {
            shell.overlay_answer(cx, ui, &owner, id, result);
        } else if let Some(ops) = ops {
            crate::shell::sheets::apply_ops(shell, cx, ui, ops, None);
        }
    }
    true
}

fn click(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope, id: &str) {
    let Some(top) = shell.data.overlays.stack.len().checked_sub(1) else {
        if id == "compact:backdrop" {
            shell.data.layout.show_compact_explorer(false);
            shell.invalidate_layout(cx);
        } else if let Some(ui) = scope.data.get_mut::<UiContext>() {
            crate::shell::commands::click(shell, cx, ui, id, KeyModifiers::default());
        }
        return;
    };
    if id == "overlay:backdrop" || id == "dialog:close" || id == "sheet:close" {
        dismiss_top(shell, cx, scope);
        return;
    }
    if id == "menu:surface" || id == "dialog:surface" || id == "select:surface" {
        return;
    }
    match &mut shell.data.overlays.stack[top] {
        Modal::Select(select) => {
            if let Some(index) = id
                .strip_prefix("select:item:")
                .and_then(|i| i.parse::<usize>().ok())
                && let Some(option) = select.options.get(index).filter(|o| o.enabled)
            {
                let (owner, select_id, value) =
                    (select.owner.clone(), select.id, option.value.clone());
                shell.data.overlays.pop(cx);
                answer(
                    shell,
                    cx,
                    scope,
                    &owner,
                    select_id,
                    json!({ "value": value }),
                );
            }
        }
        Modal::Menu(menu) => {
            if id == "menu:back" {
                if menu.pages.len() > 1 {
                    menu.pages.pop();
                    menu.scroll = 0.0;
                    menu.active = None;
                }
                shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
                return;
            }
            let entries = menu
                .pages
                .last()
                .map(|(_, e)| e.clone())
                .unwrap_or_default();
            if let Some(index) = id
                .strip_prefix("menu:sub:")
                .and_then(|i| i.parse::<usize>().ok())
            {
                if let Some(MenuEntry::Submenu { label, entries, .. }) = entries.get(index) {
                    menu.pages.push((Some(label.clone()), entries.clone()));
                    menu.scroll = 0.0;
                    menu.active = None;
                }
                shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
                return;
            }
            if let Some(index) = id
                .strip_prefix("menu:item:")
                .and_then(|i| i.parse::<usize>().ok())
                && let Some(MenuEntry::Item(item)) = entries.get(index)
            {
                let (owner, menu_id) = (menu.owner.clone(), menu.id);
                let value = item.value.clone();
                if !item.keep_open {
                    shell.data.overlays.pop(cx);
                }
                answer(shell, cx, scope, &owner, menu_id, json!({ "value": value }));
            }
        }
        Modal::Confirm { id: cid, owner, .. } => {
            let (cid, owner) = (*cid, owner.clone());
            let confirmed = match id {
                "confirm:ok" => true,
                "confirm:cancel" => false,
                _ => return,
            };
            shell.data.overlays.pop(cx);
            answer(
                shell,
                cx,
                scope,
                &owner,
                cid,
                json!({ "confirmed": confirmed }),
            );
        }
        Modal::Prompt {
            id: pid,
            owner,
            field,
            ..
        } => {
            let (pid, owner, text) = (*pid, owner.clone(), field.text());
            match id {
                "prompt:ok" => {
                    shell.data.overlays.pop(cx);
                    answer(shell, cx, scope, &owner, pid, json!({ "text": text }));
                }
                "prompt:cancel" => {
                    shell.data.overlays.pop(cx);
                    answer(shell, cx, scope, &owner, pid, json!({ "cancelled": true }));
                }
                _ => {}
            }
        }
        Modal::Sheet {
            sheet,
            id: sheet_id,
            ..
        } => {
            let sheet_id = *sheet_id;
            let Some(ui) = scope.data.get_mut::<UiContext>() else {
                return;
            };
            let mut ops = SheetOps::for_sheet(ui, sheet_id);
            let flow = sheet.click(cx, id, &mut ops);
            crate::shell::sheets::apply_ops(shell, cx, ui, ops, Some(sheet_id));
            apply_flow(shell, cx, scope, top, flow);
        }
    }
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
}

fn sheet_event(
    shell: &mut PaseoShell,
    cx: &mut Cx,
    event: &Event,
    scope: &mut Scope,
    index: usize,
) -> Option<SheetFlow> {
    let Modal::Sheet { sheet, id, .. } = &mut shell.data.overlays.stack[index] else {
        return None;
    };
    let sheet_id = *id;
    let flow = sheet.handle_event(cx, event, scope);
    // Children may have their own frame-driven effects (e.g. a caret); let
    // them request their own redraw. A navigation frame is not a data change.
    if matches!(event, Event::NextFrame(_)) && matches!(flow, SheetFlow::Stay) {
        return None;
    }
    let (ops, after) = {
        let ui = scope.data.get_mut::<UiContext>()?;
        let mut ops = SheetOps::for_sheet(ui, sheet_id);
        let after = sheet.after_event(cx, &mut ops);
        (ops, after)
    };
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        crate::shell::sheets::apply_ops(shell, cx, ui, ops, Some(sheet_id));
    }
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
    Some(if matches!(flow, SheetFlow::Stay) {
        after
    } else {
        flow
    })
}

/// Applies a sheet's navigation result to the overlay stack.
pub fn apply_flow(
    shell: &mut PaseoShell,
    cx: &mut Cx,
    _scope: &mut Scope,
    index: usize,
    flow: SheetFlow,
) {
    match flow {
        SheetFlow::Stay => {}
        SheetFlow::Close => {
            if index < shell.data.overlays.stack.len() {
                shell.data.overlays.remove(cx, index);
            }
        }
        SheetFlow::Replace(next) => {
            if index < shell.data.overlays.stack.len() {
                shell.data.overlays.remove(cx, index);
            }
            shell.data.overlays.open_sheet(next);
        }
        SheetFlow::Push(next) => {
            shell.data.overlays.open_sheet(next);
        }
    }
    shell.invalidate_region(cx, crate::shell::ShellRegion::Overlays);
}

/// Margin helper for sheets.
pub fn padded(r: Rect, d: f64) -> Rect {
    inset(r, d, d)
}

/// Text spec helper for sheet bodies.
pub fn body_spec(ui: &Ui) -> TextSpec {
    ui.spec(Font::Regular, 14.0, ui.p.foreground)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn full_screen_sheet_transition_reuses_actual_draw_list() {
        use makepad_widgets::makepad_draw::CxDraw;
        use std::time::{Duration, Instant};

        let mut cx = Cx::new(Box::new(|_, _| {}));
        PaseoShell::register_main_module(&mut cx);
        crate::app::live_design(&mut cx);
        cx.live_expand();
        cx.live_scan_dependencies();
        cx.native_load_dependencies();
        let mut shell = PaseoShell::new_local(&mut cx);
        crate::shell::sheets::register_qr_ptr(shell.qr_scan);
        let pass = Pass::new(&mut cx);
        let window = rect(0.0, 0.0, 960.0, 640.0);
        pass.set_size(&mut cx, window.size);
        let mut root = DrawList2d::new(&mut cx);
        let overlay = Overlay::new(&mut cx);
        let mut paint = |cx: &mut Cx, shell: &mut PaseoShell| {
            let event = DrawEvent::default();
            let mut draw = CxDraw::new(cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            root.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            overlay.begin(&mut cx);
            cx.begin_pass_sized_turtle(Layout::flow_overlay());
            super::draw(shell, &mut cx, &mut Scope::empty(), window);
            cx.end_pass_sized_turtle();
            overlay.end(&mut cx);
            root.end(&mut cx);
            cx.end_pass(&pass);
        };
        shell
            .data
            .overlays
            .open_sheet(Box::new(crate::shell::sheets::host_dialog::ScanSheet::new()));
        paint(&mut cx, &mut shell);
        let layer = &shell.data.overlays.layers[0];
        assert_eq!(layer.bounds, window);
        assert!(layer.texture.is_none());
        let list = layer.list.as_ref().unwrap().id();
        let original_transform = cx.draw_lists[list].draw_list_uniforms.view_transform;
        let trace =
            std::env::var_os("PASEO_FRAME_TRACE").as_deref() == Some(std::ffi::OsStr::new("1"));

        for (opening, start) in [(true, 0.0), (false, 2.0)] {
            if !opening {
                shell.data.overlays.pop(&mut cx);
                paint(&mut cx, &mut shell);
                assert!(!shell.data.overlays.is_modal());
            }
            let redraw_id = cx.draw_lists[list].redraw_id;
            let draw_count = cx.draw_lists[list].draw_items.len();
            assert!(draw_count > 1);
            let mut frames = 0;
            let mut compose = Duration::ZERO;
            while let Some(token) = shell
                .data
                .overlays
                .layers
                .first()
                .and_then(|layer| layer.visibility.scheduled_frame())
            {
                let event = Event::NextFrame(makepad_widgets::event::NextFrameEvent {
                    time: start + f64::from(frames) / 60.0,
                    set: [token].into_iter().collect(),
                    ..Default::default()
                });
                crate::common::transition::begin_event(&event);
                let before = Instant::now();
                shell.data.overlays.advance(&mut cx, &event);
                compose += before.elapsed();
                frames += 1;
                assert!(frames < 60, "motion did not settle after 1s");
                if !shell.data.overlays.layers.is_empty() {
                    assert_eq!(cx.draw_lists[list].redraw_id, redraw_id);
                    assert_eq!(cx.draw_lists[list].draw_items.len(), draw_count);
                }
            }
            assert!(frames > 1);
            if trace {
                eprintln!(
                    "PASEO_FRAME_TRACE headless ScanSheet {}: {frames} sampled frames, {:.4} ms/composition",
                    if opening { "open" } else { "close" },
                    compose.as_secs_f64() * 1000.0 / f64::from(frames),
                );
            }
            if opening {
                assert_ne!(
                    cx.draw_lists[list].draw_list_uniforms.view_transform,
                    original_transform,
                );
                paint(&mut cx, &mut shell);
                assert!(!shell.data.overlays.top_waits_for_paint());
            }
        }
        assert!(shell.data.overlays.layers.is_empty());
    }

    fn modal_transition_lifecycle(mut open: impl FnMut(&mut Overlays, &mut Cx) -> u64) {
        fn tick(overlays: &mut Overlays, cx: &mut Cx, time: f64) {
            let token = overlays.layers[0]
                .visibility
                .scheduled_frame()
                .expect("moving");
            let event = Event::NextFrame(makepad_widgets::event::NextFrameEvent {
                time,
                set: [token].into_iter().collect(),
                ..Default::default()
            });
            crate::common::transition::begin_event(&event);
            overlays.advance(cx, &event);
        }

        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut overlays = Overlays::default();
        open(&mut overlays, &mut cx);
        overlays.sync_motion(&mut cx, false);
        assert!(overlays.top_waits_for_paint());
        // Stand in for the retained layer attached on first paint. Cx can drive
        // the registered transition events without a native rendering backend.
        let list = DrawList2d::new(&mut cx);
        let list_id = list.id();
        overlays.layers[0].list = Some(list);
        tick(&mut overlays, &mut cx, 0.0);
        tick(&mut overlays, &mut cx, 0.06);
        assert!((0.0..1.0).contains(&overlays.layers[0].visibility.value()));
        tick(&mut overlays, &mut cx, 1.0);
        assert_eq!(overlays.layers[0].visibility.value(), 1.0);
        assert!(overlays.layers[0].visibility.scheduled_frame().is_none());

        let probe = dvec2(5.0, 5.0);
        overlays
            .hits
            .push(rect(0.0, 0.0, 10.0, 10.0), "old-sheet-hit".into());
        assert!(overlays.hits.at(probe).is_some());
        overlays.pop(&mut cx);
        assert!(!overlays.is_modal());
        assert!(overlays.top_layer().is_none());
        assert!(overlays.hits.at(probe).is_none());
        assert!(overlays.layers[0].closing.is_some());
        tick(&mut overlays, &mut cx, 2.0);
        tick(&mut overlays, &mut cx, 2.06);
        let closing = overlays.layers[0].visibility.value();
        assert!(closing > 0.0 && closing < 1.0);

        let incoming = open(&mut overlays, &mut cx);
        overlays.sync_motion(&mut cx, false);
        let layer = overlays.top_layer().expect("reopened owner");
        assert_eq!(overlays.layers.len(), 1);
        assert_eq!(layer.id, incoming);
        assert_eq!(layer.list.as_ref().unwrap().id(), list_id);
        assert_eq!(layer.visibility.value(), closing);
        assert!(overlays.top_waits_for_paint());
        tick(&mut overlays, &mut cx, 2.06);
        tick(&mut overlays, &mut cx, 3.0);
        assert_eq!(overlays.layers[0].visibility.value(), 1.0);
        assert!(overlays.layers[0].visibility.scheduled_frame().is_none());

        overlays.pop(&mut cx);
        tick(&mut overlays, &mut cx, 4.0);
        tick(&mut overlays, &mut cx, 5.0);
        assert!(overlays.layers.is_empty());
        assert!(!overlays.is_modal());
    }

    #[test]
    fn full_screen_sheet_transition_reverses_and_releases_input() {
        modal_transition_lifecycle(|overlays, _| {
            overlays.open_sheet(Box::new(crate::shell::sheets::host_dialog::ScanSheet::new()))
        });
    }

    #[test]
    fn small_dialog_transition_reverses_and_releases_input() {
        let mut id = 0;
        modal_transition_lifecycle(|overlays, cx| {
            id += 1;
            overlays.open(
                cx,
                OverlayRequest {
                    id,
                    owner: "test".into(),
                    window: "main".into(),
                    kind: OverlayKind::Confirm {
                        title: "Remove host".into(),
                        message: "Remove this host?".into(),
                        confirm_label: "Remove".into(),
                        destructive: true,
                    },
                },
                None,
            );
            id
        });
    }

    #[test]
    fn toast_only_owns_wheel_inside_its_window() {
        use makepad_widgets::{CxRectArea, RectArea, event::ScrollEvent};

        let mut cx = Cx::new(Box::new(|_, _| {}));
        let list = cx.draw_lists.alloc();
        let body = Rect {
            pos: dvec2(20.0, 20.0),
            size: dvec2(80.0, 40.0),
        };
        cx.draw_lists[list.id()].rect_areas.push(CxRectArea {
            rect: body,
            draw_clip: (DVec2::default(), dvec2(100.0, 100.0)),
        });
        let mut overlays = Overlays {
            area: Area::Rect(RectArea {
                draw_list_id: list.id(),
                rect_id: 0,
                redraw_id: cx.draw_lists[list.id()].redraw_id,
            }),
            ..Default::default()
        };
        overlays.toast = Some(Toast {
            message: "Copied".into(),
            error: false,
            action: None,
            timer: Timer::empty(),
            remaining: 3.0,
            started: 0.0,
            paused: false,
            rect: body,
            action_rect: Rect::default(),
        });
        let window = WindowId(1, 0);
        for (event_window, x, owned) in [
            (window, 30.0, true),
            (window, 10.0, false),
            (WindowId(2, 0), 30.0, false),
        ] {
            let event = Event::Scroll(ScrollEvent {
                window_id: event_window,
                abs: dvec2(x, 30.0),
                scroll: dvec2(4.0, 40.0),
                modifiers: Default::default(),
                handled_x: Default::default(),
                handled_y: Default::default(),
                is_mouse: true,
                time: 0.0,
            });
            let result = toast_input(&mut overlays, &mut cx, &event, Some(window));
            assert_eq!(matches!(result, ToastPress::Captured), owned);
            let Event::Scroll(scroll) = event else {
                unreachable!()
            };
            assert_eq!(
                (scroll.handled_x.get(), scroll.handled_y.get()),
                (owned, owned)
            );
        }
    }

    fn key(code: KeyCode, shift: bool) -> KeyEvent {
        KeyEvent {
            key_code: code,
            is_repeat: false,
            modifiers: KeyModifiers {
                shift,
                ..KeyModifiers::default()
            },
            time: 0.0,
        }
    }

    #[test]
    fn toast_duration_grows_with_length_and_is_capped() {
        assert_eq!(toast_duration("Copied", false), 3.0 + 0.06 * 6.0);
        assert!(toast_duration(&"x".repeat(60), false) > toast_duration("short", false));
        assert_eq!(toast_duration(&"x".repeat(500), false), 8.0);
        assert_eq!(toast_duration(&"x".repeat(500), true), 10.0);
        assert!(toast_duration("Push failed", true) > toast_duration("Push failed", false));
    }

    #[test]
    fn info_toasts_never_replace_a_showing_error() {
        let toast = |error: bool| Toast {
            message: "m".into(),
            error,
            action: None,
            timer: Timer::empty(),
            remaining: 1.0,
            started: 0.0,
            paused: false,
            rect: Rect::default(),
            action_rect: Rect::default(),
        };
        let (error, info) = (toast(true), toast(false));
        assert!(!toast_replaces(Some(&error), false));
        assert!(toast_replaces(Some(&error), true));
        assert!(toast_replaces(Some(&info), false));
        assert!(toast_replaces(None, false));
    }

    #[test]
    fn keyboard_steps_wrap_over_enabled_rows() {
        let rows = [1, 3, 4];
        assert_eq!(
            step_active(&rows, None, &key(KeyCode::ArrowDown, false)),
            Some(1)
        );
        assert_eq!(
            step_active(&rows, None, &key(KeyCode::ArrowUp, false)),
            Some(4)
        );
        assert_eq!(
            step_active(&rows, Some(4), &key(KeyCode::ArrowDown, false)),
            Some(1)
        );
        assert_eq!(
            step_active(&rows, Some(1), &key(KeyCode::Tab, true)),
            Some(4)
        );
        assert_eq!(
            step_active(&rows, Some(3), &key(KeyCode::End, false)),
            Some(4)
        );
        assert_eq!(
            step_active(&rows, Some(3), &key(KeyCode::ReturnKey, false)),
            None
        );
        assert_eq!(
            step_active(&[], None, &key(KeyCode::ArrowDown, false)),
            None
        );
    }

    #[test]
    fn scroll_into_view_reveals_rows_above_and_below() {
        assert_eq!(scroll_into_view(0.0, 500.0, 100.0, 20.0, 40.0), 0.0);
        assert_eq!(scroll_into_view(0.0, 500.0, 100.0, 150.0, 180.0), 80.0);
        assert_eq!(scroll_into_view(200.0, 500.0, 100.0, 120.0, 140.0), 120.0);
        assert_eq!(scroll_into_view(0.0, 50.0, 100.0, 150.0, 180.0), 50.0);
    }
}
