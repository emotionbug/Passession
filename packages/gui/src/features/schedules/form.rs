//! The create/edit sheet (Flutter `schedule_form_sheet.dart`, original
//! `components/schedules/schedule-form-sheet.tsx`). New-agent schedules show Name, Prompt,
//! Host (edit or several hosts), Project (once a host is chosen), then — once a project is
//! chosen — Model, Thinking, Mode, Isolation (git project on a multiplicity host) and
//! Archive on finish (multiplicity hosts), then Cadence and Max runs; a heartbeat edit
//! shows its read-only Target and the Cadence.

use std::collections::HashMap;

use makepad_widgets::*;
use crate::features::workspace::kit::{WaModalWidgetExt, WaModalWidgetRefExt};
use serde_json::{json, Map, Value};

use super::model;
use super::timezone;
use crate::common::UiContext;
use crate::features::composer::{agent_form, controls_model};
use crate::features::workspace::host::{self, Host};
use crate::features::workspace::i18n::{l, t};
use crate::common::provider_icon::ProviderIconWidgetRefExt;
use crate::common::widgets::{PLabelWidgetRefExt, PSwitchWidgetRefExt};
use crate::features::workspace::kit::{insert_newline, set_optional, sync_input, WaButtonRef, WaButtonWidgetRefExt, WaInputWidgetRefExt};
use crate::features::workspace::rpc::{array_of, object_of, str_of, str_or, Pending};
use crate::features::workspace::select::{Leading, SelectOption, SelectRequest, WaSelectWidgetExt, WaSelectWidgetRefExt};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::common::provider_icon::ProviderIcon;
    use crate::features::workspace::kit::*;
    use crate::features::workspace::select::*;

    WaFormField = <WaField> {
        hint = <View> { width: Fill, height: Fit, visible: false, text = <WaSmall> {} }
    }

    pub WaScheduleEditor = {{WaScheduleEditor}} {
        width: Fill, height: Fill, flow: Overlay
        wa_root = <View> {
            width: Fill, height: Fill, flow: Overlay
            modal = <WaModal> {
                content: {
                    sheet = <WaSheet> {
                        width: 560.
                        body = <ScrollYView> {
                            width: Fill, height: 560., flow: Down, spacing: 16.
                            padding: { left: 24., right: 24., top: 24., bottom: 24. }
                            permission = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                            target_field = <WaFormField> {
                                // Original `ScheduleAgentTargetField`: surface2, 1px border, field geometry.
                                target_box = <Surface> {
                                    width: Fill, height: 32., align: { y: 0.5 }, padding: { left: 12., right: 12. }
                                    role: Surface2, border_role: Border, radius: 6.
                                    text = <WaLine> {}
                                }
                            }
                            name_field = <WaFormField> { name = <WaInput> { field_size: Adaptive } }
                            prompt_field = <WaFormField> { prompt = <WaInput> { field_size: Adaptive, multiline: true } }
                            host_field = <WaFormField> { host = <WaFieldButton> { field_size: Adaptive } }
                            project_field = <WaFormField> { project = <WaFieldButton> { field_size: Adaptive } }
                            model_field = <WaFormField> {
                                model_box = <View> {
                                    width: Fill, height: Fit, flow: Overlay, align: { y: 0.5 }
                                    model = <WaFieldButton> { field_size: Adaptive }
                                    provider_box = <View> {
                                        width: Fit, height: Fit, visible: false, margin: { left: 13. }
                                        provider = <ProviderIcon> { width: 16., height: 16. }
                                    }
                                }
                            }
                            thinking_field = <WaFormField> { thinking = <WaFieldButton> { field_size: Adaptive } }
                            mode_field = <WaFormField> { mode = <WaFieldButton> { field_size: Adaptive } }
                            isolation_field = <WaFormField> { isolation = <WaFieldButton> { field_size: Adaptive } }
                            archive_field = <WaFormField> { archive = <WaToggle> {} }
                            cadence_field = <WaFormField> {
                                cadence_preset = <WaFieldButton> { field_size: Adaptive }
                                cron = <WaInputMono> { field_size: Adaptive, input = { empty_text: "0 9 * * *" } }
                                cadence_note = <View> { width: Fill, height: Fit, text = <WaSmall> {} }
                            }
                            max_runs_field = <WaFormField> { max_runs = <WaInput> { field_size: Adaptive, input = { is_numeric_only: true } } }
                            error = <View> { width: Fill, height: Fit, visible: false, text = <WaError> {} }
                        }
                        <WaSheetRule> {}
                        footer = <View> {
                            width: Fill, height: Fit, flow: Right, spacing: 12.
                            padding: { left: 24., right: 24., top: 12., bottom: 12. }
                            cancel = <WaButton> { width: Fill, size: Md }
                            save = <WaButtonPrimary> { width: Fill, size: Md }
                        }
                    }
                }
            }
            select = <WaSelect> {}
        }
    }
}

/// A `SelectFieldTrigger` value: the display, or the muted placeholder.
fn set_field(cx: &mut Cx, button: &WaButtonRef, display: Option<&str>, placeholder: &str) {
    button.set_text(cx, display.unwrap_or(placeholder));
    button.set_color(cx, display.is_none().then(|| crate::theme::current_palette().muted));
}

