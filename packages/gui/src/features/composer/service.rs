//! The composer's module-level runtime: one per process, independent of mounted panels
//! (Flutter `ChatDrafts.instance` + `ChatDelivery.attach` per client + `CreateHandoffs` +
//! `SentUserMessages`). It owns the draft store, durable send pipelines, host delivery,
//! creation recovery, retained image blobs and shared in-flight provider snapshot requests.
//! The shell feeds it each core event batch (`handle_core_events`) and background signals
//! (`poll_background`). GUI thread only.

use super::blobs::{BlobJob, BlobResult, BlobStore, StatFile};
use super::drafts::*;
use super::i18n::t;
use super::json::*;
use super::state_keys;
use crate::common::{UiContext, UiEffect};
use makepad_widgets::Cx;
use paseo_gui_core::{CoreError, CoreEvent, CoreHandle};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

thread_local! {
    static SERVICE: RefCell<Option<ComposerService>> = const { RefCell::new(None) };
}

/// Run `f` with the process composer service. Never re-enter from inside `f`.
pub fn with<R>(f: impl FnOnce(&mut ComposerService) -> R) -> R {
    SERVICE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let service = slot.get_or_insert_with(ComposerService::new);
        f(service)
    })
}

/// What a running send belongs to.
#[derive(Clone, Debug, PartialEq)]
pub enum SendOrigin {
    /// The composer's own pending message (`draft.pending`).
    Pending,
    /// A queue entry (send-now or automatic drain).
    Queue { drain: bool },
}

#[derive(Clone, Debug)]
pub struct SendJob {
    pub host: String,
    pub key: String,
    pub agent_id: String,
    pub message: Value,
    pub origin: SendOrigin,
    /// Panel to notify (`PromptSubmitted`) once the host has the message.
    pub panel: Option<u64>,
}

/// Results the service hands to a specific panel instance.
#[derive(Clone, Debug)]
pub enum PanelNotice {
    /// A send of `message_id` finished (success or failure); clear the panel's spinner.
    SendFinished { key: String, message_id: String, error: Option<String> },
    /// The prompt reached the host: clear attention and stick the timeline to the bottom.
    PromptSubmitted { agent_id: String },
    /// Image blobs stored for a paste/pick/drop.
    ImagesStored { token: u64, images: Result<Vec<Value>, String> },
    /// Dropped/picked paths were inspected.
    FilesStat { token: u64, files: Result<Vec<StatFile>, String> },
    /// An exact receipt check finished.
    CheckFinished { key: String, message_id: String, error: Option<String> },
}

pub(super) enum Pending {
    /// `save_state_flushed`; on success continue with `then`.
    Flush { then: AfterFlush },
    /// Send operation envelope.
    Send { job: SendJob },
    /// Exact receipt query (`agent.message_receipt.get|dismiss`).
    Receipt { host: String, key: String, id: String, dismiss: bool, panel: Option<u64> },
    /// Owned session.events subscription for receipt updates.
    ReceiptObservation { host: String },
    /// Timeline subscription that wakes the queue drain for one draft key.
    TimelineObservation { host: String, key: String },
    /// fetch_agent before draining a queue.
    DrainFetch { host: String, key: String },
    /// Creation lifecycle request.
    Creation { draft_key: String, step: super::creation::CreationStep },
}

pub(super) enum AfterFlush {
    Send(SendJob),
    Creation { draft_key: String },
    Nothing { panel: Option<u64> },
}

#[derive(Default)]
pub(super) struct HostState {
    pub connected: bool,
    pub server_info: Value,
    /// Bumped on every connection change (Flutter `_lastStatus` comparison).
    pub generation: u64,
}

/// Concurrent consumers share the core response ID, not a single-consumer response
/// route. Each mounted controller/panel still receives the untouched core batch.
#[derive(Default)]
struct ProviderRequests {
    hosts: HashMap<String, HashMap<String, String>>,
}

impl ProviderRequests {
    fn request(&mut self, host: &str, cwd: &str, send: impl FnOnce() -> String) -> String {
        let cwd = cwd.trim().trim_end_matches('/');
        if let Some(id) = self.hosts.get(host).and_then(|requests| requests.get(cwd)) {
            return id.clone();
        }
        let id = send();
        self.hosts.entry(host.to_owned()).or_default().insert(cwd.to_owned(), id.clone());
        id
    }

