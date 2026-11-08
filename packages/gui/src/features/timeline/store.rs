//! Module-level replica of every opened conversation. Hidden or unmounted
//! chats keep updating (docs/timeline-sync.md "Hidden chats update the
//! replica"), so sessions live here rather than in mounted widgets. The shell
//! calls [handle_event] once per Makepad event before widget dispatch.

use super::composer_bridge;
use super::json::*;
use super::session::{ChatSession, Io, Pending, RequestRoute, SessionKey};
use crate::common::{UiContext, UiEffect};
use makepad_widgets::{Cx, Event};
use paseo_gui_core::{CoreEvent, CoreHandle, RequestId, response_error};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct HostRecord {
    connected: bool,
    server_info: Value,
}

/// `(host, parent agent, provider subagent)`.
type SubagentKey = (String, String, String);

pub struct TimelineStore {
    pub sessions: HashMap<SessionKey, ChatSession>,
    hosts: HashMap<String, HostRecord>,
    requests: HashMap<RequestId, RequestRoute>,
    core: Option<CoreHandle>,
    app_visible: bool,
    /// provider-store.ts `hiddenFromTrack`: finished provider subagents that
    /// "Delete finished subagents" could only hide, because their daemon lacks
    /// `providerSubagentDelete`. App-wide for this app session; a later
    /// `running` descriptor for the child (it ran again) brings it back.
    hidden_subagents: HashSet<SubagentKey>,
}

thread_local! {
    static STORE: RefCell<TimelineStore> = RefCell::new(TimelineStore::new());
}

pub fn with_store<R>(f: impl FnOnce(&mut TimelineStore) -> R) -> R {
    STORE.with(|store| f(&mut store.borrow_mut()))
}

/// The host's connection and hello `server_info`: the shell's record first
/// (`state.hosts[]` / `ui.server_infos`), then the timeline's own record of
/// Connected/Disconnected events.
fn host_facts(ui: &UiContext, record: Option<&HostRecord>, host_id: &str) -> (bool, Value) {
    let row = ui.host(host_id);
    let connected = row
        .map(|row| get(row, "status") == "connected")
        .or_else(|| record.map(|r| r.connected))
        .unwrap_or(false);
    let server_info = ui
        .server_infos
        .get(host_id)
        .cloned()
        .or_else(|| row.map(|row| get(row, "serverInfo").clone()).filter(|v| v.is_object()))
        .or_else(|| record.map(|r| r.server_info.clone()))
        .unwrap_or(Value::Null);
    (connected, server_info)
}

