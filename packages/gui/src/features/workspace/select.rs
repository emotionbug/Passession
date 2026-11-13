//! `WaSelect`: the picker behind Flutter `showPaseoSelect` and `showPaseoMenu` (combobox
//! sheet with optional search, option descriptions, disabled/pending rows, separators,
//! destructive items and the selected check).
//!
//! The owner opens it with a [`SelectRequest`] and a token naming the purpose; the chosen
//! value comes back as [`WaSelectRef::picked`]. Dismissal (Escape, background tap, close)
//! reports [`WaSelectRef::dismissed`].

use makepad_widgets::*;
use super::kit::{WaModalWidgetExt};

use crate::common::widgets::{PLabelWidgetRefExt, SurfaceRole, SurfaceWidgetRefExt};

use super::kit::{WaButtonWidgetExt, WaIconWidgetRefExt, WaInputWidgetExt};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;

    WaSelectRow = <Surface> {
        width: Fill, height: Fit, flow: Right, spacing: 8.
        padding: { left: 12., right: 12., top: 8., bottom: 8. }
        align: { y: 0.5 }
        interactive: true, hover_role: Surface1, radius: 6.
        lead = <View> { width: Fit, height: Fit, icon = <WaIcon> {} }
        dot = <WaStatusDot> { visible: false }
        icon = <WaProjectIcon> { visible: false }
        body = <View> {
            width: Fill, height: Fit, flow: Down, spacing: 2.
            label = <WaLine> {}
            description = <View> { width: Fill, height: Fit, text = <WaSmallLine> {} }
        }
        trailing = <View> { width: Fit, height: Fit, text = <WaSmallFit> {} }
        check = <View> { width: Fit, height: Fit, icon = <WaIcon> { icon: "check", role: Body } }
    }

    WaSelectSeparator = <View> {
        width: Fill, height: 9., padding: { top: 4., bottom: 4. }
        rule = <WaRule> {}
    }

    WaSelectEmpty = <View> {
        width: Fill, height: Fit, padding: { left: 16., right: 16., top: 24., bottom: 24. }
        align: { x: 0.5 }
        text = <WaMuted> { width: Fit }
    }

    pub WaSelect = {{WaSelect}} {
        width: Fill, height: Fill
        modal = <WaModal> {
            scroll_body: list
            content: {
                sheet = <WaSheet> {
                    width: 420.
                    search_wrap = <View> {
                        width: Fill, height: Fit
                        padding: { left: 16., right: 16., top: 12., bottom: 8. }
                        search = <WaInput> {}
                    }
                    list = <PortalList> {
                        width: Fill, height: 320.
                        Row = <WaSelectRow> {}
                        Separator = <WaSelectSeparator> {}
                        Empty = <WaSelectEmpty> {}
                    }
                    footer = <View> { width: Fill, height: 12. }
                }
            }
        }
    }
}

/// Leading decoration of an option.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Leading {
    #[default]
    None,
    /// A Lucide icon name (`folder`, `git-branch`, `plus`...).
    Glyph(String),
    /// A host status dot.
    Status(super::host::Connection),
    /// A project identity square: (name, view key).
    Project(String, String),
}

