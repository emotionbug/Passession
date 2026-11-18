//! Shell-drawn menus and dialogs requested by any widget (Flutter `showPaseoMenu`,
//! `showPaseoSelect`, `confirmAction`, `promptText`). The shell renders them above the window
//! content; the answer `{"id": <id>, "value": <item value> | "confirmed": bool |
//! "text": string, "cancelled": bool}` comes back to the requesting tab as a [`UiCommand`] with
//! action [`OVERLAY_ACTION`] on the next dispatch. Widgets without a tab key (shell header
//! buttons, standalone views) open overlays under [`widget_owner`] via [`with_owner`] and read
//! their answers with [`take_answers`].

use std::cell::{Cell, RefCell};

use makepad_widgets::event::{TAP_COUNT_DISTANCE, TouchState};
use makepad_widgets::{Area, Cx, DVec2, Event, KeyCode, MouseButton, PassId, Rect};
use serde_json::Value;

use crate::common::UiContext;

pub const OVERLAY_ACTION: &str = "overlay.result";

/// Owner used for overlays opened by the shell itself.
pub const SHELL_OWNER: &str = "shell";
/// Owner for pages (settings/schedules/sessions) that have no tab key.
pub const PAGE_OWNER: &str = "page";

#[derive(Clone, Debug, PartialEq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
    /// Non-interactive section label.
    Label(String),
    /// `MenuSubTrigger`: opens `entries` as a sub page with a back header, showing the
    /// decision's current `value` and an accent dot while `indicator`.
    Submenu {
        label: String,
        icon: Option<String>,
        value: Option<String>,
        indicator: bool,
        entries: Vec<MenuEntry>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub value: String,
    pub label: String,
    pub icon: Option<String>,
    /// Leading SVG document (filled with the icon colour, `currentColor`) instead of a Lucide
    /// glyph: brand marks such as [`crate::common::icons::forge_brand_svg`].
    pub svg: Option<String>,
    /// Leading 8px dot instead of an icon (host status, label colour).
    pub dot: Option<makepad_widgets::Vec4>,
    /// Trailing muted text (shortcut, id prefix, status).
    pub hint: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub destructive: bool,
    /// Muted label (secondary actions, 72% opacity).
    pub muted: bool,
    /// Trailing check (`DropdownMenuItem selected`).
    pub checked: bool,
    /// Choosing it answers but leaves the menu open (display preference toggles).
    pub keep_open: bool,
}