impl TimelineStore {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            hosts: HashMap::new(),
            requests: HashMap::new(),
            core: None,
            app_visible: true,
            hidden_subagents: HashSet::new(),
        }
    }

    fn server_info(&self, host_id: &str) -> Value {
        self.hosts.get(host_id).map(|h| h.server_info.clone()).unwrap_or(Value::Null)
    }

    /// provider-store.ts `hideFromTrack`: a child whose current descriptor is
    /// running stays on the track.
    pub fn hide_subagents<'a>(&mut self, host_id: &str, parent_agent_id: &str, ids: impl IntoIterator<Item = &'a str>) {
        let parent = SessionKey { host_id: host_id.to_owned(), agent_id: parent_agent_id.to_owned(), subagent_id: None };
        let descriptors = self.sessions.get(&parent).map(|s| s.subagents.as_slice()).unwrap_or_default();
        for id in ids {
            let running = descriptors.iter().any(|d| get(d, "id") == id && get(d, "status") == "running");
            if !running {
                self.hidden_subagents.insert((host_id.to_owned(), parent_agent_id.to_owned(), id.to_owned()));
            }
        }
    }

    pub fn subagent_hidden(&self, host_id: &str, parent_agent_id: &str, id: &str) -> bool {
        // Borrowed lookup without building an owned key per row.
        self.hidden_subagents.iter().any(|(h, p, s)| h == host_id && p == parent_agent_id && s == id)
    }

    /// provider-store.ts `applyUpdate`: an upsert reporting `running` returns
    /// a dismissed child to the track.
    fn note_subagent_update(&mut self, host_id: &str, event: &Value) {
        let payload = get(event, "payload");
        if get(payload, "kind") != "upsert" {
            return;
        }
        let child = get(payload, "subagent");
        self.unhide_if_running(host_id, str_of(get(child, "parentAgentId")), child);
    }

    /// provider-store.ts `replaceList`: the same rule for a listed child.
    fn note_subagent_list(&mut self, host_id: &str, parent_agent_id: &str, body: &Value) {
        for child in list(get(body, "subagents")) {
            self.unhide_if_running(host_id, parent_agent_id, child);
        }
    }

    fn unhide_if_running(&mut self, host_id: &str, parent_agent_id: &str, child: &Value) {
        if get(child, "status") == "running" && !self.hidden_subagents.is_empty() {
            let id = str_of(get(child, "id"));
            self.hidden_subagents.retain(|(h, p, s)| !(h == host_id && p == parent_agent_id && s == id));
        }
    }

    /// Runs `f` with one session and its host plumbing.
    pub fn with_session<R>(
        &mut self,
        cx: &mut Cx,
        effects: &mut Vec<UiEffect>,
        key: &SessionKey,
        f: impl FnOnce(&mut ChatSession, &mut Io) -> R,
    ) -> Option<R> {
        let core = self.core.clone()?;
        let server_info = self.server_info(&key.host_id);
        let create_handoff = !key.is_subagent() && composer_bridge::create_handoff_active(&key.host_id, &key.agent_id);
        let session = self.sessions.get_mut(key)?;
        let mut io = Io {
            cx,
            core: &core,
            server_info: &server_info,
            requests: &mut self.requests,
            effects,
            create_handoff,
        };
        let result = f(session, &mut io);
        if session.ended_create_handoff {
            session.ended_create_handoff = false;
            if !key.is_subagent() {
                composer_bridge::clear_create_handoff(&key.host_id, &key.agent_id);
            }
        }
        Some(result)
    }

    /// A panel shows this conversation: create its replica on first sight.
    pub fn ensure(&mut self, cx: &mut Cx, ui: &mut UiContext, key: &SessionKey) {
        if self.core.is_none() {
            self.core = Some(ui.core.clone());
        }
        let (connected, server_info) = host_facts(ui, self.hosts.get(&key.host_id), &key.host_id);
        let record = self.hosts.entry(key.host_id.clone()).or_default();
        if !server_info.is_null() {
            record.server_info = server_info;
        }
        record.connected = connected;
        if self.sessions.contains_key(key) {
            return;
        }
        self.sessions.insert(key.clone(), ChatSession::new(key.clone(), connected));
        let mut effects = std::mem::take(&mut ui.effects);
        self.with_session(cx, &mut effects, key, |session, io| session.start(io));
        ui.effects = effects;
    }

    /// The tab closed: the chat leaves the selective-delivery set and its
    /// create handoff ends (viewed-timeline-sync.ts commitDesiredMembership).
    pub fn release(&mut self, cx: &mut Cx, key: &SessionKey) {
        let mut effects = Vec::new();
        self.with_session(cx, &mut effects, key, |session, io| session.dispose(io));
        if self.sessions.remove(key).is_some() {
            self.requests.retain(|_, route| route.key != *key);
            if !key.is_subagent() {
                composer_bridge::clear_create_handoff(&key.host_id, &key.agent_id);
            }
        }
    }

    fn keys_for_host(&self, host_id: &str) -> Vec<SessionKey> {
        self.sessions.keys().filter(|k| k.host_id == host_id).cloned().collect()
    }

    fn on_core_event(&mut self, cx: &mut Cx, effects: &mut Vec<UiEffect>, event: &CoreEvent) {
        match event {
            CoreEvent::Connected { host_id, server_info } => {
                let record = self.hosts.entry(host_id.clone()).or_default();
                record.connected = true;
                record.server_info = server_info.clone();
                for key in self.keys_for_host(host_id) {
                    self.with_session(cx, effects, &key, |s, io| s.connection_changed(io, true));
                }
            }
            CoreEvent::Disconnected { host_id, .. } => {
                self.hosts.entry(host_id.clone()).or_default().connected = false;
                for key in self.keys_for_host(host_id) {
                    self.with_session(cx, effects, &key, |s, io| s.connection_changed(io, false));
                }
            }
            CoreEvent::HostEvent { host_id, event } => {
                let ty = str_of(get(event, "type"));
                if ty == "host.identity_changed" || ty == "host.merged" {
                    self.remap_host(event);
                    return;
                }
                if ty == "agent.provider_subagents.update" {
                    self.note_subagent_update(host_id, event);
                }
                let subscription = ty.starts_with("subscription.");
                let keys: Vec<_> = self.sessions.iter()
                    .filter(|(key, session)| key.host_id == *host_id && (subscription || session.wants(event)))
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in keys {
                    self.with_session(cx, effects, &key, |s, io| {
                        if subscription {
                            s.on_subscription_event(io, event);
                        } else {
                            s.on_event(io, event);
                        }
                    });
                }
            }
            CoreEvent::Response { request_id, result, .. } => {
                let Some(route) = self.requests.remove(request_id) else { return };
                // Domain rejections (accepted:false, error, success:false) are failures.
                let result = match result {
                    Ok(payload) => match response_error(payload, &route.request_type) {
                        Some(error) => Err(error),
                        None => Ok(payload.clone()),
                    },
                    Err(error) => Err(error.clone()),
                };
                let key = route.key.clone();
                if matches!(route.pending, Pending::Subagents)
                    && let Ok(body) = &result
                {
                    self.note_subagent_list(&key.host_id, &key.agent_id, body);
                }
                self.with_session(cx, effects, &key, |s, io| match route.pending {
                    Pending::Membership { generation } => s.on_membership_response(io, request_id, generation, result),
                    Pending::Fetch { direction, force_replace, generation } => {
                        s.on_fetch(io, request_id, direction, force_replace, generation, result)
                    }
                    Pending::Subagents => s.on_subagents(result),
                    Pending::Action(kind) => s.on_action(kind, result),
                });
            }
            CoreEvent::State { .. } | CoreEvent::Error { .. } => {}
        }
    }

    /// The core re-keys a host whose server id became known (or merged).
    fn remap_host(&mut self, event: &Value) {
        let payload = get(event, "payload");
        let (Some(previous), Some(next)) = (non_empty_str(get(payload, "previousHostId")), non_empty_str(get(payload, "hostId"))) else {
            return;
        };
        if previous == next {
            return;
        }
        if let Some(record) = self.hosts.remove(previous) {
            self.hosts.insert(next.to_owned(), record);
        }
        if self.hidden_subagents.iter().any(|(h, _, _)| h == previous) {
            self.hidden_subagents = std::mem::take(&mut self.hidden_subagents)
                .into_iter()
                .map(|(h, p, s)| (if h == previous { next.to_owned() } else { h }, p, s))
                .collect();
        }
        let moved: Vec<SessionKey> = self.keys_for_host(previous);
        for key in moved {
            if let Some(mut session) = self.sessions.remove(&key) {
                let mut next_key = key.clone();
                next_key.host_id = next.to_owned();
                session.key = next_key.clone();
                for route in self.requests.values_mut() {
                    if route.key == key {
                        route.key = next_key.clone();
                    }
                }
                self.sessions.insert(next_key, session);
            }
        }
    }
}

