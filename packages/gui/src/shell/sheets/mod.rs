//! Adaptive sheets (Flutter `showAdaptiveSheet` content): host connection flow, host
//! connections/picker/chooser, workspace labels, shortcut help, compact workspace tabs, import
//! sessions; plus the welcome screen and the sessions page drawn in the content area.

pub mod history;
pub mod host_connections;
pub mod host_dialog;
pub mod labels;
pub mod qr_code;
pub mod shortcut_help;
pub mod welcome;
pub mod workspace_tabs;

use std::cell::Cell;
use std::sync::Arc;

use makepad_widgets::*;
use paseo_gui_core::{CoreError, CoreHandle, RequestId};
use serde_json::Value;

use crate::common::widgets::PTextFieldWidgetRefExt;
use crate::common::{UiContext, UiEffect};
use crate::shell::ui::Ui;
use crate::shell::{PaseoShell, Page, Pending};

pub fn live_design(_cx: &mut Cx) {}

thread_local! {
    /// The shell's `PTextField` template (sheets build their fields lazily from it).
    static FIELD_PTR: Cell<Option<LivePtr>> = const { Cell::new(None) };
    /// The shell's `QrScanView` template (pairing scan sheet).
    static QR_PTR: Cell<Option<LivePtr>> = const { Cell::new(None) };
    static FIELDS_ACTIVE: Cell<bool> = const { Cell::new(true) };
}

/// Shell: the `PTextField` template sheet fields are built from.
pub fn register_field_ptr(ptr: Option<LivePtr>) {
    FIELD_PTR.with(|p| p.set(ptr));
}

/// Shell: the `QrScanView` template.
pub fn register_qr_ptr(ptr: Option<LivePtr>) {
    QR_PTR.with(|p| p.set(ptr));
}

/// Covered sheets still draw their fields, but only the top owner may autofocus.
pub(super) fn set_fields_active(active: bool) {
    FIELDS_ACTIVE.with(|current| current.set(active));
}

pub(crate) fn qr_ptr() -> Option<LivePtr> {
    QR_PTR.with(Cell::get)
}

/// What a sheet text field reported this event.
pub enum FieldEvent {
    Changed(String),
    Returned(String),
}

/// A text field inside a sheet: created on first draw, keeps its text while undrawn.
pub struct Field {
    widget: Option<WidgetRef>,
    text: String,
    placeholder: String,
    password: bool,
    focus: bool,
    read_only: bool,
}

impl Field {
    pub fn new(text: &str, placeholder: &str) -> Self {
        Self { widget: None, text: text.to_string(), placeholder: placeholder.to_string(), password: false, focus: false, read_only: false }
    }

    /// Hides the text (passwords).
    pub fn password(mut self) -> Self {
        self.password = true;
        self
    }

    /// Takes key focus when first shown (`autofocus`).
    pub fn autofocus(mut self) -> Self {
        self.focus = true;
        self
    }

    pub fn text(&self) -> String {
        self.widget.as_ref().map_or_else(|| self.text.clone(), |w| w.text())
    }

    pub fn set_text(&mut self, cx: &mut Cx, text: &str) {
        self.text = text.to_string();
        if let Some(widget) = &self.widget {
            widget.set_text(cx, text);
        }
    }

    /// Shows or hides the text (password visibility toggle).
    pub fn set_password(&mut self, cx: &mut Cx, password: bool) {
        self.password = password;
        if let Some(widget) = &self.widget {
            widget.as_ptext_field().set_password(cx, password);
        }
    }

    pub fn set_placeholder(&mut self, cx: &mut Cx, text: &str) {
        self.placeholder = text.to_string();
        if let Some(widget) = &self.widget {
            widget.as_ptext_field().set_placeholder(cx, text);
        }
    }

    pub fn set_read_only(&mut self, cx: &mut Cx, read_only: bool) {
        self.read_only = read_only;
        if let Some(widget) = &self.widget {
            widget.as_ptext_field().set_read_only(cx, read_only);
        }
    }

    pub fn focus(&mut self, cx: &mut Cx) {
        match &self.widget {
            Some(widget) => widget.as_ptext_field().set_key_focus(cx),
            None => self.focus = true,
        }
    }