impl MenuItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            icon: None,
            svg: None,
            dot: None,
            hint: None,
            description: None,
            enabled: true,
            destructive: false,
            muted: false,
            checked: false,
            keep_open: false,
        }
    }
    pub fn dot(mut self, color: makepad_widgets::Vec4) -> Self {
        self.dot = Some(color);
        self
    }
    pub fn keep_open(mut self) -> Self {
        self.keep_open = true;
        self
    }
    pub fn icon(mut self, icon: &str) -> Self {
        self.icon = Some(icon.to_string());
        self
    }
    pub fn svg(mut self, svg: impl Into<String>) -> Self {
        self.svg = Some(svg.into());
        self
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn destructive(mut self, destructive: bool) -> Self {
        self.destructive = destructive;
        self
    }
    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn entry(self) -> MenuEntry {
        MenuEntry::Item(self)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OverlayKind {
    Menu {
        title: Option<String>,
        anchor: Rect,
        entries: Vec<MenuEntry>,
        /// Popover width (220 default).
        width: f64,
        /// Align the popover's end edge with the anchor's end (`alignEnd`).
        align_end: bool,
        /// Bottom sheet on compact layouts (`compactSheet`, default true).
        sheet: bool,
    },
    Confirm {
        title: String,
        message: String,
        confirm_label: String,
        destructive: bool,
    },
    Prompt {
        title: String,
        message: Option<String>,
        initial: String,
        placeholder: String,
        confirm_label: String,
        /// `promptText(multiline: true)`: 4–12 lines, Enter inserts a newline.
        multiline: bool,
    },
    /// `showPaseoSelect` (original `Combobox`): anchored popover on wide layouts, bottom sheet
    /// on compact ones, with an optional search row.
    Select {
        title: String,
        anchor: Rect,
        options: Vec<SelectOption>,
        value: Option<String>,
        searchable: bool,
        search_placeholder: Option<String>,
        empty_text: String,
        /// `desktopMinWidth` (220 default); the popover is at least the anchor's width.
        min_width: f64,
    },
}

/// One choice of [`OverlayKind::Select`] (`PaseoSelectOption`).
#[derive(Clone, Debug, PartialEq)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    /// Leading muted Lucide icon in the 16px slot, drawn at `icon_size` (16 unless set).
    pub icon: Option<String>,
    pub icon_size: f64,
    /// Leading 8px dot instead of an icon.
    pub dot: Option<makepad_widgets::Vec4>,
    /// Trailing muted text (`trailingSlot`, e.g. a branch's ahead/behind).
    pub trailing: Option<String>,
    pub enabled: bool,
    pub separator_before: bool,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
            icon: None,
            icon_size: 16.0,
            dot: None,
            trailing: None,
            enabled: true,
            separator_before: false,
        }
    }
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }
    /// A leading icon smaller than the 16px slot (the host filter's 14px server glyph).
    pub fn icon_sized(mut self, icon: &str, size: f64) -> Self {
        self.icon = Some(icon.to_string());
        self.icon_size = size;
        self
    }
    pub fn icon(mut self, icon: &str) -> Self {
        self.icon = Some(icon.to_string());
        self
    }
    pub fn dot(mut self, color: makepad_widgets::Vec4) -> Self {
        self.dot = Some(color);
        self
    }
    pub fn trailing(mut self, text: impl Into<String>) -> Self {
        self.trailing = Some(text.into());
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn separator_before(mut self, separator: bool) -> Self {
        self.separator_before = separator;
        self
    }

    /// `showPaseoSelect` filter: label or description contains the trimmed, lowercased query.
    pub fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || self.label.to_lowercase().contains(needle)
            || self
                .description
                .as_ref()
                .is_some_and(|d| d.to_lowercase().contains(needle))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayRequest {
    pub id: u64,
    /// The requesting tab key, [`PAGE_OWNER`] or [`SHELL_OWNER`].
    pub owner: String,
    /// The window whose shell shows it (`UiContext::window_id`).
    pub window: String,
    pub kind: OverlayKind,
}

/// A window's pending tooltip request: `(window, Some(tip) | None to hide)`.
type TooltipRequest = (String, Option<Tip>);

struct WindowModalOwner {
    window: String,
    widget: u64,
    tab: Option<String>,
}

/// Releases a retained feature's window input ownership when it closes or drops.
pub struct WindowModal {
    pub window_id: String,
    owner: u64,
}

impl WindowModal {
    pub fn sync(slot: &mut Option<Self>, ui: &UiContext, owner: u64, open: bool) {
        if !open
            || slot
                .as_ref()
                .is_some_and(|modal| modal.window_id != ui.window_id)
        {
            *slot = None;
        }
        if open && slot.is_none() {
            set_window_modal(ui, owner, true);
            *slot = Some(Self {
                window_id: ui.window_id.clone(),
                owner,
            });
        }
    }
}

impl Drop for WindowModal {
    fn drop(&mut self) {
        release_window_modal(&self.window_id, self.owner);
    }
}

thread_local! {
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    static QUEUE: RefCell<Vec<OverlayRequest>> = const { RefCell::new(Vec::new()) };
    static TOOLTIPS: RefCell<Vec<TooltipRequest>> = const { RefCell::new(Vec::new()) };
    static ANSWERS: RefCell<Vec<(String, u64, Value)>> = const { RefCell::new(Vec::new()) };
    static WINDOW_MODALS: RefCell<Vec<WindowModalOwner>> = const { RefCell::new(Vec::new()) };
    static FLOATS: RefCell<Vec<Float>> = const { RefCell::new(Vec::new()) };
    static POPOVERS: RefCell<Vec<Popover>> = const { RefCell::new(Vec::new()) };
}

/// Full-window modal slot (Flutter `showModalBottomSheet` covering the whole window, shell
/// header included). A panel that draws such a sheet calls this with `open: true` while it is
/// up and `false` when it closes; `owner` is any id unique to that sheet (its widget uid).
/// While one is open the shell's own chrome, sidebar and overlay hit testing stop taking
/// pointer and key input for that window, so the panel's sheet gets every event. Draw the
/// sheet in an overlay draw list (`DrawList2d::begin_overlay_last`) sized to
/// `cx.current_pass_size()` so it covers the shell header.
pub fn set_window_modal(ui: &UiContext, owner: u64, open: bool) {
    WINDOW_MODALS.with(|modals| {
        let mut modals = modals.borrow_mut();
        if !open {
            modals.retain(|modal| !(modal.window == ui.window_id && modal.widget == owner));
        } else if let Some(modal) = modals
            .iter_mut()
            .find(|modal| modal.window == ui.window_id && modal.widget == owner)
        {
            // Retained owners sync every frame; only an actual opening changes z-order.
            modal.tab = Some(ui.tab_key.as_deref().unwrap_or(PAGE_OWNER).to_owned());
        } else {
            modals.push(WindowModalOwner {
                window: ui.window_id.clone(),
                widget: owner,
                tab: Some(ui.tab_key.as_deref().unwrap_or(PAGE_OWNER).to_owned()),
            });
        }
    });
}

/// Releases one sheet's slot on hide, retarget or drop, when no `UiContext` is available.
pub fn release_window_modal(window_id: &str, owner: u64) {
    WINDOW_MODALS.with(|modals| {
        modals
            .borrow_mut()
            .retain(|m| !(m.window == window_id && m.widget == owner));
    });
}

/// Whether a panel holds the full-window modal slot of `window` ([`set_window_modal`]).
pub(crate) fn window_modal_open(window: &str) -> bool {
    WINDOW_MODALS.with(|modals| modals.borrow().iter().any(|m| m.window == window))
}

/// Only the top sheet's originating tab receives input; other panes keep receiving updates.
pub(crate) fn window_modal_accepts_tab(window: &str, tab: &str) -> bool {
    WINDOW_MODALS.with(|modals| {
        modals
            .borrow()
            .iter()
            .rev()
            .find(|m| m.window == window)
            .is_none_or(|m| m.tab.as_deref() == Some(tab))
    })
}

/// Snapshot before dispatch when multiple retained modals share a tab.
pub fn window_modal_accepts_owner(window: &str, owner: u64) -> bool {
    WINDOW_MODALS.with(|modals| {
        modals
            .borrow()
            .iter()
            .rev()
            .find(|modal| modal.window == window)
            .is_none_or(|modal| modal.widget == owner)
    })
}

/// A surface drawn above content: a popover, or the sheet holding a window's modal slot.
struct Float {
    area: Area,
    modal: Option<u64>,
}

/// Declares a floating surface: a popover, hover card or panel that a feature paints above
/// content its owner is not dispatched before (the composer's suggestions over the message
/// list). Call it each time the surface is drawn, with the hit area that covers it.
///
/// While the surface stays drawn, a pointer inside `area` reaches only that area and what is
/// drawn in its draw list: the shell declares the window's floating surfaces as pointer
/// shields before it dispatches panels and chrome ([`shield_content`]). A press, hover, wheel
/// or touch scroll there never reaches a widget underneath, including one that hits with
/// capture overload. Handlers that test raw event positions ask `Cx::pointer_shielded`.
///
/// Nothing is undone by hand: a surface redrawn without the area, detached with its layer or
/// dropped stops shielding.
pub fn float(area: Area) {
    FLOATS.with(|floats| {
        let mut floats = floats.borrow_mut();
        if !floats.iter().any(|float| float.area == area) {
            floats.push(Float { area, modal: None });
        }
    });
}

/// Declares the backdrop of a full-window sheet that holds the modal slot as `owner`
/// ([`set_window_modal`]). Call it each time the open sheet is drawn.
///
/// While that sheet is the top one of its window, the pointer reaches only what is drawn in
/// the backdrop's draw list: the page, panes and popovers behind it get no press, hover or
/// wheel, also where the sheet has no use for them.
pub fn modal_float(owner: u64, area: Area) {
    FLOATS.with(|floats| {
        let mut floats = floats.borrow_mut();
        floats.retain(|float| float.modal != Some(owner));
        floats.push(Float {
            area,
            modal: Some(owner),
        });
    });
}

/// Lifetime of a light-dismiss popover. Each window pass can have one open.
/// Only the drawn card shields the pointer; clicks outside are observed, not claimed.
pub struct AnchoredPopover(u64);

struct Popover {
    id: u64,
    pass: PassId,
    trigger: Area,
    body: Area,
    press: Option<(PopoverPointer, DVec2)>,
}

#[derive(Clone, Copy, PartialEq)]
enum PopoverPointer {
    Mouse(MouseButton),
    Touch(u64),
}

impl AnchoredPopover {
    /// Run from the trigger's normal activation path. The window does not
    /// light-dismiss on the invoker itself, so the second activation only closes.
    pub fn toggle(slot: &mut Option<Self>, cx: &mut Cx, trigger: Area) {
        let Some(pass) = float_pass(cx, trigger).filter(|_| float_drawn(cx, trigger)) else {
            return;
        };
        let same = slot.as_ref().is_some_and(|owner| {
            POPOVERS.with(|popovers| {
                popovers
                    .borrow()
                    .iter()
                    .any(|p| p.id == owner.0 && p.trigger == trigger)
            })
        });
        if same {
            *slot = None;
            return;
        }
        let id = next_id();
        POPOVERS.with(|popovers| {
            let mut popovers = popovers.borrow_mut();
            if let Some(index) = popovers.iter().position(|p| p.pass == pass) {
                popovers.remove(index).redraw(cx);
            }
            popovers.push(Popover {
                id,
                pass,
                trigger,
                body: Area::Empty,
                press: None,
            });
        });
        cx.set_key_focus(trigger);
        *slot = Some(Self(id));
    }

    pub fn is_open(&self) -> bool {
        POPOVERS.with(|popovers| popovers.borrow().iter().any(|p| p.id == self.0))
    }

    /// Publish the current drawn areas, also when a retained layer is replayed.
    pub fn drawn(&self, trigger: Area, body: Area) {
        POPOVERS.with(|popovers| {
            if let Some(p) = popovers.borrow_mut().iter_mut().find(|p| p.id == self.0) {
                p.trigger = trigger;
                p.body = body;
            }
        });
    }
}

impl Drop for AnchoredPopover {
    fn drop(&mut self) {
        POPOVERS.with(|popovers| popovers.borrow_mut().retain(|p| p.id != self.0));
    }
}

impl Popover {
    fn redraw(&self, cx: &mut Cx) {
        self.trigger.redraw(cx);
        self.body.redraw(cx);
    }

    fn outside(&self, cx: &Cx, abs: DVec2) -> bool {
        !self.trigger.clipped_rect(cx).contains(abs) && !self.body.clipped_rect(cx).contains(abs)
    }

    fn press(&mut self, cx: &Cx, pointer: PopoverPointer, abs: DVec2) {
        self.press = self.outside(cx, abs).then_some((pointer, abs));
    }

    fn moved(&mut self, abs: DVec2) {
        if self
            .press
            .is_some_and(|(_, start)| (start - abs).length() >= TAP_COUNT_DISTANCE)
        {
            self.press = None;
        }
    }

    fn release(&mut self, cx: &Cx, pointer: PopoverPointer, abs: DVec2) -> bool {
        if !self.press.is_some_and(|(pressed, _)| pressed == pointer) {
            return false;
        }
        self.moved(abs);
        self.press.take().is_some() && self.outside(cx, abs)
    }

    fn pointer_dismissed(&mut self, cx: &Cx, event: &Event) -> bool {
        match event {
            Event::MouseDown(e) => self.press(cx, PopoverPointer::Mouse(e.button), e.abs),
            Event::MouseMove(e) => self.moved(e.abs),
            Event::MouseUp(e) => return self.release(cx, PopoverPointer::Mouse(e.button), e.abs),
            Event::TouchUpdate(e) => {
                if let [touch] = e.touches.as_slice()
                    && !e.cancelled
                {
                    let pointer = PopoverPointer::Touch(touch.uid);
                    match touch.state {
                        TouchState::Start => self.press(cx, pointer, touch.abs),
                        TouchState::Move => self.moved(touch.abs),
                        TouchState::Stop => return self.release(cx, pointer, touch.abs),
                        TouchState::Stable => {}
                    }
                } else {
                    self.press = None;
                }
            }
            Event::Scroll(_)
            | Event::LongPress(_)
            | Event::MouseLeave(_)
            | Event::TextHandle(_)
            | Event::WindowGeomChange(_) => self.press = None,
            _ => {}
        }
        false
    }
}

/// Observe light dismissal for this window before its normal content dispatch.
/// Only Escape is consumed; outside input still goes to the actual target.
pub(crate) fn handle_anchored_popover(
    cx: &mut Cx,
    window: &str,
    anchor: Area,
    event: &Event,
    content_on_top: bool,
) -> bool {
    let Some(pass) = float_pass(cx, anchor) else {
        return false;
    };
    let deactivated = matches!(
        event,
        Event::AppLostFocus | Event::Background | Event::Pause
    );
    POPOVERS.with(|popovers| {
        let mut popovers = popovers.borrow_mut();
        let Some(index) = popovers.iter().position(|p| p.pass == pass) else {
            return false;
        };
        let popup = &mut popovers[index];
        if deactivated || !float_drawn(cx, popup.trigger) {
            popovers.remove(index).redraw(cx);
            return false;
        }
        let top = top_modal(window);
        if !content_on_top
            || (top.is_some()
                && !modal_backdrop(cx, pass, top)
                    .is_some_and(|area| cx.area_drawn_within(popup.trigger, area)))
        {
            popup.press = None;
            return false;
        }
        let escape = matches!(event, Event::KeyDown(key) if key.key_code == KeyCode::Escape);
        let close = match event {
            Event::KeyDown(key) if escape => !key.is_repeat,
            _ => popup.pointer_dismissed(cx, event),
        };
        if close {
            let popup = popovers.remove(index);
            if escape {
                cx.set_key_focus(popup.trigger);
            }
            popup.redraw(cx);
        }
        escape
    })
}

fn top_modal(window: &str) -> Option<u64> {
    WINDOW_MODALS.with(|modals| {
        modals
            .borrow()
            .iter()
            .rev()
            .find(|m| m.window == window)
            .map(|m| m.widget)
    })
}

fn modal_backdrop(cx: &Cx, pass: PassId, owner: Option<u64>) -> Option<Area> {
    let owner = owner?;
    FLOATS.with(|floats| {
        floats.borrow().iter().find_map(|float| {
            (float.modal == Some(owner)
                && float_pass(cx, float.area) == Some(pass)
                && float_drawn(cx, float.area))
            .then_some(float.area)
        })
    })
}

/// Shell, around the dispatch of `window`, which `anchor` is drawn in. While `content_on_top`,
/// the window's drawn floating surfaces shield the pointer for the panels and chrome
/// dispatched next: the top modal sheet and cards inside it, or the page's popovers otherwise.
/// Without it (a shell layer owns the input, or the dispatch is over) nothing does.
pub(crate) fn shield_content(cx: &mut Cx, window: &str, anchor: Area, content_on_top: bool) {
    let shields = if content_on_top {
        let top = top_modal(window);
        FLOATS.with(|floats| {
            let mut floats = floats.borrow_mut();
            floats.retain(|float| float_drawn(cx, float.area));
            let pass = float_pass(cx, anchor);
            let in_window = |float: &&Float| pass.is_some() && float_pass(cx, float.area) == pass;
            let backdrop = top.and_then(|owner| {
                floats
                    .iter()
                    .filter(in_window)
                    .find(|float| float.modal == Some(owner))
                    .map(|float| float.area)
            });
            // Retain only the top sheet's descendants; cards behind it must not punch holes.
            let mut shields: Vec<_> = backdrop
                .into_iter()
                .chain(floats.iter().filter(in_window).filter_map(|float| {
                    (float.modal.is_none()
                        && (top.is_none()
                            || backdrop.is_some_and(|area| cx.area_drawn_within(float.area, area))))
                    .then_some(float.area)
                }))
                .collect();
            POPOVERS.with(|popovers| {
                if let Some(popup) = popovers.borrow().iter().find(|p| Some(p.pass) == pass)
                    && float_drawn(cx, popup.trigger)
                    && float_drawn(cx, popup.body)
                    && (top.is_none()
                        || backdrop.is_some_and(|a| cx.area_drawn_within(popup.body, a)))
                {
                    shields.push(popup.body);
                }
            });
            shields
        })
    } else {
        Vec::new()
    };
    cx.set_pointer_shields(shields);
}

fn float_pass(cx: &Cx, area: Area) -> Option<makepad_widgets::PassId> {
    cx.draw_lists.checked_index(area.draw_list_id()?)?.pass_id
}

/// The area itself can remain valid after the drawing that attached its subtree is gone.
/// Check every attachment, not just the first parent (e.g. a hidden terminal cover's
/// wrapper and cached contents can both keep their last paint).
fn float_drawn(cx: &Cx, area: Area) -> bool {
    let Some(mut list) = area
        .draw_list_id()
        .and_then(|id| cx.draw_lists.checked_index(id))
        .filter(|_| area.is_valid(cx))
    else {
        return false;
    };
    for _ in 0..64 {
        let Some(parent) = list.codeflow_parent_id else {
            return true;
        };
        let Some(parent) = cx.draw_lists.checked_index(parent) else {
            return false;
        };
        if parent.redraw_id != list.codeflow_parent_redraw_id {
            return false;
        }
        list = parent;
    }
    false
}

/// Claim dismissal before dispatching to children; a handled Back or repeat must not
/// unwind another owner. Escape is contained by the caller's arrival-time modal lock.
pub fn take_dismissal(event: &makepad_widgets::Event) -> bool {
    use makepad_widgets::{Event, KeyCode};
    match event {
        Event::BackPressed { handled } if !handled.get() => {
            handled.set(true);
            true
        }
        Event::KeyDown(key) => key.key_code == KeyCode::Escape && !key.is_repeat,
        _ => false,
    }
}

/// Fit a popup into the visible viewport, including viewports smaller than its margin.
/// Callers remove keyboard coverage before positioning; this never imposes a minimum
/// size that could move the popup back underneath the keyboard.
pub fn fit_in_viewport(mut popup: Rect, viewport: Rect, margin: f64) -> Rect {
    let width = viewport.size.x.max(0.0);
    let height = viewport.size.y.max(0.0);
    let mx = margin.max(0.0).min(width * 0.5);
    let my = margin.max(0.0).min(height * 0.5);
    popup.size.x = popup.size.x.clamp(0.0, width - 2.0 * mx);
    popup.size.y = popup.size.y.clamp(0.0, height - 2.0 * my);
    popup.pos.x = popup.pos.x.clamp(
        viewport.pos.x + mx,
        viewport.pos.x + width - mx - popup.size.x,
    );
    popup.pos.y = popup.pos.y.clamp(
        viewport.pos.y + my,
        viewport.pos.y + height - my - popup.size.y,
    );
    popup
}

/// A release must resolve to the target that accepted the press, not a neighbour
/// exposed by scrolling or a redraw. Gesture owners cancel this when they take over.
pub struct TapTarget<T> {
    pressed: Option<T>,
}

impl<T> Default for TapTarget<T> {
    fn default() -> Self {
        Self { pressed: None }
    }
}

impl<T: PartialEq> TapTarget<T> {
    pub fn press(&mut self, target: Option<T>) {
        self.pressed = target;
    }

    pub fn cancel(&mut self) {
        self.pressed = None;
    }

    pub fn release(&mut self, target: Option<T>, valid: bool) -> Option<T> {
        let pressed = self.pressed.take()?;
        (valid && target.as_ref() == Some(&pressed)).then_some(pressed)
    }
}

/// Owner prefix for overlays opened by widgets without a tab key.
pub const WIDGET_OWNER_PREFIX: &str = "widget:";

/// The overlay owner of a widget that has no tab key (answers land in [`take_answers`]).
pub fn widget_owner(uid: makepad_widgets::WidgetUid) -> String {
    format!("{WIDGET_OWNER_PREFIX}{}", uid.0)
}

/// Runs `open` with overlays owned by `owner` (for example [`widget_owner`]).
pub fn with_owner<R>(ui: &mut UiContext, owner: &str, open: impl FnOnce(&mut UiContext) -> R) -> R {
    let previous = ui.tab_key.replace(owner.to_string());
    let result = open(ui);
    ui.tab_key = previous;
    result
}

/// Answers for a [`widget_owner`]: `(id, args)` in arrival order.
pub fn take_answers(owner: &str) -> Vec<(u64, Value)> {
    ANSWERS.with(|answers| {
        let mut answers = answers.borrow_mut();
        let (mine, rest): (Vec<_>, Vec<_>) = answers.drain(..).partition(|(o, _, _)| o == owner);
        *answers = rest;
        mine.into_iter().map(|(_, id, args)| (id, args)).collect()
    })
}

pub(crate) fn deliver_widget_answer(owner: &str, id: u64, args: Value) {
    ANSWERS.with(|answers| answers.borrow_mut().push((owner.to_string(), id, args)));
    makepad_widgets::SignalToUI::set_ui_signal();
}

/// A desktop tooltip: Flutter `Tooltip` text, the control it describes (window coordinates)
/// and its wait before showing.
#[derive(Clone, Debug, PartialEq)]
pub struct Tip {
    pub text: String,
    pub anchor: Rect,
    /// `waitDuration` in seconds (theme default 0.5).
    pub wait: f64,
}

/// Desktop hover tooltip (original `Tooltip`, 500 ms wait, never on compact layouts): call with
/// the text when the pointer enters a control and with `None` when it leaves. `abs` is the
/// pointer position (`FingerHoverEvent::abs`); prefer [`tooltip_at`] with the control's rect,
/// which places the tooltip as Flutter does.
pub fn tooltip(ui: &UiContext, text: Option<&str>, abs: DVec2) {
    tooltip_at(
        ui,
        text,
        Rect {
            pos: abs,
            size: DVec2::default(),
        },
        0.5,
    );
}

/// Flutter `Tooltip(waitDuration:)` anchored to the control `anchor`: centred on it, 24px
/// below its centre (above when it would overflow), after `wait` seconds.
pub fn tooltip_at(ui: &UiContext, text: Option<&str>, anchor: Rect, wait: f64) {
    if ui.compact {
        return;
    }
    let request = text.filter(|t| !t.is_empty()).map(|t| Tip {
        text: t.to_string(),
        anchor,
        wait,
    });
    queue_tooltip(&ui.window_id, request);
    makepad_widgets::SignalToUI::set_ui_signal();
}

fn queue_tooltip(window: &str, request: Option<Tip>) {
    TOOLTIPS.with(|tips| {
        let mut tips = tips.borrow_mut();
        if let Some((_, pending)) = tips.iter_mut().find(|(id, _)| id == window) {
            // Sibling roots receive enter/leave in either order. The entered control
            // wins over a sibling's leave until this event's requests are drained.
            if request.is_some() || pending.is_none() {
                *pending = request;
            }
        } else {
            tips.push((window.to_string(), request));
        }
    });
}

/// This event's tooltip request for `window` (`Some(None)` hides).
pub(crate) fn take_tooltip(window: &str) -> Option<Option<Tip>> {
    TOOLTIPS.with(|tips| {
        let mut tips = tips.borrow_mut();
        let index = tips.iter().position(|(w, _)| w == window)?;
        Some(tips.remove(index).1)
    })
}

fn owner_of(ui: &UiContext) -> String {
    ui.tab_key.clone().unwrap_or_else(|| PAGE_OWNER.to_string())
}

pub(crate) fn next_id() -> u64 {
    NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
}

fn enqueue(ui: &UiContext, kind: OverlayKind) -> u64 {
    let id = next_id();
    let request = OverlayRequest {
        id,
        owner: owner_of(ui),
        window: ui.window_id.clone(),
        kind,
    };
    QUEUE.with(|queue| queue.borrow_mut().push(request));
    id
}

/// Popover placement options (`showPaseoMenu` width/alignEnd/compactSheet).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuOptions {
    pub width: f64,
    pub align_end: bool,
    pub sheet: bool,
}