#[derive(Clone, Debug, DefaultNone)]
pub enum ScheduleEditorAction {
    None,
    /// The host accepted a create/update; the list reloads.
    Saved,
}

#[derive(Clone, Debug)]
enum Req {
    Catalog(u64),
    Save,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaScheduleEditor {
    #[deref]
    view: View,
    #[rust]
    open: bool,
    #[rust]
    schedule: Option<Value>,
    #[rust]
    editing: bool,
    #[rust]
    heartbeat: bool,
    #[rust]
    server_id: Option<String>,
    #[rust]
    error: Option<String>,
    /// The saved cron zone, else the device zone.
    #[rust]
    timezone: String,
    #[rust]
    saved_zone: bool,
    #[rust]
    cwd: String,
    #[rust]
    provider: Option<String>,
    #[rust]
    model: String,
    #[rust]
    mode: String,
    #[rust]
    thinking: String,
    /// The last thinking choice per `provider:model`.
    #[rust]
    thinking_drafts: HashMap<String, String>,
    #[rust]
    entries: Vec<Value>,
    #[rust]
    catalog_loading: bool,
    #[rust]
    catalog_error: Option<String>,
    #[rust]
    catalog_generation: u64,
    #[rust]
    catalog_scope: Option<String>,
    #[rust]
    isolation: String,
    #[rust]
    archive: bool,
    #[rust]
    cadence_changed: bool,
    #[rust]
    saving: bool,
    #[rust]
    prompt: String,
    #[rust]
    cron: String,
    #[rust]
    max_runs: String,
    #[rust]
    name: String,
    #[rust]
    pending: Pending<Req>,
    #[rust]
    load_catalog_pending: bool,
}

impl WaScheduleEditor {
    /// The content root as a handle (the deref `View` is not `Clone`).
    fn root(&self) -> WidgetRef {
        self.view.widget(id!(wa_root))
    }