/// Shell hook: once per Makepad event, before widget dispatch.
pub fn handle_event(cx: &mut Cx, event: &Event, ui: &mut UiContext) {
    if matches!(event, Event::Signal) && super::highlight::poll() {
        cx.redraw_all();
    }
    with_store(|store| {
        if store.core.is_none() {
            store.core = Some(ui.core.clone());
        }
        let mut effects = std::mem::take(&mut ui.effects);
        let mut image_responses = Vec::new();
        for core_event in ui.events.iter() {
            if let CoreEvent::Response { request_id, result, .. } = core_event
                && super::images::owns(request_id)
            {
                image_responses.push((request_id.clone(), result.clone().map_err(|e| e.message)));
                continue;
            }
            store.on_core_event(cx, &mut effects, core_event);
        }
        super::images::handle_event(cx, event, &image_responses);
        match event {
            Event::Timer(_) => {
                let keys: Vec<SessionKey> = store.sessions.iter()
                    .filter(|(_, session)| session.owns_timer(event))
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in keys {
                    store.with_session(cx, &mut effects, &key, |s, io| s.on_timer(io, event));
                }
            }
            Event::Foreground | Event::Resume | Event::Background | Event::Pause => {
                let visible = matches!(event, Event::Foreground | Event::Resume);
                if store.app_visible != visible {
                    store.app_visible = visible;
                    let keys: Vec<SessionKey> = store.sessions.keys().cloned().collect();
                    for key in keys {
                        store.with_session(cx, &mut effects, &key, |s, io| s.set_app_visible(io, visible));
                    }
                }
            }
            _ => {}
        }
        for session in store.sessions.values_mut() {
            session.flush_projection();
        }
        // Notices from action results surface as the panel toast; sync
        // failures stay in the callout. Nothing else is pushed globally.
        ui.effects.extend(effects);
    });
}