impl Default for MenuOptions {
    fn default() -> Self {
        Self {
            width: 220.0,
            align_end: false,
            sheet: true,
        }
    }
}

/// Anchored menu (desktop) / bottom sheet (compact). `anchor` is the trigger rect in window
/// coordinates.
pub fn open_menu(
    ui: &mut UiContext,
    anchor: Rect,
    title: Option<String>,
    entries: Vec<MenuEntry>,
) -> u64 {
    open_menu_with(ui, anchor, title, entries, MenuOptions::default())
}

pub fn open_menu_with(
    ui: &mut UiContext,
    anchor: Rect,
    title: Option<String>,
    entries: Vec<MenuEntry>,
    options: MenuOptions,
) -> u64 {
    ui.effects.push(crate::common::UiEffect::RedrawOverlays);
    enqueue(ui, menu_kind(anchor, title, entries, options))
}

pub(crate) fn menu_kind(
    anchor: Rect,
    title: Option<String>,
    entries: Vec<MenuEntry>,
    options: MenuOptions,
) -> OverlayKind {
    OverlayKind::Menu {
        title,
        anchor,
        entries,
        width: options.width,
        align_end: options.align_end,
        sheet: options.sheet,
    }
}

/// Original `confirmAction`.
pub fn confirm(
    ui: &mut UiContext,
    title: &str,
    message: &str,
    confirm_label: &str,
    destructive: bool,
) -> u64 {
    ui.effects.push(crate::common::UiEffect::RedrawOverlays);
    enqueue(
        ui,
        OverlayKind::Confirm {
            title: title.to_string(),
            message: message.to_string(),
            confirm_label: confirm_label.to_string(),
            destructive,
        },
    )
}