    fn finish(&mut self, host: &str, request_id: &str) {
        if let Some(requests) = self.hosts.get_mut(host) {
            requests.retain(|_, id| id.as_str() != request_id);
            if requests.is_empty() {
                self.hosts.remove(host);
            }
        }
    }

    fn reset(&mut self, host: &str) {
        self.hosts.remove(host);
    }
}

pub struct ComposerService {
    pub(super) core: Option<CoreHandle>,
    pub drafts: DraftStore,
    pub(super) hosts: HashMap<String, HostState>,
    pub(super) pending: HashMap<String, Pending>,
    provider_requests: ProviderRequests,
    pub(super) delivery: HashMap<String, super::delivery::HostDelivery>,
    /// Draft keys with a send in flight → message id (Flutter `sending`/`_activeSendId`).
    pub(super) active_sends: HashMap<String, String>,
    pub(super) creations: HashMap<String, super::creation::DraftCreation>,
    sent: VecDeque<(String, Value)>,
    handoffs: HashSet<String>,
    handoff_revision: u64,
    pub blobs: BlobStore,
    blob_owners: HashMap<u64, u64>,
    notices: HashMap<u64, Vec<PanelNotice>>,
    pub(super) effects: Vec<UiEffect>,
    revision: u64,
    collected: bool,
    next_panel: u64,
    /// Request id of the last full flush handed to the core, for `flush_drafts`.
    persist_error: Option<String>,
    last_state: Option<std::sync::Arc<Value>>,
}

impl ComposerService {
    fn new() -> Self {
        Self {
            core: None,
            drafts: DraftStore::default(),
            hosts: HashMap::new(),
            pending: HashMap::new(),
            provider_requests: ProviderRequests::default(),
            delivery: HashMap::new(),
            active_sends: HashMap::new(),
            creations: HashMap::new(),
            sent: VecDeque::new(),
            handoffs: HashSet::new(),
            handoff_revision: 0,
            blobs: BlobStore::new(state_keys::blob_dir()),
            blob_owners: HashMap::new(),
            notices: HashMap::new(),
            effects: Vec::new(),
            revision: 0,
            collected: false,
            next_panel: 0,
            persist_error: None,
            last_state: None,
        }
    }

    pub fn core(&self) -> Option<&CoreHandle> {
        self.core.as_ref()
    }

    /// Join one in-flight provider snapshot request for this host/cwd. Callers
    /// retain the returned ID in their own response maps; never cancel this
    /// shared request when a panel closes. Both success and failure release it.
    pub fn request_providers(&mut self, core: &CoreHandle, host: &str, cwd: &str) -> String {
        self.provider_requests.request(host, cwd, || {
            core.request(host, json!({"type": "get_providers_snapshot_request", "cwd": cwd}), "get_providers_snapshot_response")
        })
    }

