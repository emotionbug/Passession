//! Pull request pane state (original pull_request_panel.dart
//! `_PullRequestPanelState`): activity timeline query with retries, the GitLab
//! pipeline jobs query with polling, and section/entry collapse state.

use super::host;
use super::json::{flag_or, object, text};
use super::pull_request::{Check, Entry, TimelineFetchInputs, is_unsupported_timeline_error, pull_request_number, timeline_retry_delay};
use super::store::{GitStore, pr_status};
use super::wire::{self, Failure};
use crate::common::UiContext;
use makepad_widgets::*;
use paseo_gui_core::{CoreEvent, RequestId};
use serde_json::{Value, json};
use std::collections::HashSet;

struct TimelineQuery {
    generation: u64,
    number: i64,
    owner: String,
    name: String,
    failures: u32,
}

pub struct PrPane {
    pub host: String,
    pub cwd: String,
    visible: bool,
    connected: bool,
    seen_invalidation: u64,
    timeline_key: Option<String>,
    timeline_revision: Option<u64>,
    pub timeline: Option<Value>,
    pub timeline_fetching: bool,
    timeline_generation: u64,
    timeline_request: Option<(RequestId, TimelineQuery)>,
    retry: Option<(Timer, TimelineQuery)>,
    pub checks_open: bool,
    pub activity_open: bool,
    pub collapsed_groups: HashSet<Check>,
    collapsed_keys: HashSet<String>,
    expanded_keys: HashSet<String>,
    pipeline_identity: String,
    pub pipeline: Option<Value>,
    pub pipeline_error: Option<String>,
    pub pipeline_loading: bool,
    pipeline_fetching: bool,
    pipeline_generation: u64,
    pipeline_revision: Option<u64>,
    pipeline_poll: Option<Timer>,
    pipeline_request: Option<(RequestId, u64)>,
    pub dirty: bool,
}

impl PrPane {
    pub fn new(host_id: &str, cwd: &str, store: &GitStore) -> Self {
        Self {
            host: host_id.to_owned(),
            cwd: cwd.to_owned(),
            visible: true,
            connected: false,
            seen_invalidation: store.invalidation(host_id, cwd),
            timeline_key: None,
            timeline_revision: None,
            timeline: None,
            timeline_fetching: false,
            timeline_generation: 0,
            timeline_request: None,
            retry: None,
            checks_open: true,
            activity_open: true,
            collapsed_groups: HashSet::new(),
            collapsed_keys: HashSet::new(),
            expanded_keys: HashSet::new(),
            pipeline_identity: String::new(),
            pipeline: None,
            pipeline_error: None,
            pipeline_loading: false,
            pipeline_fetching: false,
            pipeline_generation: 0,
            pipeline_revision: None,
            pipeline_poll: None,
            pipeline_request: None,
            dirty: true,
        }
    }

    fn unsupported_key(&self, number: i64) -> String {
        format!("{}\u{0}{}\u{0}{number}", self.host, self.cwd)
    }

    pub fn set_visible(&mut self, cx: &mut Cx, ui: &UiContext, store: &mut GitStore, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.sync(cx, ui, store);
    }

    /// Original `_invalidateActivity`: actions and refresh mark the activity
    /// and pipeline stale.
    fn invalidate_activity(&mut self, cx: &mut Cx, ui: &UiContext, store: &mut GitStore) {
        self.timeline_key = None;
        self.pipeline_revision = None;
        self.sync(cx, ui, store);
    }

    /// Original `_sync` + `_syncPipeline`.
    pub fn sync(&mut self, cx: &mut Cx, ui: &UiContext, store: &mut GitStore) {
        let state = store.checkout(&self.host, &self.cwd);
        let payload = state.and_then(|s| s.pr_payload.clone()).unwrap_or(Value::Null);
        let status = pr_status(state).clone();
        let pr_revision = state.map(|s| s.pr_revision).unwrap_or(0);
        let number = pull_request_number(&status);
        let owner = text(&status, "repoOwner").to_owned();
        let name = text(&status, "repoName").to_owned();
        let unsupported = number.is_some_and(|n| store.timeline_unsupported(&self.unsupported_key(n)));
        let fetch = TimelineFetchInputs {
            connected: self.connected,
            timeline_enabled: self.visible,
            github_features_enabled: flag_or(&payload, "githubFeaturesEnabled", true),
            cwd: &self.cwd,
            pr_number: number,
            repo_owner: &owner,
            repo_name: &name,
            unsupported,
        }
        .should_fetch();
        // A disabled query keeps its data and refetches when enabled again only
        // if its key changed or it was invalidated meanwhile.
        match fetch.then(|| format!("{}/{owner}/{name}", number.unwrap_or_default())) {
            None => {
                if self.timeline_fetching {
                    self.timeline_generation += 1;
                    self.timeline_fetching = false;
                    self.timeline_request = None;
                    self.retry = None;
                    self.timeline_key = None;
                    self.dirty = true;
                }
            }
            Some(timeline_key) => {
                if Some(&timeline_key) != self.timeline_key.as_ref() || Some(pr_revision) != self.timeline_revision {
                    self.timeline_key = Some(timeline_key);
                    self.timeline_revision = Some(pr_revision);
                    self.timeline_generation += 1;
                    self.timeline_fetching = true;
                    self.retry = None;
                    let query = TimelineQuery {
                        generation: self.timeline_generation,
                        number: number.unwrap_or_default(),
                        owner,
                        name,
                        failures: 0,
                    };
                    self.send_timeline(ui, query);
                    self.dirty = true;
                }
            }
        }
        self.sync_pipeline(cx, ui, &status, pr_revision);
    }