    pub fn draw(&mut self, cx: &mut Cx2d, scope: &mut Scope, r: Rect) {
        if self.widget.is_none() {
            let widget = WidgetRef::new_from_ptr(cx, FIELD_PTR.with(Cell::get));
            widget.set_text(cx, &self.text);
            let field = widget.as_ptext_field();
            field.set_placeholder(cx, &self.placeholder);
            field.set_password(cx, self.password);
            field.set_read_only(cx, self.read_only);
            self.widget = Some(widget);
        }
        let widget = self.widget.clone().unwrap();
        draw_widget(cx, scope, &widget, r);
        if FIELDS_ACTIVE.with(Cell::get) && std::mem::take(&mut self.focus) {
            widget.as_ptext_field().set_key_focus(cx);
        }
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> Option<FieldEvent> {
        let widget = self.widget.clone()?;
        widget.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return None };
        let field = widget.as_ptext_field();
        if let Some(text) = field.returned(actions) {
            self.text = text.clone();
            return Some(FieldEvent::Returned(text));
        }
        if let Some(text) = field.changed(actions) {
            self.text = text.clone();
            return Some(FieldEvent::Changed(text));
        }
        None
    }
}

/// What a sheet does after an interaction.
pub enum SheetFlow {
    Stay,
    Close,
    /// Replace this sheet (form → method picker → form, as Flutter's loop).
    Replace(Box<dyn Sheet>),
    /// Stack another sheet above this one (confirmation over a form).
    Push(Box<dyn Sheet>),
}

/// The shell frame around a sheet body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetStyle {
    /// `showAdaptiveSheet`: centred dialog on wide layouts, bottom sheet on compact ones.
    Adaptive,
    /// A pushed full-screen route (`MaterialPageRoute`) with the original back header; the
    /// back island closes it.
    FullScreen,
    /// Compact `showModalBottomSheet` + `DraggableScrollableSheet(initial .6, min .3, max .9,
    /// snap)`: the base medium title 24px in, the body below it; dragging resizes, a drag far
    /// below the minimum dismisses. `material_handle` is Material's 32x4 handle in a 48px band
    /// (`showDragHandle`); otherwise the sheet's own 36x4 surface2 handle with 10px margins.
    /// Wide layouts use [`SheetStyle::Adaptive`].
    Draggable { material_handle: bool },
    /// `showGeneralDialog` bottom panel fading in over 200ms above a 35% scrim: surface2,
    /// 16px top radius, 24/16/24 padding (≥24 at the bottom), a muted 40x4 handle 16px above
    /// the body.
    FadeBottom,
}

/// Shell operations a sheet asks for; applied after the sheet call returns.
pub enum ShellOp {
    OpenSheet(Box<dyn Sheet>),
    ShowPage(Page),
    /// Close every tab of these workspaces on `host` (archived/removed workspaces).
    CloseWorkspaceTabs { host_id: String, workspace_ids: Vec<String> },
    Toast { message: String, error: bool },
    /// Answer to a sheet waiting on another sheet (confirmation result).
    Deliver { sheet: u64, tag: String, value: Value },
    /// Any other shell action (open a tab, continue a host-chooser purpose, ...).
    Run(ShellAction),
}

/// A deferred shell operation run with the shell, the frame context and the window's
/// `UiContext` once the sheet call returns.
pub type ShellAction = Box<dyn FnOnce(&mut PaseoShell, &mut Cx, &mut UiContext)>;

/// Side-effect collector handed to sheet calls (no borrow of the shell is held).
pub struct SheetOps {
    pub core: CoreHandle,
    pub state: Arc<Value>,
    pub compact: bool,
    /// The sheet this call runs for (target of answers from sheets it pushes).
    pub origin: Option<u64>,
    pub effects: Vec<UiEffect>,
    pub requests: Vec<(RequestId, String)>,
    pub shell: Vec<ShellOp>,
}

impl SheetOps {
    pub fn new(ui: &UiContext) -> Self {
        Self {
            core: ui.core.clone(),
            state: ui.state.clone(),
            compact: ui.compact,
            origin: None,
            effects: Vec::new(),
            requests: Vec::new(),
            shell: Vec::new(),
        }
    }

    pub fn for_sheet(ui: &UiContext, sheet: u64) -> Self {
        let mut ops = Self::new(ui);
        ops.origin = Some(sheet);
        ops
    }