/// Original `promptText`.
pub fn prompt_text(
    ui: &mut UiContext,
    title: &str,
    message: Option<&str>,
    initial: &str,
    placeholder: &str,
    confirm_label: &str,
) -> u64 {
    enqueue_prompt(
        ui,
        title,
        message,
        initial,
        placeholder,
        confirm_label,
        false,
    )
}

/// Original `promptText(multiline: true)`: a 4–12 line field where Enter inserts a newline;
/// only the confirm button answers.
pub fn prompt_multiline(
    ui: &mut UiContext,
    title: &str,
    message: Option<&str>,
    initial: &str,
    placeholder: &str,
    confirm_label: &str,
) -> u64 {
    enqueue_prompt(
        ui,
        title,
        message,
        initial,
        placeholder,
        confirm_label,
        true,
    )
}

fn enqueue_prompt(
    ui: &mut UiContext,
    title: &str,
    message: Option<&str>,
    initial: &str,
    placeholder: &str,
    confirm_label: &str,
    multiline: bool,
) -> u64 {
    ui.effects.push(crate::common::UiEffect::RedrawOverlays);
    enqueue(
        ui,
        OverlayKind::Prompt {
            title: title.to_string(),
            message: message.map(str::to_string),
            initial: initial.to_string(),
            placeholder: placeholder.to_string(),
            confirm_label: confirm_label.to_string(),
            multiline,
        },
    )
}