#[derive(Clone, Debug, Default)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub description: String,
    pub trailing: String,
    pub leading: Leading,
    pub enabled: bool,
    pub separator_before: bool,
    /// A trailing check independent of the single selection (multi-attach pickers).
    pub checked: bool,
    pub destructive: bool,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into(), enabled: true, ..Default::default() }
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = text.into();
        self
    }

    pub fn trailing(mut self, text: impl Into<String>) -> Self {
        self.trailing = text.into();
        self
    }

    pub fn leading(mut self, leading: Leading) -> Self {
        self.leading = leading;
        self
    }

    pub fn glyph(self, glyph: &str) -> Self {
        self.leading(Leading::Glyph(glyph.to_owned()))
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn separated(mut self, separator: bool) -> Self {
        self.separator_before = separator;
        self
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct SelectRequest {
    pub title: String,
    pub options: Vec<SelectOption>,
    pub selected: Option<String>,
    pub searchable: bool,
    pub search_placeholder: String,
    pub empty_text: String,
}

impl SelectRequest {
    pub fn new(title: impl Into<String>, options: Vec<SelectOption>) -> Self {
        Self { title: title.into(), options, ..Default::default() }
    }

    pub fn selected(mut self, value: Option<&str>) -> Self {
        self.selected = value.map(str::to_owned);
        self
    }

    pub fn searchable(mut self, placeholder: impl Into<String>) -> Self {
        self.searchable = true;
        self.search_placeholder = placeholder.into();
        self
    }

    pub fn empty(mut self, text: impl Into<String>) -> Self {
        self.empty_text = text.into();
        self
    }
}

#[derive(Clone, Debug, DefaultNone)]
pub enum WaSelectAction {
    None,
    Picked { token: String, value: String },
    Dismissed { token: String },
}

enum Row {
    Separator,
    Option(usize),
    Empty,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaSelect {
    #[deref]
    view: View,
    #[rust]
    token: String,
    #[rust]
    request: SelectRequest,
    #[rust]
    query: String,
    #[rust]
    rows: Vec<Row>,
    #[rust]
    highlighted: Option<usize>,
    #[rust]
    open: bool,
    #[rust]
    pending_focus: bool,
}

impl WaSelect {
    fn apply_request(&mut self, cx: &mut Cx, request: SelectRequest) {
        self.view.wa_input(id!(search)).set_empty_text(cx, request.search_placeholder.clone());
        self.view.widget(id!(search_wrap)).set_visible(cx, request.searchable);
        let rows = request.options.len() + request.options.iter().filter(|o| o.separator_before).count();
        let height = (rows.max(1) as f64 * 40.0).min(360.0);
        self.view.portal_list(id!(list)).apply_over(cx, live! { height: (height) });
        self.request = request;
        self.rebuild();
        self.view.redraw(cx);
    }

    fn rebuild(&mut self) {
        let needle = self.query.trim().to_lowercase();
        self.rows.clear();
        let mut shown = 0usize;
        for (index, option) in self.request.options.iter().enumerate() {
            if !needle.is_empty()
                && !option.label.to_lowercase().contains(&needle)
                && !option.description.to_lowercase().contains(&needle)
            {
                continue;
            }
            if option.separator_before && shown > 0 && needle.is_empty() {
                self.rows.push(Row::Separator);
            }
            self.rows.push(Row::Option(index));
            shown += 1;
        }
        if shown == 0 {
            self.rows.push(Row::Empty);
        }
        self.highlighted = self.rows.iter().position(|row| match row {
            Row::Option(index) => {
                let option = &self.request.options[*index];
                option.enabled && self.request.selected.as_deref() == Some(option.value.as_str())
            }
            _ => false,
        });
        if self.highlighted.is_none() && !needle.is_empty() {
            self.highlighted = self.rows.iter().position(|row| matches!(row, Row::Option(i) if self.request.options[*i].enabled));
        }
    }

    fn move_highlight(&mut self, delta: isize) {
        let enabled: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row, entry)| match entry {
                Row::Option(i) if self.request.options[*i].enabled => Some(row),
                _ => None,
            })
            .collect();
        if enabled.is_empty() {
            return;
        }
        let current = self.highlighted.and_then(|row| enabled.iter().position(|r| *r == row));
        let next = match current {
            None if delta < 0 => enabled.len() - 1,
            None => 0,
            Some(position) => ((position as isize + delta).rem_euclid(enabled.len() as isize)) as usize,
        };
        self.highlighted = Some(enabled[next]);
    }

    fn pick(&mut self, cx: &mut Cx, scope: &mut Scope, row: usize) {
        let Some(Row::Option(index)) = self.rows.get(row) else { return };
        let option = &self.request.options[*index];
        if !option.enabled {
            return;
        }
        let value = option.value.clone();
        let token = std::mem::take(&mut self.token);
        self.open = false;
        self.view.wa_modal(id!(modal)).close(cx);
        cx.widget_action(self.widget_uid(), &scope.path, WaSelectAction::Picked { token, value });
    }

    fn dismiss(&mut self, cx: &mut Cx, scope: &mut Scope) {
        if !self.open {
            return;
        }
        self.open = false;
        self.view.wa_modal(id!(modal)).close(cx);
        let token = std::mem::take(&mut self.token);
        cx.widget_action(self.widget_uid(), &scope.path, WaSelectAction::Dismissed { token });
    }
}

impl Widget for WaSelect {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if !self.open && !matches!(event, Event::KeyFocus(_) | Event::KeyFocusLost(_) | Event::Actions(_)) {
            return;
        }
        self.view.handle_event(cx, event, scope);
        if !self.open {
            return;
        }
        let modal = self.view.wa_modal(id!(modal));
        if !modal.is_open() {
            // Escape or a background tap closed the modal.
            self.dismiss(cx, scope);
            return;
        }
        if !modal.accepts_input() {
            return;
        }
        let Event::Actions(actions) = event else { return };
        let search = self.view.wa_input(id!(search));
        if let Some(query) = search.changed(actions) {
            self.query = query;
            self.rebuild();
            self.view.redraw(cx);
        }
        if let Some(key) = search.key_down_unhandled(actions) {
            match key.key_code {
                KeyCode::ArrowDown => {
                    self.move_highlight(1);
                    self.view.redraw(cx);
                }
                KeyCode::ArrowUp => {
                    self.move_highlight(-1);
                    self.view.redraw(cx);
                }
                _ => {}
            }
        }
        if search.returned(actions).is_some() && let Some(row) = self.highlighted {
            self.pick(cx, scope, row);
            return;
        }
        // Escape is owned by WaModal on key-down, before the search field.
        if self.view.wa_button(id!(close)).clicked(actions) {
            self.dismiss(cx, scope);
            return;
        }
        let list = self.view.portal_list(id!(list));
        let mut seen = Vec::new();
        for (row, item) in list.items_with_actions(actions) {
            if seen.contains(&row) {
                continue;
            }
            seen.push(row);
            if item.as_surface().clicked(actions) {
                self.pick(cx, scope, row);
                return;
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.widget(id!(sheet.header.title)).set_text(cx, &self.request.title);
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            let list_ref = item.as_portal_list();
            let Some(mut list) = list_ref.borrow_mut() else { continue };
            list.set_item_range(cx, 0, self.rows.len());
            while let Some(row) = list.next_visible_item(cx) {
                let Some(entry) = self.rows.get(row) else { continue };
                match entry {
                    Row::Separator => {
                        let widget = list.item(cx, row, live_id!(Separator));
                        widget.draw_all(cx, scope);
                    }
                    Row::Empty => {
                        let widget = list.item(cx, row, live_id!(Empty));
                        let text = if self.query.trim().is_empty() {
                            self.request.empty_text.clone()
                        } else {
                            super::i18n::t("shell.commandCenter.noMatches")
                        };
                        widget.widget(id!(text)).set_text(cx, &text);
                        widget.draw_all(cx, scope);
                    }
                    Row::Option(index) => {
                        let option = &self.request.options[*index];
                        let widget = list.item(cx, row, live_id!(Row));
                        let highlighted = self.highlighted == Some(row);
                        widget.as_surface().set_role(cx, if highlighted { SurfaceRole::Surface1 } else { SurfaceRole::Transparent });
                        let label = widget.plabel(id!(body.label));
                        label.set_text(cx, &option.label);
                        let p = crate::theme::current_palette();
                        let color = if !option.enabled {
                            p.extra_muted
                        } else if option.destructive {
                            p.danger
                        } else {
                            p.foreground
                        };
                        label.set_color(cx, Some(color));
                        super::kit::set_optional(cx, &widget.widget(id!(body.description)), &option.description);
                        super::kit::set_optional(cx, &widget.widget(id!(trailing)), &option.trailing);
                        let lead = widget.widget(id!(lead));
                        let dot = widget.widget(id!(dot));
                        let icon = widget.widget(id!(icon));
                        lead.set_visible(cx, false);
                        dot.set_visible(cx, false);
                        icon.set_visible(cx, false);
                        match &option.leading {
                            Leading::None => {}
                            Leading::Glyph(glyph) => {
                                lead.set_visible(cx, true);
                                lead.wa_icon(id!(icon)).set_icon(cx, glyph);
                            }
                            Leading::Status(status) => {
                                dot.set_visible(cx, true);
                                super::kit::set_status_dot(cx, &dot, *status);
                            }
                            Leading::Project(name, key) => {
                                icon.set_visible(cx, true);
                                super::kit::set_project_icon(cx, &icon, name, key);
                            }
                        }
                        let selected = option.checked || self.request.selected.as_deref() == Some(option.value.as_str());
                        widget.widget(id!(check)).set_visible(cx, selected);
                        widget.draw_all(cx, scope);
                    }
                }
            }
        }
        if self.pending_focus {
            self.pending_focus = false;
            if self.open
                && self.request.searchable
                && self.view.wa_modal(id!(modal)).accepts_input()
                && scope.data.get::<crate::common::UiContext>().is_some_and(|ui| !ui.compact)
            {
                self.view.wa_input(id!(search)).set_key_focus(cx);
            }
        }
        DrawStep::done()
    }
}

impl WaSelectRef {
    /// Opens the picker for `request`; `token` comes back with the answer.
    pub fn open(&self, cx: &mut Cx, token: &str, request: SelectRequest) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.token = token.to_owned();
        inner.query.clear();
        let search = inner.view.wa_input(id!(search));
        search.set_text(cx, "");
        inner.apply_request(cx, request);
        inner.open = true;
        inner.pending_focus = true;
        inner.view.wa_modal(id!(modal)).open(cx);
        inner.view.redraw(cx);
    }

    /// Refresh asynchronous options without reopening a dismissed picker, clearing
    /// its search, stealing focus, or updating a different picker's token.
    pub fn refresh(&self, cx: &mut Cx, token: &str, request: impl FnOnce() -> SelectRequest) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.open && inner.token == token {
            inner.apply_request(cx, request());
        }
    }

    pub fn is_open(&self) -> bool {
        self.borrow().is_some_and(|inner| inner.open)
    }

    pub fn close(&self, cx: &mut Cx) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.open {
            inner.open = false;
            inner.token.clear();
            inner.view.wa_modal(id!(modal)).close(cx);
        }
    }

    /// `(token, value)` of an option the user picked in `actions`.
    pub fn picked(&self, actions: &Actions) -> Option<(String, String)> {
        for action in actions.filter_widget_actions(self.widget_uid()) {
            if let WaSelectAction::Picked { token, value } = action.cast() {
                return Some((token, value));
            }
        }
        None
    }

    /// The token of a picker the user closed without choosing.
    pub fn dismissed(&self, actions: &Actions) -> Option<String> {
        for action in actions.filter_widget_actions(self.widget_uid()) {
            if let WaSelectAction::Dismissed { token } = action.cast() {
                return Some(token);
            }
        }
        None
    }
}