    /// A core request whose response comes back to `Sheet::on_response` with `tag`.
    pub fn request(&mut self, host_id: &str, message: Value, response_type: &str, tag: &str) -> RequestId {
        let id = self.core.request(host_id, message, response_type);
        self.requests.push((id.clone(), tag.to_string()));
        id
    }

    /// Routes the `Response` of a request started elsewhere (`probe_host`) to this sheet.
    pub fn track(&mut self, request: RequestId, tag: &str) {
        self.requests.push((request, tag.to_string()));
    }

    /// Runs `action` against the shell once this sheet call returns.
    pub fn run(&mut self, action: impl FnOnce(&mut PaseoShell, &mut Cx, &mut UiContext) + 'static) {
        self.shell.push(ShellOp::Run(Box::new(action)));
    }

    pub fn toast(&mut self, message: impl Into<String>, error: bool) {
        self.shell.push(ShellOp::Toast { message: message.into(), error });
    }

    pub fn effect(&mut self, effect: UiEffect) {
        self.effects.push(effect);
    }
}

pub trait Sheet {
    fn title(&self) -> String;

    /// Shows the leading back chevron; its click arrives as `click(.., "sheet:back", ..)`.
    fn can_go_back(&self) -> bool {
        false
    }

    /// Reads shell state (layout, titles) and the core state right before each draw.
    fn sync(&mut self, _shell: &PaseoShell, _state: &Value) {}

    /// Centered card width on wide layouts (`maxWidth`, 520 default).
    fn max_width(&self) -> f64 {
        520.0
    }

    /// How the shell presents this sheet (adaptive dialog/sheet unless overridden).
    fn style(&self) -> SheetStyle {
        SheetStyle::Adaptive
    }

    /// The height of a footer pinned below the scrolling body (inside a top divider and
    /// 24/12/24/12 padding, plus the bottom inset on compact draggable sheets), drawn by
    /// [`Sheet::draw_footer`] on every non-full-screen presentation except `FadeBottom`.
    fn footer_height(&self) -> f64 {
        0.0
    }

    fn draw_footer(&mut self, _cx: &mut Cx2d, _ui: &mut Ui, _x: f64, _y: f64, _w: f64, _interactive: bool) {}

    /// Full-screen sheets: the body height below the header, set before each draw
    /// (`Expanded` children fill it).
    fn set_body_height(&mut self, _height: f64) {}

    /// Draws the body at (`x`, `y`) within `w`; returns the body height. `interactive` is
    /// false while another overlay is above this one.
    #[allow(clippy::too_many_arguments)]
    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64;