    fn host<'a>(&self, ctx: &'a UiContext) -> Option<Host<'a>> {
        self.server_id.as_deref().and_then(|id| host::host(&ctx.state, id))
    }

    fn multiplicity(&self, ctx: &UiContext) -> bool {
        self.host(ctx).is_some_and(|h| h.supports("workspaceMultiplicity"))
    }

    fn project<'a>(&self, ctx: &'a UiContext) -> Option<&'a Value> {
        if self.cwd.is_empty() {
            return None;
        }
        self.host(ctx).and_then(|h| h.project_by_root(&self.cwd))
    }

    fn has_project(&self) -> bool {
        !self.cwd.trim().is_empty()
    }

    fn can_use_worktree(&self, ctx: &UiContext) -> bool {
        self.multiplicity(ctx) && self.project(ctx).is_some_and(|p| str_of(p, "projectKind") == "git")
    }

    /// `resolveEffectiveIsolation`: a worktree choice holds while the snapshot resolves, then
    /// falls back to local when the project cannot host one.
    fn effective_isolation(&self, ctx: &UiContext) -> &'static str {
        if self.isolation == "worktree" && (self.can_use_worktree(ctx) || self.catalog_loading) { "worktree" } else { "local" }
    }

    fn can_manage(&self, ctx: &UiContext) -> bool {
        self.host(ctx).is_none_or(|h| h.has_permission("automation.manage"))
    }

    fn enabled_entries(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().filter(|e| e.get("enabled") != Some(&Value::Bool(false)))
    }

    fn entry(&self, id: Option<&str>) -> Option<&Value> {
        id.and_then(|id| agent_form::provider_entry(&self.entries, id))
    }

    fn models(&self, id: Option<&str>) -> Vec<Value> {
        self.entry(id).map(agent_form::selectable_models).unwrap_or_default()
    }

    /// Form-model `resolveEffectiveModel`: the chosen model, else the default, else the first.
    fn effective_model(&self, id: Option<&str>, model_id: &str) -> Option<Value> {
        let models = self.models(id);
        if models.is_empty() || model_id.trim().is_empty() {
            return None;
        }
        models
            .iter()
            .find(|m| str_of(m, "id") == model_id.trim())
            .or_else(|| models.iter().find(|m| m.get("isDefault") == Some(&Value::Bool(true))))
            .or_else(|| models.first())
            .cloned()
    }

    fn mode_options(&self) -> Vec<Value> {
        self.entry(self.provider.as_deref()).map(|e| array_of(e, "modes").to_vec()).unwrap_or_default()
    }

    fn thinking_options(&self) -> Vec<Value> {
        self.effective_model(self.provider.as_deref(), &self.model).map(|m| array_of(&m, "thinkingOptions").to_vec()).unwrap_or_default()
    }

    fn resolve_thinking(&self, id: &str, model_id: &str, requested: &str) -> String {
        let models = self.models(Some(id));
        agent_form::resolve_thinking_option_id(Some(&models), model_id, requested)
    }

    /// `resolvePreferredModeId` without preferences: the default mode, else the first.
    fn default_mode(&self, id: &str) -> String {
        let Some(entry) = self.entry(Some(id)) else { return String::new() };
        let modes = array_of(entry, "modes");
        let preferred = str_of(entry, "defaultModeId");
        if !preferred.is_empty() && (modes.is_empty() || modes.iter().any(|m| str_of(m, "id") == preferred)) {
            return preferred.to_owned();
        }
        modes.first().map(|m| str_of(m, "id").to_owned()).unwrap_or_default()
    }

    /// `buildEntryModelSelection` kind: `models`, `loading` or `error`.
    fn selection_kind(&self, entry: &Value, ctx: &UiContext) -> &'static str {
        if !array_of(entry, "models").is_empty() || str_of(entry, "status") == "ready" {
            return "models";
        }
        if str_of(entry, "status") == "loading" {
            return if self.host(ctx).is_some_and(|h| h.connected()) { "loading" } else { "error" };
        }
        "error"
    }

    fn selection_error(&self, entry: &Value) -> String {
        if str_of(entry, "status") == "loading" {
            return t("providerSelection.readiness.hostDisconnected");
        }
        let message = str_of(entry, "error");
        if !message.is_empty() {
            return message.to_owned();
        }
        t(if str_of(entry, "status") == "unavailable" { "providerSelection.unavailable" } else { "providerSelection.unknownError" })
    }

    /// `isSelectedModelValidForProviders`.
    fn model_valid(&self, ctx: &UiContext) -> bool {
        let Some(id) = self.provider.as_deref() else { return false };
        let Some(entry) = self.enabled_entries().find(|e| str_of(e, "provider") == id) else { return false };
        if self.selection_kind(entry, ctx) != "models" {
            return false;
        }
        let selected = self.model.trim();
        selected.is_empty() || self.models(Some(id)).iter().any(|m| str_of(m, "id") == selected)
    }

    /// `resolveSelectedModelLabel`.
    fn selected_model_label(&self, ctx: &UiContext) -> String {
        let id = self.provider.as_deref();
        let entry = id.and_then(|id| self.enabled_entries().find(|e| str_of(e, "provider") == id));
        let Some(entry) = entry else {
            return if self.catalog_loading { t("providerSelection.loading") } else { t("providerSelection.selectModel") };
        };
        match self.selection_kind(entry, ctx) {
            "loading" => return t("providerSelection.loading"),
            "error" => return t("providerSelection.error"),
            _ => {}
        }
        let models = self.models(id);
        let selected = self.model.trim();
        let found = models.iter().find(|m| str_of(m, "id") == selected);
        if found.is_none() && !selected.is_empty() {
            return selected.to_owned();
        }
        found
            .or_else(|| models.iter().find(|m| m.get("isDefault") == Some(&Value::Bool(true))))
            .or_else(|| models.first())
            .map(controls_model::model_label)
            .unwrap_or_else(|| t("providerSelection.selectModel"))
    }

    /// `resolveSnapshotSelection` once the snapshot lands.
    fn resolve_selection(&mut self) {
        let Some(id) = self.provider.clone() else { return };
        let models = self.models(Some(&id));
        if !self.model.is_empty() && !models.is_empty() && let Some(canonical) = agent_form::find_model_by_reference(Some(&models), &self.model) {
            self.model = str_of(canonical, "id").to_owned();
        }
        if self.mode.is_empty() {
            self.mode = self.default_mode(&id);
        }
        if !models.is_empty() {
            let requested = if self.thinking.is_empty() {
                self.thinking_drafts.get(&format!("{id}:{}", self.model)).cloned().unwrap_or_default()
            } else {
                self.thinking.clone()
            };
            self.thinking = self.resolve_thinking(&id, &self.model.clone(), &requested);
        }
    }

    fn load_catalog(&mut self, ctx: &UiContext) {
        self.catalog_generation += 1;
        self.catalog_scope = None;
        let dir = self.cwd.trim().to_owned();
        let Some(server) = self.server_id.clone().filter(|_| !dir.is_empty()) else {
            self.entries.clear();
            self.catalog_loading = false;
            self.catalog_error = None;
            return;
        };
        self.catalog_loading = true;
        self.catalog_error = None;
        self.pending.send(&ctx.core, &server, "get_providers_snapshot_request", json!({ "cwd": dir }), Req::Catalog(self.catalog_generation));
    }

    /// `setHost`: a new host clears the project and model selection.
    fn set_host(&mut self, value: &str) {
        if self.server_id.as_deref() == Some(value) {
            return;
        }
        self.catalog_generation += 1;
        self.catalog_scope = None;
        self.server_id = Some(value.to_owned());
        self.cwd.clear();
        self.provider = None;
        self.model.clear();
        self.mode.clear();
        self.thinking.clear();
        self.entries.clear();
        self.catalog_loading = false;
        self.catalog_error = None;
    }

    /// `setProject`: a new (host, cwd) scope refetches the snapshot; the current provider
    /// and model carry over, mode and thinking re-resolve.
    fn set_project(&mut self, ctx: &UiContext, path: &str) {
        if path == self.cwd.trim() {
            return;
        }
        self.cwd = path.to_owned();
        self.mode.clear();
        self.thinking.clear();
        self.entries.clear();
        self.load_catalog(ctx);
    }

    /// `setModel`.
    fn set_model(&mut self, id: &str, model_id: &str) {
        let models = self.models(Some(id));
        let picked = if !model_id.trim().is_empty() {
            model_id.trim().to_owned()
        } else {
            models
                .iter()
                .find(|m| m.get("isDefault") == Some(&Value::Bool(true)))
                .or_else(|| models.first())
                .map(|m| str_of(m, "id").to_owned())
                .unwrap_or_default()
        };
        let draft = self.thinking_drafts.get(&format!("{id}:{picked}")).cloned().unwrap_or_default();
        let next_thinking = self.resolve_thinking(id, &picked, &draft);
        if !picked.is_empty() && !next_thinking.is_empty() {
            self.thinking_drafts.insert(format!("{id}:{picked}"), next_thinking.clone());
        }
        self.mode = if self.provider.as_deref() == Some(id) && !self.mode.trim().is_empty() { self.mode.clone() } else { self.default_mode(id) };
        self.provider = Some(id.to_owned());
        self.model = picked;
        self.thinking = next_thinking;
    }

    fn set_thinking(&mut self, id: &str) {
        if let Some(provider) = &self.provider && !self.model.is_empty() {
            self.thinking_drafts.insert(format!("{provider}:{}", self.model), id.to_owned());
        }
        self.thinking = id.to_owned();
    }

    fn cadence_error(&self) -> Option<String> {
        model::validate_cron(&self.cron)
    }

    /// `resolveCanSubmit` plus the submit-time cron validation.
    fn can_submit(&self, ctx: &UiContext) -> bool {
        if self.saving || !self.can_manage(ctx) || self.cadence_error().is_some() {
            return false;
        }
        if self.heartbeat {
            return self.cadence_changed;
        }
        if self.prompt.trim().is_empty() || (!self.editing && self.project(ctx).is_none()) || !self.has_project() {
            return false;
        }
        self.model_valid(ctx)
    }

    fn cadence(&self) -> Value {
        json!({ "type": "cron", "expression": self.cron.trim(), "timezone": self.timezone })
    }

    fn save(&mut self, ctx: &UiContext) {
        if !self.can_submit(ctx) {
            return;
        }
        self.error = None;
        let Some(server) = self.server_id.clone().filter(|id| host::host(&ctx.state, id).is_some()) else {
            self.error = Some(t("common.errors.daemonClientUnavailable"));
            return;
        };
        let max = model::parse_max_runs(&self.max_runs);
        let lifecycle = self.multiplicity(ctx);
        let name = self.name.trim().to_owned();
        let schedule_id = self.schedule.as_ref().and_then(|s| s.get("id").cloned()).unwrap_or(Value::Null);
        let isolation = self.effective_isolation(ctx);
        let optional = |value: &str| if value.is_empty() { Value::Null } else { Value::String(value.to_owned()) };
        let fields = if self.heartbeat {
            json!({ "scheduleId": schedule_id, "cadence": self.cadence() })
        } else if self.editing {
            let mut config = Map::new();
            config.insert("provider".into(), self.provider.clone().map(Value::String).unwrap_or(Value::Null));
            config.insert("model".into(), optional(&self.model));
            config.insert("modeId".into(), optional(&self.mode));
            config.insert("thinkingOptionId".into(), optional(&self.thinking));
            config.insert("cwd".into(), self.cwd.trim().into());
            if lifecycle {
                config.insert("archiveOnFinish".into(), self.archive.into());
                config.insert("isolation".into(), isolation.into());
            }
            let mut fields = Map::new();
            fields.insert("scheduleId".into(), schedule_id);
            fields.insert("name".into(), optional(&name));
            fields.insert("prompt".into(), self.prompt.trim().into());
            if self.cadence_changed {
                fields.insert("cadence".into(), self.cadence());
            }
            fields.insert("newAgentConfig".into(), Value::Object(config));
            fields.insert("maxRuns".into(), max.map(Value::from).unwrap_or(Value::Null));
            Value::Object(fields)
        } else {
            let mut config = Map::new();
            config.insert("provider".into(), self.provider.clone().map(Value::String).unwrap_or(Value::Null));
            config.insert("cwd".into(), self.cwd.trim().into());
            for (key, value) in [("model", &self.model), ("modeId", &self.mode), ("thinkingOptionId", &self.thinking)] {
                if !value.is_empty() {
                    config.insert(key.into(), value.clone().into());
                }
            }
            if lifecycle {
                config.insert("archiveOnFinish".into(), self.archive.into());
                config.insert("isolation".into(), isolation.into());
            }
            if !name.is_empty() {
                config.insert("title".into(), name.clone().into());
            }
            let mut fields = Map::new();
            fields.insert("prompt".into(), self.prompt.trim().into());
            if !name.is_empty() {
                fields.insert("name".into(), name.into());
            }
            fields.insert("cadence".into(), self.cadence());
            fields.insert("target".into(), json!({ "type": "new-agent", "config": Value::Object(config) }));
            if let Some(max) = max {
                fields.insert("maxRuns".into(), max.into());
            }
            Value::Object(fields)
        };
        let action = if self.editing { "update" } else { "create" };
        self.saving = true;
        self.pending.send_typed(&ctx.core, &server, &format!("schedule/{action}"), fields, &format!("schedule/{action}/response"), Req::Save);
    }

    fn handle_core(&mut self, cx: &mut Cx, uid: WidgetUid, path: &HeapLiveIdPath, ctx: &mut UiContext) {
        if !self.open {
            return;
        }
        if self.load_catalog_pending {
            self.load_catalog_pending = false;
            self.load_catalog(ctx);
        }
        if !self.saved_zone {
            let zone = timezone::device_time_zone();
            if zone != self.timezone {
                self.timezone = zone;
                self.view.redraw(cx);
            }
        }
        for (req, result) in self.pending.take(&ctx.events) {
            self.view.redraw(cx);
            match req {
                Req::Catalog(generation) if generation == self.catalog_generation => {
                    self.catalog_loading = false;
                    match result {
                        Ok(body) => {
                            self.catalog_scope = Some(str_of(&body, "cwd").trim().trim_end_matches('/').to_owned());
                            self.apply_catalog(cx, ctx, &body);
                        }
                        Err(error) => self.catalog_error = Some(error.to_string()),
                    }
                }
                Req::Catalog(_) => {}
                Req::Save => {
                    self.saving = false;
                    match result {
                        Ok(_) => {
                            self.open = false;
                            self.view.wa_modal(id!(modal)).close(cx);
                            cx.widget_action(uid, path, ScheduleEditorAction::Saved);
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
            }
        }
        if let Some((_, event)) = ctx.host_events().filter(|(host, event)| {
            Some(*host) == self.server_id.as_deref()
                && str_of(event, "type") == "providers_snapshot_update"
                && self.catalog_scope.as_deref()
                    == Some(str_of(object_of(event, "payload"), "cwd").trim().trim_end_matches('/'))
        }).last() {
            let body = object_of(event, "payload");
            if body.get("entries").is_some() || body.get("compactSnapshot").is_some() {
                self.catalog_generation += 1;
                self.apply_catalog(cx, ctx, body);
            } else {
                self.load_catalog(ctx);
            }
            self.view.redraw(cx);
        }
    }

    fn apply_catalog(&mut self, cx: &mut Cx, ctx: &UiContext, body: &Value) {
        self.entries = agent_form::provider_entries(body);
        self.catalog_loading = false;
        self.catalog_error = None;
        self.resolve_selection();
        self.view.wa_select(id!(select)).refresh(cx, "model", || self.model_request(ctx));
    }

    fn model_request(&self, ctx: &UiContext) -> SelectRequest {
        let mut options = Vec::new();
        for entry in self.enabled_entries() {
            let id = str_of(entry, "provider");
            let label = str_or(entry, "label", id).to_owned();
            let kind = self.selection_kind(entry, ctx);
            let separated = !options.is_empty();
            if kind != "models" {
                let description = if kind == "loading" { t("providerSelection.loading") } else { self.selection_error(entry) };
                options.push(SelectOption::new(format!("{id}\u{0}"), label).description(description).enabled(false).separated(separated));
                continue;
            }
            for (index, row) in agent_form::selectable_models(entry).iter().enumerate() {
                options.push(
                    SelectOption::new(format!("{id}\u{0}{}", str_of(row, "id")), controls_model::model_label(row))
                        .description(format!("{label} · {}", str_of(row, "id")))
                        .separated(index == 0 && separated),
                );
            }
        }
        let empty = if self.catalog_loading {
            t("modelSelector.loadingSelector")
        } else if let Some(error) = &self.catalog_error {
            error.clone()
        } else {
            t("providerSelection.readiness.noProviders")
        };
        let selected = format!("{}\u{0}{}", self.provider.clone().unwrap_or_default(), self.model.trim());
        SelectRequest::new(t("providerSelection.selectModel"), options)
            .selected(Some(&selected))
            .searchable(t("modelSelector.searchAllPlaceholder"))
            .empty(empty)
    }

    fn open_select(&self, cx: &mut Cx, token: &str, request: SelectRequest) {
        self.view.wa_select(id!(select)).open(cx, token, request);
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        if !self.open {
            return;
        }
        let view = self.root();
        let modal = view.wa_modal(id!(modal));
        if view.wa_button(id!(cancel)).clicked(actions) || view.wa_button(id!(sheet.header.close)).clicked(actions) || !modal.is_open() {
            if self.saving {
                modal.open(cx);
            } else {
                self.open = false;
                modal.close(cx);
            }
            return;
        }
        if let Some(text) = view.wa_input(id!(name)).changed(actions) {
            self.name = text;
        }
        let prompt = view.wa_input(id!(prompt));
        if let Some(text) = prompt.changed(actions) {
            self.prompt = text;
            self.view.redraw(cx);
        }
        if prompt.returned(actions).is_some() {
            self.prompt = insert_newline(cx, &prompt);
            self.view.redraw(cx);
        }
        if let Some(text) = view.wa_input(id!(cron)).changed(actions) {
            self.cron = text;
            self.cadence_changed = true;
            self.view.redraw(cx);
        }
        if let Some(text) = view.wa_input(id!(max_runs)).changed(actions) {
            self.max_runs = text;
        }
        if let Some(active) = view.pswitch(id!(archive)).changed(actions) {
            self.archive = active;
        }
        if view.wa_button(id!(save)).clicked(actions) {
            self.save(ctx);
            self.view.redraw(cx);
        }
        if view.wa_button(id!(host)).clicked(actions) && !self.editing {
            let options = host::hosts(&ctx.state).iter().map(|h| SelectOption::new(h.id(), h.label()).leading(Leading::Status(h.status()))).collect();
            self.open_select(cx, "host", SelectRequest::new(l("Host"), options).selected(self.server_id.as_deref()).empty(l("No hosts found")));
        }
        if view.wa_button(id!(project)).clicked(actions) && self.server_id.is_some() {
            let mut seen = Vec::new();
            let mut options = Vec::new();
            for project in self.host(ctx).map(|h| h.projects()).unwrap_or(&[]) {
                let path = str_of(project, "projectRootPath").trim().to_owned();
                if path.is_empty() || seen.contains(&path) {
                    continue;
                }
                seen.push(path.clone());
                let label = str_or(project, "projectDisplayName", &model::shorten_path(&path)).to_owned();
                options.push(SelectOption::new(path, label).glyph("folder"));
            }
            let selected = self.project(ctx).map(|_| self.cwd.trim().to_owned());
            let request = SelectRequest::new(l("Select project"), options)
                .selected(selected.as_deref())
                .searchable(l("Search projects..."))
                .empty(l("No projects found"));
            self.open_select(cx, "project", request);
        }
        if view.wa_button(id!(model)).clicked(actions) && self.server_id.is_some() {
            // `refetchIfStale` on open.
            if !self.catalog_loading && (self.catalog_error.is_some() || self.entries.is_empty()) {
                self.load_catalog(ctx);
            }
            self.open_select(cx, "model", self.model_request(ctx));
        }
        if view.wa_button(id!(thinking)).clicked(actions) {
            let extra = t("agentControls.thinking.extraHigh");
            let options: Vec<SelectOption> = self
                .thinking_options()
                .iter()
                .map(|o| SelectOption::new(str_of(o, "id"), controls_model::format_thinking_option_label(o, &extra)).glyph("brain"))
                .collect();
            let mut request = SelectRequest::new(l("Select thinking"), options.clone()).selected(Some(self.thinking.trim())).empty(l("No thinking options found"));
            if options.len() > 6 {
                request = request.searchable("");
            }
            self.open_select(cx, "thinking", request);
        }
        if view.wa_button(id!(mode)).clicked(actions) && !self.mode_options().is_empty() {
            let options: Vec<SelectOption> = self
                .mode_options()
                .iter()
                .map(|m| SelectOption::new(str_of(m, "id"), str_or(m, "label", str_of(m, "id"))))
                .collect();
            let mut request = SelectRequest::new(l("Select mode"), options.clone()).selected(Some(self.mode.trim())).empty(l("No modes found"));
            if options.len() > 6 {
                request = request.searchable("");
            }
            self.open_select(cx, "mode", request);
        }
        if view.wa_button(id!(isolation)).clicked(actions) {
            let options = vec![SelectOption::new("local", l("Local")).glyph("folder"), SelectOption::new("worktree", l("Worktree")).glyph("git-branch")];
            self.open_select(cx, "isolation", SelectRequest::new(l("Isolation"), options).selected(Some(self.effective_isolation(ctx))));
        }
        if view.wa_button(id!(cadence_preset)).clicked(actions) {
            let options = model::CADENCE_PRESETS.iter().map(|(id, label, _)| SelectOption::new(*id, l(label))).collect();
            let preset = model::CADENCE_PRESETS.iter().find(|p| p.2 == self.cron.trim()).map(|p| p.0);
            self.open_select(cx, "cadence", SelectRequest::new(l("Cadence"), options).selected(preset));
        }
        if let Some((token, value)) = view.wa_select(id!(select)).picked(actions) {
            match token.as_str() {
                "host" => self.set_host(&value),
                "project" => self.set_project(ctx, &value),
                "model" => {
                    if let Some((provider, model_id)) = value.split_once('\u{0}') {
                        self.set_model(provider, model_id);
                    }
                }
                "thinking" => self.set_thinking(&value),
                "mode" => self.mode = value,
                "isolation" => self.isolation = value,
                "cadence" => {
                    if let Some(preset) = model::CADENCE_PRESETS.iter().find(|p| p.0 == value) {
                        self.cron = preset.2.to_owned();
                        self.cadence_changed = true;
                        sync_input(cx, &view.wa_input(id!(cron)), &self.cron);
                    }
                }
                _ => {}
            }
            self.view.redraw(cx);
        }
    }

    fn present(&mut self, cx: &mut Cx, ctx: &UiContext) {
        if !self.open {
            return;
        }
        let view = self.root();
        let title = if !self.editing {
            l("New schedule")
        } else if self.heartbeat {
            l("Edit heartbeat")
        } else {
            l("Edit schedule")
        };
        view.widget(id!(sheet.header.title)).set_text(cx, title);
        let permission = self.host(ctx).is_some() && !self.can_manage(ctx);
        set_optional(cx, &view.widget(id!(permission)), if permission { l("This connection does not have automation.manage permission.") } else { "" });
        let field = |path: &[LiveId], label: &str, visible: bool, cx: &mut Cx| {
            let widget = view.widget(path);
            widget.set_visible(cx, visible);
            if visible {
                widget.widget(id!(label)).set_text(cx, label);
            }
            widget
        };
        let heartbeat = self.heartbeat;
        // Read-only heartbeat target.
        let target = field(id!(target_field), l("Target"), heartbeat, cx);
        if heartbeat {
            let agent_id = self.schedule.as_ref().map(|s| str_of(object_of(s, "target"), "agentId").to_owned()).unwrap_or_default();
            let label = match self.host(ctx).and_then(|h| h.agent(&agent_id)) {
                None => l("Agent unavailable").to_owned(),
                Some(agent) if str_of(agent, "title").trim().is_empty() => l("Untitled agent").to_owned(),
                Some(agent) => str_of(agent, "title").trim().to_owned(),
            };
            target.widget(id!(target_box.text)).set_text(cx, &label);
        }
        field(id!(name_field), l("Name"), !heartbeat, cx);
        view.wa_input(id!(name)).set_empty_text(cx, l("Optional").into());
        field(id!(prompt_field), l("Prompt"), !heartbeat, cx);
        view.wa_input(id!(prompt)).set_empty_text(cx, l("What should the agent do each run?").into());
        let hosts = host::hosts(&ctx.state);
        let show_host = !heartbeat && (self.editing || hosts.len() > 1);
        field(id!(host_field), l("Host"), show_host, cx);
        if show_host {
            let label = self.host(ctx).map(|h| h.label()).or(self.server_id.clone());
            let button = view.wa_button(id!(host));
            set_field(cx, &button, label.as_deref(), l("Select host"));
            button.set_enabled(cx, !self.editing);
        }
        let show_project = !heartbeat && (self.editing || self.server_id.is_some());
        let project_field = field(id!(project_field), l("Project"), show_project, cx);
        if show_project {
            let display = match self.project(ctx) {
                Some(project) => Some(str_or(project, "projectDisplayName", &model::shorten_path(self.cwd.trim())).to_owned()),
                None if self.has_project() => Some(model::shorten_path(self.cwd.trim())),
                None => None,
            };
            let button = view.wa_button(id!(project));
            set_field(cx, &button, display.as_deref(), l("Select project"));
            button.set_enabled(cx, self.server_id.is_some());
            set_optional(cx, &project_field.widget(id!(hint)), if self.server_id.is_none() { l("Choose a host first.") } else { "" });
        }
        let has_project = !heartbeat && self.has_project();
        field(id!(model_field), l("Model"), has_project, cx);
        if has_project {
            let label = if self.model.trim().is_empty() {
                self.selected_model_label(ctx)
            } else {
                self.effective_model(self.provider.as_deref(), &self.model).map(|m| controls_model::model_label(&m)).unwrap_or_else(|| self.model.trim().to_owned())
            };
            let button = view.wa_button(id!(model));
            button.set_text(cx, &label);
            button.set_color(cx, self.model.trim().is_empty().then(|| crate::theme::current_palette().muted));
            button.set_enabled(cx, self.server_id.is_some());
            // The trigger leads with the provider glyph laid over its leading edge.
            let provider = self.provider.clone().filter(|p| !p.trim().is_empty());
            view.widget(id!(model_box.provider_box)).set_visible(cx, provider.is_some());
            button.apply_over(cx, live! { lead_space: (if provider.is_some() { 24.0 } else { 0.0 }) });
            if let Some(provider) = &provider {
                view.provider_icon(id!(model_box.provider)).set_provider(cx, provider, None);
            }
        }
        let thinking_options = self.thinking_options();
        let show_thinking = has_project && self.provider.is_some() && !self.model.trim().is_empty() && !thinking_options.is_empty();
        field(id!(thinking_field), l("Thinking"), show_thinking, cx);
        if show_thinking {
            let extra = t("agentControls.thinking.extraHigh");
            let current = thinking_options.iter().find(|o| str_of(o, "id") == self.thinking.trim()).cloned().unwrap_or_else(|| json!({ "id": self.thinking.trim() }));
            let label = (!self.thinking.trim().is_empty()).then(|| controls_model::format_thinking_option_label(&current, &extra));
            set_field(cx, &view.wa_button(id!(thinking)), label.as_deref(), l("Select thinking"));
        }
        let modes = self.mode_options();
        let show_mode = has_project && self.provider.is_some() && !modes.is_empty();
        let mode_field = field(id!(mode_field), l("Mode"), show_mode, cx);
        if show_mode {
            let label = (!self.mode.trim().is_empty()).then(|| {
                modes.iter().find(|m| str_of(m, "id") == self.mode.trim()).map(|m| str_or(m, "label", str_of(m, "id")).to_owned()).unwrap_or_else(|| self.mode.trim().to_owned())
            });
            set_field(cx, &view.wa_button(id!(mode)), label.as_deref(), l("Default mode"));
            set_optional(cx, &mode_field.widget(id!(hint)), "");
        }
        let show_isolation = has_project && self.can_use_worktree(ctx);
        field(id!(isolation_field), l("Isolation"), show_isolation, cx);
        if show_isolation {
            let worktree = self.effective_isolation(ctx) == "worktree";
            let isolation = view.wa_button(id!(isolation));
            isolation.set_text(cx, if worktree { l("Worktree") } else { l("Local") });
            isolation.set_icon(cx, if worktree { "git-branch" } else { "folder" });
        }
        let show_archive = has_project && self.multiplicity(ctx);
        field(id!(archive_field), l("Archive on finish"), show_archive, cx);
        view.pswitch(id!(archive)).set_value(cx, self.archive);
        let cadence = field(id!(cadence_field), l("Cadence"), true, cx);
        let expression = self.cron.trim().to_owned();
        let preset = model::CADENCE_PRESETS.iter().find(|p| p.2 == expression);
        view.wa_button(id!(cadence_preset)).set_text(cx, preset.map(|p| l(p.1)).unwrap_or(l("Custom cron")));
        let (note, error) = match self.cadence_error() {
            Some(error) => (error, true),
            None if expression.is_empty() => (String::new(), false),
            None => (model::describe_cron(&expression, Some(&self.timezone)).unwrap_or(expression), false),
        };
        set_optional(cx, &cadence.widget(id!(cadence_note)), &note);
        let p = crate::theme::current_palette();
        cadence.plabel(id!(cadence_note.text)).set_color(cx, Some(if error { p.field_error } else { p.muted }));
        field(id!(max_runs_field), l("Max runs"), !heartbeat, cx);
        view.wa_input(id!(max_runs)).set_empty_text(cx, l("Unlimited").into());
        set_optional(cx, &view.widget(id!(body.error)), self.error.as_deref().unwrap_or(""));
        let cancel = view.wa_button(id!(cancel));
        cancel.set_text(cx, l("Cancel"));
        cancel.set_enabled(cx, !self.saving);
        let save = view.wa_button(id!(save));
        save.set_text(cx, if self.editing { l("Save changes") } else { l("Create schedule") });
        save.set_busy(cx, self.saving);
        save.set_enabled(cx, self.can_submit(ctx));
    }
}

impl Widget for WaScheduleEditor {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let uid = self.widget_uid();
        let path = scope.path.clone();
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, uid, &path, ctx);
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event && let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.present(cx, ctx);
        }
        self.view.draw_walk(cx, scope, walk)
    }
}

