//! Port of `lib/features/chat/creation_recovery.dart` (keyed creation lifecycle) and the
//! draft half of `agent_panel.dart` `_create`. Creation has no deadline and a lost
//! connection never fails it: once the host is back, its receipt is followed until it
//! commits, and the intent is submitted again only when the host holds no receipt for it.
//! Only an intentional close of the host ends a creation without an outcome.

use super::attachments::wire_attachment;
use super::json::*;
use super::service::{AfterFlush, ComposerService, Pending, PanelNotice};
use crate::common::{PanelTarget, TabPlacement, UiEffect};
use paseo_gui_core::CoreError;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Workspace,
    Agent,
}

impl RunKind {
    fn name(self) -> &'static str {
        match self {
            RunKind::Workspace => "workspace",
            RunKind::Agent => "agent",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CreationStep {
    Submit(RunKind),
    Recover(RunKind),
    Observe(RunKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Recover,
    Resume,
}

struct Run {
    kind: RunKind,
    key: String,
    fields: Value,
    revision: i64,
    recovering: bool,
    observation: Option<String>,
    waiting: Option<Wait>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Persisting `creationKey` before anything is submitted.
    FlushKey,
    Workspace,
    /// Persisting the frozen `creationRequest`.
    FlushRequest,
    Agent,
}

pub struct DraftCreation {
    host: String,
    panel: Option<u64>,
    message: Value,
    config: Value,
    cwd: String,
    workspace_id: String,
    server_title: String,
    recovering: bool,
    stage: Stage,
    run: Option<Run>,
}

/// What finished a run.
enum Outcome {
    Complete(Value),
    /// `{message, outcomeUnknown}`.
    Failed(Value),
}

fn unknown(message: impl Into<String>) -> Value {
    json!({"message": message.into(), "outcomeUnknown": true})
}

fn rejected(message: impl Into<String>) -> Value {
    json!({"message": message.into(), "outcomeUnknown": false})
}

/// A definite failure the host reruns when the same intent is resubmitted.
fn rerun(snapshot: &Value) -> bool {
    s(get(snapshot, "phase")) == "failed"
        && get(snapshot, "outcomeUnknown") != &Value::Bool(true)
        && !(s(get(snapshot, "failedStage")) == "prompt" && get(snapshot, "agent").is_object())
}

fn offline_code(code: &str) -> bool {
    matches!(code, "DAEMON_CONNECTION_LOST" | "DAEMON_NOT_CONNECTED")
}

impl ComposerService {
    /// Flutter `_create`: submit the draft `message` as a new agent (creating the workspace
    /// first when the draft has none). `config` is the effective submit config with offered
    /// feature values; `workspace_id` the target's (may be empty).
    pub fn start_creation(&mut self, host: &str, draft_key: &str, panel: Option<u64>, message: Value, config: Value, target: CreationTarget) {
        let CreationTarget { cwd, workspace_id, title } = target;
        let draft = self.drafts.draft(draft_key).clone();
        let workspace_id = if workspace_id.is_empty() {
            owned(get(&draft, "creationWorkspaceId"))
        } else {
            workspace_id.to_owned()
        };
        let recovering = get(&draft, "creationKey").is_string();
        let key = get(&draft, "creationKey")
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.drafts.update(draft_key, json!({"creationKey": key}));
        self.active_sends.insert(draft_key.to_owned(), owned(get(&message, "messageId")));
        self.creations.insert(
            draft_key.to_owned(),
            DraftCreation {
                host: host.to_owned(),
                panel,
                message,
                config,
                cwd: cwd.to_owned(),
                workspace_id,
                server_title: title.to_owned(),
                recovering,
                stage: Stage::FlushKey,
                run: None,
            },
        );
        self.bump();
        self.flush_then(AfterFlush::Creation { draft_key: draft_key.to_owned() });
    }

    pub fn is_creating(&self, draft_key: &str) -> bool {
        self.creations.contains_key(draft_key)
    }

    pub(super) fn creation_flushed(&mut self, draft_key: &str) {
        let Some((stage, workspace_id, cwd, recovering)) = self
            .creations
            .get(draft_key)
            .map(|c| (c.stage, c.workspace_id.clone(), c.cwd.clone(), c.recovering))
        else {
            return;
        };
        match stage {
            Stage::FlushKey if workspace_id.is_empty() => {
                let creation_key = self.drafts.field(draft_key, "creationKey");
                let fields = json!({
                    "idempotencyKey": format!("{}:workspace", s(&creation_key)),
                    "source": {"kind": "directory", "path": cwd},
                    "subscribe": false,
                });
                self.start_run(draft_key, RunKind::Workspace, fields, recovering);
            }
            Stage::FlushKey => self.prepare_agent_request(draft_key),
            Stage::FlushRequest => {
                // `recovering` now records whether a frozen request existed before this attempt.
                let original = self.drafts.field(draft_key, "creationRequest");
                self.start_run(draft_key, RunKind::Agent, original, recovering);
            }
            Stage::Workspace | Stage::Agent => {}
        }
    }

    fn prepare_agent_request(&mut self, draft_key: &str) {
        let Some(creation) = self.creations.get_mut(draft_key) else { return };
        let original = self.drafts.field(draft_key, "creationRequest");
        // A frozen request from an earlier attempt is replayed byte-for-byte (recovery).
        creation.recovering = non_empty_obj(&original);
        let fields = if non_empty_obj(&original) {
            original
        } else {
            let key = self.drafts.field(draft_key, "creationKey");
            let message = &creation.message;
            json!({
                "idempotencyKey": key,
                "workspaceId": creation.workspace_id,
                "config": creation.config,
                "initialPrompt": get(message, "text"),
                "clientMessageId": get(message, "messageId"),
                "images": list(get(message, "images")),
                "attachments": maps(get(message, "attachments")).iter().map(wire_attachment).collect::<Vec<_>>(),
                "subscribe": false,
            })
        };
        creation.stage = Stage::FlushRequest;
        self.drafts.update(draft_key, json!({"creationRequest": fields}));
        self.flush_then(AfterFlush::Creation { draft_key: draft_key.to_owned() });
    }

    fn start_run(&mut self, draft_key: &str, kind: RunKind, fields: Value, recover: bool) {
        let Some(creation) = self.creations.get_mut(draft_key) else { return };
        creation.stage = if kind == RunKind::Workspace { Stage::Workspace } else { Stage::Agent };
        let key = owned(get(&fields, "idempotencyKey"));
        creation.run = Some(Run { kind, key, fields, revision: -1, recovering: false, observation: None, waiting: None });
        if recover {
            self.run_recover(draft_key);
        } else {
            self.run_submit(draft_key);
        }
    }

    fn run_submit(&mut self, draft_key: &str) {
        let Some(core) = self.core.clone() else { return };
        let blob_dir = self.blobs.dir().to_path_buf();
        let Some(creation) = self.creations.get(draft_key) else { return };
        let Some(run) = &creation.run else { return };
        let kind = run.kind;
        let mut fields = run.fields.clone();
        let images = if kind == RunKind::Agent {
            Some(list(get(&fields, "images")))
        } else {
            None
        };
        if images.is_none() {
            remove(&mut fields, "images");
        }
        let name = kind.name();
        let request = core.operation(&creation.host, move |client| {
            super::ops::rpc_with_images(
                client,
                format!("{name}.create.request"),
                format!("{name}.create.response"),
                fields,
                images,
                blob_dir,
                None,
            )
        });
        self.pending.insert(request, Pending::Creation { draft_key: draft_key.to_owned(), step: CreationStep::Submit(kind) });
    }

    fn run_recover(&mut self, draft_key: &str) {
        let Some(core) = self.core.clone() else { return };
        let Some(creation) = self.creations.get_mut(draft_key) else { return };
        let host = creation.host.clone();
        let connected = self.hosts.get(&host).is_some_and(|h| h.connected);
        let Some(run) = creation.run.as_mut() else { return };
        if !connected {
            run.waiting = Some(Wait::Recover);
            return;
        }
        run.waiting = None;
        let kind = run.kind;
        let request = core.request(
            &host,
            json!({"type": "creation.subscribe.request", "kind": kind.name(), "idempotencyKey": run.key, "subscribe": false}),
            "creation.subscribe.response",
        );
        self.pending.insert(request, Pending::Creation { draft_key: draft_key.to_owned(), step: CreationStep::Recover(kind) });
    }

    /// CreationClient.reconnect: follow this intent's receipt over a subscription.
    fn run_resume(&mut self, draft_key: &str) {
        let Some(core) = self.core.clone() else { return };
        let Some(creation) = self.creations.get_mut(draft_key) else { return };
        let host = creation.host.clone();
        let connected = self.hosts.get(&host).is_some_and(|h| h.connected);
        let Some(run) = creation.run.as_mut() else { return };
        run.recovering = true;
        if run.observation.is_some() {
            // The core restores the subscription after reconnects (subscription.restored).
            return;
        }
        if !connected {
            run.waiting = Some(Wait::Resume);
            return;
        }
        run.waiting = None;
        let kind = run.kind;
        let request = core.observe(
            &host,
            json!({"type": "creation.subscribe.request", "kind": kind.name(), "idempotencyKey": run.key, "subscribe": true}),
            "creation.subscribe.response",
        );
        run.observation = Some(request.clone());
        self.pending.insert(request, Pending::Creation { draft_key: draft_key.to_owned(), step: CreationStep::Observe(kind) });
    }

    pub(super) fn creation_response(
        &mut self,
        _host: &str,
        draft_key: &str,
        request_id: &str,
        step: CreationStep,
        result: Result<Value, CoreError>,
    ) {
        if self.creations.get(draft_key).is_none_or(|c| c.run.is_none()) {
            return;
        }
        match step {
            CreationStep::Submit(_) => self.submit_result(draft_key, result),
            CreationStep::Recover(_) => match result {
                Ok(payload) => {
                    if let Some(error) = paseo_gui_core::response_error(&payload, "creation.subscribe.request") {
                        return self.run_finished(draft_key, Outcome::Failed(unknown(error.message)));
                    }
                    let snapshot = get(&payload, "snapshot").clone();
                    if !snapshot.is_object() || rerun(&snapshot) {
                        return self.run_submit(draft_key);
                    }
                    self.apply_receipt_snapshot(draft_key, &snapshot);
                    if self.creations.get(draft_key).is_some_and(|c| c.run.is_some()) {
                        self.run_resume(draft_key);
                    }
                }
                Err(error) => {
                    if offline_code(&error.code) || !self.creation_connected(draft_key) {
                        self.set_waiting(draft_key, Wait::Recover);
                        self.run_recover(draft_key);
                    } else {
                        // The earlier attempt's outcome is still unknown; keep its identity.
                        self.run_finished(draft_key, Outcome::Failed(unknown(error.message)));
                    }
                }
            },
            CreationStep::Observe(_) => match result {
                Ok(payload) => {
                    let current = self
                        .creations
                        .get(draft_key)
                        .and_then(|c| c.run.as_ref())
                        .and_then(|r| r.observation.clone());
                    if current.as_deref() != Some(request_id) {
                        if let Some(core) = &self.core {
                            core.cancel_request(request_id);
                        }
                        return;
                    }
                    self.receipt_snapshot(draft_key, get(&payload, "snapshot"));
                }
                Err(error) => {
                    if let Some(run) = self.creations.get_mut(draft_key).and_then(|c| c.run.as_mut()) {
                        run.observation = None;
                    }
                    if offline_code(&error.code) || !self.creation_connected(draft_key) {
                        self.set_waiting(draft_key, Wait::Resume);
                        self.run_resume(draft_key);
                    } else {
                        self.run_finished(draft_key, Outcome::Failed(unknown(error.message)));
                    }
                }
            },
        }
    }

    fn creation_connected(&self, draft_key: &str) -> bool {
        self.creations.get(draft_key).is_some_and(|c| self.connected(&c.host))
    }

    fn set_waiting(&mut self, draft_key: &str, wait: Wait) {
        if let Some(run) = self.creations.get_mut(draft_key).and_then(|c| c.run.as_mut()) {
            run.waiting = Some(wait);
        }
    }

    /// The host's snapshot on (re)subscribing.
    fn receipt_snapshot(&mut self, draft_key: &str, snapshot: &Value) {
        if snapshot.is_object() {
            self.apply_receipt_snapshot(draft_key, snapshot);
        } else {
            self.run_submit(draft_key);
        }
    }

    fn submit_result(&mut self, draft_key: &str, result: Result<Value, CoreError>) {
        let (kind, key, recovering) = match self.creations.get(draft_key).and_then(|c| c.run.as_ref()) {
            Some(run) => (run.kind, run.key.clone(), run.recovering),
            None => return,
        };
        let connected = self.creation_connected(draft_key);
        let payload = match result.map(|envelope| super::ops::open_envelope(&envelope)) {
            Ok(Ok(payload)) => payload,
            Ok(Err(error)) => {
                let details = get(&error, "details");
                if !get(details, "creation").is_object() {
                    let code = s(get(&error, "code"));
                    if offline_code(code) || !connected {
                        // A lost connection never fails a creation; follow its receipt.
                        if !recovering {
                            self.run_resume(draft_key);
                        }
                        return;
                    }
                    let message = owned(get(&error, "message"));
                    let outcome = if get(&error, "outcomeUnknown") == &Value::Bool(true) { unknown(message) } else { rejected(message) };
                    return self.run_finished(draft_key, Outcome::Failed(outcome));
                }
                let receipt = get(details, "creation");
                if s(get(receipt, "kind")) != kind.name() || s(get(receipt, "idempotencyKey")) != key {
                    return self.run_finished(
                        draft_key,
                        Outcome::Failed(unknown("Creation failure carried a mismatched receipt.")),
                    );
                }
                details.clone()
            }
            Err(error) => {
                if offline_code(&error.code) || !connected {
                    if !recovering {
                        self.run_resume(draft_key);
                    }
                    return;
                }
                let outcome = if error.outcome_unknown { unknown(error.message) } else { rejected(error.message) };
                return self.run_finished(draft_key, Outcome::Failed(outcome));
            }
        };
        let creation = get(&payload, "creation").clone();
        if get(&creation, "outcomeUnknown") == &Value::Bool(true) {
            let message = s_or(get(&creation, "error"), "Creation outcome is unknown. Recover this same intent.").to_owned();
            return self.run_finished(draft_key, Outcome::Failed(unknown(message)));
        }
        if s(get(&creation, "phase")) == "failed" && s(get(&creation, "failedStage")) == "prompt" && get(&creation, "agent").is_object() {
            return self.run_finished(
                draft_key,
                Outcome::Complete(json!({"agent": get(&creation, "agent"), "creation": creation, "promptError": get(&creation, "error")})),
            );
        }
        // requireSuccess
        if !get(&payload, "error").is_null()
            || get(&payload, "accepted") == &Value::Bool(false)
            || get(&payload, "ok") == &Value::Bool(false)
            || get(&payload, "success") == &Value::Bool(false)
        {
            let message = s_or(get(&payload, "error"), "The daemon rejected this operation").to_owned();
            return self.run_finished(draft_key, Outcome::Failed(rejected(message)));
        }
        self.run_finished(draft_key, Outcome::Complete(payload));
    }

    /// CreationClient.receive: an older revision never replaces a newer one.
    fn apply_receipt_snapshot(&mut self, draft_key: &str, receipt: &Value) {
        let Some(run) = self.creations.get_mut(draft_key).and_then(|c| c.run.as_mut()) else { return };
        if s(get(receipt, "idempotencyKey")) != run.key {
            return;
        }
        let revision = get(receipt, "revision").as_i64().unwrap_or(-1);
        if revision < run.revision {
            return;
        }
        run.revision = revision;
        match s(get(receipt, "phase")) {
            "failed" => {
                let text = s_or(get(receipt, "error"), "Creation failed").to_owned();
                if get(receipt, "outcomeUnknown") != &Value::Bool(true)
                    && s(get(receipt, "failedStage")) == "prompt"
                    && get(receipt, "agent").is_object()
                {
                    return self.run_finished(
                        draft_key,
                        Outcome::Complete(json!({"agent": get(receipt, "agent"), "creation": receipt, "promptError": text})),
                    );
                }
                let outcome = if get(receipt, "outcomeUnknown") == &Value::Bool(true) { unknown(text) } else { rejected(text) };
                self.run_finished(draft_key, Outcome::Failed(outcome));
            }
            "completed" => self.run_finished(
                draft_key,
                Outcome::Complete(json!({"agent": get(receipt, "agent"), "workspace": get(receipt, "workspace"), "creation": receipt})),
            ),
            _ => {}
        }
    }

    /// Creation events routed by observation id.
    pub(super) fn creation_event(&mut self, host: &str, event: &Value, observations: &[String]) {
        let kind_name = s(get(event, "type"));
        let payload = get(event, "payload");
        let keys: Vec<String> = self
            .creations
            .iter()
            .filter(|(_, c)| c.host == host)
            .filter_map(|(key, c)| {
                let observation = c.run.as_ref()?.observation.clone()?;
                let addressed = observations.contains(&observation)
                    || s(get(payload, "observationId")) == observation;
                addressed.then(|| key.clone())
            })
            .collect();
        for draft_key in keys {
            let Some(run_kind) = self.creations.get(&draft_key).and_then(|c| c.run.as_ref()).map(|r| r.kind) else { continue };
            if kind_name == format!("{}.create.update", run_kind.name()) {
                self.apply_receipt_snapshot(&draft_key, payload);
            } else if kind_name == "subscription.restored" {
                self.receipt_snapshot(&draft_key, get(payload, "snapshot"));
            } else if kind_name == "subscription.error" {
                let message = s_or(get(payload, "error"), "Creation subscription failed").to_owned();
                self.run_finished(&draft_key, Outcome::Failed(unknown(message)));
            }
        }
    }

    pub(super) fn creation_connection_changed(&mut self, host: &str, closed: bool) {
        let keys: Vec<String> = self.creations.iter().filter(|(_, c)| c.host == host).map(|(k, _)| k.clone()).collect();
        for draft_key in keys {
            if closed {
                // CreationClient.close: closing the client fails every unsettled creation.
                if self.creations.get(&draft_key).is_some_and(|c| c.run.is_some()) {
                    self.run_finished(&draft_key, Outcome::Failed(unknown("Daemon client closed")));
                }
                continue;
            }
            if !self.connected(host) {
                continue;
            }
            let waiting = self.creations.get(&draft_key).and_then(|c| c.run.as_ref()).and_then(|r| r.waiting);
            match waiting {
                Some(Wait::Recover) => self.run_recover(&draft_key),
                Some(Wait::Resume) => self.run_resume(&draft_key),
                None => {}
            }
        }
    }

    fn run_finished(&mut self, draft_key: &str, outcome: Outcome) {
        let Some(creation) = self.creations.get_mut(draft_key) else { return };
        let Some(run) = creation.run.take() else { return };
        if let (Some(observation), Some(core)) = (run.observation, self.core.clone()) {
            core.cancel_request(&observation);
        }
        match (run.kind, outcome) {
            (_, Outcome::Failed(error)) => self.creation_failed(draft_key, &error),
            (RunKind::Workspace, Outcome::Complete(result)) => {
                let workspace_id = owned(get(get(&result, "workspace"), "id"));
                if workspace_id.is_empty() {
                    return self.creation_failed(draft_key, &rejected("Workspace creation returned no workspace identity."));
                }
                if let Some(creation) = self.creations.get_mut(draft_key) {
                    creation.workspace_id = workspace_id.clone();
                }
                self.drafts.update(draft_key, json!({"creationWorkspaceId": workspace_id}));
                self.prepare_agent_request(draft_key);
            }
            (RunKind::Agent, Outcome::Complete(result)) => self.agent_created(draft_key, result),
        }
        self.after_mutation();
    }

    fn agent_created(&mut self, draft_key: &str, response: Value) {
        let Some(creation) = self.creations.remove(draft_key) else { return };
        let agent = get(&response, "agent").clone();
        let agent_id = owned(get(&agent, "id"));
        if agent_id.is_empty() {
            self.creations.insert(draft_key.to_owned(), creation);
            return self.creation_failed(
                draft_key,
                &json!({"message": "Creation has not returned an agent yet. Retry recovers the same creation intent.", "outcomeUnknown": true}),
            );
        }
        let message = creation.message.clone();
        self.remember_sent(&agent_id, &message);
        let workspace_id = s_or(get(&agent, "workspaceId"), &creation.workspace_id).to_owned();
        let cwd = s_or(get(&agent, "cwd"), &creation.cwd).to_owned();
        let title = s_or(get(&agent, "title"), "New session").to_owned();
        let prompt_error = get(&response, "promptError").clone();
        let mut data = json!({});
        if !prompt_error.is_null() {
            let created_key = draft_key_for(&creation.host, &workspace_id, &agent_id);
            if s(&self.drafts.field(&created_key, "text")).is_empty() {
                self.drafts.update(
                    &created_key,
                    json!({
                        "text": get(&message, "text"),
                        "images": get(&message, "images"),
                        "attachments": get(&message, "attachments"),
                        "pending": null,
                    }),
                );
            }
            self.drafts.update(draft_key, json!({"pending": null}));
            data = json!({"submissionError": display(&prompt_error)});
        } else {
            self.drafts.acknowledge(draft_key, &message);
            self.drafts.update(draft_key, json!({"creationKey": null, "creationRequest": null}));
            // create-flow-store markLifecycle "sent": optimistic running handoff.
            self.mark_sent(&creation.host, &agent_id);
        }
        self.flush_then(AfterFlush::Nothing { panel: None });
        self.active_sends.remove(draft_key);
        let _ = &creation.server_title;
        self.effects.push(UiEffect::OpenTab {
            target: PanelTarget {
                kind: "agent".into(),
                server_id: creation.host.clone(),
                workspace_id,
                cwd,
                id: Some(agent_id.clone()),
                title: Some(title),
                data,
            },
            placement: TabPlacement::Main,
        });
        self.notify(
            creation.panel,
            PanelNotice::SendFinished { key: draft_key.to_owned(), message_id: owned(get(&message, "messageId")), error: None },
        );
        self.bump();
    }

    pub(super) fn creation_failed(&mut self, draft_key: &str, error: &Value) {
        let Some(creation) = self.creations.remove(draft_key) else { return };
        let message = super::service::send_error_message(error);
        let definite = get(error, "outcomeUnknown") != &Value::Bool(true);
        let failed = self.drafts.fail_send(draft_key, &creation.message, &message, definite);
        if failed && definite {
            self.drafts.update(draft_key, json!({"creationKey": null, "creationRequest": null}));
        }
        self.active_sends.remove(draft_key);
        self.notify(
            creation.panel,
            PanelNotice::SendFinished {
                key: draft_key.to_owned(),
                message_id: owned(get(&creation.message, "messageId")),
                error: failed.then_some(message),
            },
        );
        self.bump();
    }
}

fn draft_key_for(host: &str, workspace: &str, agent: &str) -> String {
    super::drafts::draft_key(host, workspace, agent)
}

/// Where a creation lands: the draft tab's directory, workspace (may be empty) and title.
pub struct CreationTarget<'a> {
    pub cwd: &'a str,
    pub workspace_id: &'a str,
    pub title: &'a str,
}