    fn send_timeline(&mut self, ui: &UiContext, query: TimelineQuery) {
        let request = wire::send(
            &ui.core,
            &self.host,
            "pull_request_timeline_request",
            json!({
                "cwd": self.cwd,
                "prNumber": query.number,
                "repoOwner": query.owner,
                "repoName": query.name,
            }),
        );
        self.timeline_request = Some((request, query));
    }

    /// Original `_gitlabPipelineSection`.
    pub fn gitlab_pipeline_section(&self, ui: &UiContext, facts: &Value) -> bool {
        text(facts, "forge") == "gitlab"
            && facts.get("pipelineId").is_some_and(|id| !id.is_null())
            && host::supports(ui, &self.host, "forgeProviders")
    }

    fn sync_pipeline(&mut self, cx: &mut Cx, ui: &UiContext, status: &Value, pr_revision: u64) {
        let facts = object(status, "forgeSpecific");
        let section = self.gitlab_pipeline_section(ui, facts);
        let identity = if section {
            format!("{}:{}", pull_request_number(status).unwrap_or_default(), facts["pipelineId"])
        } else {
            String::new()
        };
        if identity != self.pipeline_identity {
            self.pipeline_identity = identity;
            self.pipeline = None;
            self.pipeline_error = None;
            self.pipeline_loading = false;
            self.pipeline_fetching = false;
            self.pipeline_revision = None;
            self.pipeline_generation += 1;
            self.pipeline_request = None;
            self.dirty = true;
        }
        let fetch = section
            && self.visible
            && self.checks_open
            && self.connected
            && !self.cwd.is_empty()
            && host::supports(ui, &self.host, "forgeCheckDetails");
        let live = fetch
            && super::policy::GITLAB_ACTIVE_PIPELINE_STATUSES.contains(&text(facts, "pipelineStatus"));
        if !fetch && (self.pipeline_fetching || self.pipeline_poll.is_some()) {
            // An interrupted fetch, or a live pipeline's polled jobs, load again
            // once the query is enabled.
            self.pipeline_generation += 1;
            self.pipeline_fetching = false;
            self.pipeline_loading = false;
            self.pipeline_revision = None;
            self.pipeline_request = None;
            self.dirty = true;
        }
        // Polled with re-armed timeouts so a dropped pane leaves no interval behind.
        if live && self.pipeline_poll.is_none() {
            self.pipeline_poll = Some(cx.start_timeout(15.0));
        } else if !live {
            self.pipeline_poll = None;
        }
        if !fetch || Some(pr_revision) == self.pipeline_revision {
            return;
        }
        self.pipeline_revision = Some(pr_revision);
        self.fetch_pipeline(ui, status);
    }

    fn fetch_pipeline(&mut self, ui: &UiContext, status: &Value) {
        let facts = object(status, "forgeSpecific");
        let Some(pipeline_id) = facts.get("pipelineId").filter(|id| !id.is_null()).cloned() else {
            return;
        };
        self.pipeline_generation += 1;
        self.pipeline_fetching = true;
        self.pipeline_loading = self.pipeline.is_none();
        let request = wire::send(
            &ui.core,
            &self.host,
            "checkout.forge.get_check_details.request",
            json!({
                "cwd": self.cwd,
                "checkRunId": pipeline_id,
                "changeRequestNumber": pull_request_number(status),
            }),
        );
        self.pipeline_request = Some((request, self.pipeline_generation));
        self.dirty = true;
    }

