//! `WaDialog`: Flutter `confirmAction` (title, message, confirm label, destructive styling)
//! and `promptText` (title, initial value, optional multi-line), answered through
//! [`WaDialogRef::confirmed`] / [`WaDialogRef::submitted`] with the caller's token.

use makepad_widgets::*;
use super::kit::{WaModalWidgetExt};

use super::kit::{WaButtonWidgetExt, WaInputWidgetExt};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::features::workspace::kit::*;

    pub WaDialog = {{WaDialog}} {
        width: Fill, height: Fill
        modal = <WaModal> {
            content: {
                sheet = <WaSheet> {
                    width: 440.
                    body = <View> {
                        width: Fill, height: Fit, flow: Down, spacing: 16.
                        padding: { left: 24., right: 24., top: 24., bottom: 20. }
                        message = <View> { width: Fill, height: Fit, text = <WaMuted> {} }
                        input_wrap = <View> {
                            width: Fill, height: Fit
                            input = <WaInput> {}
                        }
                        buttons = <View> {
                            width: Fill, height: Fit, flow: Right, spacing: 8.
                            align: { x: 1.0 }
                            cancel = <WaButtonGhost> {}
                            confirm = <WaButtonPrimary> {}
                            destructive = <WaButtonDanger> { visible: false }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, DefaultNone)]
pub enum WaDialogAction {
    None,
    Confirmed { token: String },
    Submitted { token: String, value: String },
    Cancelled { token: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Confirm,
    Prompt,
    PromptMultiline,
}

/// What one dialog shows.
struct Content<'a> {
    title: &'a str,
    message: &'a str,
    confirm: &'a str,
    destructive: bool,
    value: &'a str,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaDialog {
    #[deref]
    view: View,
    #[rust]
    token: String,
    #[rust]
    kind: Kind,
    #[rust]
    open: bool,
    #[rust]
    focus_input: bool,
}

impl WaDialog {
    fn finish(&mut self, cx: &mut Cx, scope: &mut Scope, action: impl FnOnce(String, String) -> WaDialogAction) {
        if !self.open {
            return;
        }
        self.open = false;
        let value = self.view.wa_input(id!(input)).text();
        let token = std::mem::take(&mut self.token);
        self.view.wa_modal(id!(modal)).close(cx);
        cx.widget_action(self.widget_uid(), &scope.path, action(token, value));
    }
}

impl Widget for WaDialog {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if !self.open {
            return;
        }
        if !self.view.wa_modal(id!(modal)).is_open() {
            self.finish(cx, scope, |token, _| WaDialogAction::Cancelled { token });
            return;
        }
        let Event::Actions(actions) = event else { return };
        let input = self.view.wa_input(id!(input));
        if self.kind == Kind::PromptMultiline && input.returned(actions).is_some() {
            super::kit::insert_newline(cx, &input);
        }
        let submit = |this: &mut Self, cx: &mut Cx, scope: &mut Scope| match this.kind {
            Kind::Confirm => this.finish(cx, scope, |token, _| WaDialogAction::Confirmed { token }),
            _ => this.finish(cx, scope, |token, value| WaDialogAction::Submitted { token, value }),
        };
        if self.view.wa_button(id!(confirm)).clicked(actions) || self.view.wa_button(id!(destructive)).clicked(actions) {
            submit(self, cx, scope);
            return;
        }
        if self.kind == Kind::Prompt && input.returned(actions).is_some() {
            submit(self, cx, scope);
            return;
        }
        if self.view.wa_button(id!(cancel)).clicked(actions)
            || self.view.wa_button(id!(close)).clicked(actions)
            || input.escaped(actions)
        {
            self.finish(cx, scope, |token, _| WaDialogAction::Cancelled { token });
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let step = self.view.draw_walk(cx, scope, walk);
        if self.focus_input {
            self.focus_input = false;
            self.view.wa_input(id!(input)).set_key_focus(cx);
        }
        step
    }
}

impl WaDialogRef {
    fn show(&self, cx: &mut Cx, token: &str, kind: Kind, content: Content<'_>) {
        let Content { title, message, confirm, destructive, value } = content;
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.token = token.to_owned();
        inner.kind = kind;
        inner.open = true;
        inner.focus_input = kind != Kind::Confirm;
        let view = &inner.view;
        view.widget(id!(sheet.header.title)).set_text(cx, title);
        super::kit::set_optional(cx, &view.widget(id!(message)), message);
        view.widget(id!(input_wrap)).set_visible(cx, kind != Kind::Confirm);
        view.wa_input(id!(input)).set_text(cx, value);
        view.wa_button(id!(cancel)).set_text(cx, super::i18n::l("Cancel"));
        view.wa_button(id!(confirm)).set_visible(cx, !destructive);
        view.wa_button(id!(destructive)).set_visible(cx, destructive);
        view.wa_button(id!(confirm)).set_text(cx, confirm);
        view.wa_button(id!(destructive)).set_text(cx, confirm);
        view.wa_modal(id!(modal)).open(cx);
        inner.view.redraw(cx);
    }

    /// `confirmAction`.
    pub fn confirm(&self, cx: &mut Cx, token: &str, title: &str, message: &str, confirm: &str, destructive: bool) {
        self.show(cx, token, Kind::Confirm, Content { title, message, confirm, destructive, value: "" });
    }

    /// `promptText`.
    pub fn prompt(&self, cx: &mut Cx, token: &str, title: &str, initial: &str, multiline: bool) {
        let kind = if multiline { Kind::PromptMultiline } else { Kind::Prompt };
        self.show(cx, token, kind, Content { title, message: "", confirm: super::i18n::l("OK"), destructive: false, value: initial });
    }

    pub fn is_open(&self) -> bool {
        self.borrow().is_some_and(|inner| inner.open)
    }

    pub fn confirmed(&self, actions: &Actions) -> Option<String> {
        actions.filter_widget_actions(self.widget_uid()).find_map(|action| match action.cast() {
            WaDialogAction::Confirmed { token } => Some(token),
            _ => None,
        })
    }

    pub fn submitted(&self, actions: &Actions) -> Option<(String, String)> {
        actions.filter_widget_actions(self.widget_uid()).find_map(|action| match action.cast() {
            WaDialogAction::Submitted { token, value } => Some((token, value)),
            _ => None,
        })
    }

    pub fn cancelled(&self, actions: &Actions) -> Option<String> {
        actions.filter_widget_actions(self.widget_uid()).find_map(|action| match action.cast() {
            WaDialogAction::Cancelled { token } => Some(token),
            _ => None,
        })
    }
}