/// `showPaseoSelect` arguments beyond the anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectRequest {
    pub title: String,
    pub options: Vec<SelectOption>,
    pub value: Option<String>,
    pub searchable: bool,
    pub search_placeholder: Option<String>,
    pub empty_text: String,
    pub min_width: f64,
}

impl SelectRequest {
    pub fn new(title: impl Into<String>, options: Vec<SelectOption>) -> Self {
        Self {
            title: title.into(),
            options,
            value: None,
            searchable: false,
            search_placeholder: None,
            empty_text: crate::common::tr("common.empty.noOptions"),
            min_width: 220.0,
        }
    }
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
    pub fn searchable(mut self, placeholder: Option<String>) -> Self {
        self.searchable = true;
        self.search_placeholder = placeholder;
        self
    }
    pub fn empty_text(mut self, text: impl Into<String>) -> Self {
        self.empty_text = text.into();
        self
    }
    pub fn min_width(mut self, width: f64) -> Self {
        self.min_width = width;
        self
    }
}

/// Original `showPaseoSelect`; `anchor` is the trigger rect in window coordinates. Answers
/// `{"value": <option value>}` or `{"cancelled": true}`.
pub fn open_select(ui: &mut UiContext, anchor: Rect, request: SelectRequest) -> u64 {
    ui.effects.push(crate::common::UiEffect::RedrawOverlays);
    let SelectRequest {
        title,
        options,
        value,
        searchable,
        search_placeholder,
        empty_text,
        min_width,
    } = request;
    enqueue(
        ui,
        OverlayKind::Select {
            title,
            anchor,
            options,
            value,
            searchable,
            search_placeholder,
            empty_text,
            min_width,
        },
    )
}

