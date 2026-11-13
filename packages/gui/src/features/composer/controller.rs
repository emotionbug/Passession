//! Per-panel composer logic (the non-rendering half of Flutter `_AgentPanelState` for the
//! composer): target/draft identity, provider catalog and agent configuration, submission,
//! queue actions, delivery checks, attachments, autocomplete and forge auto-attach. The
//! widget in `ui/panel.rs` owns one controller per mounted instance.

use super::agent_form::*;
use super::attachments::*;
use super::autocomplete::*;
use super::drafts::*;
use super::forge::*;
use super::i18n::{t, t_args};
use super::json::*;
use super::service::{self, PanelNotice, SendJob, SendOrigin};
use crate::common::{UiContext, UiEffect};
use paseo_gui_core::{CoreError, CoreEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ComposerKind {
    #[default]
    Agent,
    Draft,
    ProviderSubagent,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ComposerTarget {
    pub kind: ComposerKind,
    pub host_id: String,
    pub workspace_id: String,
    pub cwd: String,
    /// Agent id, or the draft tab id for `Draft`.
    pub id: String,
    pub tab_key: String,
    pub data: Value,
}

impl ComposerTarget {
    pub fn draft_key(&self) -> String {
        let id = if self.id.is_empty() { "draft" } else { &self.id };
        draft_key(&self.host_id, &self.workspace_id, id)
    }
}

#[derive(Clone, Debug, Default)]
pub struct AgentSnapshot {
    pub agent: Arc<Value>,
    pub running: bool,
    pub execution_authorized: bool,
    pub has_pending_permissions: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerCommand {
    Focus,
    Send,
    CycleMode,
    Interrupt,
}

/// What each panel-issued request was for.
#[derive(Clone, Debug)]
pub enum Tag {
    DaemonConfig,
    Providers,
    Commands { generation: u64 },
    Directory { generation: u64, query: String },
    DraftFeatures { generation: u64, config: Value },
    CheckoutStatus,
    CheckoutForge,
    ForgeAuto { lookup: u64, index: usize, cwd: String },
    ForgePicker { generation: u64 },
    AgentConfig { kind: String },
    Cancel,
    Archive { fresh: bool, config: Value },
    Upload,
    Usage { request: String, refresh: Option<Value> },
}

/// Platform requests (clipboard, pickers, open URL).
#[derive(Clone, Debug)]
pub enum PlatformTag {
    Paste { images_only: bool, silent: bool },
    Pick { images: bool },
    OpenUrl,
}

/// UI effects the widget applies after a controller call.
#[derive(Default)]
pub struct Outputs {
    /// Replace the editor text and put the caret at this byte offset.
    pub set_editor: Option<(String, usize)>,
    /// Insert text at the caret.
    pub insert: Option<String>,
    pub focus: bool,
    pub prompt_submitted: Option<String>,
    pub reload_agent: Option<String>,
    pub start_file_debounce: bool,
    pub start_forge_debounce: Option<bool>,
    pub redraw: bool,
}

pub struct Controller {
    pub panel: u64,
    pub target: ComposerTarget,
    pub key: String,
    pub agent: AgentSnapshot,
    pub initialized: bool,
    pub selected: bool,
    pub daemon_config: Value,
    pub providers: Vec<Value>,
    pub providers_loaded: bool,
    pub profiles: Vec<Value>,
    pub commands: Vec<Value>,
    pub draft_features: Vec<Value>,
    pub command_generation: u64,
    pub feature_generation: u64,
    pub remote_url: Option<String>,
    pub forge: String,
    forge_loading: bool,
    pub autocomplete: ComposerAutocomplete,
    pub forge_auto: ForgeAutoAttach,
    /// Transient failure (Flutter `error` / `_showFailure`).
    pub error: Option<String>,
    pub not_ready_notice: bool,
    pub checking: bool,
    pub cancelling: bool,
    /// The name of the file uploading now (Flutter `uploadName`), shown on one pending pill.
    pub uploading: Vec<String>,
    upload_request: Option<String>,
    /// Files of the selection still to upload, the files done so far and the draft they
    /// attach to.
    upload_queue: Vec<(std::path::PathBuf, String, String)>,
    uploaded: Vec<Value>,
    upload_key: String,
    pub requests: HashMap<String, Tag>,
    pub platform_requests: HashMap<u64, PlatformTag>,
    /// Blob tokens → whether they came from a drop (images join the tray either way).
    pub blob_tokens: HashMap<u64, bool>,
    /// The editor text the controller last synced (Flutter `_syncingEditor` guard).
    pub editor_text: String,
    pub editor_generation_seen: Option<i64>,
    pub fullscreen_open: bool,
    pub usage_reports: Vec<Value>,
    pub usage_settled: bool,
    pub usage_error: Option<String>,
    pub usage_refreshing: Vec<Value>,
    pub usage_refresh_failed: Vec<Value>,
    pub forge_picker_items: Vec<Value>,
    pub forge_picker_error: Option<String>,
    pub forge_picker_fetching: bool,
    forge_picker_generation: u64,
    pub forge_picker_query: String,
    providers_request_pending: bool,
    pending_forge_searches: Vec<ForgeSearch>,
    /// A send-now that first needed an exact receipt check (Flutter awaited it).
    pending_queue_now: Option<Value>,
}

fn behavior_pref(ui: &UiContext) -> String {
    let saved = ui.preference("sendBehavior").and_then(Value::as_str).unwrap_or("steer");
    if matches!(saved, "interrupt" | "steer" | "queue") { saved.to_owned() } else { "steer".to_owned() }
}

fn scope(cwd: &str) -> String {
    cwd.trim().trim_end_matches('/').to_owned()
}

fn core_error_text(error: &CoreError) -> String {
    error.message.clone()
}

impl Controller {
    pub fn new(panel: u64) -> Self {
        Self {
            panel,
            target: ComposerTarget::default(),
            key: String::new(),
            agent: AgentSnapshot::default(),
            initialized: false,
            selected: true,
            daemon_config: json!({}),
            providers: Vec::new(),
            providers_loaded: false,
            profiles: Vec::new(),
            commands: Vec::new(),
            draft_features: Vec::new(),
            command_generation: 0,
            feature_generation: 0,
            remote_url: None,
            forge: "github".into(),
            forge_loading: false,
            autocomplete: ComposerAutocomplete::new(),
            forge_auto: ForgeAutoAttach::default(),
            error: None,
            not_ready_notice: false,
            checking: false,
            cancelling: false,
            uploading: Vec::new(),
            upload_request: None,
            upload_queue: Vec::new(),
            uploaded: Vec::new(),
            upload_key: String::new(),
            requests: HashMap::new(),
            platform_requests: HashMap::new(),
            blob_tokens: HashMap::new(),
            editor_text: String::new(),
            editor_generation_seen: None,
            fullscreen_open: false,
            usage_reports: Vec::new(),
            usage_settled: false,
            usage_error: None,
            usage_refreshing: Vec::new(),
            usage_refresh_failed: Vec::new(),
            forge_picker_items: Vec::new(),
            forge_picker_error: None,
            forge_picker_fetching: false,
            forge_picker_generation: 0,
            forge_picker_query: String::new(),
            providers_request_pending: false,
            pending_forge_searches: Vec::new(),
            pending_queue_now: None,
        }
    }

    pub fn is_draft(&self) -> bool {
        self.target.kind == ComposerKind::Draft
    }

    pub fn is_subagent(&self) -> bool {
        self.target.kind == ComposerKind::ProviderSubagent
    }

    pub fn is_sheet(&self) -> bool {
        s(get(&self.target.data, "paneHost")) == "sheet"
    }

    pub fn host(&self) -> &str {
        &self.target.host_id
    }

    pub fn connected(&self) -> bool {
        service::with(|svc| svc.connected(&self.target.host_id))
    }

    pub fn server_info(&self) -> Value {
        service::with(|svc| svc.server_info(&self.target.host_id))
    }

    pub fn supports(&self, feature: &str) -> bool {
        service::with(|svc| svc.supports(&self.target.host_id, feature))
    }

    pub fn draft(&self) -> Value {
        service::with(|svc| svc.drafts.draft(&self.key).clone())
    }

    pub fn sending(&self) -> bool {
        service::with(|svc| svc.is_sending(&self.key))
    }

    pub fn active_send(&self) -> Option<String> {
        service::with(|svc| svc.active_send(&self.key).map(str::to_owned))
    }

    pub fn running(&self) -> bool {
        !self.is_draft() && self.agent.running
    }

    /// Saved history stays readable while the provider prepares; sending waits for a live,
    /// ready agent.
    pub fn execution_blocked(&self) -> bool {
        !self.is_draft() && !self.agent.execution_authorized
    }

    /// input/state.ts resolveActiveSendBehavior.
    pub fn behavior(&self, ui: &UiContext) -> String {
        let value = behavior_pref(ui);
        if value == "queue" && self.agent.has_pending_permissions {
            "interrupt".into()
        } else {
            value
        }
    }

    pub fn form_preferences(&self, ui: &UiContext) -> Value {
        parse_form_preferences(ui.preference(FORM_PREFERENCES_KEY).unwrap_or(&Value::Null))
    }

    fn remember_preferences(&self, ui: &mut UiContext, next: Value) {
        ui.push(UiEffect::SetPreference { key: FORM_PREFERENCES_KEY.into(), value: next });
    }

    fn request(&mut self, ui: &mut UiContext, message: Value, response: &str, tag: Tag) -> String {
        let id = ui.core.request(&self.target.host_id, message, response);
        self.requests.insert(id.clone(), tag);
        id
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    fn update_draft(&self, fields: Value) {
        service::with(|svc| {
            svc.drafts.update(&self.key, fields);
            svc.after_mutation();
        });
    }

    // ----- lifecycle -----

    pub fn set_target(&mut self, ui: &mut UiContext, target: ComposerTarget) -> Outputs {
        let mut out = Outputs::default();
        if target == self.target {
            return out;
        }
        let same_identity = target.host_id == self.target.host_id
            && target.workspace_id == self.target.workspace_id
            && target.id == self.target.id
            && target.kind == self.target.kind;
        let intent_changed = get(&target.data, "attachmentIntentId") != get(&self.target.data, "attachmentIntentId");
        let provider_scope_changed = target.host_id != self.target.host_id || scope(&target.cwd) != scope(&self.target.cwd);
        if provider_scope_changed {
            self.requests.retain(|_, tag| !matches!(tag, Tag::Providers));
            self.providers_request_pending = false;
            self.providers.clear();
            self.providers_loaded = false;
        }
        self.target = target;
        self.key = self.target.draft_key();
        if same_identity {
            if intent_changed {
                self.target_attachments();
            }
            if provider_scope_changed && self.initialized {
                self.reload_providers(ui);
            }
            return out;
        }
        // An upload belongs to the previous draft; stop it rather than attach elsewhere.
        if let Some(id) = self.upload_request.take() {
            ui.core.cancel_request(&id);
            self.requests.remove(&id);
            self.uploading.clear();
            self.upload_queue.clear();
            self.uploaded.clear();
        }
        self.initialized = false;
        self.providers.clear();
        self.providers_loaded = false;
        self.commands.clear();
        self.draft_features.clear();
        self.autocomplete = ComposerAutocomplete::new();
        self.autocomplete.is_draft = self.is_draft();
        self.autocomplete.can_load_files = !self.target.cwd.trim().is_empty();
        self.forge_auto = ForgeAutoAttach::default();
        self.remote_url = None;
        self.editor_generation_seen = None;
        self.error = None;
        self.initialize(ui, &mut out);
        out
    }

    /// Flutter `_initialize`.
    fn initialize(&mut self, ui: &mut UiContext, out: &mut Outputs) {
        let loaded = service::with(|svc| svc.drafts.is_loaded());
        if !loaded {
            return;
        }
        let draft = self.draft();
        if self.is_draft() && get(&draft, "draft") != &Value::Bool(true) {
            self.update_draft(json!({"draft": true}));
        }
        if !non_empty_obj(get(&draft, "config")) && get(&self.target.data, "config").is_object() {
            self.update_draft(json!({"config": get(&self.target.data, "config")}));
        }
        if s(get(&draft, "text")).is_empty()
            && let Some(prompt) = get(&self.target.data, "initialPrompt").as_str()
        {
            self.update_draft(json!({"text": prompt}));
        }
        self.target_attachments();
        self.take_attachments();
        self.initialized = true;
        self.sync_editor(out);
        if let Some(error) = get(&self.target.data, "submissionError").as_str().map(str::to_owned) {
            self.fail(error);
        }
        if self.target.host_id.is_empty() {
            return;
        }
        self.request(ui, json!({"type": "get_daemon_config_request"}), "get_daemon_config_response", Tag::DaemonConfig);
        self.reload_providers(ui);
        self.load_commands(ui);
        self.load_remote_url(ui);
    }

    /// Called when drafts finish loading after the panel mounted.
    pub fn ensure_initialized(&mut self, ui: &mut UiContext) -> Outputs {
        let mut out = Outputs::default();
        if !self.initialized && !self.key.is_empty() {
            self.initialize(ui, &mut out);
        }
        out
    }

    fn target_attachments(&mut self) {
        let attachments = maps(get(&self.target.data, "attachments"));
        if attachments.is_empty() {
            return;
        }
        let intent = get(&self.target.data, "attachmentIntentId")
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:{}", self.target.tab_key, serde_json::to_string(&attachments).unwrap_or_default()));
        service::with(|svc| {
            svc.drafts.add_draft_attachments(&self.key, attachments, Some(&intent));
            svc.after_mutation();
        });
    }

    pub fn take_attachments(&mut self) {
        if self.is_subagent() || !self.selected {
            return;
        }
        service::with(|svc| {
            svc.drafts.take_attachments(&self.target.host_id, &self.target.workspace_id, &self.key);
            svc.after_mutation();
        });
    }

    /// Flutter `_draftChanged`: the editor follows external draft changes (restore, ACK,
    /// queued edit), never its own keystrokes.
    pub fn sync_editor(&mut self, out: &mut Outputs) {
        let draft = self.draft();
        let text = owned(get(&draft, "text"));
        if text != self.editor_text {
            self.editor_text = text.clone();
            let caret = text.len();
            out.set_editor = Some((text, caret));
            out.redraw = true;
        }
        self.editor_generation_seen = get(&draft, "editorGeneration").as_i64();
    }

    /// Flutter `_editorChanged`: the user's own edit.
    pub fn editor_changed(&mut self, text: &str, cursor: usize) -> Outputs {
        let mut out = Outputs::default();
        if self.autocomplete.update(text, cursor).is_some() {
            out.start_file_debounce = true;
        }
        if text != self.editor_text {
            self.editor_text = text.to_owned();
            self.update_draft(json!({"text": text}));
        }
        self.sync_forge_auto_attach(&mut out);
        out.redraw = true;
        out
    }

    pub fn cursor_moved(&mut self, cursor: usize) -> Outputs {
        let mut out = Outputs::default();
        let text = self.editor_text.clone();
        if self.autocomplete.update(&text, cursor).is_some() {
            out.start_file_debounce = true;
        }
        out.redraw = true;
        out
    }

    // ----- configuration -----

    /// Flutter `_config`.
    pub fn config(&self) -> Value {
        if self.is_draft() {
            let draft = self.draft();
            return merged(get(&draft, "config"), &json!({"cwd": self.target.cwd}));
        }
        let agent = &*self.agent.agent;
        let mut out = serde_json::Map::new();
        out.insert("provider".into(), get(agent, "provider").clone());
        out.insert("cwd".into(), json!(self.target.cwd));
        if !get(agent, "model").is_null() {
            out.insert("model".into(), get(agent, "model").clone());
        }
        if !get(agent, "currentModeId").is_null() {
            out.insert("modeId".into(), get(agent, "currentModeId").clone());
        }
        if !get(agent, "thinkingOptionId").is_null() {
            out.insert("thinkingOptionId".into(), get(agent, "thinkingOptionId").clone());
        }
        let mut features = serde_json::Map::new();
        for feature in maps(get(agent, "features")) {
            features.insert(owned(get(&feature, "id")), get(&feature, "value").clone());
        }
        out.insert("featureValues".into(), Value::Object(features));
        Value::Object(out)
    }

    pub fn modes(&self) -> Vec<Value> {
        if !self.is_draft() {
            return maps(get(&self.agent.agent, "availableModes"));
        }
        let config = self.config();
        provider_entry(&self.providers, s(get(&config, "provider")))
            .map(|entry| maps(get(entry, "modes")))
            .unwrap_or_default()
    }

    pub fn features(&self) -> Vec<Value> {
        if self.is_draft() {
            self.draft_features.clone()
        } else {
            maps(get(&self.agent.agent, "features"))
        }
    }

    /// The host's agent profiles (daemon config); refetched when the model sheet opens so
    /// profiles saved in settings show up.
    pub fn reload_daemon_config(&mut self, ui: &mut UiContext) {
        if self.target.host_id.is_empty() || self.requests.values().any(|t| matches!(t, Tag::DaemonConfig)) {
            return;
        }
        self.request(ui, json!({"type": "get_daemon_config_request"}), "get_daemon_config_response", Tag::DaemonConfig);
    }

    pub fn reload_providers(&mut self, ui: &mut UiContext) {
        if self.providers_request_pending {
            return;
        }
        self.providers_request_pending = true;
        let id = service::with(|svc| svc.request_providers(&ui.core, &self.target.host_id, &self.target.cwd));
        self.requests.insert(id, Tag::Providers);
    }

    /// use-providers-snapshot.ts refetchIfStale on selector open.
    pub fn refetch_if_loading(&mut self, ui: &mut UiContext) {
        let config = self.config();
        let entry = provider_entry(&self.providers, s(get(&config, "provider")));
        if entry.is_none_or(|e| s(get(e, "status")) == "loading") {
            self.reload_providers(ui);
        }
    }

    fn set_providers(&mut self, ui: &mut UiContext, providers: Vec<Value>) {
        self.providers = providers;
        self.providers_loaded = true;
        self.resolve_draft_config(ui);
    }

    /// use-agent-form-state.ts: a draft's selection resolves once the catalog is known.
    fn resolve_draft_config(&mut self, ui: &mut UiContext) {
        if !self.is_draft() || get(&self.draft(), "configResolved") == &Value::Bool(true) {
            return;
        }
        let draft = self.draft();
        let resolved = resolve_initial_agent_config(get(&draft, "config"), &self.form_preferences(ui), &self.providers);
        self.update_draft(json!({"config": merged(&resolved, &json!({"cwd": self.target.cwd})), "configResolved": true}));
        self.refresh_draft_features(ui, None);
        self.load_commands(ui);
    }

    /// Flutter `_saveConfig`.
    pub fn save_config(&mut self, ui: &mut UiContext, value: Value) -> Outputs {
        let mut out = Outputs::default();
        let preferences = self.form_preferences(ui);
        let previous = self.config();
        if self.is_draft() {
            let mut next = value.clone();
            let provider = s(get(&value, "provider"));
            if !provider.is_empty() && (get(&value, "provider") != get(&previous, "provider") || get(&value, "model") != get(&previous, "model")) {
                next = merged(
                    &apply_user_model_choice(&previous, provider, s(get(&value, "model")), &preferences, &self.providers),
                    &json!({"cwd": self.target.cwd}),
                );
            }
            self.update_draft(json!({"config": next, "configResolved": true}));
            self.remember_preferences(ui, remember_user_selection(&preferences, &previous, &next));
            self.refresh_draft_features(ui, None);
            self.load_commands(ui);
            out.redraw = true;
            return out;
        }
        let provider = owned(get(&self.agent.agent, "provider"));
        if s(get(&value, "provider")) != provider {
            self.fail("A running session cannot change provider. Fork into a new session instead.");
            out.redraw = true;
            return out;
        }
        let mut remembered = preferences.clone();
        let mut requests: Vec<(&str, Value)> = Vec::new();
        let model = owned(get(&value, "model"));
        if get(&value, "model") != get(&previous, "model") {
            if !model.is_empty() {
                remembered = merge_provider_preferences(&remembered, &provider, ProviderPrefsUpdate { model: Some(&model), ..Default::default() });
            }
            requests.push(("set_agent_model", json!({"modelId": get(&value, "model")})));
        }
        if get(&value, "modeId") != get(&previous, "modeId") {
            let mode = owned(get(&value, "modeId"));
            remembered = merge_provider_preferences(
                &remembered,
                &provider,
                ProviderPrefsUpdate { mode: (!mode.is_empty()).then_some(mode.as_str()), ..Default::default() },
            );
            requests.push(("set_agent_mode", json!({"modeId": mode})));
        }
        if get(&value, "thinkingOptionId") != get(&previous, "thinkingOptionId") {
            let thinking = owned(get(&value, "thinkingOptionId"));
            let runtime_model = owned(get(get(&self.agent.agent, "runtimeInfo"), "model"));
            let active_model = s_or(get(&value, "model"), &runtime_model).to_owned();
            if !active_model.is_empty() {
                let mut map = serde_json::Map::new();
                map.insert(active_model.clone(), json!(thinking));
                remembered = merge_provider_preferences(
                    &remembered,
                    &provider,
                    ProviderPrefsUpdate { model: Some(&active_model), thinking_by_model: Some(map), ..Default::default() },
                );
            }
            requests.push(("set_agent_thinking", json!({"thinkingOptionId": get(&value, "thinkingOptionId")})));
        }
        let before = obj(get(&previous, "featureValues"));
        if let Some(after) = get(&value, "featureValues").as_object() {
            for (id, feature_value) in after {
                if before.get(id) == Some(feature_value) {
                    continue;
                }
                let mut map = serde_json::Map::new();
                map.insert(id.clone(), feature_value.clone());
                remembered = merge_provider_preferences(&remembered, &provider, ProviderPrefsUpdate { feature_values: Some(map), ..Default::default() });
                requests.push(("set_agent_feature", json!({"featureId": id, "value": feature_value})));
            }
        }
        if requests.is_empty() {
            return out;
        }
        self.remember_preferences(ui, remembered);
        for (kind, fields) in requests {
            let message = merged(&fields, &json!({"type": format!("{kind}_request"), "agentId": self.target.id}));
            self.request(ui, message, &format!("{kind}_response"), Tag::AgentConfig { kind: kind.into() });
        }
        out.redraw = true;
        out
    }

    /// Flutter `_applyProfile`.
    pub fn apply_profile(&mut self, ui: &mut UiContext, profile: &Value) -> Outputs {
        let mut out = Outputs::default();
        let preferences = self.form_preferences(ui);
        let mut materialized = serde_json::Map::new();
        for key in ["provider", "model", "modeId", "thinkingOptionId"] {
            materialized.insert(key.into(), json!(s(get(profile, key)).trim()));
        }
        materialized.insert("featureValues".into(), Value::Object(obj(get(profile, "featureValues"))));
        let materialized = Value::Object(materialized);
        if self.is_draft() {
            let previous = self.config();
            let next = merged(&apply_profile_choice(&materialized, &preferences, &self.providers), &json!({"cwd": self.target.cwd}));
            self.update_draft(json!({"config": next, "configResolved": true}));
            self.remember_preferences(
                ui,
                remember_profile_selection(&preferences, s(get(&previous, "provider")), &self.providers, &next, get(&materialized, "featureValues")),
            );
            self.refresh_draft_features(ui, None);
            self.load_commands(ui);
            out.redraw = true;
            return out;
        }
        let provider = owned(get(&self.agent.agent, "provider"));
        let Some(entry) = provider_entry(&self.providers, &provider).cloned() else { return out };
        if s(get(&materialized, "provider")) != provider {
            return out;
        }
        let mode_ids: Vec<String> = maps(get(&entry, "modes")).iter().map(|m| owned(get(m, "id"))).collect();
        let model = owned(get(&materialized, "model"));
        let mut mode = owned(get(&materialized, "modeId"));
        if !mode.is_empty() && !mode_ids.contains(&mode) {
            mode.clear();
        }
        let thinking = owned(get(&materialized, "thinkingOptionId"));
        let features = obj(get(&materialized, "featureValues"));
        let mut thinking_map = serde_json::Map::new();
        if !model.is_empty() && !thinking.is_empty() {
            thinking_map.insert(model.clone(), json!(thinking));
        }
        self.remember_preferences(
            ui,
            merge_provider_preferences(
                &preferences,
                &provider,
                ProviderPrefsUpdate {
                    model: (!model.is_empty()).then_some(model.as_str()),
                    clear_mode: mode.is_empty(),
                    mode: (!mode.is_empty()).then_some(mode.as_str()),
                    thinking_by_model: (!thinking_map.is_empty()).then_some(thinking_map),
                    feature_values: (!features.is_empty()).then(|| features.clone()),
                },
            ),
        );
        let mut config = serde_json::Map::new();
        if !model.is_empty() {
            config.insert("modelId".into(), json!(model));
        }
        if !mode.is_empty() {
            config.insert("modeId".into(), json!(mode));
        }
        if !thinking.is_empty() {
            config.insert("thinkingOptionId".into(), json!(thinking));
        }
        if !features.is_empty() {
            config.insert("featureValues".into(), Value::Object(features));
        }
        self.request(
            ui,
            json!({"type": "agent.config.apply.request", "agentId": self.target.id, "config": config}),
            "agent.config.apply.response",
            Tag::AgentConfig { kind: "agent.config.apply".into() },
        );
        out.redraw = true;
        out
    }

    /// Flutter `_cycleMode` (message-input.mode-cycle).
    pub fn cycle_mode(&mut self, ui: &mut UiContext) -> Outputs {
        let modes = self.modes();
        if modes.is_empty() {
            self.fail(t("composer.errors.noModes"));
            return Outputs { redraw: true, ..Default::default() };
        }
        let config = self.config();
        let index = modes.iter().position(|m| get(m, "id") == get(&config, "modeId"));
        let next = &modes[index.map(|i| (i + 1) % modes.len()).unwrap_or(0)];
        self.save_config(ui, merged(&config, &json!({"modeId": get(next, "id")})))
    }

    /// use-draft-agent-features.ts: features for the draft's effective selection.
    pub fn refresh_draft_features(&mut self, ui: &mut UiContext, config: Option<Value>) {
        let value = config.unwrap_or_else(|| self.config());
        let provider = owned(get(&value, "provider"));
        if provider.is_empty() || !self.connected() {
            self.draft_features.clear();
            return;
        }
        let mut effective = effective_submit_config(&value, &self.providers);
        remove(&mut effective, "featureValues");
        self.feature_generation += 1;
        let generation = self.feature_generation;
        self.request(
            ui,
            json!({"type": "list_provider_features_request", "draftConfig": merged(&effective, &json!({"cwd": self.target.cwd}))}),
            "list_provider_features_response",
            Tag::DraftFeatures { generation, config: value },
        );
    }

    /// use-agent-commands-query.
    pub fn load_commands(&mut self, ui: &mut UiContext) {
        self.command_generation += 1;
        if self.is_subagent() || self.target.host_id.is_empty() {
            return;
        }
        let config = self.config();
        let empty_provider = s(get(&config, "provider")).is_empty();
        self.autocomplete.is_draft = self.is_draft();
        self.autocomplete.draft_empty_key =
            (self.is_draft() && empty_provider).then_some("agentAutocomplete.chooseModelForCommands");
        if self.is_draft() && empty_provider {
            self.autocomplete.set_commands(Vec::new(), false, None);
            return;
        }
        let commands = self.commands.clone();
        let loading = commands.is_empty();
        self.autocomplete.set_commands(commands, loading, None);
        let generation = self.command_generation;
        let mut message = json!({"type": "list_commands_request", "agentId": if self.is_draft() { "draft".to_owned() } else { self.target.id.clone() }});
        if self.is_draft() {
            set(&mut message, "draftConfig", config);
        }
        self.request(ui, message, "list_commands_response", Tag::Commands { generation });
    }

    /// The 180ms debounce for the @mention lookup elapsed.
    pub fn fetch_files(&mut self, ui: &mut UiContext) {
        let Some(query) = self.autocomplete.pending_file_query.clone() else { return };
        if !self.autocomplete.can_load_files {
            return;
        }
        let generation = self.autocomplete.begin_file_fetch();
        self.request(
            ui,
            json!({"type": "directory_suggestions_request", "cwd": self.target.cwd, "query": query, "limit": 50, "includeFiles": true, "includeDirectories": true}),
            "directory_suggestions_response",
            Tag::Directory { generation, query },
        );
    }

    /// An autocomplete row was chosen.
    pub fn apply_autocomplete(&mut self, ui: &mut UiContext, option: &AutocompleteOption) -> Outputs {
        let draft = self.draft();
        let has_attachments = non_empty_list(get(&draft, "attachments")) || non_empty_list(get(&draft, "images"));
        if option.client_command && !has_attachments && !self.is_draft() {
            return self.client_command(ui, &format!("/{}", option.id));
        }
        let mut out = Outputs::default();
        if let Some(next) = self.autocomplete.applied(option) {
            let caret = next.len();
            out.set_editor = Some((next.clone(), caret));
            self.editor_text = next.clone();
            self.update_draft(json!({"text": next}));
            let _ = self.autocomplete.update(&next, caret);
            out.focus = true;
            out.redraw = true;
        }
        out
    }

    // ----- forge -----

    fn load_remote_url(&mut self, ui: &mut UiContext) {
        let cwd = self.target.cwd.trim().to_owned();
        if self.is_subagent() || cwd.is_empty() || !self.connected() {
            return;
        }
        self.request(ui, json!({"type": "checkout_status_request", "cwd": cwd}), "checkout_status_response", Tag::CheckoutStatus);
    }

    pub fn ensure_forge(&mut self, ui: &mut UiContext) {
        if self.forge_loading || self.target.cwd.trim().is_empty() || !self.connected() {
            return;
        }
        self.forge_loading = true;
        self.request(
            ui,
            json!({"type": "checkout_pr_status_request", "cwd": self.target.cwd}),
            "checkout_pr_status_response",
            Tag::CheckoutForge,
        );
    }

    fn sync_forge_auto_attach(&mut self, out: &mut Outputs) {
        if self.is_subagent() || !self.initialized {
            return;
        }
        let draft = self.draft();
        let effects = self.forge_auto.update(ForgeInput {
            text: self.editor_text.clone(),
            remote_url: self.remote_url.clone(),
            attachments: maps(get(&draft, "attachments")),
            connected: self.connected(),
            server_id: self.target.host_id.clone(),
            cwd: self.target.cwd.clone(),
        });
        self.apply_forge_effects(effects, out);
    }

    fn apply_forge_effects(&mut self, effects: ForgeEffects, out: &mut Outputs) {
        if effects.start_timer {
            out.start_forge_debounce = Some(true);
        } else if effects.cancel_timer {
            out.start_forge_debounce = Some(false);
        }
        for item in effects.attach {
            self.auto_attach_forge(&item);
            out.redraw = true;
        }
        self.pending_forge_searches.extend(effects.searches);
    }

    /// forge-auto-attach.ts drainOutcomes → toggleForgeAttachment.
    fn auto_attach_forge(&mut self, item: &Value) {
        let current = maps(get(&self.draft(), "attachments"));
        let key = forge_item_key(item);
        if current.iter().any(|a| forge_attachment_key(a).as_deref() == Some(key.as_str())) {
            return;
        }
        let legacy = uses_legacy_forge(&self.server_info());
        let mut next = current;
        next.push(forge_attachment(item, legacy));
        self.update_draft(json!({"attachments": next}));
    }

    pub fn forge_timer_fired(&mut self, ui: &mut UiContext) -> Outputs {
        let mut out = Outputs::default();
        let effects = self.forge_auto.timer_fired();
        self.apply_forge_effects(effects, &mut out);
        self.issue_forge_searches(ui);
        out
    }

    fn issue_forge_searches(&mut self, ui: &mut UiContext) {
        let legacy = uses_legacy_forge(&self.server_info());
        for search in std::mem::take(&mut self.pending_forge_searches) {
            let (message, response) = forge_search_request(legacy, &search.cwd, &search.query);
            self.request(ui, message, response, Tag::ForgeAuto { lookup: search.lookup, index: search.index, cwd: search.cwd });
        }
    }

    /// The forge picker searched (250ms debounce owned by the overlay).
    pub fn search_forge_picker(&mut self, ui: &mut UiContext, query: &str) {
        if self.target.cwd.trim().is_empty() {
            self.forge_picker_items.clear();
            self.forge_picker_fetching = false;
            return;
        }
        self.forge_picker_generation += 1;
        self.forge_picker_query = query.to_owned();
        self.forge_picker_fetching = true;
        self.forge_picker_error = None;
        let legacy = uses_legacy_forge(&self.server_info());
        let (message, response) = forge_search_request(legacy, &self.target.cwd, query);
        let generation = self.forge_picker_generation;
        self.request(ui, message, response, Tag::ForgePicker { generation });
    }

    /// handleToggleGithubItem: choosing an attached item removes it, otherwise attaches.
    pub fn toggle_forge_item(&mut self, item: &Value) -> Outputs {
        let key = forge_item_key(item);
        let mut current = maps(get(&self.draft(), "attachments"));
        let index = current.iter().position(|a| forge_labels(a).is_some() && forge_item_key(a) == key);
        match index {
            Some(index) => {
                let removed = current.remove(index);
                self.forge_auto.mark_removed(&removed);
            }
            None => current.push(forge_attachment(item, uses_legacy_forge(&self.server_info()))),
        }
        self.update_draft(json!({"attachments": current}));
        Outputs { focus: true, redraw: true, ..Default::default() }
    }

    pub fn attached_forge_keys(&self) -> Vec<String> {
        maps(get(&self.draft(), "attachments"))
            .iter()
            .filter(|a| forge_labels(a).is_some())
            .map(forge_item_key)
            .collect()
    }

    // ----- attachments -----

    pub fn remove_image(&mut self, index: usize) {
        let mut images = maps(get(&self.draft(), "images"));
        if index < images.len() {
            images.remove(index);
            self.update_draft(json!({"images": images}));
        }
    }

    pub fn remove_attachment(&mut self, index: usize) {
        let mut attachments = maps(get(&self.draft(), "attachments"));
        if index < attachments.len() {
            // markForgeAttachmentRemoved: a removed reference never re-attaches.
            let removed = attachments.remove(index);
            self.forge_auto.mark_removed(&removed);
            self.update_draft(json!({"attachments": attachments}));
        }
    }

    pub fn add_images(&mut self, images: Vec<Value>) {
        if images.is_empty() {
            return;
        }
        let mut current = maps(get(&self.draft(), "images"));
        current.extend(images);
        self.update_draft(json!({"images": current}));
    }

    /// Flutter `_paste`: clipboard images become image pills; text pastes as text. `silent`
    /// is the Ctrl/Cmd+V path, where the platform already inserted any text natively.
    pub fn paste(&mut self, images_only: bool, silent: bool) {
        if let Some(platform) = paseo_gui_platform::platform() {
            let id = platform.read_clipboard(images_only && !silent);
            self.platform_requests.insert(id, PlatformTag::Paste { images_only, silent });
        }
    }

    pub fn pick_files(&mut self, images: bool) {
        if let Some(platform) = paseo_gui_platform::platform() {
            let id = platform.pick_files(paseo_gui_platform::PickFilesOptions {
                multiple: true,
                images_only: images,
                title: None,
            });
            self.platform_requests.insert(id, PlatformTag::Pick { images });
        }
    }

    pub fn open_url(&mut self, url: &str) {
        let valid = url.starts_with("https://") || url.starts_with("http://");
        if !valid {
            self.fail(t("composer.errors.attachmentUrl"));
            return;
        }
        if let Some(platform) = paseo_gui_platform::platform() {
            let id = platform.open_external(url);
            self.platform_requests.insert(id, PlatformTag::OpenUrl);
        }
    }

    /// composer/index.tsx `useFileDrop(…, { disabled: isSubmitLoadingVisible })`: files
    /// cannot be dropped while a message is submitting (the submit path clears and restores
    /// the attachments) or while files are uploading, counting a drop that is still being
    /// inspected. The host's drop zone refuses the drag then.
    pub fn accepts_file_drop(&self) -> bool {
        self.initialized
            && !self.is_subagent()
            && !self.sending()
            && !self.uploading()
            && !self.blob_tokens.values().any(|dropped| *dropped)
    }

    /// Dropped paths: raster images join the tray, everything else uploads (useFileDrop).
    pub fn drop_paths(&mut self, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() || !self.accepts_file_drop() {
            return;
        }
        let token = service::with(|svc| svc.stat_paths(self.panel, paths));
        self.blob_tokens.insert(token, true);
    }

    /// Picked files: all images (image picker) or all uploads (file picker).
    pub fn picked_files(&mut self, ui: &mut UiContext, files: Vec<paseo_gui_platform::PickedFile>, images: bool) {
        if files.is_empty() {
            return;
        }
        if images {
            let list: Vec<(std::path::PathBuf, String)> = files.into_iter().map(|f| (f.path, f.name)).collect();
            let token = service::with(|svc| svc.import_images(self.panel, list));
            self.blob_tokens.insert(token, false);
            return;
        }
        let stat: Vec<super::blobs::StatFile> = files
            .into_iter()
            .map(|f| super::blobs::StatFile { image_mime: None, size: f.size, name: f.name, path: f.path })
            .collect();
        self.upload(ui, stat);
    }

    pub fn files_stat(&mut self, ui: &mut UiContext, files: Vec<super::blobs::StatFile>) {
        let (images, uploads): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| f.image_mime.is_some());
        if !images.is_empty() {
            let list = images.into_iter().map(|f| (f.path, f.name)).collect();
            let token = service::with(|svc| svc.import_images(self.panel, list));
            self.blob_tokens.insert(token, true);
        }
        if !uploads.is_empty() {
            self.upload(ui, uploads);
        }
    }

    /// actions.ts uploadFileAttachments.
    fn upload(&mut self, ui: &mut UiContext, files: Vec<super::blobs::StatFile>) {
        if let Err(too_large) = check_upload_sizes(files.iter().map(|f| (f.name.as_str(), f.size))) {
            ui.toast(t_args("composer.errors.fileTooLarge", &[("size", "50MB"), ("fileName", &too_large.file_name)]), true);
            return;
        }
        if !self.connected() {
            ui.toast(t("composer.errors.hostDisconnected"), true);
            return;
        }
        self.upload_key = self.key.clone();
        self.uploaded.clear();
        self.upload_queue = files
            .into_iter()
            .map(|f| {
                let mime = mime_type_for_path(&f.name).to_owned();
                (f.path, f.name, mime)
            })
            .collect();
        self.upload_next(ui);
    }

    /// Upload the next file of the selection (Flutter `_uploadFiles`: one file at a time,
    /// its name on the pending pill).
    fn upload_next(&mut self, ui: &mut UiContext) {
        if self.upload_queue.is_empty() {
            return;
        }
        let job = self.upload_queue.remove(0);
        self.uploading = vec![job.1.clone()];
        let id = ui.core.operation(&self.target.host_id, move |client| super::ops::upload_files(client, vec![job]));
        self.upload_request = Some(id.clone());
        self.requests.insert(id, Tag::Upload);
    }

    pub fn uploading(&self) -> bool {
        !self.uploading.is_empty()
    }

    // ----- submission -----

    /// Flutter `_submit`. `retry`: resend this exact failed/unknown message.
    pub fn submit(&mut self, ui: &mut UiContext, retry: Option<Value>, override_behavior: Option<&str>) -> Outputs {
        let mut out = Outputs { redraw: true, ..Default::default() };
        if self.sending() || self.uploading() || self.is_subagent() {
            return out;
        }
        if self.execution_blocked() {
            // Nothing is sent or cleared: the draft stays for a ready agent.
            self.not_ready_notice = true;
            return out;
        }
        let draft = self.draft();
        let empty = self.editor_text.trim().is_empty()
            && !non_empty_list(get(&draft, "attachments"))
            && !non_empty_list(get(&draft, "images"));
        if retry.is_none() && empty {
            return out;
        }
        if retry.is_none() && (non_empty_obj(get(&draft, "pending")) || non_empty_obj(get(&draft, "failedMessage"))) {
            self.fail(t("composer.delivery.blocked"));
            return out;
        }
        if let Some(retry) = &retry {
            let receipt = get(get(&draft, "receipts"), s(get(retry, "messageId"))).clone();
            if !self.is_draft() && self.supports("agentMessageReceipts") && !receipt_can_retry(&receipt) {
                return out;
            }
        }
        if self.is_draft() {
            let config = self.config();
            let connected = self.connected();
            if let Some(blocker) = draft_submission_blocker(
                &config,
                &self.providers,
                connected,
                !self.providers_loaded && connected,
            ) {
                self.update_draft(json!({"sendError": t(blocker), "sendErrorMessageId": null}));
                return out;
            }
        }
        if retry.is_none() && !self.is_draft() {
            let has_attachments = non_empty_list(get(&draft, "attachments")) || non_empty_list(get(&draft, "images"));
            if let Some(command) = resolve_client_slash_command(&self.editor_text, has_attachments) {
                return self.client_command(ui, &format!("/{command}"));
            }
        }
        let choice = override_behavior.map(str::to_owned).unwrap_or_else(|| self.behavior(ui));
        let message = match retry.clone() {
            Some(message) => message,
            None => {
                // The snapshot is of the persisted draft; make sure it holds the latest text.
                service::with(|svc| svc.drafts.update(&self.key, json!({"text": self.editor_text})));
                let snapshot = service::with(|svc| svc.drafts.snapshot(&self.key));
                merged(&snapshot, &json!({"activeTurnBehavior": if choice == "steer" { "steer" } else { "interrupt" }}))
            }
        };
        if !self.is_draft() && self.agent.running && choice == "queue" && retry.is_none() {
            let mut queue = maps(get(&draft, "queue"));
            queue.push(merged(&message, &json!({"state": "queued", "createdAt": super::ops::now_rfc3339()})));
            service::with(|svc| {
                svc.drafts.update(&self.key, json!({"queue": queue}));
                svc.drafts.acknowledge(&self.key, &message);
                svc.after_mutation();
            });
            self.sync_editor(&mut out);
            return out;
        }
        self.error = None;
        self.not_ready_notice = false;
        // Submitted content stays with its exact request, separate from later edits.
        let mut fields = json!({
            "pending": merged(&message, &json!({"createdAt": super::ops::now_rfc3339()})),
            "sendError": null,
            "sendErrorMessageId": null,
        });
        if retry.is_some() {
            set(&mut fields, "failedMessage", Value::Null);
        } else {
            set(&mut fields, "text", json!(""));
            set(&mut fields, "attachments", json!([]));
            set(&mut fields, "images", json!([]));
        }
        service::with(|svc| svc.drafts.update(&self.key, fields));
        if self.is_draft() {
            self.create(ui, message);
        } else {
            let job = SendJob {
                host: self.target.host_id.clone(),
                key: self.key.clone(),
                agent_id: self.target.id.clone(),
                message,
                origin: SendOrigin::Pending,
                panel: Some(self.panel),
            };
            service::with(|svc| svc.start_send(job));
        }
        self.sync_editor(&mut out);
        out
    }

    /// Flutter `_create`.
    fn create(&mut self, ui: &mut UiContext, message: Value) {
        let mut config = effective_submit_config(&self.config(), &self.providers);
        let provider = owned(get(&config, "provider"));
        let preferences = self.form_preferences(ui);
        let local = get(&config, "featureValues").clone();
        remove(&mut config, "featureValues");
        let feature_values = resolve_feature_values(&self.draft_features, &remembered_feature_values(&preferences, &provider), &local);
        if non_empty_obj(&feature_values) {
            set(&mut config, "featureValues", feature_values);
        }
        if provider.is_empty() {
            service::with(|svc| {
                svc.drafts.fail_send(&self.key, &message, &t("composer.errors.selectProvider"), true);
                svc.after_mutation();
            });
            return;
        }
        self.remember_preferences(ui, remember_submitted_selection(&preferences, &config, &self.providers));
        let title = t("sessionNavigation.newSession");
        let host = self.target.host_id.clone();
        let key = self.key.clone();
        let cwd = self.target.cwd.clone();
        let workspace = self.target.workspace_id.clone();
        let panel = self.panel;
        service::with(|svc| svc.start_creation(&host, &key, Some(panel), message, config, crate::features::composer::creation::CreationTarget { cwd: &cwd, workspace_id: &workspace, title: &title }));
    }

    /// input/state.ts runAlternateSendAction.
    pub fn alternate_send(&mut self, ui: &mut UiContext) -> Outputs {
        if self.behavior(ui) == "queue" {
            self.submit(ui, None, Some("interrupt"))
        } else {
            self.submit(ui, None, Some("queue"))
        }
    }

    /// Flutter `_queueNow` (send-now on a queued row).
    pub fn queue_now(&mut self, ui: &mut UiContext, message: &Value) -> Outputs {
        let out = Outputs { redraw: true, ..Default::default() };
        if self.sending() {
            return out;
        }
        if self.execution_blocked() {
            self.not_ready_notice = true;
            return out;
        }
        let id = owned(get(message, "messageId"));
        if service::with(|svc| svc.drafts.is_dismissed(&self.key, &id)) {
            return out;
        }
        let attempted = get(message, "deliveryAttempted") == &Value::Bool(true)
            || matches!(s(get(message, "state")), "sending" | "unacknowledged" | "rejected");
        if attempted && self.supports("agentMessageReceipts") {
            let receipt = get(get(&self.draft(), "receipts"), &id).clone();
            if !receipt_can_retry(&receipt) {
                // Check the exact outcome first; continue only when the host says the
                // message was not delivered and may be retried.
                self.pending_queue_now = Some(message.clone());
                self.check_delivery(message, false);
                return out;
            }
        }
        let mut queue = maps(get(&self.draft(), "queue"));
        let Some(index) = queue.iter().position(|q| s(get(q, "messageId")) == id) else { return out };
        if s(get(&queue[index], "state")) == "sending" {
            return out;
        }
        // index.tsx handleSendQueuedNow: steer with the steer preference, else interrupt.
        let behavior = if self.behavior(ui) == "steer" { "steer" } else { "interrupt" };
        let sending = queued_message_for_delivery(&queue[index], behavior);
        queue[index] = merged(&sending, &json!({"state": "sending"}));
        service::with(|svc| svc.drafts.update(&self.key, json!({"queue": queue})));
        let job = SendJob {
            host: self.target.host_id.clone(),
            key: self.key.clone(),
            agent_id: self.target.id.clone(),
            message: sending,
            origin: SendOrigin::Queue { drain: false },
            panel: Some(self.panel),
        };
        service::with(|svc| svc.start_send(job));
        out
    }

    /// actions.ts editQueuedComposerMessage.
    pub fn edit_queued(&mut self, message: &Value) -> Outputs {
        let mut out = Outputs { redraw: true, focus: true, ..Default::default() };
        let mut queue = maps(get(&self.draft(), "queue"));
        let Some(index) = queue.iter().position(|q| get(q, "messageId") == get(message, "messageId")) else { return out };
        if !matches!(s(get(&queue[index], "state")), "queued" | "blocked" | "rejected") {
            return out;
        }
        let item = queue.remove(index);
        self.update_draft(json!({
            "queue": queue,
            "text": get(&item, "text"),
            "attachments": list(get(&item, "attachments")),
            "images": list(get(&item, "images")),
        }));
        self.sync_editor(&mut out);
        out
    }

    /// Remove a queue row: attempted rows go through the host's dismissal.
    pub fn remove_queued(&mut self, message: &Value) -> bool {
        let attempted = get(message, "deliveryAttempted") == &Value::Bool(true)
            || matches!(s(get(message, "state")), "sending" | "unacknowledged" | "rejected");
        if attempted {
            // Needs the stop-waiting confirmation first.
            return true;
        }
        let id = owned(get(message, "messageId"));
        service::with(|svc| {
            svc.drafts.dismiss(&self.key, &id);
            svc.after_mutation();
        });
        false
    }

    /// Flutter `_checkDelivery` after confirmation (when dismissing).
    pub fn check_delivery(&mut self, message: &Value, dismiss: bool) {
        if self.checking {
            return;
        }
        let id = owned(get(message, "messageId"));
        let remote = !self.is_draft() && self.supports("agentMessageReceipts");
        if remote {
            self.checking = true;
            let host = self.target.host_id.clone();
            let key = self.key.clone();
            let panel = self.panel;
            service::with(|svc| svc.check_receipt(&host, &key, &id, dismiss, Some(panel)));
        } else if dismiss {
            service::with(|svc| {
                svc.drafts.dismiss(&self.key, &id);
                svc.after_mutation();
            });
        }
    }

    pub fn restore_failed(&mut self) -> Outputs {
        let mut out = Outputs { redraw: true, ..Default::default() };
        let draft = self.draft();
        let failed = get(&draft, "failedMessage").clone();
        self.update_draft(json!({
            "text": get(&failed, "text"),
            "attachments": get(&failed, "attachments"),
            "images": get(&failed, "images"),
            "failedMessage": null,
            "sendError": null,
            "sendErrorMessageId": null,
        }));
        self.sync_editor(&mut out);
        out
    }

    pub fn discard_failed(&mut self) {
        self.update_draft(json!({"failedMessage": null, "sendError": null, "sendErrorMessageId": null}));
    }

    /// Flutter `_clientCommand` (/exit, /clear).
    pub fn client_command(&mut self, ui: &mut UiContext, command: &str) -> Outputs {
        let mut out = Outputs { redraw: true, ..Default::default() };
        let fresh = command == "/clear" || command == "/new";
        let config = self.config();
        self.update_draft(json!({"text": "", "attachments": [], "images": []}));
        self.sync_editor(&mut out);
        self.not_ready_notice = false;
        self.request(
            ui,
            json!({"type": "archive_agent_request", "agentId": self.target.id}),
            "agent_archived",
            Tag::Archive { fresh, config },
        );
        out
    }

    /// actions.ts cancelComposerAgent.
    pub fn cancel_agent(&mut self, ui: &mut UiContext) -> Outputs {
        let out = Outputs { redraw: true, focus: true, ..Default::default() };
        if self.is_draft() || !self.agent.running || self.cancelling || !self.connected() {
            return out;
        }
        self.cancelling = true;
        self.request(ui, json!({"type": "cancel_agent_request", "agentId": self.target.id}), "cancel_agent_response", Tag::Cancel);
        out
    }

    /// Agent usage reports for the context window details (usage/queries.ts).
    pub fn load_usage(&mut self, ui: &mut UiContext) {
        if !(self.connected() && self.supports("usageSources")) {
            return;
        }
        let request = format!("rust-agent-usage-{}", uuid::Uuid::new_v4());
        self.usage_settled = false;
        self.usage_error = None;
        let message = json!({"type": "usage.list_reports.request", "requestId": request, "agentId": self.target.id, "forceRefresh": false});
        let id = ui.core.request(&self.target.host_id, message, "usage.list_reports.response");
        self.requests.insert(id, Tag::Usage { request, refresh: None });
    }

    pub fn refresh_usage(&mut self, ui: &mut UiContext, report_id: &Value) {
        self.usage_refreshing.push(report_id.clone());
        self.usage_refresh_failed.retain(|r| r != report_id);
        let request = format!("rust-agent-usage-refresh-{}", uuid::Uuid::new_v4());
        let message = json!({"type": "usage.list_reports.request", "requestId": request, "agentId": self.target.id, "forceRefresh": true});
        let id = ui.core.request(&self.target.host_id, message, "usage.list_reports.response");
        self.requests.insert(id, Tag::Usage { request, refresh: Some(report_id.clone()) });
    }

    fn upsert_report(&mut self, report: Value) {
        let id = get(&report, "id").clone();
        match self.usage_reports.iter().position(|r| get(r, "id") == &id) {
            None => self.usage_reports.push(report),
            Some(index) => {
                if s(get(&report, "fetchedAt")) >= s(get(&self.usage_reports[index], "fetchedAt")) {
                    self.usage_reports[index] = report;
                }
            }
        }
    }

    // ----- event processing -----

    /// Apply this panel's responses and host events from the current core batch.
    pub fn handle_batch(&mut self, ui: &mut UiContext) -> Outputs {
        let mut out = Outputs::default();
        let mut responses: Vec<(Tag, Result<Value, CoreError>)> = Vec::new();
        let mut host_events: Vec<Value> = Vec::new();
        let mut reconnected = false;
        if ui.events.iter().any(|event| matches!(event,
            CoreEvent::Connected { host_id, .. } | CoreEvent::Disconnected { host_id, .. }
                if host_id == &self.target.host_id))
        {
            self.requests.retain(|_, tag| !matches!(tag, Tag::Providers));
            self.providers_request_pending = false;
        }
        for event in ui.events.iter() {
            match event {
                CoreEvent::Response { request_id, result, .. } => {
                    if let Some(tag) = self.requests.remove(request_id) {
                        responses.push((tag, result.clone()));
                    }
                }
                CoreEvent::HostEvent { host_id, event }
                    if host_id == &self.target.host_id && matches!(s(get(event, "type")),
                        "providers_snapshot_update" | "usage.list_reports.update") =>
                {
                    host_events.push(event.clone());
                }
                CoreEvent::Connected { host_id, .. } if host_id == &self.target.host_id => {
                    reconnected = true;
                    out.redraw = true;
                }
                CoreEvent::Disconnected { host_id, .. } if host_id == &self.target.host_id => out.redraw = true,
                _ => {}
            }
        }
        if reconnected {
            if self.initialized && !self.providers_loaded {
                self.reload_providers(ui);
            }
            if self.remote_url.is_none() {
                self.load_remote_url(ui);
            }
        }
        for (tag, result) in responses {
            self.response(ui, tag, result, &mut out);
        }
        for event in host_events {
            self.host_event(ui, &event, &mut out);
        }
        out
    }

    fn host_event(&mut self, ui: &mut UiContext, event: &Value, out: &mut Outputs) {
        let kind = s(get(event, "type"));
        let payload = get(event, "payload");
        if kind == "providers_snapshot_update" {
            if scope(s(get(payload, "cwd"))) != scope(&self.target.cwd) {
                return;
            }
            if payload.get("entries").is_some() || payload.get("compactSnapshot").is_some() {
                let providers = provider_entries(payload);
                self.set_providers(ui, providers);
                out.redraw = true;
                return;
            }
            self.reload_providers(ui);
        } else if kind == "usage.list_reports.update" {
            let request = s(get(payload, "requestId"));
            let ours = self.requests.values().any(|tag| matches!(tag, Tag::Usage { request: r, .. } if r == request));
            if ours {
                self.upsert_report(get(payload, "report").clone());
                out.redraw = true;
            }
        }
    }

    fn response(&mut self, ui: &mut UiContext, tag: Tag, result: Result<Value, CoreError>, out: &mut Outputs) {
        out.redraw = true;
        let request_type = match &tag {
            Tag::DaemonConfig => "get_daemon_config_request",
            Tag::Providers => "get_providers_snapshot_request",
            Tag::Commands { .. } => "list_commands_request",
            Tag::Directory { .. } => "directory_suggestions_request",
            Tag::DraftFeatures { .. } => "list_provider_features_request",
            Tag::CheckoutStatus => "checkout_status_request",
            Tag::CheckoutForge => "checkout_pr_status_request",
            Tag::ForgeAuto { .. } | Tag::ForgePicker { .. } => "forge.search.request",
            Tag::AgentConfig { .. } => "agent.config.apply.request",
            Tag::Cancel => "cancel_agent_request",
            Tag::Archive { .. } => "archive_agent_request",
            Tag::Upload => "",
            Tag::Usage { .. } => "usage.list_reports.request",
        };
        // Domain rejections arrive as Ok payloads (core-api.md); requireSuccess them.
        let checked: Result<Value, String> = match result {
            Ok(payload) if matches!(tag, Tag::Upload) => Ok(payload),
            Ok(payload) if matches!(tag, Tag::CheckoutStatus | Tag::CheckoutForge) => Ok(payload),
            Ok(payload) => match paseo_gui_core::response_error(&payload, request_type) {
                Some(error) => Err(error.message),
                None => Ok(payload),
            },
            Err(error) => Err(core_error_text(&error)),
        };
        match tag {
            Tag::DaemonConfig => match checked {
                Ok(payload) => {
                    self.daemon_config = get(&payload, "config").clone();
                    self.profiles = maps(get(&self.daemon_config, "agentProfiles"));
                }
                Err(error) => self.fail(error),
            },
            Tag::Providers => {
                self.providers_request_pending = false;
                match checked {
                    Ok(payload) => {
                        let providers = provider_entries(&payload);
                        self.set_providers(ui, providers);
                        if self.is_draft() {
                            self.refresh_draft_features(ui, None);
                        }
                    }
                    Err(error) => self.fail(error),
                }
            }
            Tag::Commands { generation } => {
                if generation != self.command_generation {
                    return;
                }
                match checked {
                    Ok(payload) => {
                        self.commands = maps(get(&payload, "commands"));
                        self.autocomplete.set_commands(self.commands.clone(), false, None);
                    }
                    Err(error) => self.autocomplete.set_commands(self.commands.clone(), false, Some(error)),
                }
            }
            Tag::Directory { generation, query } => {
                let result = checked.map(|payload| {
                    let entries = maps(get(&payload, "entries"));
                    if !entries.is_empty() {
                        entries
                    } else {
                        list_ref(get(&payload, "directories"))
                            .iter()
                            .map(|path| json!({"path": display(path), "kind": "directory"}))
                            .collect()
                    }
                });
                self.autocomplete.files_loaded(generation, &query, result);
            }
            Tag::DraftFeatures { generation, config } => {
                if generation != self.feature_generation {
                    return;
                }
                match checked {
                    Ok(payload) => {
                        let offered = maps(get(&payload, "features"));
                        let provider = owned(get(&config, "provider"));
                        let values = resolve_feature_values(
                            &offered,
                            &remembered_feature_values(&self.form_preferences(ui), &provider),
                            get(&config, "featureValues"),
                        );
                        self.draft_features = apply_feature_values(&offered, &values);
                    }
                    Err(error) => self.fail(error),
                }
            }
            Tag::CheckoutStatus => {
                if let Ok(payload) = checked {
                    let url = owned(get(&payload, "remoteUrl"));
                    self.remote_url = (!url.is_empty()).then_some(url);
                    self.sync_forge_auto_attach(out);
                }
                // Without a remote nothing auto-attaches (extractForgeRefs).
            }
            Tag::CheckoutForge => match checked {
                // A payload carrying a domain error still names its forge.
                Ok(payload) => {
                    let forge = s(get(&payload, "forge"));
                    self.forge = if forge.is_empty() { "github".into() } else { forge.into() };
                }
                Err(_) => self.forge_loading = false,
            },
            Tag::ForgeAuto { lookup, index, cwd } => {
                let legacy = uses_legacy_forge(&self.server_info());
                let items = checked.map(|payload| forge_search_items(legacy, &payload));
                let effects = self.forge_auto.search_finished(lookup, index, &cwd, items);
                self.apply_forge_effects(effects, out);
                self.issue_forge_searches(ui);
            }
            Tag::ForgePicker { generation } => {
                if generation != self.forge_picker_generation {
                    return;
                }
                self.forge_picker_fetching = false;
                let legacy = uses_legacy_forge(&self.server_info());
                match checked {
                    Ok(payload) => self.forge_picker_items = forge_search_items(legacy, &payload),
                    Err(error) => self.forge_picker_error = Some(error),
                }
            }
            Tag::AgentConfig { .. } => {
                if let Err(error) = checked {
                    ui.toast(error, true);
                }
                if !self.requests.values().any(|t| matches!(t, Tag::AgentConfig { .. })) {
                    out.reload_agent = Some(self.target.id.clone());
                    self.load_commands(ui);
                }
            }
            Tag::Cancel => {
                self.cancelling = false;
                if let Err(error) = checked
                    && !error.trim().is_empty()
                {
                    ui.toast(error, true);
                }
            }
            Tag::Archive { fresh, config } => match checked {
                Ok(_) => {
                    if fresh {
                        let draft_id = uuid::Uuid::new_v4().to_string();
                        ui.push(UiEffect::OpenDraft {
                            host_id: self.target.host_id.clone(),
                            workspace_id: self.target.workspace_id.clone(),
                            cwd: self.target.cwd.clone(),
                            draft_id,
                            title: t("sessionNavigation.newSession"),
                            data: json!({"config": config}),
                        });
                    }
                    ui.push(UiEffect::CloseTab { tab_key: self.target.tab_key.clone() });
                }
                Err(error) => {
                    self.update_draft(json!({"sendError": error, "sendErrorMessageId": null}));
                }
            },
            Tag::Upload => {
                self.upload_request = None;
                let outcome = match checked {
                    Ok(envelope) => super::ops::open_envelope(&envelope).map_err(|e| owned(get(&e, "message"))),
                    Err(error) => Err(error),
                };
                match outcome {
                    Ok(payload) => {
                        self.uploaded.extend(maps(get(&payload, "files")));
                        if !self.upload_queue.is_empty() {
                            self.upload_next(ui);
                            return;
                        }
                        // The uploaded files attach together once all succeed, to the
                        // draft the upload started from.
                        self.uploading.clear();
                        let uploaded = std::mem::take(&mut self.uploaded);
                        let key = std::mem::take(&mut self.upload_key);
                        service::with(|svc| {
                            let mut current = maps(get(svc.drafts.draft(&key), "attachments"));
                            current.extend(uploaded);
                            svc.drafts.update(&key, json!({"attachments": current}));
                            svc.after_mutation();
                        });
                    }
                    Err(error) => {
                        self.uploading.clear();
                        self.upload_queue.clear();
                        self.uploaded.clear();
                        let message = if error.trim().is_empty() { t("composer.errors.uploadFailed") } else { error };
                        ui.toast(message, true);
                    }
                }
            }
            Tag::Usage { refresh: None, .. } => match checked {
                Ok(payload) => {
                    if get(&payload, "complete") == &Value::Bool(false) {
                        self.usage_error = Some(t("composer.errors.usagePartial"));
                        return;
                    }
                    if let Some(finished) = payload.get("reports").and_then(Value::as_array) {
                        // model.ts settleReports: keep shown order, drop missing, append new.
                        let mut by_id: Vec<Value> = finished.clone();
                        let mut settled = Vec::new();
                        for shown in &self.usage_reports {
                            if let Some(index) = by_id.iter().position(|r| get(r, "id") == get(shown, "id")) {
                                let arriving = by_id.remove(index);
                                settled.push(if s(get(&arriving, "fetchedAt")) >= s(get(shown, "fetchedAt")) { arriving } else { shown.clone() });
                            }
                        }
                        settled.extend(by_id);
                        self.usage_reports = settled;
                    }
                    self.usage_settled = true;
                }
                Err(error) => self.usage_error = Some(error),
            },
            Tag::Usage { refresh: Some(report_id), .. } => {
                self.usage_refreshing.retain(|r| r != &report_id);
                match checked {
                    Ok(payload) if get(&payload, "complete") != &Value::Bool(false) => {
                        let fresh = list_ref(get(&payload, "reports")).iter().find(|r| get(r, "id") == &report_id).cloned();
                        match fresh {
                            Some(report) => {
                                if let Some(index) = self.usage_reports.iter().position(|r| get(r, "id") == &report_id) {
                                    self.usage_reports[index] = report;
                                }
                            }
                            None => self.usage_reports.retain(|r| get(r, "id") != &report_id),
                        }
                    }
                    _ => self.usage_refresh_failed.push(report_id),
                }
            }
        }
    }

    /// Service notices for this panel.
    pub fn handle_notices(&mut self, ui: &mut UiContext, notices: Vec<PanelNotice>) -> Outputs {
        let mut out = Outputs { redraw: !notices.is_empty(), ..Default::default() };
        for notice in notices {
            match notice {
                PanelNotice::SendFinished { error, .. } => {
                    if error.is_some() {
                        self.error = None;
                    }
                }
                PanelNotice::PromptSubmitted { agent_id } => {
                    if agent_id == self.target.id {
                        out.prompt_submitted = Some(agent_id);
                    }
                }
                PanelNotice::ImagesStored { token, images } => {
                    if self.blob_tokens.remove(&token).is_some() {
                        match images {
                            Ok(images) => self.add_images(images),
                            Err(error) => {
                                let message = if error.trim().is_empty() { t("composer.errors.uploadFailed") } else { error };
                                ui.toast(message, true);
                            }
                        }
                    }
                }
                PanelNotice::FilesStat { token, files } => {
                    if self.blob_tokens.remove(&token).is_some() {
                        match files {
                            Ok(files) => self.files_stat(ui, files),
                            Err(error) => ui.toast(error, true),
                        }
                    }
                }
                PanelNotice::CheckFinished { error, message_id, .. } => {
                    self.checking = false;
                    let resume = self
                        .pending_queue_now
                        .take()
                        .filter(|pending| s(get(pending, "messageId")) == message_id);
                    match error {
                        Some(error) => self.fail(error),
                        None => {
                            if let Some(message) = resume {
                                let receipt = get(get(&self.draft(), "receipts"), &message_id).clone();
                                if receipt_can_retry(&receipt) {
                                    let next = self.queue_now(ui, &message);
                                    out.redraw |= next.redraw;
                                }
                            }
                        }
                    }
                }
            }
        }
        self.sync_editor(&mut out);
        out
    }

    /// Platform results (clipboard, pickers, URL opening) addressed to this panel.
    pub fn handle_platform(&mut self, ui: &mut UiContext, request: u64, result: Result<paseo_gui_platform::PlatformValue, paseo_gui_platform::PlatformError>) -> Outputs {
        use paseo_gui_platform::PlatformValue;
        let mut out = Outputs { redraw: true, ..Default::default() };
        let Some(tag) = self.platform_requests.remove(&request) else { return Outputs::default() };
        match (tag, result) {
            (PlatformTag::Paste { images_only, silent }, Ok(PlatformValue::Clipboard(content))) => {
                if let Some(image) = content.images.into_iter().next() {
                    let token = service::with(|svc| svc.store_image_bytes(self.panel, image.bytes.clone(), Some(image.mime_type.clone())));
                    self.blob_tokens.insert(token, false);
                } else if silent {
                    // Ctrl/Cmd+V: the platform already pasted any text into the editor.
                } else if let (Some(text), false) = (content.text, images_only) {
                    out.insert = Some(text);
                } else if images_only {
                    ui.toast(t("composer.errors.noClipboardImage"), true);
                }
            }
            (PlatformTag::Paste { images_only, silent }, Err(error)) => {
                if error.code != "cancelled" && !silent {
                    let message = if images_only { t("composer.errors.pasteImageFailed") } else { error.message };
                    ui.toast(message, true);
                }
            }
            (PlatformTag::Pick { images }, Ok(PlatformValue::Files(files))) => self.picked_files(ui, files, images),
            (PlatformTag::Pick { .. }, Err(error)) => {
                if error.code != "cancelled" {
                    let message = if error.message.trim().is_empty() { t("composer.errors.uploadFailed") } else { error.message };
                    ui.toast(message, true);
                }
            }
            (PlatformTag::OpenUrl, Err(error)) => self.fail(error.message),
            _ => {}
        }
        out
    }

    /// Whether the composer has content to send (input.tsx hasLiveText or attachments).
    pub fn has_message(&self) -> bool {
        let draft = self.draft();
        !self.editor_text.trim().is_empty()
            || non_empty_list(get(&draft, "attachments"))
            || non_empty_list(get(&draft, "images"))
    }

    /// Whether the composer may refuse closing its tab (sends, uploads, fullscreen).
    pub fn close_guard_dirty(&self) -> bool {
        self.sending() || self.uploading() || self.fullscreen_open
    }
}
