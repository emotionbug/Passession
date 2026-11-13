//! Project settings (Flutter `project_settings_panel.dart`, original
//! `project-settings-screen.tsx`): `paseo.json` worktree lifecycle hooks, service port
//! allocation, scripts, metadata generation instructions, revision-checked saves, the
//! project name/icon editor and workspace recovery.

use std::collections::HashMap;

use makepad_widgets::*;
use super::kit::{WaModalWidgetExt, WaModalWidgetRefExt};
use serde_json::{json, Value};

use super::host::{self, Connection};
use super::i18n::{l, lf, t, ta};
use super::identity::project_view_key;
use super::kit::{
    clicked_in, insert_newline, set_alert, set_optional, set_project_icon, sync_input, WaButtonWidgetRefExt, WaInputWidgetRefExt,
    WaRepeatWidgetRefExt,
};
use crate::common::widgets::PSwitchWidgetRefExt;
use super::project_model as model;
use super::rpc::{object_of, require_success, str_of, Pending};
use super::select::{SelectOption, SelectRequest, WaSelectWidgetRefExt};
use super::dialog::{WaDialogWidgetExt, WaDialogWidgetRefExt};
use crate::common::{UiContext, UiEffect};
use paseo_client::RequestOptions;
use paseo_gui_platform::{platform, PickFilesOptions, PlatformAction, PlatformValue};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;
    use crate::features::workspace::select::*;
    use crate::features::workspace::dialog::*;

    // Top-aligned with explicit insets: a centred row shifts already-drawn glyphs past their
    // clip rect, cutting the label's descenders.
    WaGroupHeader = <View> {
        width: Fill, height: Fit, flow: Right, spacing: 8.
        padding: { bottom: 16. }
        title = <WaTextFit> { margin: { top: 1. } }
        info = <WaGlyphButton> { icon: "info" }
        <View> { width: Fill, height: 1. }
    }
    WaInfo = <View> { width: Fill, height: Fit, visible: false, padding: { bottom: 12. }, text = <WaSmall> {} }
    WaSectionHeader = <View> {
        width: Fill, height: Fit, flow: Right, spacing: 8.
        padding: { bottom: 8. }
        title = <WaMuted> { width: Fit, margin: { top: 2. } }
        <View> { width: Fill, height: 1. }
        docs = <WaLinkButton> { trailing: "arrow-up-right" }
    }

    WaProjectScriptRow = <View> {
        width: Fill, height: Fit, flow: Down
        rule = <WaRule> {}
        body = <View> {
            width: Fill, height: Fit, flow: Right, spacing: 8., padding: 16., align: { y: 0.5 }
            main = <View> {
                width: Fill, height: Fit, flow: Down, spacing: 4.
                cursor: Hand
                name = <WaLine> {}
                hint = <View> { width: Fill, height: Fit, text = <WaSmallLine> {} }
            }
            menu = <WaGlyphButton> { icon: "ellipsis-vertical" }
        }
    }

    WaActionRow = <WaRow> {
        controls = { action = <WaButtonOutlineSmall> {} }
    }

    pub WaProjectSettingsView = {{WaProjectSettingsView}} {
        width: Fill, height: Fill, flow: Overlay
        wa_root = <Surface> {
            width: Fill, height: Fill, flow: Overlay
            role: Surface0
            scroll = <ScrollYView> {
                width: Fill, height: Fill, flow: Down
                align: { x: 0.5 }
                form = <View> {
                    width: 720., height: Fit, flow: Down
                    padding: 16.
                    header = <View> {
                        width: Fill, height: Fit, flow: Right, spacing: 12., align: { y: 0.5 }
                        padding: { top: 8., bottom: 16. }
                        icon = <WaProjectIcon> { width: 28., height: 28. }
                        // Long names ellipsize so the trailing edit button stays on screen.
                        name = <WaMedium> { width: Fill, ellipsis: true }
                        edit_project = <WaGlyphButton> { icon: "pencil" }
                    }
                    host_gone = <View> { width: Fill, height: Fit, padding: 16., visible: false, text = <WaMuted> {} }
                    loading = <View> { width: Fill, height: Fit, padding: 24., align: { x: 0.5 }, visible: false, <WaSpinner> {} }
                    read_failure = <WaAlert> {
                        visible: false, margin: { top: 8. }, large: true
                        padding: { left: 24., right: 24., top: 16., bottom: 16. }
                        body = { actions = { read_reload = <WaButtonOutlineSmall> {} } }
                    }
                    body = <View> {
                        width: Fill, height: Fit, flow: Down, visible: false
                        worktree_group = <View> {
                            width: Fill, height: Fit, flow: Down, padding: { bottom: 32. }
                            header = <WaGroupHeader> {}
                            info = <WaInfo> {}
                            setup_section = <View> {
                                width: Fill, height: Fit, flow: Down, spacing: 8., padding: { bottom: 24. }
                                header = <WaSectionHeader> {}
                                uncommitted = <WaWarningAlert> { visible: false }
                                setup_input = <WaTextArea> { input = { empty_text: "npm install" } }
                            }
                            teardown_section = <View> {
                                width: Fill, height: Fit, flow: Down, spacing: 8., padding: { bottom: 24. }
                                header = <WaSectionHeader> {}
                                teardown_input = <WaTextArea> { input = { empty_text: "docker compose down" } }
                            }
                            ports_section = <View> {
                                width: Fill, height: Fit, flow: Down, spacing: 8.
                                title = <WaMutedLine> {}
                                card = <WaCard> { ports_row = <WaActionRow> {} }
                            }
                        }
                        scripts_group = <View> {
                            width: Fill, height: Fit, flow: Down, padding: { bottom: 32. }
                            header = <WaGroupHeader> { add_script = <WaGlyphButton> { icon: "plus" } }
                            info = <WaInfo> {}
                            card = <WaCard> {
                                scripts_empty = <View> { width: Fill, height: Fit, padding: 16., text = <WaMuted> {} }
                                script_rows = <WaRepeat> { template: <WaProjectScriptRow> {} }
                            }
                        }
                        metadata_group = <View> {
                            width: Fill, height: Fit, flow: Down, padding: { bottom: 32. }
                            header = <WaGroupHeader> {}
                            info = <WaInfo> {}
                            branch_section = <View> {
                                width: Fill, height: Fit, flow: Down, spacing: 8., padding: { bottom: 24. }
                                title = <WaMutedLine> {}
                                meta_branch = <WaTextArea> {}
                            }
                            commit_section = <View> {
                                width: Fill, height: Fit, flow: Down, spacing: 8., padding: { bottom: 24. }
                                title = <WaMutedLine> {}
                                meta_commit = <WaTextArea> {}
                            }
                            pr_section = <View> {
                                width: Fill, height: Fit, flow: Down, spacing: 8.
                                title = <WaMutedLine> {}
                                meta_pr = <WaTextArea> {}
                            }
                        }
                        stale = <WaAlert> { visible: false, margin: { top: 12. }, body = { actions = { stale_reload = <WaButtonOutlineSmall> {} } } }
                        write_failed = <WaAlert> {
                            visible: false, margin: { top: 12. }
                            body = { actions = { write_retry = <WaButtonOutlineSmall> {}, write_reload = <WaButtonOutlineSmall> {} } }
                        }
                        save_error = <WaAlert> { visible: false, margin: { top: 12. } }
                        save_row = <View> {
                            width: Fill, height: Fit, align: { x: 1.0 }, padding: { top: 16. }
                            save = <WaButtonPrimary> { size: Md }
                        }
                    }
                    recovery = <View> {
                        width: Fill, height: Fit, flow: Down, spacing: 8., padding: { top: 32. }, visible: false
                        title = <WaSectionTitle> {}
                        card = <WaCard> {
                            inspect_row = <WaActionRow> {}
                            restore_row = <View> {
                                width: Fill, height: Fit, flow: Down, visible: false
                                <WaRule> {}
                                row = <WaActionRow> {}
                            }
                        }
                        recovery_error = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                    }
                }
            }

            script_sheet = <WaModal> {
                content: {
                    sheet = <WaSheet> {
                        width: 560.
                        fields = <View> {
                            width: Fill, height: Fit, flow: Down, spacing: 16.
                            padding: { left: 24., right: 24., top: 24., bottom: 20. }
                            name_field = <WaField> {
                                script_name = <WaInput> { field_size: Sm, input = { empty_text: "dev" } }
                                error = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                            }
                            command_field = <WaField> {
                                script_command = <WaInput> { field_size: Sm, input = { empty_text: "npm run dev" } }
                                error = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                            }
                            port_field = <WaField> { script_port = <WaInput> { field_size: Sm, input = { empty_text: "3000 or $PORT" } } }
                            service_row = <View> {
                                width: Fill, height: Fit, flow: Right, spacing: 12., align: { y: 0.5 }
                                texts = <View> {
                                    width: Fill, height: Fit, flow: Down, spacing: 4.
                                    label = <WaMedium> {}
                                    hint = <WaSmall> {}
                                }
                                service = <WaToggle> {}
                            }
                            buttons = <View> {
                                width: Fill, height: Fit, flow: Right, spacing: 8., align: { x: 1.0 }, padding: { top: 8. }
                                script_cancel = <WaButtonGhost> { size: Md }
                                script_save = <WaButtonPrimary> { size: Md }
                            }
                        }
                    }
                }
            }

            project_sheet = <WaModal> {
                content: {
                    sheet = <WaSheet> {
                        width: 440.
                        fields = <View> {
                            width: Fill, height: Fit, flow: Down, spacing: 16.
                            padding: { left: 24., right: 24., top: 24., bottom: 20. }
                            name_field = <WaField> {
                                project_name = <WaInput> { field_size: Adaptive }
                                error = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                            }
                            icon_field = <WaField> {
                                icon_row = <View> {
                                    width: Fill, height: Fit, flow: Right, spacing: 8., align: { y: 0.5 }
                                    preview_icon = <WaProjectIcon> { width: 40., height: 40. }
                                    preview_image = <Image> { width: 40., height: 40., fit: Smallest, visible: false }
                                    choose_image = <WaButtonOutlineSmall> {}
                                    use_automatic = <WaButtonGhost> { size: Xs }
                                }
                                picked = <View> { width: Fill, height: Fit, visible: false, text = <WaSmall> {} }
                                icon_url = <WaInput> { field_size: Adaptive }
                                error = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                            }
                            buttons = <View> {
                                width: Fill, height: Fit, flow: Right, spacing: 8., align: { x: 1.0 }
                                project_cancel = <WaButton> { size: Md, width: 112. }
                                project_save = <WaButtonPrimary> { size: Md, width: 112. }
                            }
                        }
                    }
                }
            }

            select = <WaSelect> {}
            dialog = <WaDialog> {}
        }
    }
}