/// Requests for one window's shell; others stay queued for their own window.
pub(crate) fn take_requests(window: &str) -> Vec<OverlayRequest> {
    QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        let (mine, rest): (Vec<_>, Vec<_>) = queue.drain(..).partition(|r| r.window == window);
        *queue = rest;
        mine
    })
}

/// Reads an overlay answer addressed to this widget: `Some((id, args))`.
pub fn result_of(command: &crate::common::UiCommand) -> Option<(u64, &Value)> {
    if command.action != OVERLAY_ACTION {
        return None;
    }
    let id = command.args.get("id").and_then(Value::as_u64)?;
    Some((id, &command.args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_anchored_popover(test: impl FnOnce(&mut Cx, [Area; 3], Option<AnchoredPopover>)) {
        use makepad_widgets::{CxRectArea, LiveNew, Pass, RectArea, dvec2};

        let mut cx = Cx::new(Box::new(|_, _| {}));
        let pass = Pass::new(&mut cx);
        let lists = std::array::from_fn::<_, 3, _>(|_| cx.draw_lists.alloc());
        let areas = std::array::from_fn(|index| {
            let (x, width) = [(0.0, 100.0), (10.0, 10.0), (25.0, 40.0)][index];
            let list = &mut cx.draw_lists[lists[index].id()];
            list.pass_id = Some(pass.pass_id());
            list.rect_areas.push(CxRectArea {
                rect: Rect {
                    pos: dvec2(x, 0.0),
                    size: dvec2(width, 100.0),
                },
                draw_clip: (dvec2(0.0, 0.0), dvec2(100.0, 100.0)),
            });
            Area::Rect(RectArea {
                draw_list_id: lists[index].id(),
                rect_id: 0,
                redraw_id: list.redraw_id,
            })
        });
        let mut slot = None;
        let [page, trigger, body] = areas;
        AnchoredPopover::toggle(&mut slot, &mut cx, trigger);
        slot.as_ref().unwrap().drawn(trigger, body);
        shield_content(&mut cx, "main", page, true);
        test(&mut cx, areas, slot);
    }

    fn popup_mouse(x: f64, down: bool) -> Event {
        use makepad_widgets::{MouseDownEvent, MouseUpEvent, WindowId, dvec2};
        if down {
            Event::MouseDown(MouseDownEvent {
                abs: dvec2(x, 5.0),
                button: MouseButton::PRIMARY,
                window_id: WindowId(0, 0),
                modifiers: Default::default(),
                handled: Cell::new(Area::Empty),
                time: 1.0,
            })
        } else {
            Event::MouseUp(MouseUpEvent {
                abs: dvec2(x, 5.0),
                button: MouseButton::PRIMARY,
                window_id: WindowId(0, 0),
                modifiers: Default::default(),
                time: 1.1,
            })
        }
    }

    #[test]
    fn anchored_popover_outside_click_dismisses_on_release_and_reaches_its_target() {
        use makepad_widgets::Hit;
        with_anchored_popover(|cx, [page, trigger, _], slot| {
            let popup = slot.unwrap();
            let down = popup_mouse(80.0, true);
            assert!(!handle_anchored_popover(cx, "main", page, &down, true));
            assert!(popup.is_open());
            assert!(matches!(down.hits(cx, page), Hit::FingerDown(_)));
            cx.set_key_focus(page);

            let up = popup_mouse(80.0, false);
            assert!(!handle_anchored_popover(cx, "main", page, &up, true));
            assert!(!popup.is_open());
            assert!(matches!(up.hits(cx, page), Hit::FingerUp(hit) if hit.is_over));
            assert!(!cx.clear_key_focus_if(trigger));
            assert!(cx.clear_key_focus_if(page));
        });
    }

    #[test]
    fn anchored_popover_invoker_toggles_instead_of_reopening() {
        with_anchored_popover(|cx, [page, trigger, _], mut slot| {
            for event in [popup_mouse(15.0, true), popup_mouse(15.0, false)] {
                assert!(!handle_anchored_popover(cx, "main", page, &event, true));
                assert!(slot.as_ref().unwrap().is_open());
            }
            AnchoredPopover::toggle(&mut slot, cx, trigger);
            assert!(slot.is_none());
            AnchoredPopover::toggle(&mut slot, cx, trigger);
            assert!(slot.unwrap().is_open());
        });
    }

    #[test]
    fn anchored_popover_escape_restores_trigger_focus_and_not_when_covered() {
        use makepad_widgets::KeyEvent;
        with_anchored_popover(|cx, [page, trigger, body], slot| {
            let popup = slot.unwrap();
            cx.set_key_focus(body);
            let event = Event::KeyDown(KeyEvent {
                key_code: KeyCode::Escape,
                ..Default::default()
            });
            assert!(!handle_anchored_popover(cx, "main", page, &event, false));
            assert!(popup.is_open());
            assert!(handle_anchored_popover(cx, "main", page, &event, true));
            assert!(!popup.is_open());
            assert!(cx.clear_key_focus_if(trigger));
        });
    }

    #[test]
    fn anchored_popover_inside_input_is_shielded_and_does_not_dismiss() {
        use makepad_widgets::event::ScrollEvent;
        use makepad_widgets::{Hit, WindowId, dvec2};
        with_anchored_popover(|cx, [page, _, body], slot| {
            let popup = slot.unwrap();
            let down = popup_mouse(26.0, true);
            assert!(!handle_anchored_popover(cx, "main", page, &down, true));
            assert!(matches!(down.hits(cx, page), Hit::Nothing));
            assert!(matches!(down.hits(cx, body), Hit::FingerDown(_)));
            let up = popup_mouse(26.0, false);
            assert!(!handle_anchored_popover(cx, "main", page, &up, true));
            assert!(matches!(up.hits(cx, body), Hit::FingerUp(hit) if hit.is_over));
            for x in [26.0, 80.0] {
                let scroll = Event::Scroll(ScrollEvent {
                    abs: dvec2(x, 5.0),
                    scroll: dvec2(0.0, 8.0),
                    window_id: WindowId(0, 0),
                    modifiers: Default::default(),
                    handled_x: Cell::new(false),
                    handled_y: Cell::new(false),
                    is_mouse: true,
                    time: 2.0,
                });
                assert!(!handle_anchored_popover(cx, "main", page, &scroll, true));
                assert_eq!(cx.pointer_shielded(page, dvec2(x, 5.0)), x == 26.0);
            }
            assert!(popup.is_open());
        });
    }

    #[test]
    fn anchored_popover_ignores_drag_and_cross_boundary_release_but_closes_on_blur() {
        with_anchored_popover(|cx, [page, _, _], slot| {
            let popup = slot.unwrap();
            for (start, end) in [(26.0, 23.0), (23.0, 26.0), (80.0, 90.0)] {
                for event in [popup_mouse(start, true), popup_mouse(end, false)] {
                    assert!(!handle_anchored_popover(cx, "main", page, &event, true));
                    assert!(popup.is_open());
                }
            }
            assert!(!handle_anchored_popover(
                cx,
                "main",
                page,
                &Event::AppLostFocus,
                true
            ));
            assert!(!popup.is_open());
        });
    }

    #[test]
    fn modal_keeps_nested_cards_shielded_without_reviving_the_page_popover() {
        use makepad_widgets::{CxRectArea, LiveNew, Pass, RectArea};

        let mut cx = Cx::new(Box::new(|_, _| {}));
        let pass = Pass::new(&mut cx);
        let lists: Vec<_> = (0..5).map(|_| cx.draw_lists.alloc()).collect();
        let mut areas = Vec::new();
        for (index, (x, width)) in [
            (0.0, 100.0),
            (0.0, 100.0),
            (0.0, 100.0),
            (40.0, 20.0),
            (45.0, 5.0),
        ]
        .into_iter()
        .enumerate()
        {
            let list = &mut cx.draw_lists[lists[index].id()];
            list.pass_id = Some(pass.pass_id());
            list.rect_areas.push(CxRectArea {
                rect: Rect {
                    pos: DVec2 { x, y: 0.0 },
                    size: DVec2 { x: width, y: 100.0 },
                },
                draw_clip: (DVec2::default(), DVec2 { x: 100.0, y: 100.0 }),
            });
            areas.push(Area::Rect(RectArea {
                draw_list_id: lists[index].id(),
                rect_id: 0,
                redraw_id: list.redraw_id,
            }));
        }
        let [page, behind, modal, card, control] = areas[..] else {
            unreachable!()
        };
        cx.draw_lists[lists[3].id()].codeflow_parent_id = Some(lists[2].id());
        cx.draw_lists[lists[4].id()].codeflow_parent_id = Some(lists[3].id());
        WINDOW_MODALS.with(|modals| {
            *modals.borrow_mut() = vec![WindowModalOwner {
                window: "main".into(),
                widget: 1,
                tab: Some("tab".into()),
            }];
        });
        FLOATS.with(|floats| floats.borrow_mut().clear());
        float(behind);
        float(card);
        modal_float(1, modal);
        shield_content(&mut cx, "main", page, true);

        let outside_card = DVec2 { x: 10.0, y: 5.0 };
        assert!(cx.pointer_shielded(page, outside_card));
        assert!(!cx.pointer_shielded(modal, outside_card));
        let inside_card = DVec2 { x: 47.0, y: 5.0 };
        assert!(cx.pointer_shielded(modal, inside_card));
        assert!(!cx.pointer_shielded(control, inside_card));

        shield_content(&mut cx, "main", page, false);
        release_window_modal("main", 1);
        FLOATS.with(|floats| floats.borrow_mut().clear());
    }

    #[test]
    fn detached_terminal_cover_no_longer_shields_the_session_list() {
        use makepad_widgets::{
            Cx2d, CxDraw, DrawEvent, DrawList, DrawListExt, Layout, LiveNew, Pass, dvec2,
        };

        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::makepad_draw::live_design(&mut cx);
        makepad_widgets::makepad_draw::shader::draw_trapezoid::DrawTrapezoidVector::register_main_module(
            &mut cx,
        );
        cx.live_expand();
        let pass = Pass::new(&mut cx);
        let size = dvec2(100.0, 100.0);
        pass.set_size(&mut cx, size);
        let mut root = DrawList::new(&mut cx);
        let mut sidebar = DrawList::new(&mut cx);
        let mut terminal = DrawList::new(&mut cx);
        let mut wrapper = DrawList::new(&mut cx);
        let mut cover = DrawList::new(&mut cx);
        let mut paint = |cx: &mut Cx, terminal: Option<&mut DrawList>| {
            let event = DrawEvent::default();
            let mut draw = CxDraw::new(cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            root.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            cx.begin_sized_turtle(size, Layout::default());
            let rect = Rect {
                pos: DVec2::default(),
                size,
            };
            let mut sidebar_area = Area::Empty;
            sidebar.begin_always(&mut cx);
            cx.add_aligned_rect_area(&mut sidebar_area, rect);
            sidebar.end(&mut cx);
            // On the second frame the root draws again, replaying the unchanged terminal.
            if let Some(terminal) = terminal
                && terminal.begin_maybe(&mut cx, false).is_ok()
            {
                wrapper.begin_always(&mut cx);
                cover.begin_always(&mut cx);
                let mut cover_area = Area::Empty;
                cx.add_aligned_rect_area(&mut cover_area, rect);
                float(cover_area);
                cover.end(&mut cx);
                wrapper.end(&mut cx);
                terminal.end(&mut cx);
            }
            cx.end_pass_sized_turtle();
            root.end(&mut cx);
            cx.end_pass(&pass);
            sidebar_area
        };
        let point = dvec2(10.0, 10.0);
        for _ in 0..2 {
            let sidebar_area = paint(&mut cx, Some(&mut terminal));
            shield_content(&mut cx, "main", sidebar_area, true);
            assert!(cx.pointer_shielded(sidebar_area, point));
        }

        // Closing omits the whole terminal branch. The nested wrapper and cover
        // still have matching old paints, even if the lists have been pooled.
        drop(terminal);
        let sidebar_area = paint(&mut cx, None);
        shield_content(&mut cx, "main", sidebar_area, true);
        assert!(!cx.pointer_shielded(sidebar_area, point));
    }

    #[test]
    fn backdrop_requires_same_target_valid_release_and_cannot_replay() {
        let mut tap = TapTarget::default();
        tap.press(Some("surface"));
        assert_eq!(tap.release(Some("backdrop"), true), None);
        tap.press(Some("backdrop"));
        assert_eq!(tap.release(Some("backdrop"), false), None);
        tap.press(Some("backdrop"));
        tap.cancel();
        assert_eq!(tap.release(Some("backdrop"), true), None);
        tap.press(Some("backdrop"));
        assert_eq!(tap.release(Some("backdrop"), true), Some("backdrop"));
        assert_eq!(tap.release(Some("backdrop"), true), None);
    }

    #[test]
    fn back_is_claimed_once_and_repeated_escape_does_not_unwind_stack() {
        use makepad_widgets::{Event, KeyCode, KeyEvent};
        let back = Event::BackPressed {
            handled: Cell::new(false),
        };
        assert!(take_dismissal(&back));
        assert!(!take_dismissal(&back));
        let escape = KeyEvent {
            key_code: KeyCode::Escape,
            ..Default::default()
        };
        assert!(take_dismissal(&Event::KeyDown(escape)));
        assert!(!take_dismissal(&Event::KeyDown(KeyEvent {
            is_repeat: true,
            ..escape
        })));
        assert!(!take_dismissal(&Event::KeyDown(KeyEvent {
            key_code: KeyCode::ReturnKey,
            ..Default::default()
        })));
    }

    #[test]
    fn popup_fits_keyboard_viewport_and_degenerate_window() {
        use makepad_widgets::dvec2;
        let popup = Rect {
            pos: dvec2(-20.0, 400.0),
            size: dvec2(480.0, 300.0),
        };
        let viewport = Rect {
            pos: dvec2(20.0, 30.0),
            size: dvec2(320.0, 140.0),
        };
        let fitted = fit_in_viewport(popup, viewport, 8.0);
        assert_eq!(fitted.pos, dvec2(28.0, 38.0));
        assert_eq!(fitted.size, dvec2(304.0, 124.0));
        let tiny = fit_in_viewport(
            popup,
            Rect {
                pos: dvec2(10.0, 12.0),
                size: dvec2(4.0, 0.0),
            },
            8.0,
        );
        assert_eq!(tiny.pos, dvec2(12.0, 12.0));
        assert_eq!(tiny.size, dvec2(0.0, 0.0));
    }

    #[test]
    fn modal_owner_gates_siblings_even_when_they_share_a_tab() {
        WINDOW_MODALS.with(|modals| {
            *modals.borrow_mut() = vec![
                WindowModalOwner {
                    window: "main".into(),
                    widget: 1,
                    tab: Some("same".into()),
                },
                WindowModalOwner {
                    window: "main".into(),
                    widget: 2,
                    tab: Some("same".into()),
                },
            ];
        });
        assert!(!window_modal_accepts_owner("main", 1));
        assert!(window_modal_accepts_owner("main", 2));
        release_window_modal("main", 2);
        assert!(window_modal_accepts_owner("main", 1));
        release_window_modal("main", 1);
    }

    #[test]
    fn tooltip_enter_wins_over_sibling_leave_in_either_order() {
        let tip = Tip {
            text: "file.rs".into(),
            anchor: Rect::default(),
            wait: 0.3,
        };
        for enter_first in [true, false] {
            if enter_first {
                queue_tooltip("main", Some(tip.clone()));
                queue_tooltip("main", None);
            } else {
                queue_tooltip("main", None);
                queue_tooltip("main", Some(tip.clone()));
            }
            assert_eq!(take_tooltip("main"), Some(Some(tip.clone())));
            queue_tooltip("main", None);
            assert_eq!(take_tooltip("main"), Some(None));
        }
    }

    #[test]
    fn tooltip_updates_keep_windows_independent() {
        let first = Tip {
            text: "first.rs".into(),
            anchor: Rect::default(),
            wait: 0.3,
        };
        let next = Tip {
            text: "next.rs".into(),
            ..first.clone()
        };
        queue_tooltip("main", Some(first.clone()));
        queue_tooltip("secondary", Some(first.clone()));
        queue_tooltip("main", Some(next.clone()));
        assert_eq!(take_tooltip("main"), Some(Some(next)));
        assert_eq!(take_tooltip("secondary"), Some(Some(first)));
        assert_eq!(take_tooltip("main"), None);
    }

    #[test]
    fn releasing_modal_preserves_nested_owners_and_other_windows() {
        WINDOW_MODALS.with(|modals| {
            *modals.borrow_mut() = vec![
                WindowModalOwner {
                    window: "main".into(),
                    widget: 1,
                    tab: Some("parent".into()),
                },
                WindowModalOwner {
                    window: "main".into(),
                    widget: 2,
                    tab: Some("child".into()),
                },
                WindowModalOwner {
                    window: "secondary".into(),
                    widget: 1,
                    tab: Some("other".into()),
                },
            ];
        });
        assert!(!window_modal_accepts_tab("main", "parent"));
        assert!(window_modal_accepts_tab("main", "child"));
        assert!(!window_modal_accepts_tab("main", "other"));
        assert!(window_modal_accepts_tab("secondary", "other"));

        release_window_modal("main", 1);
        release_window_modal("main", 1);
        assert!(window_modal_open("main"));
        assert!(window_modal_open("secondary"));

        release_window_modal("main", 2);
        assert!(!window_modal_open("main"));
        assert!(window_modal_accepts_tab("main", "parent"));
        assert!(window_modal_open("secondary"));

        release_window_modal("secondary", 1);
        assert!(!window_modal_open("secondary"));
    }

    #[test]
    fn closing_top_modal_restores_its_parent_tab_input() {
        WINDOW_MODALS.with(|modals| {
            *modals.borrow_mut() = vec![
                WindowModalOwner {
                    window: "main".into(),
                    widget: 1,
                    tab: Some("parent".into()),
                },
                WindowModalOwner {
                    window: "main".into(),
                    widget: 2,
                    tab: Some("child".into()),
                },
            ];
        });
        assert!(!window_modal_accepts_tab("main", "parent"));
        release_window_modal("main", 2);
        assert!(window_modal_accepts_tab("main", "parent"));
        assert!(!window_modal_accepts_tab("main", "child"));
        release_window_modal("main", 1);
        assert!(window_modal_accepts_tab("main", "child"));
    }

    #[test]
    fn page_modal_blocks_tabs_and_releases_on_drop() {
        WINDOW_MODALS.with(|modals| {
            *modals.borrow_mut() = vec![
                WindowModalOwner {
                    window: "main".into(),
                    widget: 1,
                    tab: Some("parent".into()),
                },
                WindowModalOwner {
                    window: "main".into(),
                    widget: 2,
                    tab: Some(PAGE_OWNER.into()),
                },
            ];
        });
        let modal = WindowModal {
            window_id: "main".into(),
            owner: 2,
        };
        assert!(window_modal_accepts_tab("main", PAGE_OWNER));
        assert!(!window_modal_accepts_tab("main", "parent"));
        drop(modal);
        assert!(!window_modal_accepts_tab("main", PAGE_OWNER));
        assert!(window_modal_accepts_tab("main", "parent"));
        release_window_modal("main", 1);
    }
}