impl WaScheduleEditorRef {
    /// Opens the sheet for a new schedule (`None`) or the given row (tagged `_serverId`).
    pub fn open(&self, cx: &mut Cx, hosts: &[String], schedule: Option<Value>) {
        let Some(mut inner) = self.borrow_mut() else { return };
        let editing = schedule.is_some();
        let source = schedule.clone().unwrap_or(Value::Null);
        let target = object_of(&source, "target");
        let saved = object_of(target, "config");
        let cadence = object_of(&source, "cadence").clone();
        let saved_zone = if str_of(&cadence, "type") == "cron" { str_of(&cadence, "timezone").to_owned() } else { String::new() };
        let provider = str_of(saved, "provider");
        let next = &mut *inner;
        next.open = true;
        next.editing = editing;
        next.heartbeat = editing && str_of(target, "type") == "agent";
        next.provider = (!provider.is_empty()).then(|| provider.to_owned());
        next.model = str_of(saved, "model").to_owned();
        next.mode = str_of(saved, "modeId").to_owned();
        next.thinking = str_of(saved, "thinkingOptionId").to_owned();
        next.thinking_drafts.clear();
        if let Some(provider) = &next.provider && !next.model.is_empty() && !next.thinking.is_empty() {
            next.thinking_drafts.insert(format!("{provider}:{}", next.model), next.thinking.clone());
        }
        next.server_id = if editing {
            Some(str_of(&source, "_serverId").to_owned())
        } else if hosts.len() == 1 {
            Some(hosts[0].clone())
        } else {
            None
        };
        next.cwd = str_of(saved, "cwd").to_owned();
        next.archive = saved.get("archiveOnFinish") != Some(&Value::Bool(false));
        next.isolation = if str_of(saved, "isolation") == "worktree" { "worktree".into() } else { "local".into() };
        next.saved_zone = !saved_zone.is_empty();
        next.timezone = if saved_zone.is_empty() { timezone::device_time_zone() } else { saved_zone };
        next.name = str_of(&source, "name").to_owned();
        next.prompt = str_of(&source, "prompt").to_owned();
        let hourly = json!({ "type": "every", "everyMs": 3_600_000 });
        next.cron = model::cron_expression_for(if editing { &cadence } else { &hourly });
        next.max_runs = source.get("maxRuns").filter(|v| !v.is_null()).map(|v| v.to_string()).unwrap_or_default();
        next.entries.clear();
        next.catalog_error = None;
        next.catalog_loading = false;
        next.catalog_scope = None;
        next.cadence_changed = false;
        next.saving = false;
        next.error = None;
        next.load_catalog_pending = !next.heartbeat && next.server_id.is_some() && !next.cwd.trim().is_empty();
        next.catalog_loading = next.load_catalog_pending;
        let view = inner.root();
        sync_input(cx, &view.wa_input(id!(name)), &inner.name);
        sync_input(cx, &view.wa_input(id!(prompt)), &inner.prompt);
        sync_input(cx, &view.wa_input(id!(cron)), &inner.cron);
        sync_input(cx, &view.wa_input(id!(max_runs)), &inner.max_runs);
        view.wa_modal(id!(modal)).open(cx);
        view.redraw(cx);
    }

    pub fn saved(&self, actions: &Actions) -> bool {
        actions.filter_widget_actions(self.widget_uid()).any(|a| matches!(a.cast(), ScheduleEditorAction::Saved))
    }
}