#[derive(Clone, Debug)]
enum Req {
    Read,
    Write,
    Inspect,
    Restore,
    EditProject,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum IconChoice {
    #[default]
    Unchanged,
    Automatic,
    Upload,
    Url,
}

#[derive(Default)]
struct ProjectSheet {
    open: bool,
    project: Value,
    display_name: String,
    view_key: String,
    picked: Option<(std::path::PathBuf, String)>,
    choice: IconChoice,
    saving: bool,
    name_error: Option<String>,
    icon_error: Option<String>,
    pick_request: Option<u64>,
}

#[derive(Default)]
struct ScriptSheet {
    open: bool,
    old_name: Option<String>,
    name_touched: bool,
    command_touched: bool,
    service: bool,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaProjectSettingsView {
    #[deref]
    view: View,
    #[rust]
    host_id: String,
    #[rust]
    root: String,
    #[rust]
    workspace_id: String,
    #[rust]
    tab_key: String,
    #[rust]
    started: bool,
    #[rust]
    config: Option<Value>,
    /// The config as last read or saved (lifecycle hook shapes).
    #[rust]
    loaded: Value,
    #[rust]
    revision: Value,
    #[rust]
    read_error: Option<String>,
    #[rust]
    read_error_code: Option<String>,
    #[rust]
    write_error_code: Option<String>,
    #[rust]
    error: Option<String>,
    #[rust]
    recovery: Option<Value>,
    #[rust]
    recovery_error: Option<String>,
    #[rust]
    loading: bool,
    #[rust]
    saving: bool,
    #[rust]
    busy: bool,
    #[rust]
    dirty: bool,
    #[rust]
    guard_dirty: Option<bool>,
    #[rust]
    uncommitted: bool,
    #[rust]
    kinds: HashMap<String, &'static str>,
    #[rust]
    form_epoch: u64,
    #[rust]
    applied_epoch: u64,
    #[rust]
    info_open: [bool; 3],
    #[rust]
    pending: Pending<Req>,
    #[rust]
    script_sheet: ScriptSheet,
    #[rust]
    project_sheet: ProjectSheet,
    #[rust]
    port_range_draft: String,
    #[rust]
    last_status: Option<Connection>,
    #[rust]
    form_width: f64,
}

impl WaProjectSettingsView {
    /// The content root as a handle (the deref `View` is not `Clone`).
    fn root(&self) -> WidgetRef {
        self.view.widget(id!(wa_root))
    }

    pub fn set_project(&mut self, cx: &mut Cx, host_id: &str, root: &str, workspace_id: &str, tab_key: &str) {
        if self.host_id == host_id && self.root == root && self.workspace_id == workspace_id {
            self.tab_key = tab_key.to_owned();
            return;
        }
        self.host_id = host_id.to_owned();
        self.root = root.to_owned();
        self.workspace_id = workspace_id.to_owned();
        self.tab_key = tab_key.to_owned();
        self.started = false;
        self.config = None;
        self.dirty = false;
        self.view.redraw(cx);
    }

    fn read(&mut self, ctx: &UiContext) {
        self.started = true;
        self.loading = true;
        self.read_error = None;
        self.read_error_code = None;
        self.write_error_code = None;
        self.pending.send(&ctx.core, &self.host_id, "read_project_config_request", json!({ "repoRoot": self.root }), Req::Read);
    }