    /// Monotonic revision of everything panels render from the service.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn bump(&mut self) {
        self.revision += 1;
    }

    pub fn allocate_panel(&mut self) -> u64 {
        self.next_panel += 1;
        self.next_panel
    }

    pub fn release_panel(&mut self, panel: u64) {
        self.notices.remove(&panel);
        self.blob_owners.retain(|_, owner| *owner != panel);
    }

    pub fn take_notices(&mut self, panel: u64) -> Vec<PanelNotice> {
        self.notices.remove(&panel).unwrap_or_default()
    }

    /// Effects produced outside a core batch (creation results after a flush, etc.).
    pub fn take_effects(&mut self) -> Vec<UiEffect> {
        std::mem::take(&mut self.effects)
    }

    pub(super) fn notify(&mut self, panel: Option<u64>, notice: PanelNotice) {
        if let Some(panel) = panel {
            self.notices.entry(panel).or_default().push(notice);
            self.bump();
        }
    }

    /// Whether the host has an admitted connection.
    pub fn connected(&self, host: &str) -> bool {
        self.hosts.get(host).is_some_and(|h| h.connected)
    }

    pub fn server_info(&self, host: &str) -> Value {
        self.hosts.get(host).map(|h| h.server_info.clone()).unwrap_or(Value::Null)
    }

    pub fn supports(&self, host: &str, feature: &str) -> bool {
        self.hosts
            .get(host)
            .is_some_and(|h| get(get(&h.server_info, "features"), feature) == &Value::Bool(true))
    }

    pub fn is_sending(&self, key: &str) -> bool {
        self.active_sends.contains_key(key)
    }

    pub fn active_send(&self, key: &str) -> Option<&str> {
        self.active_sends.get(key).map(String::as_str)
    }

    pub fn persistence_error(&self) -> Option<String> {
        self.drafts.persistence_error.clone().or_else(|| self.persist_error.clone())
    }

    // ----- sent messages / handoffs (timeline read API) -----

    /// Flutter SentUserMessages.remember (64 entries).
    pub(super) fn remember_sent(&mut self, agent_id: &str, message: &Value) {
        let id = s(get(message, "messageId"));
        let images = list(get(message, "images"));
        let attachments = list(get(message, "attachments"));
        if agent_id.is_empty() || id.is_empty() || (images.is_empty() && attachments.is_empty()) {
            return;
        }
        let key = format!("{agent_id}/{id}");
        self.sent.retain(|(k, _)| k != &key);
        self.sent.push_back((key, json!({"images": images, "attachments": attachments})));
        while self.sent.len() > 64 {
            self.sent.pop_front();
        }
    }

    pub fn sent_lookup(&self, agent_id: &str, client_message_id: &str) -> Option<Value> {
        if client_message_id.is_empty() {
            return None;
        }
        let key = format!("{agent_id}/{client_message_id}");
        self.sent.iter().find(|(k, _)| k == &key).map(|(_, v)| v.clone())
    }

    fn handoff_key(host: &str, agent: &str) -> String {
        serde_json::to_string(&json!([host, agent])).unwrap_or_default()
    }

    pub(super) fn mark_sent(&mut self, host: &str, agent: &str) {
        if !agent.is_empty() && self.handoffs.insert(Self::handoff_key(host, agent)) {
            self.handoff_revision += 1;
            self.bump();
        }
    }

    pub fn handoff_active(&self, host: &str, agent: &str) -> bool {
        !agent.is_empty() && self.handoffs.contains(&Self::handoff_key(host, agent))
    }

    pub fn clear_handoff(&mut self, host: &str, agent: &str) {
        if self.handoffs.remove(&Self::handoff_key(host, agent)) {
            self.handoff_revision += 1;
            self.bump();
        }
    }

    pub fn handoff_revision(&self) -> u64 {
        self.handoff_revision
    }

    // ----- core batch -----

    pub fn handle_core_events(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        if self.core.is_none() {
            self.core = Some(ui.core.clone());
        }
        // Host connection state from the batch (Connected/Disconnected) and the published
        // server info.
        let mut touched_hosts: HashSet<String> = HashSet::new();
        let mut closed_hosts: HashSet<String> = HashSet::new();
        for event in &ui.events {
            match event {
                CoreEvent::Connected { host_id, server_info } => {
                    let host = self.hosts.entry(host_id.clone()).or_default();
                    host.connected = true;
                    host.server_info = server_info.clone();
                    host.generation += 1;
                    touched_hosts.insert(host_id.clone());
                }
                CoreEvent::Disconnected { host_id, error } => {
                    let host = self.hosts.entry(host_id.clone()).or_default();
                    host.connected = false;
                    host.generation += 1;
                    touched_hosts.insert(host_id.clone());
                    if error.is_none() {
                        closed_hosts.insert(host_id.clone());
                    }
                }
                _ => {}
            }
        }
        for (host_id, info) in ui.server_infos.iter() {
            let host = self.hosts.entry(host_id.clone()).or_default();
            if host.server_info.is_null() {
                host.server_info = info.clone();
            }
        }
        // Initial status from state (hosts connected before the composer saw an event).
        if self.last_state.as_ref().map(std::sync::Arc::as_ptr) != Some(std::sync::Arc::as_ptr(&ui.state)) {
            self.last_state = Some(ui.state.clone());
            for host in list_ref(get(&ui.state, "hosts")) {
                let id = s(get(host, "serverId"));
                if id.is_empty() {
                    continue;
                }
                let entry = self.hosts.entry(id.to_owned()).or_default();
                let connected = s(get(host, "status")) == "connected";
                if entry.connected != connected && !touched_hosts.contains(id) {
                    entry.connected = connected;
                    entry.generation += 1;
                    touched_hosts.insert(id.to_owned());
                }
                if entry.server_info.is_null() && !get(host, "serverInfo").is_null() {
                    entry.server_info = get(host, "serverInfo").clone();
                }
            }
            if !self.drafts.is_loaded()
                && let Some(payload) = state_keys::drafts_payload(&ui.state)
            {
                self.drafts.load(payload);
                self.bump();
            }
            if let Some(error) = state_keys::storage_error(&ui.state) {
                self.persist_error = Some(error);
            }
        }
        if self.drafts.is_loaded() && !self.collected {
            self.collected = true;
            let keep = self.drafts.referenced_blobs();
            self.blobs.submit(BlobJob::Collect { keep });
        }

        // Reset before panels process connection changes and issue fresh requests.
        // Old responses cannot release a replacement request with a different ID.
        for host in &touched_hosts {
            self.provider_requests.reset(host);
        }
        for event in ui.events.iter() {
            match event {
                CoreEvent::Response { host_id, request_id, result } => {
                    self.provider_requests.finish(host_id, request_id);
                    if let Some(pending) = self.pending.remove(request_id) {
                        self.on_response(cx, host_id, request_id, pending, result.clone());
                    }
                }
                CoreEvent::HostEvent { host_id, event } => self.on_host_event(host_id, event),
                CoreEvent::Error { host_id: None, error } if error.code.starts_with("STORAGE") || error.code == "INVALID_STATE_KEY" => {
                    self.persist_error = Some(error.message.clone());
                    self.bump();
                }
                _ => {}
            }
        }
        for host in &touched_hosts {
            self.connection_changed(host, closed_hosts.contains(host));
        }
        self.after_mutation();
        ui.effects.append(&mut self.effects);
    }

    /// After any draft/delivery mutation: persist, wake delivery, redraw panels.
    pub(super) fn after_mutation(&mut self) {
        let changed = self.drafts.drain_changed();
        let inbox = self.drafts.drain_inbox_changed();
        if !changed.is_empty() || !inbox.is_empty() {
            self.bump();
        }
        if let Some(payload) = self.drafts.take_save()
            && let Some(core) = &self.core
        {
            core.save_state(&state_keys::drafts_save_key(), payload);
        }
        for host in self.drafts.drain_delivery_changed() {
            self.schedule(&host);
        }
    }

    /// Persist everything now and continue with `then` once it is durable
    /// (Flutter `await drafts.flush()`).
    pub(super) fn flush_then(&mut self, then: AfterFlush) {
        let Some(core) = self.core.clone() else {
            self.flush_failed(then, "Composer storage is not ready.".into());
            return;
        };
        if !self.drafts.is_loaded() {
            self.flush_failed(then, "Conversation drafts are still loading.".into());
            return;
        }
        let payload = self.drafts.take_save().unwrap_or_else(|| self.full_payload());
        let request = core.save_state_flushed(&state_keys::drafts_save_key(), payload);
        self.pending.insert(request, Pending::Flush { then });
    }

    fn full_payload(&mut self) -> Value {
        // take_save returned None: nothing changed since the last save; persist the same state
        // again so this flush has its own durability acknowledgement.
        self.drafts.mark_dirty();
        self.drafts.take_save().unwrap_or(Value::Null)
    }

    fn flush_failed(&mut self, then: AfterFlush, error: String) {
        let message = format!("Draft persistence failed: {error}");
        match then {
            AfterFlush::Send(job) => self.send_failed(job, &json!({"message": message, "outcomeUnknown": false, "local": true})),
            AfterFlush::Creation { draft_key } => self.creation_failed(&draft_key, &json!({"message": message, "outcomeUnknown": true})),
            AfterFlush::Nothing { panel } => {
                self.persist_error = Some(message);
                self.notify(panel, PanelNotice::CheckFinished { key: String::new(), message_id: String::new(), error: self.persist_error.clone() });
            }
        }
    }

    fn on_response(&mut self, cx: &mut Cx, host: &str, request_id: &str, pending: Pending, result: Result<Value, CoreError>) {
        let _ = cx;
        match pending {
            Pending::Flush { then } => match result {
                Ok(_) => {
                    self.persist_error = None;
                    match then {
                        AfterFlush::Send(job) => self.dispatch_send(job),
                        AfterFlush::Creation { draft_key } => self.creation_flushed(&draft_key),
                        AfterFlush::Nothing { .. } => {}
                    }
                }
                Err(error) => self.flush_failed(then, error.message),
            },
            Pending::Send { job } => self.send_result(job, result),
            Pending::Receipt { host, key, id, dismiss, panel } => self.receipt_result(&host, &key, &id, dismiss, panel, result),
            Pending::ReceiptObservation { host } => self.receipt_observation_result(&host, request_id, result),
            Pending::TimelineObservation { host, key } => self.timeline_observation_result(&host, &key, request_id, result),
            Pending::DrainFetch { host, key } => self.drain_fetch_result(&host, &key, result),
            Pending::Creation { draft_key, step } => self.creation_response(host, &draft_key, request_id, step, result),
        }
        self.after_mutation();
    }

    // ----- sends -----

    /// Persist the job's state, then send (Flutter: `await drafts.flush()` before the RPC).
    pub fn start_send(&mut self, job: SendJob) {
        self.active_sends.insert(job.key.clone(), s(get(&job.message, "messageId")).to_owned());
        if job.origin == SendOrigin::Pending || matches!(job.origin, SendOrigin::Queue { .. }) {
            self.remember_sent(&job.agent_id, &job.message);
        }
        self.bump();
        self.flush_then(AfterFlush::Send(job));
        self.after_mutation();
    }

    fn dispatch_send(&mut self, job: SendJob) {
        let Some(core) = self.core.clone() else { return };
        let fields = agent_message_fields(&job.agent_id, &job.message);
        let images = list(get(&job.message, "images"));
        let blob_dir = self.blobs.dir().to_path_buf();
        let request = core.operation(&job.host, move |client| {
            super::ops::rpc_with_images(
                client,
                "send_agent_message_request".into(),
                "send_agent_message_response".into(),
                fields,
                Some(images),
                blob_dir,
                Some(std::time::Duration::from_secs(30)),
            )
        });
        self.pending.insert(request, Pending::Send { job });
    }

    fn send_result(&mut self, job: SendJob, result: Result<Value, CoreError>) {
        let outcome = match result {
            Ok(envelope) => super::ops::open_envelope(&envelope),
            Err(error) => Err(json!({"code": error.code, "message": error.message, "outcomeUnknown": error.outcome_unknown})),
        };
        match outcome {
            Ok(payload) => self.send_succeeded(job, &payload),
            Err(error) => self.send_failed(job, &error),
        }
    }

    fn send_succeeded(&mut self, job: SendJob, payload: &Value) {
        let receipts = self.supports(&job.host, "agentMessageReceipts");
        let id = owned(get(&job.message, "messageId"));
        match job.origin {
            SendOrigin::Pending => {
                if receipts {
                    let receipt = get(payload, "receipt");
                    if non_empty_obj(receipt) {
                        self.drafts.apply_receipt(&job.key, receipt);
                    } else {
                        self.check_receipt(&job.host, &job.key, &id, false, job.panel);
                    }
                } else {
                    self.drafts.acknowledge(&job.key, &job.message);
                }
            }
            SendOrigin::Queue { .. } => {
                if receipts {
                    self.apply_receipt_everywhere(&job.host, get(payload, "receipt"));
                    let mut queue = maps(&self.drafts.field(&job.key, "queue"));
                    if let Some(entry) = queue.iter_mut().find(|q| s(get(q, "messageId")) == id)
                        && s(get(entry, "state")) == "sending"
                    {
                        set(entry, "state", json!("unacknowledged"));
                        self.drafts.update(&job.key, json!({"queue": queue}));
                    }
                } else {
                    let queue: Vec<Value> = maps(&self.drafts.field(&job.key, "queue"))
                        .into_iter()
                        .filter(|q| s(get(q, "messageId")) != id)
                        .collect();
                    self.drafts.update(&job.key, json!({"queue": queue}));
                }
            }
        }
        self.finish_send(&job, None);
        self.notify(job.panel, PanelNotice::PromptSubmitted { agent_id: job.agent_id.clone() });
        if job.panel.is_none() {
            self.broadcast_prompt(&job.agent_id);
        }
    }

    pub(super) fn send_failed(&mut self, job: SendJob, error: &Value) {
        if let Some(receipt) = get(error, "details").get("receipt").filter(|r| non_empty_obj(r)) {
            self.drafts.apply_receipt(&job.key, receipt);
        }
        let unknown = get(error, "outcomeUnknown") == &Value::Bool(true);
        let message = send_error_message(error);
        let id = owned(get(&job.message, "messageId"));
        let mut shown = None;
        match job.origin {
            SendOrigin::Pending => {
                // Persistence failures before the RPC are not definite rejections.
                let rejected = !unknown && get(error, "local") != &Value::Bool(true);
                if self.drafts.fail_send(&job.key, &job.message, &message, rejected) {
                    shown = Some(message.clone());
                }
            }
            SendOrigin::Queue { .. } => {
                let mut queue = maps(&self.drafts.field(&job.key, "queue"));
                if let Some(entry) = queue.iter_mut().find(|q| s(get(q, "messageId")) == id) {
                    let was_rejected = s(get(entry, "state")) == "rejected";
                    let state = if was_rejected || !unknown { "rejected" } else { "unacknowledged" };
                    let previous_error = get(entry, "error").clone();
                    set(entry, "state", json!(state));
                    set(
                        entry,
                        "error",
                        if was_rejected && !previous_error.is_null() { previous_error } else { json!(message) },
                    );
                    shown = Some(message.clone());
                }
                self.drafts.update(&job.key, json!({"queue": queue}));
            }
        }
        let drain = matches!(job.origin, SendOrigin::Queue { drain: true });
        self.finish_send(&job, if drain { None } else { shown });
    }

    fn finish_send(&mut self, job: &SendJob, error: Option<String>) {
        let id = owned(get(&job.message, "messageId"));
        if self.active_sends.get(&job.key) == Some(&id) {
            self.active_sends.remove(&job.key);
        }
        if let SendOrigin::Queue { drain: true } = job.origin
            && let Some(delivery) = self.delivery.get_mut(&job.host)
        {
            delivery.busy.remove(&job.key);
        }
        self.notify(job.panel, PanelNotice::SendFinished { key: job.key.clone(), message_id: id, error });
        self.bump();
        self.schedule(&job.host);
    }

    /// An automatic drain has no panel owner; every panel of that agent reacts.
    fn broadcast_prompt(&mut self, agent_id: &str) {
        for notices in self.notices.values_mut() {
            notices.push(PanelNotice::PromptSubmitted { agent_id: agent_id.to_owned() });
        }
        self.bump();
    }

    pub(super) fn apply_receipt_everywhere(&mut self, host: &str, receipt: &Value) {
        if !non_empty_obj(receipt) {
            return;
        }
        let agent = s(get(receipt, "agentId"));
        for key in self.drafts.agent_keys_for_server(host) {
            if key_identity(&key).get(2).map(String::as_str) == Some(agent) {
                self.drafts.apply_receipt(&key, receipt);
            }
        }
    }

    // ----- blobs -----

    /// Store clipboard image bytes for `panel`; the result arrives as `ImagesStored`.
    pub fn store_image_bytes(&mut self, panel: u64, bytes: std::sync::Arc<Vec<u8>>, mime: Option<String>) -> u64 {
        let token = self.blobs.next_token();
        self.blob_owners.insert(token, panel);
        self.blobs.submit(BlobJob::StoreBytes { token, bytes, mime });
        token
    }

    pub fn import_images(&mut self, panel: u64, files: Vec<(std::path::PathBuf, String)>) -> u64 {
        let token = self.blobs.next_token();
        self.blob_owners.insert(token, panel);
        self.blobs.submit(BlobJob::ImportImages { token, files });
        token
    }

    pub fn stat_paths(&mut self, panel: u64, paths: Vec<std::path::PathBuf>) -> u64 {
        let token = self.blobs.next_token();
        self.blob_owners.insert(token, panel);
        self.blobs.submit(BlobJob::Stat { token, paths });
        token
    }

    pub fn poll_background(&mut self, cx: &mut Cx) {
        let mut any = false;
        while let Some(result) = self.blobs.try_recv() {
            any = true;
            match result {
                BlobResult::Decoded { key, result } => self.blobs.install(cx, key, result),
                BlobResult::Stored { token, images } => {
                    if let Some(panel) = self.blob_owners.remove(&token) {
                        self.notify(Some(panel), PanelNotice::ImagesStored { token, images });
                    }
                }
                BlobResult::Stat { token, files } => {
                    if let Some(panel) = self.blob_owners.remove(&token) {
                        self.notify(Some(panel), PanelNotice::FilesStat { token, files });
                    }
                }
            }
        }
        if any {
            self.bump();
        }
    }

    /// Persist everything and report whether the store is healthy (shell quit path).
    pub fn flush_for_quit(&mut self, ui: &UiContext) -> Result<(), String> {
        if self.core.is_none() {
            self.core = Some(ui.core.clone());
        }
        if let Some(payload) = self.drafts.take_save() {
            ui.core.save_state(&state_keys::drafts_save_key(), payload);
        }
        // In-flight sends are already durable (pending/queue state was flushed before the RPC),
        // so only a persistence failure keeps the window open (Flutter shell `drafts.flush()`).
        match self.persistence_error() {
            Some(error) => Err(format!("Draft persistence failed: {error}")),
            None => Ok(()),
        }
    }
}