    /// A hit id recorded by `draw` was clicked.
    fn click(&mut self, cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow;

    /// Embedded text fields and other widgets.
    fn handle_event(&mut self, _cx: &mut Cx, _event: &Event, _scope: &mut Scope) -> SheetFlow {
        SheetFlow::Stay
    }

    /// Follow-up work after `handle_event` (requests for typed input, submitting on Enter).
    fn after_event(&mut self, _cx: &mut Cx, _ops: &mut SheetOps) -> SheetFlow {
        SheetFlow::Stay
    }

    /// Responses to requests registered through `SheetOps::request`.
    fn on_response(&mut self, _cx: &mut Cx, _tag: &str, _result: &Result<Value, CoreError>, _ops: &mut SheetOps) -> SheetFlow {
        SheetFlow::Stay
    }

    /// Answers from sheets this one opened (`ShellOp::Deliver`).
    fn on_deliver(&mut self, _cx: &mut Cx, _tag: &str, _value: &Value, _ops: &mut SheetOps) -> SheetFlow {
        SheetFlow::Stay
    }

    /// Dismissed by scrim/close/escape.
    fn closed(&mut self, _cx: &mut Cx, _ops: &mut SheetOps) {}
}

/// Applies collected sheet operations; `sheet` is the issuing sheet (response routing).
pub fn apply_ops(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, ops: SheetOps, sheet: Option<u64>) {
    ui.effects.extend(ops.effects);
    if let Some(sheet) = sheet {
        for (request, tag) in ops.requests {
            shell.data.pending.insert(request, Pending::Sheet { sheet, tag });
        }
    }
    for op in ops.shell {
        match op {
            ShellOp::OpenSheet(next) => {
                shell.data.overlays.open_sheet(next);
            }
            ShellOp::ShowPage(page) => crate::shell::commands::show_page(shell, cx, page),
            ShellOp::CloseWorkspaceTabs { host_id, workspace_ids } => {
                crate::shell::commands::close_workspace_tabs(shell, cx, ui, &host_id, &workspace_ids);
            }
            ShellOp::Toast { message, error } => crate::shell::overlays::toast(&mut shell.data.overlays, cx, message, error),
            ShellOp::Deliver { sheet: target, tag, value } => deliver(shell, cx, ui, target, &tag, &value),
            ShellOp::Run(action) => action(shell, cx, ui),
        }
    }
    shell.redraw(cx);
}

/// Routes a core response to the sheet that asked for it.
pub fn route_response(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, sheet: u64, tag: &str, result: &Result<Value, CoreError>) {
    let mut ops = SheetOps::for_sheet(ui, sheet);
    let flow = match shell.data.overlays.sheet_mut(sheet) {
        Some(target) => target.on_response(cx, tag, result, &mut ops),
        None => {
            // The sheet closed meanwhile; pages (sessions) take their own responses.
            crate::shell::sheets::history::on_response(shell, cx, tag, result, &mut ops);
            SheetFlow::Stay
        }
    };
    apply_ops(shell, cx, ui, ops, Some(sheet));
    apply_sheet_flow(shell, cx, sheet, flow);
}

fn deliver(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, sheet: u64, tag: &str, value: &Value) {
    let mut ops = SheetOps::for_sheet(ui, sheet);
    let flow = match shell.data.overlays.sheet_mut(sheet) {
        Some(target) => target.on_deliver(cx, tag, value, &mut ops),
        None => SheetFlow::Stay,
    };
    apply_ops(shell, cx, ui, ops, Some(sheet));
    apply_sheet_flow(shell, cx, sheet, flow);
}

fn apply_sheet_flow(shell: &mut PaseoShell, cx: &mut Cx, sheet: u64, flow: SheetFlow) {
    let index = shell.data.overlays.stack.iter().position(|m| matches!(m, crate::shell::overlays::Modal::Sheet { id, .. } if *id == sheet));
    if let Some(index) = index {
        crate::shell::overlays::apply_flow(shell, cx, &mut Scope::empty(), index, flow);
    }
}

/// Original `confirmAction` above a sheet: answers the sheet that pushed it through
/// `on_deliver(tag, true|false)`.
pub struct ConfirmSheet {
    title: String,
    message: String,
    confirm_label: String,
    destructive: bool,
    reply_to: Option<u64>,
    tag: String,
    answered: bool,
}

impl ConfirmSheet {
    pub fn new(ops: &SheetOps, title: &str, message: &str, confirm_label: &str, destructive: bool, tag: &str) -> Self {
        Self {
            title: title.to_string(),
            message: message.to_string(),
            confirm_label: confirm_label.to_string(),
            destructive,
            reply_to: ops.origin,
            tag: tag.to_string(),
            answered: false,
        }
    }

    fn answer(&mut self, ops: &mut SheetOps, confirmed: bool) {
        if std::mem::replace(&mut self.answered, true) {
            return;
        }
        if let Some(sheet) = self.reply_to {
            ops.shell.push(ShellOp::Deliver { sheet, tag: self.tag.clone(), value: Value::Bool(confirmed) });
        }
    }
}

impl Sheet for ConfirmSheet {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn max_width(&self) -> f64 {
        420.0
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        use crate::common::paint::{Font, rect};
        let spec = ui.spec(Font::Regular, 14.0, ui.p.muted);
        let h = ui.painter.paragraph_height(cx, w, spec, &self.message);
        ui.painter.paragraph(cx, dvec2(x, y), w, spec, &self.message);
        let by = y + h + 24.0;
        let cancel = crate::common::tr("common.actions.cancel");
        let cw = ui.button_width(cx, &cancel, None, crate::theme::ControlSize::Sm);
        let ow = ui.button_width(cx, &self.confirm_label, None, crate::theme::ControlSize::Sm);
        let variant = if self.destructive { crate::shell::ui::Variant::Destructive } else { crate::shell::ui::Variant::Primary };
        let right = x + w;
        ui.button(cx, rect(right - ow, by, ow, 32.0), "confirm:ok", &self.confirm_label, None, variant, crate::theme::ControlSize::Sm, interactive);
        ui.button(cx, rect(right - ow - 8.0 - cw, by, cw, 32.0), "confirm:cancel", &cancel, None, crate::shell::ui::Variant::Ghost, crate::theme::ControlSize::Sm, interactive);
        h + 24.0 + 32.0
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        match id {
            "confirm:ok" => {
                self.answer(ops, true);
                SheetFlow::Close
            }
            "confirm:cancel" => {
                self.answer(ops, false);
                SheetFlow::Close
            }
            _ => SheetFlow::Stay,
        }
    }