    fn save(&mut self, ctx: &UiContext) {
        if self.write_error_code.as_deref() == Some("stale_project_config") || self.saving {
            return;
        }
        let Some(config) = self.config.clone() else { return };
        self.saving = true;
        self.error = None;
        self.pending.send(
            &ctx.core,
            &self.host_id,
            "write_project_config_request",
            json!({ "repoRoot": self.root, "config": config, "expectedRevision": self.revision }),
            Req::Write,
        );
    }

    fn change(&mut self, change: impl FnOnce(&mut Value, &Value, &mut HashMap<String, &'static str>)) {
        let Some(config) = self.config.as_mut() else { return };
        change(config, &self.loaded, &mut self.kinds);
        self.dirty = true;
    }

    fn host_gone(&self, ctx: &UiContext) -> bool {
        matches!(
            host::host(&ctx.state, &self.host_id).map(|h| h.status()),
            Some(Connection::Disconnected) | Some(Connection::Error) | None
        )
    }

    fn project<'a>(&self, ctx: &'a UiContext) -> Option<&'a Value> {
        host::host(&ctx.state, &self.host_id).and_then(|h| h.project_by_root(&self.root))
    }

    fn display_name(&self, ctx: &UiContext) -> String {
        let fallback = host::last_segment(&self.root);
        self.project(ctx).map(|p| super::rpc::str_or(p, "projectDisplayName", &fallback).to_owned()).unwrap_or(fallback)
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        if self.host_id.is_empty() {
            return;
        }
        host::learn(&ctx.events);
        let status = host::host(&ctx.state, &self.host_id).map(|h| h.status());
        if !self.started {
            self.last_status = status;
            self.read(ctx);
            self.view.redraw(cx);
        } else if status != self.last_status {
            self.last_status = status;
            // A config that never loaded is read again once the host is back.
            if status == Some(Connection::Connected) && self.config.is_none() && !self.loading {
                self.read(ctx);
            }
            self.view.redraw(cx);
        }
        for (req, result) in self.pending.take_raw(&ctx.events) {
            self.view.redraw(cx);
            match req {
                Req::Read => {
                    self.loading = false;
                    match result {
                        Ok(body) => {
                            let code = str_of(object_of(&body, "error"), "code").to_owned();
                            let code = if code.is_empty() { body.get("error").and_then(Value::as_str).map(|_| "read_failed".to_owned()).unwrap_or_default() } else { code };
                            if !code.is_empty() || body.get("ok") == Some(&Value::Bool(false)) {
                                self.read_error_code = Some(if code.is_empty() { "read_failed".into() } else { code });
                            } else {
                                let config = object_of(&body, "config").clone();
                                self.loaded = config.clone();
                                self.config = Some(config);
                                self.revision = body.get("revision").cloned().unwrap_or(Value::Null);
                                self.dirty = false;
                                self.kinds.clear();
                                self.uncommitted = super::rpc::flag(&body, "hasUncommittedWorktreeSetupChanges");
                                self.form_epoch += 1;
                            }
                        }
                        Err(error) => self.read_error = Some(error.to_string()),
                    }
                }
                Req::Write => {
                    self.saving = false;
                    match result {
                        Ok(body) => {
                            let code = str_of(object_of(&body, "error"), "code").to_owned();
                            if code == "stale_project_config" || code == "write_failed" {
                                self.write_error_code = Some(code);
                            } else if let Err(error) = require_success(&body).and_then(|_| match super::rpc::domain_error(&body, "write_project_config_request") {
                                Some(error) => Err(error),
                                None => Ok(()),
                            }) {
                                self.error = Some(error.to_string());
                            } else {
                                self.write_error_code = None;
                                let config = object_of(&body, "config").clone();
                                self.loaded = config.clone();
                                self.config = Some(config);
                                self.revision = body.get("revision").cloned().unwrap_or(Value::Null);
                                self.dirty = false;
                                self.kinds.clear();
                                if body.get("hasUncommittedWorktreeSetupChanges").is_some() {
                                    self.uncommitted = super::rpc::flag(&body, "hasUncommittedWorktreeSetupChanges");
                                }
                                self.form_epoch += 1;
                                ctx.effects.push(UiEffect::Toast { message: t("settings.project.actions.saved"), is_error: false });
                                ctx.core.refresh_host(&self.host_id);
                            }
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
                Req::Inspect => {
                    self.busy = false;
                    match result.and_then(|body| require_success(&body).map(|_| body)) {
                        Ok(body) => {
                            self.recovery = Some(object_of(&body, "state").clone());
                            self.recovery_error = None;
                        }
                        Err(error) => self.recovery_error = Some(error.to_string()),
                    }
                }
                Req::Restore => {
                    self.busy = false;
                    match result.and_then(|body| require_success(&body)) {
                        Ok(()) => {
                            ctx.core.refresh_host(&self.host_id);
                            self.recovery = None;
                            self.recovery_error = None;
                        }
                        Err(error) => self.recovery_error = Some(error.to_string()),
                    }
                }
                Req::EditProject => {
                    let sheet = &mut self.project_sheet;
                    sheet.saving = false;
                    match result {
                        Ok(outcome) => {
                            let name_error = str_of(&outcome, "nameError");
                            let icon_error = str_of(&outcome, "iconError");
                            if name_error.is_empty() && icon_error.is_empty() {
                                sheet.open = false;
                                self.view.wa_modal(id!(project_sheet)).close(cx);
                                ctx.effects.push(UiEffect::Toast { message: t("settings.project.edit.savedToast"), is_error: false });
                                ctx.core.refresh_host(&self.host_id);
                            } else {
                                sheet.name_error = (!name_error.is_empty()).then(|| name_error.to_owned());
                                sheet.icon_error = (!icon_error.is_empty()).then(|| icon_error.to_owned());
                            }
                        }
                        Err(error) => sheet.icon_error = Some(error.to_string()),
                    }
                }
            }
        }
        if self.guard_dirty != Some(self.dirty) && !self.tab_key.is_empty() {
            self.guard_dirty = Some(self.dirty);
            ctx.effects.push(UiEffect::SetCloseGuard { owner: self.tab_key.clone(), dirty: self.dirty });
        }
    }

    fn reload(&mut self, cx: &mut Cx, ctx: &UiContext) {
        if self.dirty {
            self.view.wa_dialog(id!(dialog)).confirm(
                cx,
                "discard-reload",
                l("Discard unsaved project settings?"),
                l("Your edits have not been saved to the host."),
                l("Discard"),
                true,
            );
            return;
        }
        self.read(ctx);
        self.view.redraw(cx);
    }

    fn open_script_sheet(&mut self, cx: &mut Cx, old_name: Option<String>) {
        let config = self.config.clone().unwrap_or(Value::Null);
        let existing = old_name.as_ref().map(|name| object_of(object_of(&config, "scripts"), name).clone()).unwrap_or(Value::Null);
        let port = match existing.get("port") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        self.script_sheet = ScriptSheet {
            open: true,
            old_name: old_name.clone(),
            name_touched: false,
            command_touched: false,
            service: str_of(&existing, "type") == "service",
        };
        let view = self.root();
        view.wa_input(id!(script_name)).set_text(cx, old_name.as_deref().unwrap_or(""));
        view.wa_input(id!(script_command)).set_text(cx, str_of(&existing, "command"));
        view.wa_input(id!(script_port)).set_text(cx, &port);
        view.pswitch(id!(service)).set_value(cx, self.script_sheet.service);
        view.wa_modal(id!(script_sheet)).open(cx);
        if old_name.is_none() {
            view.wa_input(id!(script_name)).set_key_focus(cx);
        }
        view.redraw(cx);
    }

    fn save_script(&mut self, cx: &mut Cx) {
        let view = self.root();
        let name = view.wa_input(id!(script_name)).text();
        let command = view.wa_input(id!(script_command)).text();
        let port = view.wa_input(id!(script_port)).text();
        let config = self.config.clone().unwrap_or(Value::Null);
        let old_name = self.script_sheet.old_name.clone();
        let name_error = model::script_name_error(&name, old_name.as_deref(), &config);
        if name_error.is_some() || command.trim().is_empty() {
            self.script_sheet.name_touched = true;
            self.script_sheet.command_touched = true;
            self.view.redraw(cx);
            return;
        }
        let existing = old_name.as_ref().map(|n| object_of(object_of(&config, "scripts"), n).clone()).unwrap_or(Value::Null);
        let entry = model::script_entry(&existing, &command, &port, self.script_sheet.service);
        let key = name.trim().to_owned();
        self.change(|config, _, _| model::replace_script(config, old_name.as_deref(), &key, entry));
        self.script_sheet.open = false;
        view.wa_modal(id!(script_sheet)).close(cx);
        self.view.redraw(cx);
    }

    fn open_project_sheet(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let Some(project) = self.project(ctx).cloned() else { return };
        let display_name = self.display_name(ctx);
        let view_key = project_view_key(&self.host_id, &project);
        let view = self.root();
        view.wa_input(id!(project_name)).set_text(cx, str_of(&project, "projectCustomName"));
        view.wa_input(id!(project_name)).set_empty_text(cx, display_name.trim().to_owned());
        view.wa_input(id!(icon_url)).set_text(cx, "");
        self.project_sheet = ProjectSheet { open: true, project, display_name, view_key, ..Default::default() };
        view.wa_modal(id!(project_sheet)).open(cx);
        view.redraw(cx);
    }

    fn has_custom_icon(&self) -> bool {
        self.project_sheet.project.get("projectCustomIconRevision").is_some_and(|v| !v.is_null())
    }

    /// `_iconIntent` as a kind, plus the url text.
    fn icon_intent(&self, url: &str) -> Option<IconChoice> {
        match self.project_sheet.choice {
            IconChoice::Automatic if self.has_custom_icon() => Some(IconChoice::Automatic),
            IconChoice::Upload if self.project_sheet.picked.is_some() => Some(IconChoice::Upload),
            IconChoice::Url if !url.trim().is_empty() => Some(IconChoice::Url),
            _ => None,
        }
    }

    fn renamed(&self, name: &str) -> bool {
        name.trim() != str_of(&self.project_sheet.project, "projectCustomName")
    }

    /// `submitProjectEdit` in one host operation: acquire icon bytes (file or URL), then the
    /// rename, then the icon; each failure is reported under its own field.
    fn submit_project(&mut self, ctx: &UiContext) {
        let view = self.root();
        let name = view.wa_input(id!(project_name)).text();
        let url = view.wa_input(id!(icon_url)).text();
        let intent = self.icon_intent(&url);
        let rename = self.renamed(&name);
        if !rename && intent.is_none() {
            return;
        }
        if intent == Some(IconChoice::Url) && let Some(error) = model::icon_url_error(&url) {
            self.project_sheet.icon_error = Some(l(error).to_owned());
            return;
        }
        let sheet = &mut self.project_sheet;
        sheet.saving = true;
        sheet.name_error = None;
        sheet.icon_error = None;
        let project_id = str_of(&sheet.project, "projectId").to_owned();
        let picked = sheet.picked.as_ref().map(|(path, _)| path.clone());
        let id = ctx.core.operation(&self.host_id, move |client| async move {
            let source = match intent {
                Some(IconChoice::Automatic) => Some(json!({ "type": "automatic" })),
                Some(IconChoice::Upload) => match picked.map(|path| read_icon_file(&path)) {
                    Some(Ok(data)) => Some(json!({ "type": "upload", "data": data })),
                    Some(Err(error)) => return Ok(json!({ "iconError": error })),
                    None => None,
                },
                Some(IconChoice::Url) => match import_icon_from_url(url.trim()).await {
                    Ok(data) => Some(json!({ "type": "upload", "data": data })),
                    Err(error) => return Ok(json!({ "iconError": error })),
                },
                _ => None,
            };
            if rename {
                let trimmed = name.trim();
                let custom = if trimmed.is_empty() { Value::Null } else { Value::String(trimmed.to_owned()) };
                let outcome = client
                    .rpc("project.rename.request", "project.rename.response", json!({ "projectId": project_id, "customName": custom }), RequestOptions::default())
                    .await;
                if let Some(error) = operation_error(outcome) {
                    return Ok(json!({ "nameError": error }));
                }
            }
            if let Some(source) = source {
                let outcome = client
                    .rpc("project.icon.set.request", "project.icon.set.response", json!({ "projectId": project_id, "source": source }), RequestOptions::default())
                    .await;
                if let Some(error) = operation_error(outcome) {
                    return Ok(json!({ "iconError": error }));
                }
            }
            Ok(json!({}))
        });
        self.pending.track(id, "project.edit", Req::EditProject);
    }

    fn handle_platform(&mut self, cx: &mut Cx, actions: &Actions) {
        let Some(request) = self.project_sheet.pick_request else { return };
        for action in actions.iter() {
            let Some(PlatformAction::Completed { request: id, result }) = action.downcast_ref::<PlatformAction>() else { continue };
            if *id != request {
                continue;
            }
            self.project_sheet.pick_request = None;
            match result {
                Ok(PlatformValue::Files(files)) => {
                    if let Some(file) = files.first() {
                        if file.size as usize > model::MAX_ICON_BYTES {
                            self.project_sheet.icon_error = Some(l("Icon must be 512 KB or smaller").to_owned());
                        } else {
                            let image = self.view.image(id!(preview_image));
                            let _ = image.load_image_file_by_path_async(cx, &file.path);
                            self.project_sheet.picked = Some((file.path.clone(), file.name.clone()));
                            self.project_sheet.choice = IconChoice::Upload;
                            self.project_sheet.name_error = None;
                            self.project_sheet.icon_error = None;
                        }
                    }
                }
                Ok(_) => {}
                Err(error) if error.code == "cancelled" => {}
                Err(error) => self.project_sheet.icon_error = Some(error.message.clone()),
            }
            self.view.redraw(cx);
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        self.handle_platform(cx, actions);
        let view = self.root();
        let dialog = view.wa_dialog(id!(dialog));
        if let Some(token) = dialog.confirmed(actions) {
            if token == "discard-reload" {
                self.dirty = false;
                self.read(ctx);
            } else if let Some(name) = token.strip_prefix("remove:") {
                let name = name.to_owned();
                self.change(|config, _, _| model::remove_script(config, &name));
            } else if token == "restore" {
                self.busy = true;
                self.pending.send(&ctx.core, &self.host_id, "workspace.recovery.restore.request", json!({ "workspaceId": self.workspace_id }), Req::Restore);
            }
            self.view.redraw(cx);
        }
        if let Some((token, value)) = dialog.submitted(actions) {
            if token == "port-range" {
                self.port_range_draft = value;
                let script = self.config.as_ref().map(|c| str_of(object_of(object_of(c, "worktree"), "servicePorts"), "portScript").to_owned()).unwrap_or_default();
                dialog.prompt(cx, "port-script", l("Port allocation command (optional)"), &script, true);
            } else if token == "port-script" {
                let range = std::mem::take(&mut self.port_range_draft);
                if !model::valid_port_range(&range) {
                    ctx.effects.push(UiEffect::Toast { message: l("Enter an inclusive port range between 1 and 65535.").to_owned(), is_error: true });
                } else {
                    self.change(|config, _, _| model::set_service_ports(config, &range, &value));
                }
            }
            self.view.redraw(cx);
        }
        let select = view.wa_select(id!(select));
        if let Some((token, value)) = select.picked(actions) && let Some(name) = token.strip_prefix("script-menu:") {
            let name = name.to_owned();
            if value == "edit" {
                self.open_script_sheet(cx, Some(name));
            } else if value == "remove" {
                let label = if name.is_empty() { t("settings.project.scripts.removeFallbackName") } else { name.clone() };
                dialog.confirm(
                    cx,
                    &format!("remove:{name}"),
                    &t("settings.project.scripts.removeTitle"),
                    &ta("settings.project.scripts.removeMessage", &[("name", &label)]),
                    &t("settings.project.scripts.actions.remove"),
                    true,
                );
            }
        }
        if view.wa_button(id!(read_reload)).clicked(actions)
            || view.wa_button(id!(stale_reload)).clicked(actions)
            || view.wa_button(id!(write_reload)).clicked(actions)
        {
            self.reload(cx, ctx);
        }
        if (view.wa_button(id!(write_retry)).clicked(actions) || view.wa_button(id!(save)).clicked(actions)) && !self.config.as_ref().is_some_and(model::invalid_scripts) {
            self.save(ctx);
            self.view.redraw(cx);
        }
        for (index, button) in [id!(worktree_group.header.info), id!(scripts_group.header.info), id!(metadata_group.header.info)].iter().enumerate() {
            if view.wa_button(*button).clicked(actions) {
                self.info_open[index] = !self.info_open[index];
                self.view.redraw(cx);
            }
        }
        if view.wa_button(id!(setup_section.header.docs)).clicked(actions) || view.wa_button(id!(teardown_section.header.docs)).clicked(actions) {
            ctx.effects.push(UiEffect::OpenExternal { url: model::WORKTREE_DOCS_URL.to_owned() });
        }
        // Text areas: Enter inserts a newline (multi-line inputs), every change marks dirty.
        let areas: [(&[LiveId], &str); 5] = [
            (id!(setup_input), "setup"),
            (id!(teardown_input), "teardown"),
            (id!(meta_branch), "meta:branchName"),
            (id!(meta_commit), "meta:commitMessage"),
            (id!(meta_pr), "meta:pullRequest"),
        ];
        for (path, field) in areas {
            let input = view.wa_input(path);
            let mut text = input.changed(actions);
            if input.returned(actions).is_some() {
                text = Some(insert_newline(cx, &input));
            }
            let Some(text) = text else { continue };
            if let Some(key) = field.strip_prefix("meta:") {
                self.change(|config, _, _| model::set_metadata_text(config, key, &text));
            } else {
                self.change(|config, loaded, kinds| model::set_lifecycle_text(config, loaded, kinds, field, &text));
            }
            self.view.redraw(cx);
        }
        if view.wa_button(id!(ports_row.controls.action)).clicked(actions) && !self.saving {
            let range = self.config.as_ref().map(|c| str_of(object_of(object_of(c, "worktree"), "servicePorts"), "range").to_owned()).unwrap_or_default();
            dialog.prompt(cx, "port-range", l("Service port range (for example 3000-3999)"), &range, false);
        }
        if view.wa_button(id!(add_script)).clicked(actions) && !self.saving {
            self.open_script_sheet(cx, None);
        }
        let rows = view.wa_repeat(id!(script_rows));
        let names: Vec<String> = self.config.as_ref().map(model::scripts).unwrap_or_default().into_iter().map(|(n, _)| n).collect();
        if let Some(index) = super::kit::tapped_in(&rows, id!(main), actions) && !self.saving && let Some(name) = names.get(index) {
            self.open_script_sheet(cx, Some(name.clone()));
        }
        if let Some(index) = clicked_in(&rows, id!(menu), actions) && let (Some(name), false) = (names.get(index), self.saving) {
            select.open(
                cx,
                &format!("script-menu:{name}"),
                SelectRequest::new(
                    t("settings.project.scripts.menuAccessibility"),
                    vec![
                        SelectOption::new("edit", t("settings.project.scripts.actions.edit")),
                        SelectOption::new("remove", t("settings.project.scripts.actions.remove")).destructive(),
                    ],
                ),
            );
        }
        // Script sheet.
        if self.script_sheet.open {
            if !view.wa_modal(id!(script_sheet)).is_open() || view.wa_button(id!(script_sheet.sheet.header.close)).clicked(actions) || view.wa_button(id!(script_cancel)).clicked(actions) {
                self.script_sheet.open = false;
                view.wa_modal(id!(script_sheet)).close(cx);
                self.view.redraw(cx);
            }
            let command = view.wa_input(id!(script_command));
            if command.returned(actions).is_some() {
                insert_newline(cx, &command);
            }
            if view.wa_input(id!(script_name)).changed(actions).is_some() || command.changed(actions).is_some() {
                self.view.redraw(cx);
            }
            if let Some(active) = view.pswitch(id!(service)).changed(actions) {
                self.script_sheet.service = active;
            }
            for action in actions.iter().filter_map(|a| a.as_widget_action()) {
                if let TextInputAction::KeyFocusLost = action.cast() {
                    if action.widget_uid == view.wa_input(id!(script_name)).widget_uid() {
                        self.script_sheet.name_touched = true;
                        self.view.redraw(cx);
                    }
                    if action.widget_uid == command.widget_uid() {
                        self.script_sheet.command_touched = true;
                        self.view.redraw(cx);
                    }
                }
            }
            if view.wa_button(id!(script_save)).clicked(actions) {
                self.save_script(cx);
            }
        }
        // Project sheet.
        if view.wa_button(id!(edit_project)).clicked(actions) {
            self.open_project_sheet(cx, ctx);
        }
        if self.project_sheet.open {
            let modal = view.wa_modal(id!(project_sheet));
            let cancel = view.wa_button(id!(project_cancel)).clicked(actions) || view.wa_button(id!(project_sheet.sheet.header.close)).clicked(actions);
            if !modal.is_open() || cancel {
                // `handleClose`: the sheet stays while a save is in flight.
                if self.project_sheet.saving {
                    modal.open(cx);
                } else {
                    self.project_sheet.open = false;
                    modal.close(cx);
                }
                self.view.redraw(cx);
            }
            if view.wa_input(id!(project_name)).changed(actions).is_some() {
                self.project_sheet.name_error = None;
                self.project_sheet.icon_error = None;
                self.view.redraw(cx);
            }
            if let Some(url) = view.wa_input(id!(icon_url)).changed(actions) {
                // Emptying the box withdraws the URL without discarding an earlier pick.
                let sheet = &mut self.project_sheet;
                sheet.choice = if !url.trim().is_empty() {
                    IconChoice::Url
                } else if sheet.picked.is_some() {
                    IconChoice::Upload
                } else {
                    IconChoice::Unchanged
                };
                sheet.name_error = None;
                sheet.icon_error = None;
                self.view.redraw(cx);
            }
            if view.wa_button(id!(choose_image)).clicked(actions) && !self.project_sheet.saving {
                match platform() {
                    Some(platform) => {
                        self.project_sheet.pick_request = Some(platform.pick_files(PickFilesOptions {
                            multiple: false,
                            images_only: true,
                            title: Some(t("settings.project.edit.chooseImage")),
                        }));
                    }
                    None => self.project_sheet.icon_error = Some("Platform services are unavailable".into()),
                }
            }
            if view.wa_button(id!(use_automatic)).clicked(actions) && !self.project_sheet.saving {
                let custom = self.has_custom_icon();
                let sheet = &mut self.project_sheet;
                sheet.picked = None;
                sheet.choice = if custom { IconChoice::Automatic } else { IconChoice::Unchanged };
                sheet.name_error = None;
                sheet.icon_error = None;
                view.wa_input(id!(icon_url)).set_text(cx, "");
                self.view.redraw(cx);
            }
            if view.wa_button(id!(project_save)).clicked(actions) && !self.project_sheet.saving {
                self.submit_project(ctx);
                self.view.redraw(cx);
            }
        }
        // Recovery.
        if view.wa_button(id!(inspect_row.controls.action)).clicked(actions) && !self.busy {
            self.busy = true;
            self.recovery_error = None;
            self.pending.send(&ctx.core, &self.host_id, "workspace.recovery.inspect.request", json!({ "workspaceId": self.workspace_id }), Req::Inspect);
            self.view.redraw(cx);
        }
        if view.wa_button(id!(restore_row.row.controls.action)).clicked(actions) && !self.busy && let Some(recovery) = &self.recovery {
            let message = format!("{}\n{}", str_of(recovery, "action"), str_of(recovery, "branch"));
            dialog.confirm(cx, "restore", l("Restore workspace?"), &message, l("Restore"), false);
        }
    }

    fn present(&mut self, cx: &mut Cx2d, ctx: &UiContext, width: f64) {
        let view = self.root();
        let form_width = width.clamp(0.0, 720.0);
        if width > 0.0 && (form_width - self.form_width).abs() > 0.5 {
            self.form_width = form_width;
            view.widget(id!(form)).apply_over(cx, live! { width: (form_width) });
        }
        let project = self.project(ctx).cloned();
        let name = self.display_name(ctx);
        let key = project.as_ref().map(|p| project_view_key(&self.host_id, p)).unwrap_or_else(|| json!([self.host_id, self.root]).to_string());
        set_project_icon(cx, &view.widget(id!(header.icon)), &name, &key);
        view.widget(id!(header.name)).set_text(cx, &format!("{name}{}", if self.dirty { " •" } else { "" }));
        let edit = view.wa_button(id!(edit_project));
        edit.set_visible(cx, project.is_some());
        edit.set_tooltip(cx, &t("settings.project.edit.title"));

        let gone = self.host_gone(ctx);
        let show_loading = !gone && self.config.is_none() && (self.loading || (self.read_error.is_none() && self.read_error_code.is_none()));
        let show_failure = !gone && !show_loading && (self.read_error.is_some() || self.read_error_code.is_some());
        let show_form = !gone && !show_loading && !show_failure && self.config.is_some();
        let gone_view = view.widget(id!(host_gone));
        gone_view.set_visible(cx, gone);
        gone_view.widget(id!(text)).set_text(cx, &t("settings.project.noEditableTarget"));
        view.widget(id!(loading)).set_visible(cx, show_loading);
        let failure = view.widget(id!(read_failure));
        failure.set_visible(cx, show_failure);
        if show_failure {
            let (title, description) = match self.read_error_code.as_deref() {
                Some("invalid_project_config") => (t("settings.project.readFailures.invalidTitle"), t("settings.project.readFailures.invalidDescription")),
                Some("project_not_found") => (t("settings.project.readFailures.missingTitle"), t("settings.project.readFailures.missingSingleHost")),
                None => (
                    t("settings.project.readFailures.transportTitle"),
                    self.read_error.clone().unwrap_or_else(|| t("settings.project.readFailures.transportFallback")),
                ),
                Some(_) => (t("settings.project.readFailures.failedTitle"), t("settings.project.readFailures.failedDescription")),
            };
            set_alert(cx, &failure, &title, &description);
            let reload = view.wa_button(id!(read_reload));
            reload.set_text(cx, &t("settings.project.actions.reload"));
            reload.set_enabled(cx, !self.loading);
        }
        view.widget(id!(body)).set_visible(cx, show_form);
        if show_form {
            self.present_form(cx);
        }
        let recovery = view.widget(id!(recovery));
        recovery.set_visible(cx, !self.workspace_id.is_empty());
        if !self.workspace_id.is_empty() {
            recovery.widget(id!(title)).set_text(cx, l("Workspace recovery"));
            let inspect = view.widget(id!(inspect_row));
            inspect.widget(id!(body.label)).set_text(cx, l("Inspect missing workspace recovery"));
            let hint = self.recovery.as_ref().map(|r| super::rpc::str_or(r, "message", str_of(r, "action")).to_owned()).unwrap_or_default();
            set_optional(cx, &inspect.widget(id!(body.hint)), &hint);
            let run = view.wa_button(id!(inspect_row.controls.action));
            run.set_text(cx, &t("settings.diagnostics.app.run"));
            run.set_enabled(cx, !self.busy);
            let recoverable = self.recovery.as_ref().is_some_and(|r| str_of(r, "kind") == "recoverable");
            view.widget(id!(restore_row)).set_visible(cx, recoverable);
            if let (true, Some(r)) = (recoverable, &self.recovery) {
                let row = view.widget(id!(restore_row.row));
                row.widget(id!(body.label)).set_text(cx, l("Restore workspace"));
                set_optional(cx, &row.widget(id!(body.hint)), &format!("{}\n{}", str_of(r, "action"), str_of(r, "branch")));
                let restore = view.wa_button(id!(restore_row.row.controls.action));
                restore.set_text(cx, l("Restore"));
                restore.set_enabled(cx, !self.busy);
            }
            set_optional(cx, &view.widget(id!(recovery_error)), self.recovery_error.as_deref().unwrap_or(""));
        }
        if self.script_sheet.open {
            self.present_script_sheet(cx);
        }
        if self.project_sheet.open {
            self.present_project_sheet(cx);
        }
    }

    fn present_form(&mut self, cx: &mut Cx) {
        let view = self.root();
        let config = self.config.clone().unwrap_or(Value::Null);
        let groups = [
            (id!(worktree_group), "settings.project.worktree.title", "settings.project.worktree.info"),
            (id!(scripts_group), "settings.project.scripts.title", "settings.project.scripts.info"),
            (id!(metadata_group), "settings.project.metadata.title", "settings.project.metadata.info"),
        ];
        for (index, (path, title, info)) in groups.iter().enumerate() {
            let group = view.widget(*path);
            group.widget(id!(header.title)).set_text(cx, &t(title));
            // `Tooltip(triggerMode: tap)`: hover shows it on desktop, a tap shows it inline.
            group.wa_button(id!(header.info)).set_tooltip(cx, &t(info));
            set_optional(cx, &group.widget(id!(info)), if self.info_open[index] { t(info) } else { String::new() }.as_str());
        }
        for (path, title) in [(id!(setup_section), "settings.project.worktree.setup"), (id!(teardown_section), "settings.project.worktree.teardown")] {
            let section = view.widget(path);
            section.widget(id!(header.title)).set_text(cx, &t(title));
            section.wa_button(id!(header.docs)).set_text(cx, &t("settings.project.worktree.docs"));
        }
        let uncommitted = view.widget(id!(uncommitted));
        uncommitted.set_visible(cx, self.uncommitted);
        if self.uncommitted {
            set_alert(cx, &uncommitted, &t("settings.project.worktree.uncommittedTitle"), &t("settings.project.worktree.uncommittedDescription"));
        }
        if self.applied_epoch != self.form_epoch {
            self.applied_epoch = self.form_epoch;
            sync_input(cx, &view.wa_input(id!(setup_input)), &model::lifecycle_text(&config, "setup"));
            sync_input(cx, &view.wa_input(id!(teardown_input)), &model::lifecycle_text(&config, "teardown"));
            sync_input(cx, &view.wa_input(id!(meta_branch)), &model::metadata_text(&config, "branchName"));
            sync_input(cx, &view.wa_input(id!(meta_commit)), &model::metadata_text(&config, "commitMessage"));
            sync_input(cx, &view.wa_input(id!(meta_pr)), &model::metadata_text(&config, "pullRequest"));
        }
        for path in [id!(setup_input), id!(teardown_input), id!(meta_branch), id!(meta_commit), id!(meta_pr)] {
            view.wa_input(path).set_is_read_only(cx, self.saving);
        }
        let ports = view.widget(id!(ports_section));
        ports.widget(id!(title)).set_text(cx, l("Service ports"));
        let row = view.widget(id!(ports_row));
        row.widget(id!(body.label)).set_text(cx, l("Service port allocation"));
        let range = super::rpc::str_or(object_of(object_of(&config, "worktree"), "servicePorts"), "range", l("Host default")).to_owned();
        set_optional(cx, &row.widget(id!(body.hint)), &range);
        let edit = view.wa_button(id!(ports_row.controls.action));
        edit.set_text(cx, &t("settings.project.scripts.actions.edit"));
        edit.set_enabled(cx, !self.saving);

        let scripts = model::scripts(&config);
        set_optional(cx, &view.widget(id!(scripts_empty)), if scripts.is_empty() { t("settings.project.scripts.empty") } else { String::new() }.as_str());
        let rows = view.wa_repeat(id!(script_rows));
        rows.set_count(cx, scripts.len());
        for (index, (name, script)) in scripts.iter().enumerate() {
            let item = rows.item(cx, index);
            item.widget(id!(rule)).set_visible(cx, index > 0);
            item.widget(id!(name)).set_text(cx, &if name.is_empty() { t("settings.project.scripts.untitled") } else { name.clone() });
            let hint = model::script_hint(script, |port| ta("settings.project.scripts.port", &[("port", port)]));
            set_optional(cx, &item.widget(id!(hint)), &hint);
            let menu = item.wa_button(id!(menu));
            menu.set_enabled(cx, !self.saving);
            menu.set_tooltip(cx, &t("settings.project.scripts.menuAccessibility"));
        }
        let add = view.wa_button(id!(add_script));
        add.set_enabled(cx, !self.saving);
        add.set_tooltip(cx, &t("settings.project.scripts.actions.add"));
        for (path, key) in [(id!(branch_section), "branchName"), (id!(commit_section), "commitMessage"), (id!(pr_section), "pullRequest")] {
            let section = view.widget(path);
            section.widget(id!(title)).set_text(cx, &t(&format!("settings.project.metadata.{key}")));
        }
        view.wa_input(id!(meta_branch)).set_empty_text(cx, t("settings.project.metadata.branchNamePlaceholder"));
        view.wa_input(id!(meta_commit)).set_empty_text(cx, t("settings.project.metadata.commitMessagePlaceholder"));
        view.wa_input(id!(meta_pr)).set_empty_text(cx, t("settings.project.metadata.pullRequestPlaceholder"));

        let stale = self.write_error_code.as_deref() == Some("stale_project_config");
        let write_failed = self.write_error_code.as_deref() == Some("write_failed");
        let stale_view = view.widget(id!(stale));
        stale_view.set_visible(cx, stale);
        if stale {
            set_alert(cx, &stale_view, &t("settings.project.writeFailures.staleTitle"), &t("settings.project.writeFailures.staleDescription"));
            view.wa_button(id!(stale_reload)).set_text(cx, &t("settings.project.actions.reload"));
        }
        let failed_view = view.widget(id!(write_failed));
        failed_view.set_visible(cx, write_failed);
        if write_failed {
            set_alert(cx, &failed_view, &t("settings.project.writeFailures.failedTitle"), &t("settings.project.writeFailures.failedDescription"));
            let retry = view.wa_button(id!(write_retry));
            retry.set_text(cx, &t("settings.project.actions.tryAgain"));
            retry.set_enabled(cx, !self.saving);
            view.wa_button(id!(write_reload)).set_text(cx, &t("settings.project.actions.reload"));
        }
        let error_view = view.widget(id!(save_error));
        error_view.set_visible(cx, self.error.is_some());
        if let Some(error) = &self.error {
            set_alert(cx, &error_view, &t("settings.project.writeFailures.failedTitle"), error);
        }
        let save = view.wa_button(id!(save));
        save.set_text(cx, &if self.saving { t("settings.project.actions.saving") } else { t("settings.project.actions.save") });
        save.set_enabled(cx, !self.saving && !stale && !model::invalid_scripts(&config));
    }

    fn present_script_sheet(&mut self, cx: &mut Cx) {
        let view = self.root();
        let name = view.wa_input(id!(script_name)).text();
        let key = name.trim().to_owned();
        view.widget(id!(script_sheet.sheet.header.title)).set_text(
            cx,
            &if key.is_empty() { t("settings.project.scripts.newScript") } else { ta("settings.project.scripts.editScript", &[("name", &key)]) },
        );
        let config = self.config.clone().unwrap_or(Value::Null);
        let name_error = match model::script_name_error(&name, self.script_sheet.old_name.as_deref(), &config) {
            Some(model::ScriptNameError::Required) => t("settings.project.scripts.nameRequired"),
            Some(model::ScriptNameError::Duplicate) => lf("A script named \"{name}\" already exists", &[("name", &key)]),
            None => String::new(),
        };
        let command_error = if view.wa_input(id!(script_command)).text().trim().is_empty() { t("settings.project.scripts.commandRequired") } else { String::new() };
        view.widget(id!(script_sheet.sheet.fields.name_field.label)).set_text(cx, &t("settings.project.scripts.name"));
        view.widget(id!(script_sheet.sheet.fields.command_field.label)).set_text(cx, &t("settings.project.scripts.command"));
        view.widget(id!(script_sheet.sheet.fields.port_field.label)).set_text(cx, l("Port"));
        set_optional(cx, &view.widget(id!(name_field.error)), if self.script_sheet.name_touched { &name_error } else { "" });
        set_optional(cx, &view.widget(id!(command_field.error)), if self.script_sheet.command_touched { &command_error } else { "" });
        view.widget(id!(service_row.texts.label)).set_text(cx, &t("settings.project.scripts.runAsService"));
        view.widget(id!(service_row.texts.hint)).set_text(cx, &t("settings.project.scripts.serviceHint"));
        view.wa_button(id!(script_cancel)).set_text(cx, &t("settings.project.actions.cancel"));
        view.wa_button(id!(script_save)).set_text(cx, &t("settings.project.actions.save"));
    }

    fn present_project_sheet(&mut self, cx: &mut Cx) {
        let view = self.root();
        let sheet = &self.project_sheet;
        view.widget(id!(project_sheet.sheet.header.title)).set_text(cx, &t("settings.project.edit.title"));
        view.widget(id!(project_sheet.sheet.fields.name_field.label)).set_text(cx, &t("settings.project.edit.name"));
        view.widget(id!(project_sheet.sheet.fields.icon_field.label)).set_text(cx, &t("settings.project.edit.icon"));
        let show_picked = sheet.choice == IconChoice::Upload && sheet.picked.is_some();
        view.widget(id!(preview_icon)).set_visible(cx, !show_picked);
        view.widget(id!(preview_image)).set_visible(cx, show_picked);
        if !show_picked {
            set_project_icon(cx, &view.widget(id!(preview_icon)), sheet.display_name.trim(), &sheet.view_key);
        }
        set_optional(cx, &view.widget(id!(picked)), if show_picked { sheet.picked.as_ref().map(|(_, n)| n.as_str()).unwrap_or("") } else { "" });
        let choose = view.wa_button(id!(choose_image));
        choose.set_text(cx, &t("settings.project.edit.chooseImage"));
        choose.set_enabled(cx, !sheet.saving);
        let url = view.wa_input(id!(icon_url)).text();
        let intent = self.icon_intent(&url);
        let can_use_automatic = match intent {
            Some(kind) => kind != IconChoice::Automatic,
            None => self.has_custom_icon(),
        };
        let automatic = view.wa_button(id!(use_automatic));
        automatic.set_visible(cx, can_use_automatic);
        automatic.set_text(cx, &t("settings.project.edit.useAutomatic"));
        view.wa_input(id!(icon_url)).set_empty_text(cx, t("settings.project.edit.imageUrl"));
        let sheet = &self.project_sheet;
        set_optional(cx, &view.widget(id!(project_sheet.sheet.fields.name_field.error)), sheet.name_error.as_deref().unwrap_or(""));
        set_optional(cx, &view.widget(id!(icon_field.error)), sheet.icon_error.as_deref().unwrap_or(""));
        let name = view.wa_input(id!(project_name)).text();
        let can_submit = self.renamed(&name) || intent.is_some();
        view.wa_button(id!(project_cancel)).set_text(cx, &t("common.actions.cancel"));
        view.wa_button(id!(project_cancel)).set_enabled(cx, !sheet.saving);
        let save = view.wa_button(id!(project_save));
        save.set_text(cx, &t("settings.project.edit.save"));
        save.set_enabled(cx, can_submit && !sheet.saving);
        for path in [id!(project_name), id!(icon_url)] {
            view.wa_input(path).set_is_read_only(cx, sheet.saving);
        }
    }
}

fn operation_error(outcome: anyhow::Result<Value>) -> Option<String> {
    match outcome {
        Ok(body) => require_success(&body)
            .err()
            .or_else(|| super::rpc::domain_error(&body, "project"))
            .map(|error| error.to_string()),
        Err(error) => Some(error.to_string()),
    }
}

fn read_icon_file(path: &std::path::Path) -> Result<String, String> {
    use base64::Engine;
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() > model::MAX_ICON_BYTES {
        return Err(l("Icon must be 512 KB or smaller").to_owned());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

async fn fetch_ok(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let response = client.get(url).send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(lf("Download failed ({status})", &[("status", &status.as_u16().to_string())]));
    }
    Ok(response)
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let too_large = || l("Icon must be 512 KB or smaller").to_owned();
    if response.content_length().is_some_and(|length| length as usize > model::MAX_ICON_BYTES) {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > model::MAX_ICON_BYTES {
            return Err(too_large());
        }
    }
    Ok(bytes)
}

/// `importProjectIconFromUrl`: an image URL uploads its bytes; a website offers its declared
/// non-SVG `<link rel=icon>`s, then `/favicon.ico`, at most eight candidates.
async fn import_icon_from_url(url: &str) -> Result<String, String> {
    use base64::Engine;
    if let Some(error) = model::icon_url_error(url) {
        return Err(l(error).to_owned());
    }
    let client = reqwest::Client::builder().build().map_err(|error| error.to_string())?;
    let response = fetch_ok(&client, url).await?;
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/html"));
    let bytes = read_capped(response).await?;
    if !html {
        return Ok(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    let page = String::from_utf8_lossy(&bytes);
    let mut candidates: Vec<String> = model::declared_icons(&page).iter().map(|href| model::resolve_url(url, href)).collect();
    candidates.push(model::resolve_url(url, "/favicon.ico"));
    let mut last = l("Website does not expose a supported icon").to_owned();
    for candidate in candidates.into_iter().take(8) {
        match fetch_ok(&client, &candidate).await {
            Ok(response) => match read_capped(response).await {
                Ok(bytes) => return Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
                Err(error) => last = error,
            },
            Err(error) => last = error,
        }
    }
    Err(last)
}

impl Widget for WaProjectSettingsView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event && let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        // Use this allocation: the previous content-sized area can exceed a compact viewport.
        let width = cx.peek_walk_turtle(walk).size.x;
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.present(cx, ctx, width);
        }
        self.view.draw_walk(cx, scope, walk)
    }
}