/// submit.ts: the error's own message, else composer.errors.failedToSend.
pub fn send_error_message(error: &Value) -> String {
    let message = s(get(error, "message")).trim().to_owned();
    if message.is_empty() { t("composer.errors.failedToSend") } else { message }
}

#[cfg(test)]
mod provider_request_tests {
    use super::ProviderRequests;

    #[test]
    fn concurrent_consumers_share_one_response_id_per_scope() {
        let mut requests = ProviderRequests::default();
        let mut sends = 0;
        for _ in 0..20 {
            let id = requests.request("host", " /workspace/ ", || {
                sends += 1;
                "shared".into()
            });
            assert_eq!(id, "shared");
        }
        assert_eq!(sends, 1);
        assert_eq!(requests.request("host", "/workspace", || panic!("already in flight")), "shared");
        assert_eq!(requests.request("other-host", "/workspace", || "other-host".into()), "other-host");
        assert_eq!(requests.request("host", "/other", || "other-cwd".into()), "other-cwd");
    }

    #[test]
    fn completion_releases_only_its_request_for_success_or_failure() {
        let mut requests = ProviderRequests::default();
        requests.request("host", "/workspace", || "first".into());
        requests.request("host", "/other", || "other".into());
        requests.finish("host", "unrelated");
        assert_eq!(requests.request("host", "/workspace", || panic!("still in flight")), "first");
        // The service invokes finish before inspecting either Ok or Err.
        for next in ["after-success", "after-failure"] {
            let previous = requests.request("host", "/workspace", || panic!("still in flight"));
            requests.finish("host", &previous);
            assert_eq!(requests.request("host", "/workspace", || next.into()), next);
            assert_eq!(requests.request("host", "/other", || panic!("unrelated scope")), "other");
        }
    }

    #[test]
    fn reconnect_releases_ownership_without_old_response_releasing_replacement() {
        let mut requests = ProviderRequests::default();
        requests.request("host", "/workspace", || "old".into());
        requests.request("other-host", "/workspace", || "other".into());
        requests.reset("host");
        assert_eq!(requests.request("host", "/workspace", || "new".into()), "new");
        requests.finish("host", "old");
        assert_eq!(requests.request("host", "/workspace", || panic!("replacement in flight")), "new");
        assert_eq!(requests.request("other-host", "/workspace", || panic!("unrelated host")), "other");
    }
}