/// Shell hook: a chat tab closed.
pub fn release_session(cx: &mut Cx, _ui: &UiContext, host_id: &str, agent_id: &str) {
    let key = SessionKey { host_id: host_id.to_owned(), agent_id: agent_id.to_owned(), subagent_id: None };
    with_store(|store| store.release(cx, &key));
}

/// Shell hook: a provider-subagent tab closed.
pub fn release_subagent(cx: &mut Cx, _ui: &UiContext, host_id: &str, parent_agent_id: &str, subagent_id: &str) {
    let key = SessionKey {
        host_id: host_id.to_owned(),
        agent_id: parent_agent_id.to_owned(),
        subagent_id: Some(subagent_id.to_owned()),
    };
    with_store(|store| store.release(cx, &key));
}

pub fn server_info(host_id: &str) -> Value {
    with_store(|store| store.server_info(host_id))
}

#[cfg(test)]
mod hidden_subagent_tests {
    use super::*;
    use serde_json::json;

    fn upsert(parent: &str, id: &str, status: &str) -> Value {
        json!({"type": "agent.provider_subagents.update", "payload": {"kind": "upsert",
            "subagent": {"id": id, "parentAgentId": parent, "status": status}}})
    }

    /// Without daemon deletion, hidden finished children belong to the app,
    /// not to one panel or session: later descriptor updates and list
    /// refreshes keep them hidden; only a child that runs again returns.
    #[test]
    fn hidden_provider_subagents_stay_hidden_until_they_run_again() {
        let mut store = TimelineStore::new();
        store.hide_subagents("host", "parent", ["a", "b"]);
        assert!(store.subagent_hidden("host", "parent", "a"));
        assert!(!store.subagent_hidden("host", "other", "a"));
        assert!(!store.subagent_hidden("other", "parent", "a"));

        store.note_subagent_update("host", &upsert("parent", "a", "completed"));
        store.note_subagent_update("host", &upsert("parent", "c", "running"));
        store.note_subagent_list("host", "parent", &json!({"subagents": [
            {"id": "a", "status": "completed"}, {"id": "b", "status": "failed"}, {"id": "c", "status": "running"},
        ]}));
        assert!(store.subagent_hidden("host", "parent", "a"));
        assert!(store.subagent_hidden("host", "parent", "b"));

        store.note_subagent_update("host", &upsert("parent", "a", "running"));
        assert!(!store.subagent_hidden("host", "parent", "a"));
        store.note_subagent_list("host", "parent", &json!({"subagents": [{"id": "b", "status": "running"}]}));
        assert!(!store.subagent_hidden("host", "parent", "b"));
    }

    #[test]
    fn hidden_provider_subagents_follow_a_rekeyed_host() {
        let mut store = TimelineStore::new();
        store.hide_subagents("address", "parent", ["a"]);
        store.remap_host(&json!({"type": "host.identity_changed", "payload": {"previousHostId": "address", "hostId": "server"}}));
        assert!(store.subagent_hidden("server", "parent", "a"));
        assert!(!store.subagent_hidden("address", "parent", "a"));
    }
}