    /// Applies this pane's share of an event and re-syncs with the store.
    pub fn process(&mut self, cx: &mut Cx, event: &Event, ui: &UiContext, store: &mut GitStore) -> bool {
        let connected = host::connected(ui, &self.host);
        if connected != self.connected {
            self.connected = connected;
            self.dirty = true;
            if connected {
                // Original reconnect: every checkout query of the host refetches.
                self.timeline_key = None;
                self.pipeline_revision = None;
            }
        }
        for core_event in &ui.events {
            if let CoreEvent::Response { request_id, result, .. } = core_event {
                if self.timeline_request.as_ref().is_some_and(|(id, _)| id == request_id) {
                    let (_, query) = self.timeline_request.take().unwrap();
                    self.on_timeline(cx, store, query, result);
                } else if self.pipeline_request.as_ref().is_some_and(|(id, _)| id == request_id) {
                    let (_, generation) = self.pipeline_request.take().unwrap();
                    self.on_pipeline(generation, result);
                }
            }
        }
        if let Some((timer, _)) = &self.retry
            && timer.is_event(event).is_some()
        {
            let (_, mut query) = self.retry.take().unwrap();
            if query.generation == self.timeline_generation {
                query.failures += 1;
                self.send_timeline(ui, query);
            }
        }
        if let Some(timer) = &self.pipeline_poll
            && timer.is_event(event).is_some()
        {
            let status = pr_status(store.checkout(&self.host, &self.cwd)).clone();
            self.fetch_pipeline(ui, &status);
            self.pipeline_poll = Some(cx.start_timeout(15.0));
        }
        let invalidation = store.invalidation(&self.host, &self.cwd);
        if invalidation != self.seen_invalidation {
            self.seen_invalidation = invalidation;
            self.invalidate_activity(cx, ui, store);
        } else {
            self.sync(cx, ui, store);
        }
        std::mem::take(&mut self.dirty)
    }

    fn on_timeline(
        &mut self,
        cx: &mut Cx,
        store: &mut GitStore,
        query: TimelineQuery,
        result: &Result<Value, paseo_gui_core::CoreError>,
    ) {
        if query.generation != self.timeline_generation {
            return;
        }
        self.dirty = true;
        match wire::checkout_result(result) {
            Ok(payload) => {
                self.timeline = Some(payload);
                self.timeline_fetching = false;
            }
            Err(Failure { code, .. }) => {
                let unsupported = is_unsupported_timeline_error(&code, "pull_request_timeline_request");
                if unsupported {
                    store.mark_timeline_unsupported(self.unsupported_key(query.number));
                }
                match timeline_retry_delay(query.failures, unsupported) {
                    // The query keeps its last data; the pane shows no error for activity.
                    None => self.timeline_fetching = false,
                    Some(delay) => self.retry = Some((cx.start_timeout(delay), query)),
                }
            }
        }
    }

    fn on_pipeline(&mut self, generation: u64, result: &Result<Value, paseo_gui_core::CoreError>) {
        if generation != self.pipeline_generation {
            return;
        }
        self.dirty = true;
        let outcome = wire::checkout_result(result).and_then(|payload| {
            if payload.get("success") == Some(&Value::Bool(false)) {
                Err(Failure::message(
                    wire::payload_error(&payload).unwrap_or_else(|| super::text::t("panels.pullRequest.pipelineJobsFailed")),
                ))
            } else {
                Ok(payload)
            }
        });
        match outcome {
            Ok(payload) => {
                let pipeline = &object(&payload, "details")["pipeline"];
                self.pipeline = pipeline.is_object().then(|| pipeline.clone());
                self.pipeline_error = None;
            }
            Err(failure) => self.pipeline_error = Some(failure.message),
        }
        self.pipeline_fetching = false;
        self.pipeline_loading = false;
    }

    pub fn toggle_checks(&mut self, cx: &mut Cx, ui: &UiContext, store: &mut GitStore) {
        self.checks_open = !self.checks_open;
        self.dirty = true;
        let state = store.checkout(&self.host, &self.cwd);
        let status = pr_status(state).clone();
        let revision = state.map(|s| s.pr_revision).unwrap_or(0);
        self.sync_pipeline(cx, ui, &status, revision);
    }

    pub fn toggle_group(&mut self, group: Check) {
        if !self.collapsed_groups.remove(&group) {
            self.collapsed_groups.insert(group);
        }
        self.dirty = true;
    }

    fn entry_key(number: Option<i64>, id: &str) -> String {
        format!("{}:{id}", number.map(|n| n.to_string()).unwrap_or_else(|| "null".into()))
    }

    /// Original activity-state.ts: an explicit toggle wins over the default.
    pub fn is_collapsed(&self, number: Option<i64>, id: &str, collapsed_by_default: bool) -> bool {
        let key = Self::entry_key(number, id);
        self.collapsed_keys.contains(&key) || (collapsed_by_default && !self.expanded_keys.contains(&key))
    }

    pub fn entry_collapsed(&self, number: Option<i64>, entry: &Entry) -> bool {
        self.is_collapsed(number, &entry.id(), entry.collapsed_by_default())
    }

    pub fn toggle_entry(&mut self, number: Option<i64>, id: &str, collapsed_by_default: bool) {
        let key = Self::entry_key(number, id);
        if self.is_collapsed(number, id, collapsed_by_default) {
            self.collapsed_keys.remove(&key);
            self.expanded_keys.insert(key);
        } else {
            self.expanded_keys.remove(&key);
            self.collapsed_keys.insert(key);
        }
        self.dirty = true;
    }
}