    fn closed(&mut self, _cx: &mut Cx, ops: &mut SheetOps) {
        self.answer(ops, false);
    }
}

/// Lays out a vertical form: returns the next y after a row of height `h` plus `gap`.
pub fn advance(y: &mut f64, h: f64, gap: f64) -> f64 {
    let top = *y;
    *y += h + gap;
    top
}

/// Draws an embedded widget (text field) at `r`.
pub fn draw_widget(cx: &mut Cx2d, scope: &mut Scope, widget: &WidgetRef, r: Rect) {
    cx.begin_turtle(Walk::abs_rect(r), Layout::default());
    widget.draw_all(cx, scope);
    cx.end_turtle();
}

thread_local! {
    static SPINNING: Cell<bool> = const { Cell::new(false) };
}

/// A small indeterminate ring (`CircularProgressIndicator`); keeps the sheet animating.
pub fn spinner(ui: &mut Ui, cx: &mut Cx2d, r: Rect, color: Vec4) {
    let rotation = (cx.seconds_since_app_start() * std::f64::consts::TAU) % std::f64::consts::TAU;
    ui.painter.ring(cx, r, color, 1.5, 0.25, rotation);
    SPINNING.with(|s| s.set(true));
}

/// Overlays: whether a spinner was drawn since the last call.
pub fn take_spinning() -> bool {
    SPINNING.with(|s| s.replace(false))
}

/// Original menu-style sheet row (`_item`): 16px leading slot, label, optional trailing glyph;
/// 44 tall on compact layouts, else 32; surface2 while hovered; muted and inert without `id`.
#[allow(clippy::too_many_arguments)]
pub fn row(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, id: Option<&str>, label: &str, color: Option<Vec4>) -> (Rect, f64) {
    use crate::common::paint::{Font, rect};
    let h = if ui.compact { 44.0 } else { 32.0 };
    let r = rect(x, y, w, h);
    if let Some(id) = id {
        if ui.is_hover(id) || ui.is_pressed(id) {
            let c = ui.p.surface2;
            ui.fill(cx, r, c, 6.0);
        }
        ui.hit(r, id.to_string());
    }
    let fg = if id.is_none() { ui.p.muted } else { color.unwrap_or(ui.p.foreground) };
    let spec = ui.spec(Font::Regular, 14.0, fg);
    ui.text(cx, rect(x + 12.0 + 16.0 + 10.0, y, (w - 12.0 - 16.0 - 10.0 - 12.0 - 26.0).max(0.0), h), spec, label);
    (r, h)
}

/// The 16px leading slot of [`row`].
pub fn row_leading(r: Rect) -> Rect {
    crate::common::paint::rect(r.pos.x + 12.0, r.pos.y + (r.size.y - 16.0) * 0.5, 16.0, 16.0)
}

/// The trailing 16px slot of [`row`].
pub fn row_trailing(r: Rect) -> Rect {
    crate::common::paint::rect(r.pos.x + r.size.x - 12.0 - 16.0, r.pos.y + (r.size.y - 16.0) * 0.5, 16.0, 16.0)
}

/// Small hint text (`_hint`): 12/6 padding, muted or destructive; returns its height.
pub fn hint(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, text: &str, error: bool) -> f64 {
    use crate::common::paint::Font;
    let color = if error { ui.p.destructive } else { ui.p.muted };
    let spec = ui.spec(Font::Regular, 12.0, color);
    let h = ui.painter.paragraph_height(cx, w - 24.0, spec, text);
    ui.painter.paragraph(cx, dvec2(x + 12.0, y + 6.0), w - 24.0, spec, text);
    h + 12.0
}

/// 1px divider with 4px above and below (9px).
pub fn separator(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64) -> f64 {
    let color = ui.p.border;
    ui.painter.hline(cx, x, y + 4.0, w, color);
    9.0
}
