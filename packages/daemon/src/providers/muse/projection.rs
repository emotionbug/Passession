use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};

use super::transport::{Connection, Host};
use crate::providers::api::{HistoryCoverage, subagent_row};

/// MSP snapshots remain intact; only the outward projection uses native events.
pub struct Projection {
    host: Arc<dyn Host>,
    native_id: String,
    cwd: String,
    provider: String,
    turn_id: Option<String>,
    messages: HashMap<String, String>,
    timeline: Timeline,
    children: Vec<Child>,
    live: bool,
}

/// A child announced by this parent. `view` is present while this run collects
/// the child's body: children spawned live, and recovered children once an
/// opened replay is applied. A child recovered from resumed history stays a
/// descriptor until then.
struct Child {
    id: String,
    view: Option<View>,
    pending: Option<Pending>,
}

/// An opened child's replay published on the parent stream whose outcome the
/// daemon has not reported yet. Polls of the child pause and its pushes wait,
/// so later rows are projected against whichever view the daemon kept.
struct Pending {
    view: View,
    pushes: Vec<Value>,
}

/// One child view as this client has folded it. The next page starts strictly
/// after `cursor`; `timeline` keeps each item's latest revision, so later pages
/// and pushes project only what changed and never repeat text.
#[derive(Default)]
struct View {
    cursor: Option<String>,
    timeline: Timeline,
}

#[derive(Default)]
struct Timeline {
    items: HashMap<String, Value>,
}

/// What one forward page read produced, in view order.
enum Paged {
    Row(Value, Option<Value>),
    Child(String, Value),
}

/// An opened child's body: its rows (and its own children's descriptors) plus
/// the folded view that live collection continues from. `collected` is true
/// when this live session collects the child and now awaits the replay outcome.
pub struct Replay {
    pub events: Vec<Value>,
    pub coverage: HistoryCoverage,
    pub collected: bool,
    view: View,
}

impl Projection {
    pub fn new(host: Arc<dyn Host>, native_id: String, cwd: String) -> Self {
        Self {
            host,
            native_id,
            cwd,
            provider: "muse".into(),
            turn_id: None,
            messages: HashMap::new(),
            timeline: Timeline::default(),
            children: Vec::new(),
            live: false,
        }
    }

    pub fn set_provider(&mut self, provider: &str) {
        self.provider = provider.into();
    }
    pub fn set_turn_id(&mut self, turn_id: Option<String>) {
        self.turn_id = turn_id;
    }
    pub fn correlate(
        &mut self,
        command_id: impl Into<String>,
        client_message_id: impl Into<String>,
    ) {
        self.messages
            .insert(command_id.into(), client_message_id.into());
    }

    pub async fn fold(&mut self, item: Value) -> Result<Vec<Value>> {
        let item = normalize_item(item)?;
        let child_id = eligible_child(&item).map(str::to_owned);
        let call_id = item.get("callId").unwrap_or(&item["itemId"]).clone();
        let timestamp = item.get("recordedAt").cloned();
        let client_id = item
            .get("commandId")
            .and_then(Value::as_str)
            .and_then(|id| self.messages.get(id))
            .map(String::as_str);
        let (accepted, projected) = self
            .timeline
            .fold(self.host.as_ref(), &self.native_id, item, client_id)
            .await?;
        let mut events = Vec::new();
        if let Some(item) = projected {
            events.push(self.timeline_event(item, timestamp));
        }
        if accepted && let Some(id) = child_id {
            let index = match self.children.iter().position(|child| child.id == id) {
                Some(index) => index,
                None => {
                    events.push(
                        descriptor(
                            self.host.as_ref(),
                            &self.provider,
                            &id,
                            &call_id,
                            None,
                            &self.cwd,
                        )
                        .await?,
                    );
                    self.children.push(Child {
                        id,
                        view: self.live.then(View::default),
                        pending: None,
                    });
                    self.children.len() - 1
                }
            };
            events.extend(self.page_child(index).await?);
        }
        Ok(events)
    }

    pub fn delta(&mut self, params: &Value) -> Result<Vec<Value>> {
        Ok(self
            .timeline
            .delta(params)?
            .map(|item| self.timeline_event(item, None))
            .into_iter()
            .collect())
    }

    /// Items before this point are recovered history; items after it are this
    /// run's live stream, so children they spawn are collected as they run.
    pub fn go_live(&mut self) {
        self.live = true;
    }

    pub async fn child_notify(&mut self, session_id: &str, event: &Value) -> Result<Vec<Value>> {
        let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.id == session_id)
        else {
            return Ok(Vec::new());
        };
        if let Some(pending) = &mut child.pending {
            pending.pushes.push(event.clone());
            return Ok(Vec::new());
        }
        let Some(view) = &mut child.view else {
            return Ok(Vec::new());
        };
        Ok(view
            .apply(self.host.as_ref(), &self.provider, session_id, event)
            .await?
            .into_iter()
            .collect())
    }

    /// Polls every child this run collects; recovered unopened children are not read.
    pub async fn refresh_children(&mut self) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for index in 0..self.children.len() {
            events.extend(self.page_child(index).await?);
        }
        Ok(events)
    }

    async fn page_child(&mut self, index: usize) -> Result<Vec<Value>> {
        let child = &mut self.children[index];
        if child.pending.is_some() {
            return Ok(Vec::new());
        }
        let Some(view) = child.view.as_mut() else {
            return Ok(Vec::new());
        };
        let (paged, _) = view.page(self.host.as_ref(), &child.id).await?;
        Ok(paged
            .into_iter()
            .filter_map(|paged| match paged {
                Paged::Row(item, timestamp) => {
                    Some(live_row(&self.provider, &child.id, item, timestamp))
                }
                Paged::Child(..) => None,
            })
            .collect())
    }

    /// Snapshots an opened child through this live session for publication on
    /// the parent stream. Until `settle_replay` reports the daemon's outcome,
    /// that child is neither polled nor projected, so no row of it can land
    /// between the snapshot and its publication.
    pub async fn replay_child(&mut self, id: &str) -> Result<Replay> {
        ensure!(
            !self
                .children
                .iter()
                .any(|child| child.id == id && child.pending.is_some()),
            "Muse subagent {id} is already awaiting its replay outcome"
        );
        let mut replay = read_child(self.host.as_ref(), &self.provider, id, &self.cwd).await?;
        if let Some(child) = self.children.iter_mut().find(|child| child.id == id) {
            child.pending = Some(Pending {
                view: std::mem::take(&mut replay.view),
                pushes: Vec::new(),
            });
            replay.collected = true;
        }
        Ok(replay)
    }

    /// Resumes a child after the daemon's outcome for its published replay. An
    /// applied replay rebases collection on the replayed end; a retained one
    /// keeps the view whose rows the daemon still stores. Waiting pushes, then a
    /// poll, are projected against that view, so no row repeats or goes missing.
    pub async fn settle_replay(&mut self, id: &str, applied: bool) -> Result<Vec<Value>> {
        let Some(index) = self.children.iter().position(|child| child.id == id) else {
            return Ok(Vec::new());
        };
        let mut events = self.resume(index, applied).await?;
        events.extend(self.page_child(index).await?);
        Ok(events)
    }

    async fn resume(&mut self, index: usize, applied: bool) -> Result<Vec<Value>> {
        let child = &mut self.children[index];
        let Some(pending) = child.pending.take() else {
            return Ok(Vec::new());
        };
        if applied {
            child.view = Some(pending.view);
        }
        let mut events = Vec::new();
        if let Some(view) = &mut child.view {
            for push in &pending.pushes {
                events.extend(
                    view.apply(self.host.as_ref(), &self.provider, &child.id, push)
                        .await?,
                );
            }
        }
        Ok(events)
    }

    /// Completes every child. A replay still awaiting its outcome when the
    /// parent stream ends is treated as retained, like a missing outcome.
    pub async fn close_children(&mut self) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for index in 0..self.children.len() {
            events.extend(self.resume(index, false).await?);
        }
        for child in self.children.drain(..) {
            events.push(json!({
                "type":"provider_subagent","provider":self.provider,
                "event":{"type":"upsert","id":child.id,"status":"completed"}
            }));
        }
        Ok(events)
    }

    fn timeline_event(&self, item: Value, timestamp: Option<Value>) -> Value {
        let mut event = json!({"type":"timeline","provider":self.provider,"item":item});
        if let Some(turn_id) = &self.turn_id {
            event["turnId"] = json!(turn_id);
        }
        if let Some(timestamp) = timestamp {
            event["timestamp"] = timestamp;
        }
        event
    }
}

/// Descriptor-only discovery. MSP `session/read` never attaches or subscribes,
/// and `excludeItems: true` makes it metadata-only (title, workspace); the body
/// is read only by `read_child` when that child is opened, or by live collection.
async fn descriptor(
    host: &dyn Host,
    provider: &str,
    id: &str,
    call_id: &Value,
    parent: Option<&str>,
    fallback_cwd: &str,
) -> Result<Value> {
    let result = host
        .request("session/read", json!({"sessionId":id,"excludeItems":true}))
        .await?;
    let session = object(&result, "session")?;
    string(session, "sessionId")?;
    string(&result, "viewCursor")?;
    let cwd = match session.get("workspaceRoot") {
        Some(Value::String(value)) => value.as_str(),
        Some(Value::Null) => fallback_cwd,
        _ => bail!("Muse child workspaceRoot must be a string or null"),
    };
    optional_string(session, "title")?;
    let mut event = json!({
        "type":"upsert","id":id,"parentSubagentId":parent,"toolCallId":call_id,
        "title":session.get("title").unwrap_or(&Value::Null),"description":null,
        "status":"running","cwd":cwd,"source":{"sessionId":id}
    });
    // A live parent announces a child as it is spawned and completes it when the
    // parent closes. A replayed child's own children are a snapshot, so their
    // native session status decides whether they are still running.
    if parent.is_some() && optional_string(session, "status")? != Some("running") {
        event["status"] = json!("completed");
    }
    Ok(json!({"type":"provider_subagent","provider":provider,"event":event}))
}

/// The native session an opened child reads. Muse descriptor IDs are native
/// session IDs, so a descriptor stored without a `source` still names its
/// session; a stored `source` must name that same session.
pub fn child_session<'a>(child_id: &'a str, source: &Value) -> Result<&'a str> {
    ensure!(!child_id.is_empty(), "Muse subagent ID must not be empty");
    match source {
        Value::Null => Ok(child_id),
        Value::Object(source)
            if source.get("sessionId").and_then(Value::as_str) == Some(child_id) =>
        {
            Ok(child_id)
        }
        _ => bail!("Muse subagent source does not name session {child_id}"),
    }
}

/// Reads one opened child's native view from its first event. Only rows for
/// `child_id` and descriptors for the child's own children are produced; no
/// other session's body is read. Nothing is returned unless every page reads.
pub async fn read_child(
    host: &dyn Host,
    provider: &str,
    child_id: &str,
    cwd: &str,
) -> Result<Replay> {
    let mut view = View::default();
    let (paged, gap) = view.page(host, child_id).await?;
    let coverage = if gap {
        HistoryCoverage::Unverified
    } else {
        HistoryCoverage::Complete
    };
    let mut announced = HashSet::new();
    let mut events = Vec::with_capacity(paged.len());
    for paged in paged {
        match paged {
            Paged::Row(item, timestamp) => events.push(subagent_row(
                provider,
                child_id,
                item,
                timestamp.as_ref(),
                coverage,
            )),
            Paged::Child(id, call_id) => {
                if announced.insert(id.clone()) {
                    events.push(
                        descriptor(host, provider, &id, &call_id, Some(child_id), cwd).await?,
                    );
                }
            }
        }
    }
    Ok(Replay {
        events,
        coverage,
        collected: false,
        view,
    })
}

fn live_row(provider: &str, id: &str, item: Value, timestamp: Option<Value>) -> Value {
    let mut event = json!({"type":"timeline","id":id,"item":item});
    if let Some(timestamp) = timestamp {
        event["timestamp"] = timestamp;
    }
    json!({"type":"provider_subagent","provider":provider,"event":event})
}

impl View {
    /// Projects one pushed notification for this child as its live row, if any.
    async fn apply(
        &mut self,
        host: &dyn Host,
        provider: &str,
        id: &str,
        event: &Value,
    ) -> Result<Option<Value>> {
        let method = string(event, "method")?;
        let params = object(event, "params")?;
        let (item, timestamp) = match method {
            "item/started" | "item/updated" | "item/completed" => {
                let item = normalize_item(object(params, "item")?.clone())?;
                let timestamp = item.get("recordedAt").cloned();
                let (_, projected) = self.timeline.fold(host, id, item, None).await?;
                (projected, timestamp)
            }
            "item/delta" => (self.timeline.delta(params)?, None),
            _ => (None, None),
        };
        Ok(item.map(|item| live_row(provider, id, item, timestamp)))
    }

    /// Pages forward from `cursor` to the end of the view. Returns the projected
    /// rows and child sessions in view order, and whether the host reported a gap.
    async fn page(&mut self, host: &dyn Host, id: &str) -> Result<(Vec<Paged>, bool)> {
        let mut paged = Vec::new();
        let mut gap = false;
        loop {
            let mut params = json!({"sessionId":id,"limit":1000,"direction":"forward"});
            if let Some(cursor) = &self.cursor {
                params["cursor"] = json!(cursor);
            }
            let page = host.request("view/page", params).await?;
            let next = match page.get("nextCursor") {
                Some(Value::String(cursor)) => Some(cursor.clone()),
                Some(Value::Null) => None,
                _ => bail!("Muse page nextCursor must be a string or null"),
            };
            for event in array(&page, "events")? {
                let method = string(event, "method")?;
                let params = object(event, "params")?;
                if let Some(cursor) = optional_string(params, "viewCursor")?
                    && !cursor.is_empty()
                {
                    self.cursor = Some(cursor.into());
                }
                if optional_string(params, "sessionId")?.is_some_and(|session| session != id) {
                    continue;
                }
                match method {
                    "item/started" | "item/updated" | "item/completed" => {
                        let item = normalize_item(object(params, "item")?.clone())?;
                        let timestamp = item.get("recordedAt").cloned();
                        let child = eligible_child(&item).map(str::to_owned);
                        let call_id = item.get("callId").unwrap_or(&item["itemId"]).clone();
                        let (accepted, projected) = self.timeline.fold(host, id, item, None).await?;
                        if let Some(item) = projected {
                            paged.push(Paged::Row(item, timestamp));
                        }
                        if accepted && let Some(child) = child {
                            paged.push(Paged::Child(child, call_id));
                        }
                    }
                    "item/delta" => {
                        if let Some(item) = self.timeline.delta(params)? {
                            paged.push(Paged::Row(item, None));
                        }
                    }
                    // The host no longer retains part of this view.
                    "view/gap" => gap = true,
                    _ => {}
                }
            }
            match next {
                Some(next) => self.cursor = Some(next),
                None => break,
            }
        }
        Ok((paged, gap))
    }
}

impl Timeline {
    async fn fold(
        &mut self,
        host: &dyn Host,
        native_id: &str,
        item: Value,
        client_id: Option<&str>,
    ) -> Result<(bool, Option<Value>)> {
        if item["kind"] == "reminderChild" {
            return Ok((false, None));
        }
        let id = string(&item, "itemId")?;
        let previous = self.items.get(id);
        if previous.is_some_and(|previous| revision(previous) >= revision(&item)) {
            return Ok((false, None));
        }
        let patch =
            if item["status"] == "completed" {
                if let Some(reference) = item.get("patchRef") {
                    let output = host.request("item/readOutput", json!({
                    "sessionId":native_id,"itemId":id,"outputRef":string(reference,"id")?
                })).await?;
                    ensure!(
                        output["encoding"] == "utf8" && output["eof"] == true,
                        "invalidPatch: Muse edit patch is not a complete UTF-8 document"
                    );
                    Some(unified_diff(string(&output, "content")?)?)
                } else {
                    None
                }
            } else {
                None
            };
        let old_text = previous.and_then(|previous| snapshot_text(previous, &item));
        let projected = timeline_item(&item, old_text.as_deref(), client_id, patch.as_deref())?;
        self.items.insert(id.to_owned(), item);
        Ok((true, projected))
    }

    fn delta(&mut self, params: &Value) -> Result<Option<Value>> {
        let id = string(params, "itemId")?;
        let delta = string(params, "delta")?;
        let field = optional_string(params, "field")?.unwrap_or("text");
        let Some(item) = self.items.get_mut(id) else {
            return Ok(None);
        };
        if item["status"] != "inProgress" {
            return Ok(None);
        }
        let old_text = snapshot_text(item, item).map(Cow::into_owned);
        match field {
            "text" | "output" => {
                let key = if field == "output" {
                    "visibleOutput"
                } else {
                    "text"
                };
                if let Value::String(text) = &mut item[key] {
                    text.push_str(delta);
                }
            }
            _ => {
                let Some(index) = field.strip_prefix("summary.").filter(|index| {
                    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
                }) else {
                    return Ok(None);
                };
                let index: usize = index
                    .parse()
                    .map_err(|_| anyhow!("Muse summary index is too large"))?;
                let length = index
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("Muse summary index is too large"))?;
                let summary = item["summary"].as_array_mut().expect("normalized summary");
                if summary.len() < length {
                    summary
                        .try_reserve(length - summary.len())
                        .map_err(|_| anyhow!("Muse summary index is too large"))?;
                    summary.resize(length, Value::Null);
                }
                if summary[index].is_null() {
                    summary[index] = Value::String(String::new());
                }
                if let Value::String(text) = &mut summary[index] {
                    text.push_str(delta);
                }
            }
        }
        timeline_item(item, old_text.as_deref(), None, None)
    }
}

fn revision(item: &Value) -> f64 {
    item["revision"].as_f64().expect("validated revision")
}

fn normalize_item(mut item: Value) -> Result<Value> {
    ensure!(item.is_object(), "Muse item must be an object");
    for key in ["itemId", "kind", "status"] {
        string(&item, key)?;
    }
    let revision = item
        .get("revision")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("Muse item revision must be an integer"))?;
    ensure!(
        revision.fract() == 0.0,
        "Muse item revision must be an integer"
    );
    for key in [
        "childSessionId",
        "role",
        "objective",
        "entryId",
        "commandId",
        "recordedAt",
        "displayText",
        "callId",
        "commandText",
        "failureReason",
    ] {
        optional_string(&item, key)?;
    }
    if let Some(turn_id) = item.get("turnId") {
        ensure!(
            turn_id.is_null() || turn_id.is_string(),
            "Muse turnId must be a string or null"
        );
    }
    for (key, default) in [
        ("text", ""),
        ("message", ""),
        ("tool", ""),
        ("args", "{}"),
        ("visibleOutput", ""),
        ("fallbackText", ""),
    ] {
        if item.get(key).is_none() {
            item[key] = json!(default);
        }
        string(&item, key)?;
    }
    if item.get("summary").is_none() {
        item["summary"] = json!([]);
    }
    ensure!(
        array(&item, "summary")?.iter().all(Value::is_string),
        "Muse summary must contain strings"
    );
    for key in ["exitCode", "tokensBefore"] {
        if let Some(value) = item.get(key) {
            ensure!(value.is_number(), "Muse {key} must be a number");
        }
    }
    if let Some(patch) = item.get("patchRef") {
        string(patch, "id")?;
    }
    Ok(item)
}

fn eligible_child(item: &Value) -> Option<&str> {
    let kind = item["kind"].as_str()?;
    let tool = item["tool"].as_str()?;
    if matches!(kind, "subagent" | "workflow")
        || tool == "workflow"
        || tool.starts_with("subagent_")
    {
        item.get("childSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
    } else {
        None
    }
}

fn snapshot_text<'a>(previous: &'a Value, item: &Value) -> Option<Cow<'a, str>> {
    if previous["kind"] != item["kind"] {
        return None;
    }
    match item["kind"].as_str()? {
        "agentMessage" => Some(previous["text"].as_str()?.into()),
        "userMessage" => Some(
            previous
                .get("displayText")
                .unwrap_or(&previous["text"])
                .as_str()?
                .into(),
        ),
        "reasoning" => Some(Cow::Owned(summary_text(previous))),
        _ => None,
    }
}

fn summary_text(item: &Value) -> String {
    let summary = item["summary"].as_array().expect("normalized summary");
    let length = summary
        .iter()
        .map(|line| line.as_str().unwrap_or("").len())
        .sum::<usize>()
        + summary.len().saturating_sub(1);
    let mut text = String::with_capacity(length);
    for (index, line) in summary.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(line.as_str().unwrap_or(""));
    }
    text
}

fn timeline_item(
    item: &Value,
    previous_text: Option<&str>,
    client_id: Option<&str>,
    patch: Option<&str>,
) -> Result<Option<Value>> {
    let id = string(item, "itemId")?;
    let kind = string(item, "kind")?;
    let result = match kind {
        "userMessage" => {
            let text = item
                .get("displayText")
                .unwrap_or(&item["text"])
                .as_str()
                .expect("normalized text");
            if previous_text == Some(text) {
                return Ok(None);
            }
            let mut result = json!({"type":"user_message","messageId":id,"text":text});
            if let Some(client_id) = client_id {
                result["clientMessageId"] = json!(client_id);
            }
            result
        }
        "agentMessage" | "reasoning" => {
            let text: Cow<'_, str> = if kind == "reasoning" {
                Cow::Owned(summary_text(item))
            } else {
                Cow::Borrowed(string(item, "text")?)
            };
            let delta = text
                .strip_prefix(previous_text.unwrap_or(""))
                .unwrap_or(&text);
            if delta.is_empty() {
                return Ok(None);
            }
            if kind == "reasoning" {
                json!({"type":"reasoning","text":delta})
            } else {
                json!({"type":"assistant_message","messageId":id,"text":delta})
            }
        }
        "compaction" => {
            let mut result = json!({"type":"compaction","status":if item["status"] == "inProgress" {"loading"} else {"completed"}});
            copy(item, "tokensBefore", &mut result, "preTokens");
            result
        }
        "toolCall" | "workflow" | "subagent" => {
            let detail = if kind == "toolCall" {
                tool_detail(item, patch)
            } else {
                subagent_detail(item, None)
            };
            let (status, error) = match string(item, "status")? {
                "inProgress" => ("running", Value::Null),
                "cancelled" => ("canceled", Value::Null),
                "completed" => ("completed", Value::Null),
                _ => (
                    "failed",
                    item.get("failureReason").unwrap_or(&item["status"]).clone(),
                ),
            };
            json!({"type":"tool_call","callId":item.get("callId").unwrap_or(&item["itemId"]),
                "name":if kind == "toolCall" { string(item,"tool")? } else {kind},
                "status":status,"error":error,"detail":detail})
        }
        _ => json!({"type":"tool_call","callId":id,"name":kind,"status":"completed","error":null,
            "detail":{"type":"plain_text","label":kind,"text":item["fallbackText"]}}),
    };
    Ok(Some(result))
}

fn tool_detail(item: &Value, patch: Option<&str>) -> Value {
    let unknown = || json!({"type":"unknown","input":item["args"],"output":item["visibleOutput"]});
    let Ok(args) = serde_json::from_str::<Value>(item["args"].as_str().expect("normalized args"))
    else {
        return unknown();
    };
    if !args.is_object()
        || [
            "objective",
            "role",
            "command",
            "path",
            "find",
            "replace",
            "content",
            "query",
            "pattern",
            "url",
        ]
        .iter()
        .any(|key| args.get(*key).is_some_and(|value| !value.is_string()))
    {
        return unknown();
    }
    let arg = |key| args.get(key).and_then(Value::as_str).unwrap_or("");
    match item["tool"].as_str().expect("normalized tool") {
        "workflow" | "subagent_spawn" | "subagent_wait" | "subagent_read_result" => {
            subagent_detail(item, Some(&args))
        }
        "shell" | "bash" => {
            let mut detail = json!({"type":"shell","command":item.get("commandText").and_then(Value::as_str).unwrap_or_else(||arg("command")),"output":item["visibleOutput"]});
            copy(item, "exitCode", &mut detail, "exitCode");
            detail
        }
        "read_file" => {
            json!({"type":"read","filePath":arg("path"),"content":item["visibleOutput"]})
        }
        "edit_file" => {
            let mut detail = json!({"type":"edit","filePath":arg("path")});
            copy(&args, "find", &mut detail, "oldString");
            copy(&args, "replace", &mut detail, "newString");
            if let Some(patch) = patch {
                detail["unifiedDiff"] = json!(patch);
            }
            detail
        }
        "write_file" => {
            let mut detail = json!({"type":"write","filePath":arg("path")});
            copy(&args, "content", &mut detail, "content");
            detail
        }
        "grep" | "glob" | "ls" | "web_search" => json!({"type":"search",
            "query":([arg("query"),arg("pattern"),arg("path")].into_iter().find(|value| !value.is_empty()).unwrap_or("")),"content":item["visibleOutput"]}),
        "web_fetch" => json!({"type":"fetch","url":arg("url"),"result":item["visibleOutput"]}),
        _ => unknown(),
    }
}

fn subagent_detail(item: &Value, args: Option<&Value>) -> Value {
    let mut detail = json!({"type":"sub_agent","log":(["message","visibleOutput","fallbackText"].iter()
        .filter_map(|key| item.get(*key).and_then(Value::as_str)).find(|value| !value.is_empty()).unwrap_or(""))});
    if let Some(description) = item
        .get("objective")
        .or_else(|| item.get("entryId"))
        .or_else(|| args.and_then(|args| args.get("objective")))
    {
        detail["description"] = description.clone();
    }
    if let Some(role) = item
        .get("role")
        .or_else(|| args.and_then(|args| args.get("role")))
    {
        detail["subAgentType"] = role.clone();
    }
    copy(item, "childSessionId", &mut detail, "childSessionId");
    detail
}

pub fn unified_diff(content: &str) -> Result<String> {
    use std::fmt::Write;
    let patch: Value = serde_json::from_str(content)?;
    let mut output = String::new();
    for file in array(&patch, "files")? {
        let path = string(file, "path")?;
        writeln!(&mut output, "--- a/{path}\n+++ b/{path}")?;
        for hunk in array(file, "hunks")? {
            for key in ["oldStart", "oldLines", "newStart", "newLines"] {
                ensure!(
                    hunk.get(key).is_some_and(Value::is_number),
                    "Muse patch {key} must be numeric"
                );
            }
            writeln!(
                &mut output,
                "@@ -{},{} +{},{} @@",
                hunk["oldStart"], hunk["oldLines"], hunk["newStart"], hunk["newLines"]
            )?;
            let lines = array(hunk, "lines")?;
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    output.push('\n');
                }
                output.push_str(
                    line.as_str()
                        .ok_or_else(|| anyhow!("Muse patch lines must be strings"))?,
                );
            }
            output.push('\n');
        }
    }
    Ok(output)
}

pub fn todo(input: &Value) -> Result<Value> {
    string(input, "viewCursor")?;
    let items = array(input, "items")?
        .iter()
        .map(|item| {
            let text = string(item, "text")?;
            let status = match string(item, "status")? {
                "inProgress" => "in_progress",
                "completed" => "completed",
                _ => "pending",
            };
            optional_string(item, "activeForm")?;
            let mut projected =
                json!({"text":text,"status":status,"completed":status == "completed"});
            copy(item, "activeForm", &mut projected, "activeForm");
            Ok(projected)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"type":"todo","items":items}))
}

pub struct Questions {
    host: Arc<Connection>,
    native_id: String,
    provider: String,
    turn_id: Option<String>,
    pending: Vec<Value>,
    settled: HashSet<String>,
    responses: HashMap<String, Value>,
}

impl Questions {
    pub fn new(host: Arc<Connection>, native_id: String) -> Self {
        Self {
            host,
            native_id,
            provider: "muse".into(),
            turn_id: None,
            pending: Vec::new(),
            settled: HashSet::new(),
            responses: HashMap::new(),
        }
    }
    pub fn set_provider(&mut self, provider: &str) {
        self.provider = provider.into();
    }
    pub fn set_turn_id(&mut self, turn_id: Option<String>) {
        self.turn_id = turn_id;
    }

    pub fn requested(&mut self, input: &Value) -> Result<Vec<Value>> {
        validate_question(input)?;
        let id = string(input, "userInputId")?;
        if self.settled.contains(id) {
            return Ok(Vec::new());
        }
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending["userInputId"] == id)
        {
            *pending = input.clone();
        } else {
            self.pending.push(input.clone());
        }
        let request = permission_request(input, &self.provider);
        Ok(vec![self.event(
            json!({"type":"permission_requested","provider":self.provider,"request":request}),
        )])
    }

    pub fn resolved(&mut self, input: &Value) -> Result<Vec<Value>> {
        let id = string(input, "userInputId")?;
        self.settled.insert(id.into());
        self.pending.retain(|pending| pending["userInputId"] != id);
        let response = self
            .responses
            .remove(id)
            .unwrap_or_else(|| json!({"behavior":"deny","message":"Resolved by provider"}));
        Ok(vec![self.event(json!({"type":"permission_resolved","provider":self.provider,"requestId":id,"resolution":response}))])
    }

    pub fn pending(&self) -> Vec<Value> {
        self.pending
            .iter()
            .map(|request| permission_request(request, &self.provider))
            .collect()
    }

    pub async fn answer(&mut self, id: &str, response: &Value) -> Result<bool> {
        let Some(request) = self
            .pending
            .iter()
            .find(|pending| pending["userInputId"] == id)
        else {
            return Ok(false);
        };
        let (method, params) = if response["behavior"] == "deny" {
            optional_string(response, "message")?;
            let mut params = json!({"sessionId":self.native_id,"userInputId":id});
            copy(response, "message", &mut params, "reason");
            ("userInput/cancel", params)
        } else {
            let answers = question_answers(
                request,
                response
                    .get("updatedInput")
                    .ok_or_else(|| anyhow!("Muse question answer requires updatedInput"))?,
            )?;
            (
                "userInput/answer",
                json!({"sessionId":self.native_id,"userInputId":id,"answers":answers}),
            )
        };
        let previous = self.responses.insert(id.into(), response.clone());
        let result = self.host.command(method, params).await.and_then(|ack| {
            ensure!(
                ack.is_object(),
                "Muse question acknowledgement must be an object"
            );
            Ok(ack)
        });
        if let Err(error) = result {
            if let Some(previous) = previous {
                self.responses.insert(id.into(), previous);
            } else {
                self.responses.remove(id);
            }
            return Err(error);
        }
        Ok(true)
    }

    fn event(&self, mut event: Value) -> Value {
        if let Some(turn_id) = &self.turn_id {
            event["turnId"] = json!(turn_id);
        }
        event
    }
}

fn validate_question(input: &Value) -> Result<()> {
    string(input, "userInputId")?;
    string(input, "toolName")?;
    for question in array(input, "questions")? {
        for key in ["id", "header", "question"] {
            string(question, key)?;
        }
        for option in array(question, "options")? {
            string(option, "label")?;
            optional_string(option, "description")?;
        }
        let selection = object(question, "selection")?;
        ensure!(
            matches!(string(selection, "mode")?, "single" | "multiple"),
            "Muse question selection mode must be single or multiple"
        );
        for key in ["minSelections", "maxSelections"] {
            if let Some(value) = selection.get(key) {
                ensure!(value.is_number(), "Muse question {key} must be numeric");
            }
        }
    }
    Ok(())
}

fn permission_request(input: &Value, provider: &str) -> Value {
    let questions = input["questions"].as_array().expect("validated questions").iter().map(|question| {
        let options = question["options"].as_array().expect("validated options").iter().map(|option| {
            json!({"label":option["label"],"description":option.get("description").and_then(Value::as_str).unwrap_or("")})
        }).collect::<Vec<_>>();
        json!({"id":question["id"],"header":question["header"],"question":question["question"],"options":options,
            "multiSelect":question["selection"]["mode"] == "multiple","allowOther":true,
            "allowEmpty":question["selection"]["minSelections"].as_f64() == Some(0.0)})
    }).collect::<Vec<_>>();
    json!({"id":input["userInputId"],"provider":provider,"name":input["toolName"],"kind":"question","input":{"questions":questions}})
}

fn question_answers(request: &Value, input: &Value) -> Result<Vec<Value>> {
    let supplied = object(input, "answers")?
        .as_object()
        .expect("object checked");
    ensure!(
        supplied.values().all(Value::is_string),
        "Muse question answers must be strings"
    );
    array(request, "questions")?
        .iter()
        .map(|question| {
            let header = string(question, "header")?;
            let text = supplied
                .get(header)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missingAnswer: Answer {header}"))?;
            let options = array(question, "options")?;
            let known = |label: &str| options.iter().any(|option| option["label"] == label);
            if question["selection"]["mode"] == "single" {
                Ok(if known(text) {
                    json!({"questionId":question["id"],"selectedLabel":text})
                } else {
                    json!({"questionId":question["id"],"freeText":text})
                })
            } else {
                let labels: Vec<&str> = text
                    .split(',')
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .collect();
                Ok(if labels.iter().all(|label| known(label)) {
                    json!({"questionId":question["id"],"selectedLabels":labels})
                } else {
                    json!({"questionId":question["id"],"freeText":text})
                })
            }
        })
        .collect()
}

fn copy(source: &Value, key: &str, target: &mut Value, target_key: &str) {
    if let Some(value) = source.get(key) {
        target[target_key] = value.clone();
    }
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Muse {key} must be a string"))
}
fn optional_string<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>> {
    value
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("Muse {key} must be a string"))
        })
        .transpose()
}
fn object<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .get(key)
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow!("Muse {key} must be an object"))
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Muse {key} must be an array"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, status: &str) -> Value {
        normalize_item(json!({"itemId":"i","kind":kind,"revision":1,"status":status})).unwrap()
    }

    type Reply = Box<dyn Fn(&str, &Value) -> Result<Value> + Send + Sync>;

    /// Records every MSP request so tests can prove which sessions were read.
    struct RecordedHost {
        calls: std::sync::Mutex<Vec<(String, Value)>>,
        reply: Reply,
    }

    impl RecordedHost {
        fn new(reply: impl Fn(&str, &Value) -> Result<Value> + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                reply: Box::new(reply),
            })
        }
        fn reads(&self) -> Vec<(String, String)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(method, params)| {
                    (method.clone(), params["sessionId"].as_str().unwrap().to_owned())
                })
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl Host for RecordedHost {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), params.clone()));
            (self.reply)(method, &params)
        }
    }

    fn session_read(id: &str, root: Value, status: &str) -> Value {
        json!({"session":{"sessionId":id,"title":format!("Title {id}"),"workspaceRoot":root,"status":status},
            "history":{"items":null,"mode":"none","noneReason":"excluded"},"viewCursor":format!("v:{id}:1")})
    }

    fn spawn_item(item_id: &str, child: &str, revision: u64, status: &str) -> Value {
        json!({"itemId":item_id,"kind":"toolCall","tool":"subagent_spawn","callId":format!("call-{item_id}"),
            "childSessionId":child,"revision":revision,"status":status,
            "args":"{\"objective\":\"Do it\",\"role\":\"helper\"}"})
    }

    fn item_event(method: &str, session: &str, item: Value) -> Value {
        json!({"method":method,"params":{"sessionId":session,"item":item}})
    }

    fn message(id: &str, revision: u64, status: &str, text: &str) -> Value {
        json!({"itemId":id,"kind":"agentMessage","revision":revision,"status":status,"text":text,
            "recordedAt":"2026-11-03T00:00:00Z"})
    }

    fn upserts(events: &[Value]) -> Vec<&Value> {
        events
            .iter()
            .filter(|event| event["type"] == "provider_subagent" && event["event"]["type"] == "upsert")
            .map(|event| &event["event"])
            .collect()
    }

    type Views = Arc<std::sync::Mutex<HashMap<String, Vec<Value>>>>;

    /// A host whose child views grow over time; `view/page` serves the events
    /// strictly after the requested cursor, as MSP does.
    fn growing_host(views: &Views) -> Arc<RecordedHost> {
        let views = views.clone();
        RecordedHost::new(move |method, params| {
            let id = params["sessionId"].as_str().unwrap();
            match method {
                "session/read" => Ok(session_read(id, Value::Null, "running")),
                "view/page" => {
                    let views = views.lock().unwrap();
                    let events = views.get(id).ok_or_else(|| anyhow!("sessionNotFound"))?;
                    let start = match params.get("cursor") {
                        None => 0,
                        Some(cursor) => {
                            events
                                .iter()
                                .position(|event| event["params"]["viewCursor"] == *cursor)
                                .ok_or_else(|| anyhow!("unknown cursor"))?
                                + 1
                        }
                    };
                    Ok(json!({"nextCursor":null,"events":events[start..]}))
                }
                other => bail!("unexpected request {other}"),
            }
        })
    }

    fn append(views: &Views, session: &str, item: Value) {
        let mut views = views.lock().unwrap();
        let events = views.entry(session.into()).or_default();
        let cursor = format!("v:{session}:{}", events.len() + 1);
        events.push(json!({"method":"item/updated","params":{"sessionId":session,
            "viewCursor":cursor,"item":item}}));
    }

    fn texts(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event["event"]["item"]["text"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn read(method: &str, session: &str) -> (String, String) {
        (method.to_owned(), session.to_owned())
    }

    fn delta(session: &str, item: &str, text: &str) -> Value {
        json!({"method":"item/delta","params":{"sessionId":session,"itemId":item,"delta":text}})
    }

    /// A live parent collecting `child-a`, whose view starts with "Hel".
    async fn collecting() -> (Views, Arc<RecordedHost>, Projection) {
        let views = Views::default();
        append(&views, "child-a", message("m", 1, "inProgress", "Hel"));
        let host = growing_host(&views);
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        projection.go_live();
        let events = projection
            .fold(spawn_item("spawn-a", "child-a", 1, "inProgress"))
            .await
            .unwrap();
        assert_eq!(texts(&events), ["Hel"]);
        (views, host, projection)
    }

    #[tokio::test]
    async fn same_id_delta_after_the_snapshot_follows_an_applied_replay_once() {
        let (views, host, mut projection) = collecting().await;
        let replay = projection.replay_child("child-a").await.unwrap();
        assert!(replay.collected);
        assert_eq!(texts(&replay.events), ["Hel"]);
        // Between the snapshot and the daemon's outcome the child is paused:
        // its pushed delta waits and its poll reads nothing.
        assert!(projection.child_notify("child-a", &delta("child-a", "m", "lo")).await.unwrap().is_empty());
        append(&views, "child-a", message("m", 2, "inProgress", "Hello"));
        let reads = host.reads().len();
        assert!(projection.refresh_children().await.unwrap().is_empty());
        assert_eq!(host.reads().len(), reads);
        // Applied: the delta is projected after the marker, against the replayed
        // end, and the durable revision that carries it adds nothing more.
        let settled = projection.settle_replay("child-a", true).await.unwrap();
        assert_eq!(texts(&settled), ["lo"]);
        assert!(settled.iter().all(|event| event["event"]["id"] == "child-a"
            && event["event"].get("historyComplete").is_none()));
        assert!(projection.refresh_children().await.unwrap().is_empty());
        append(&views, "child-a", message("m", 3, "inProgress", "Hello!"));
        assert_eq!(texts(&projection.refresh_children().await.unwrap()), ["!"]);
    }

    #[tokio::test]
    async fn retained_replay_keeps_the_previous_view_for_later_rows() {
        let (views, _host, mut projection) = collecting().await;
        append(&views, "child-a", message("m", 2, "inProgress", "Hello"));
        let replay = projection.replay_child("child-a").await.unwrap();
        assert_eq!(texts(&replay.events), ["Hel", "lo"]);
        // Retained: the daemon still stores only "Hel", so the rows after it
        // come from the previous view.
        assert_eq!(
            texts(&projection.settle_replay("child-a", false).await.unwrap()),
            ["lo"]
        );
        assert_eq!(
            texts(&projection.child_notify("child-a", &delta("child-a", "m", "!")).await.unwrap()),
            ["!"]
        );
        assert!(projection.refresh_children().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_retained_replay_of_a_recovered_child_leaves_it_unread() {
        let views = Views::default();
        append(&views, "child-a", message("m", 1, "inProgress", "Hel"));
        let host = growing_host(&views);
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        projection
            .fold(spawn_item("spawn-a", "child-a", 1, "inProgress"))
            .await
            .unwrap();
        projection.go_live();
        assert!(projection.replay_child("child-a").await.unwrap().collected);
        assert!(projection.settle_replay("child-a", false).await.unwrap().is_empty());
        append(&views, "child-a", message("m", 2, "inProgress", "Hello"));
        let reads = host.reads().len();
        assert!(projection.refresh_children().await.unwrap().is_empty());
        assert_eq!(host.reads().len(), reads);
    }

    #[tokio::test]
    async fn closing_before_the_outcome_keeps_the_previous_view() {
        let (_views, _host, mut projection) = collecting().await;
        projection.replay_child("child-a").await.unwrap();
        assert!(projection.child_notify("child-a", &delta("child-a", "m", "lo")).await.unwrap().is_empty());
        let closed = projection.close_children().await.unwrap();
        assert_eq!(texts(&closed), ["lo"]);
        assert_eq!(closed.last().unwrap()["event"]["status"], "completed");
    }

    #[tokio::test]
    async fn resumed_parent_history_announces_children_without_reading_their_bodies() {
        let host = RecordedHost::new(|method, params| match method {
            "session/read" => {
                let id = params["sessionId"].as_str().unwrap();
                let root = if id == "child-b" { json!("/worktree") } else { Value::Null };
                Ok(session_read(id, root, "notLoaded"))
            }
            other => bail!("parent history must not request {other}"),
        });
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        let mut events = Vec::new();
        for item in [
            spawn_item("spawn-a", "child-a", 1, "inProgress"),
            spawn_item("spawn-a", "child-a", 2, "completed"),
            spawn_item("spawn-b", "child-b", 1, "completed"),
        ] {
            events.extend(projection.fold(item).await.unwrap());
        }
        assert_eq!(
            upserts(&events),
            [
                &json!({"type":"upsert","id":"child-a","parentSubagentId":null,"toolCallId":"call-spawn-a",
                    "title":"Title child-a","description":null,"status":"running","cwd":"/repo",
                    "source":{"sessionId":"child-a"}}),
                &json!({"type":"upsert","id":"child-b","parentSubagentId":null,"toolCallId":"call-spawn-b",
                    "title":"Title child-b","description":null,"status":"running","cwd":"/worktree",
                    "source":{"sessionId":"child-b"}}),
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| event["type"] == "timeline" || event["event"]["type"] == "upsert"),
            "parent history carries no child rows: {events:?}"
        );
        // The live stream starts; a later update to a recovered child's spawn
        // item and the watchdog poll still never read an unopened body.
        projection.go_live();
        projection
            .fold(spawn_item("spawn-a", "child-a", 3, "completed"))
            .await
            .unwrap();
        assert!(projection.refresh_children().await.unwrap().is_empty());
        assert_eq!(
            host.reads(),
            [read("session/read", "child-a"), read("session/read", "child-b")]
        );
        assert!(
            host.calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, params)| params["excludeItems"] == true),
            "descriptor reads must request metadata only"
        );
    }

    #[tokio::test]
    async fn live_spawned_child_is_collected_as_it_runs_exactly_once() {
        let views = Views::default();
        append(&views, "child-a", message("m", 1, "inProgress", "Hel"));
        let host = growing_host(&views);
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        projection.go_live();
        let events = projection
            .fold(spawn_item("spawn-a", "child-a", 1, "inProgress"))
            .await
            .unwrap();
        assert_eq!(upserts(&events).len(), 1);
        assert_eq!(
            events.last().unwrap(),
            &json!({"type":"provider_subagent","provider":"muse","event":{"type":"timeline","id":"child-a",
                "item":{"type":"assistant_message","messageId":"m","text":"Hel"},"timestamp":"2026-11-03T00:00:00Z"}})
        );
        // A push of an item revision already paged projects nothing.
        let repeated = item_event("item/started", "child-a", message("m", 1, "inProgress", "Hel"));
        assert!(projection.child_notify("child-a", &repeated).await.unwrap().is_empty());
        append(&views, "child-a", message("m", 2, "inProgress", "Hello"));
        let polled = projection.refresh_children().await.unwrap();
        assert_eq!(texts(&polled), ["lo"]);
        // Pushed deltas still stream between polls, then the poll does not repeat them.
        let delta = json!({"method":"item/delta","params":{"sessionId":"child-a","itemId":"m","delta":"!"}});
        assert_eq!(texts(&projection.child_notify("child-a", &delta).await.unwrap()), ["!"]);
        append(&views, "child-a", message("m", 3, "completed", "Hello!"));
        assert!(projection.refresh_children().await.unwrap().is_empty());
        let sibling = item_event("item/started", "child-z", message("z", 1, "inProgress", "No"));
        assert!(projection.child_notify("child-z", &sibling).await.unwrap().is_empty());
        assert_eq!(
            projection.close_children().await.unwrap(),
            [json!({"type":"provider_subagent","provider":"muse",
                "event":{"type":"upsert","id":"child-a","status":"completed"}})]
        );
        assert_eq!(
            host.reads(),
            [
                read("session/read", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
            ]
        );
    }

    #[tokio::test]
    async fn opened_child_replay_continues_live_collection_exactly_once() {
        let views = Views::default();
        append(&views, "child-a", message("m", 1, "inProgress", "Hel"));
        append(&views, "child-b", message("s", 1, "completed", "sibling"));
        let host = growing_host(&views);
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        for item in [
            spawn_item("spawn-a", "child-a", 1, "inProgress"),
            spawn_item("spawn-b", "child-b", 1, "completed"),
        ] {
            projection.fold(item).await.unwrap();
        }
        projection.go_live();

        // Opening a recovered running child replays its body; once the daemon
        // applies it, the child is collected from the replayed end.
        let replay = projection.replay_child("child-a").await.unwrap();
        assert!(projection.settle_replay("child-a", true).await.unwrap().is_empty());
        assert!(replay.coverage == HistoryCoverage::Complete);
        assert_eq!(texts(&replay.events), ["Hel"]);
        assert!(replay.events.iter().all(|event| event["event"]["id"] == "child-a"
            && event["event"]["historyComplete"] == true));
        append(&views, "child-a", message("m", 2, "completed", "Hello"));
        append(&views, "child-a", message("n", 1, "completed", "Done"));
        let live = projection.refresh_children().await.unwrap();
        assert_eq!(texts(&live), ["lo", "Done"]);
        assert_eq!(
            [texts(&replay.events), texts(&live)[..1].to_vec()].concat().concat(),
            "Hello"
        );
        assert!(projection.refresh_children().await.unwrap().is_empty());

        // Reopening replaces the live view; collection resumes after the new replay.
        let reopened = projection.replay_child("child-a").await.unwrap();
        assert!(projection.settle_replay("child-a", true).await.unwrap().is_empty());
        assert_eq!(texts(&reopened.events), ["Hel", "lo", "Done"]);
        append(&views, "child-a", message("n", 2, "completed", "Done."));
        assert_eq!(texts(&projection.refresh_children().await.unwrap()), ["."]);
        assert_eq!(
            host.reads(),
            [
                read("session/read", "child-a"),
                read("session/read", "child-b"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("view/page", "child-a"),
            ]
        );
    }

    #[tokio::test]
    async fn opened_child_replays_only_its_rows_and_announces_its_children() {
        let host = RecordedHost::new(|method, params| {
            match (method, params["sessionId"].as_str().unwrap(), params.get("cursor")) {
                ("view/page", "child-a", None) => Ok(json!({"nextCursor":"c1","events":[
                    item_event("item/completed", "child-a", json!({"itemId":"u","kind":"userMessage",
                        "revision":1,"status":"completed","text":"Objective"})),
                    item_event("item/started", "child-a", message("m", 1, "inProgress", "Hi")),
                    {"method":"item/delta","params":{"sessionId":"child-a","itemId":"m","delta":" there"}},
                ]})),
                ("view/page", "child-a", Some(cursor)) if *cursor == "c1" => Ok(json!({"nextCursor":null,"events":[
                    item_event("item/completed", "child-a", message("m", 2, "completed", "Hi there")),
                    item_event("item/completed", "child-a", spawn_item("spawn-g", "grand", 2, "completed")),
                    item_event("item/completed", "child-b", message("s", 1, "completed", "sibling")),
                    {"method":"turn/completed","params":{"sessionId":"child-a","turnId":"t"}},
                ]})),
                ("session/read", "grand", None) => Ok(session_read("grand", Value::Null, "notLoaded")),
                other => bail!("unexpected read {other:?}"),
            }
        });
        let replay = read_child(host.as_ref(), "muse", "child-a", "/repo").await.unwrap();
        assert!(replay.coverage == HistoryCoverage::Complete);
        let events = replay.events;
        let rows: Vec<_> = events
            .iter()
            .filter(|event| event["event"]["type"] == "timeline")
            .collect();
        assert!(rows.iter().all(|row| row["event"]["id"] == "child-a"
            && row["event"]["historyComplete"] == true));
        assert_eq!(texts(&events), ["Objective", "Hi", " there"]);
        assert!(rows.iter().any(|row| row["event"]["item"]["name"] == "subagent_spawn"));
        assert_eq!(
            upserts(&events),
            [&json!({"type":"upsert","id":"grand","parentSubagentId":"child-a","toolCallId":"call-spawn-g",
                "title":"Title grand","description":null,"status":"completed","cwd":"/repo",
                "source":{"sessionId":"grand"}})]
        );
        assert!(!serde_json::to_string(&events).unwrap().contains("sibling"));
        assert_eq!(
            host.reads(),
            [
                read("view/page", "child-a"),
                read("view/page", "child-a"),
                read("session/read", "grand"),
            ]
        );
    }

    #[tokio::test]
    async fn opened_child_failures_stay_errors_and_gaps_are_unverified() {
        let missing = RecordedHost::new(|method, params| match params.get("cursor") {
            None if method == "view/page" => Ok(json!({"nextCursor":"c1","events":[
                item_event("item/completed", "child-a", message("m", 1, "completed", "partial")),
            ]})),
            _ => bail!("sessionNotFound"),
        });
        let error = read_child(missing.as_ref(), "muse", "child-a", "/repo")
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("sessionNotFound"));
        assert_eq!(missing.reads().len(), 2);

        let malformed = RecordedHost::new(|_, _| Ok(json!({"nextCursor":7,"events":[]})));
        assert!(read_child(malformed.as_ref(), "muse", "child-a", "/repo").await.is_err());

        let unreadable_grandchild = RecordedHost::new(|method, _| match method {
            "view/page" => Ok(json!({"nextCursor":null,"events":[
                item_event("item/completed", "child-a", spawn_item("spawn-g", "grand", 1, "completed")),
            ]})),
            _ => bail!("sessionNotFound"),
        });
        assert!(
            read_child(unreadable_grandchild.as_ref(), "muse", "child-a", "/repo")
                .await
                .is_err()
        );

        let gapped = RecordedHost::new(|_, _| {
            Ok(json!({"nextCursor":null,"events":[
                {"method":"view/gap","params":{"sessionId":"child-a","after":"v:child-a:1"}},
                item_event("item/completed", "child-a", message("m", 1, "completed", "after gap")),
            ]}))
        });
        let replay = read_child(gapped.as_ref(), "muse", "child-a", "/repo")
            .await
            .unwrap();
        assert!(replay.coverage == HistoryCoverage::Unverified);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0]["event"]["historyComplete"], false);
    }

    #[tokio::test]
    async fn opening_a_live_collected_child_replays_matching_rows_then_continues() {
        let views = Views::default();
        append(&views, "child-a", message("m", 1, "inProgress", "Hel"));
        let host = growing_host(&views);
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        projection.go_live();
        let live = projection
            .fold(spawn_item("spawn-a", "child-a", 1, "inProgress"))
            .await
            .unwrap();
        let live_row = live.last().unwrap()["event"]["item"].clone();
        let replay = projection.replay_child("child-a").await.unwrap();
        assert!(projection.settle_replay("child-a", true).await.unwrap().is_empty());
        // The replay carries the stored live row's identity, so the daemon's
        // guarded replacement supersedes it instead of keeping both.
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0]["event"]["item"], live_row);
        append(&views, "child-a", message("m", 2, "completed", "Hello"));
        let after = projection.refresh_children().await.unwrap();
        assert_eq!(texts(&after), ["lo"]);
        assert_eq!(after[0]["event"]["item"]["messageId"], "m");
        assert!(projection.refresh_children().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_replay_keeps_the_live_view_without_repeating_rows() {
        let views = Views::default();
        append(&views, "child-a", message("m", 1, "inProgress", "Hel"));
        let host = growing_host(&views);
        let mut projection = Projection::new(host.clone(), "parent".into(), "/repo".into());
        projection.go_live();
        projection
            .fold(spawn_item("spawn-a", "child-a", 1, "inProgress"))
            .await
            .unwrap();
        let rows = views.lock().unwrap().remove("child-a").unwrap();
        assert!(projection.replay_child("child-a").await.is_err());
        views.lock().unwrap().insert("child-a".into(), rows);
        append(&views, "child-a", message("m", 2, "inProgress", "Hello"));
        assert_eq!(texts(&projection.refresh_children().await.unwrap()), ["lo"]);
    }

    #[test]
    fn child_source_must_name_the_opened_session() {
        assert_eq!(child_session("child-a", &Value::Null).unwrap(), "child-a");
        assert_eq!(
            child_session("child-a", &json!({"sessionId":"child-a"})).unwrap(),
            "child-a"
        );
        assert!(child_session("child-a", &json!({"sessionId":"child-b"})).is_err());
        assert!(child_session("child-a", &json!({"thread":"child-a"})).is_err());
        assert!(child_session("", &Value::Null).is_err());
    }

    #[test]
    fn native_message_projection_emits_suffixes_and_replacement_text() {
        let mut message = item("agentMessage", "inProgress");
        message["text"] = json!("Hello");
        assert_eq!(
            timeline_item(&message, Some("Hel"), None, None).unwrap(),
            Some(json!({"type":"assistant_message","messageId":"i","text":"lo"}))
        );
        assert!(
            timeline_item(&message, Some("Hello"), None, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            timeline_item(&message, Some("Old"), None, None)
                .unwrap()
                .unwrap()["text"],
            "Hello"
        );
        let mut user = item("userMessage", "completed");
        user["text"] = json!("native instructions and prompt");
        user["displayText"] = json!("prompt");
        assert_eq!(
            timeline_item(&user, None, Some("client-1"), None).unwrap(),
            Some(
                json!({"type":"user_message","messageId":"i","text":"prompt","clientMessageId":"client-1"})
            )
        );
        assert!(
            timeline_item(&user, Some("prompt"), None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deltas_default_to_text_and_stop_after_completion() {
        let mut timeline = Timeline::default();
        timeline
            .items
            .insert("i".into(), item("agentMessage", "inProgress"));
        assert_eq!(
            timeline
                .delta(&json!({"itemId":"i","delta":"Hello"}))
                .unwrap()
                .unwrap()["text"],
            "Hello"
        );
        assert_eq!(
            timeline
                .delta(&json!({"itemId":"i","delta":" world"}))
                .unwrap()
                .unwrap()["text"],
            " world"
        );
        assert_eq!(timeline.items["i"]["text"], "Hello world");
        timeline.items.get_mut("i").unwrap()["status"] = json!("completed");
        assert!(
            timeline
                .delta(&json!({"itemId":"i","delta":" ignored"}))
                .unwrap()
                .is_none()
        );
        assert!(
            timeline
                .delta(&json!({"itemId":"absent","delta":" ignored"}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reasoning_sparse_summary_deltas_preserve_line_boundaries() {
        let mut timeline = Timeline::default();
        timeline
            .items
            .insert("i".into(), item("reasoning", "inProgress"));
        assert_eq!(
            timeline
                .delta(&json!({"itemId":"i","field":"summary.2","delta":"Third"}))
                .unwrap()
                .unwrap()["text"],
            "\n\nThird"
        );
        assert_eq!(
            timeline
                .delta(&json!({"itemId":"i","field":"summary.2","delta":" line"}))
                .unwrap()
                .unwrap()["text"],
            " line"
        );
        assert!(
            timeline
                .delta(&json!({"itemId":"i","field":"unsupported","delta":"x"}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn structured_tools_keep_native_identity_and_patch() {
        let mut tool = item("toolCall", "completed");
        tool["tool"] = json!("edit_file");
        tool["args"] = json!("{\"path\":\"file.rs\",\"find\":\"old\",\"replace\":\"new\"}");
        let diff = unified_diff(r#"{"files":[{"path":"file.rs","hunks":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-old","+new"]}]}]}"#).unwrap();
        assert_eq!(
            diff,
            "--- a/file.rs\n+++ b/file.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n"
        );
        assert_eq!(
            timeline_item(&tool, None, None, Some(&diff)).unwrap(),
            Some(json!({
                "type":"tool_call","callId":"i","name":"edit_file","status":"completed","error":null,
                "detail":{"type":"edit","filePath":"file.rs","oldString":"old","newString":"new","unifiedDiff":diff}
            }))
        );
        tool["args"] = json!("{\"path\":42}");
        assert_eq!(tool_detail(&tool, None)["type"], "unknown");
        tool["tool"] = json!("subagent_read_result");
        tool["args"] = json!("{}");
        tool["visibleOutput"] = json!("child result");
        assert_eq!(
            tool_detail(&tool, None),
            json!({"type":"sub_agent","log":"child result"})
        );
        assert!(eligible_child(&tool).is_none());
        tool["childSessionId"] = json!("real-child");
        assert_eq!(eligible_child(&tool), Some("real-child"));
    }

    #[test]
    fn todo_and_question_answers_preserve_selection_behavior() {
        assert_eq!(
            todo(&json!({"viewCursor":"c","items":[
                {"text":"First","status":"inProgress","activeForm":"Doing first"},
                {"text":"Second","status":"completed"},{"text":"Third","status":"unknown"}
            ]}))
            .unwrap(),
            json!({"type":"todo","items":[
                {"text":"First","status":"in_progress","completed":false,"activeForm":"Doing first"},
                {"text":"Second","status":"completed","completed":true},
                {"text":"Third","status":"pending","completed":false}
            ]})
        );
        let request = json!({"userInputId":"q","toolName":"ask","questions":[
            {"id":"color","header":"Color","question":"Which?","options":[{"label":"Blue"},{"label":"Green"}],
                "selection":{"mode":"single"}},
            {"id":"many","header":"Many","question":"Which ones?","options":[{"label":"One"},{"label":"Two"}],
                "selection":{"mode":"multiple","minSelections":0}}
        ]});
        validate_question(&request).unwrap();
        let native = permission_request(&request, "custom-muse");
        assert_eq!(native["provider"], "custom-muse");
        assert_eq!(
            native["input"]["questions"][0]["options"][0]["description"],
            ""
        );
        assert_eq!(native["input"]["questions"][1]["allowEmpty"], true);
        assert_eq!(
            question_answers(
                &request,
                &json!({"answers":{"Color":"Blue","Many":" One, Two, "}})
            )
            .unwrap(),
            vec![
                json!({"questionId":"color","selectedLabel":"Blue"}),
                json!({"questionId":"many","selectedLabels":["One","Two"]})
            ]
        );
        assert_eq!(
            question_answers(
                &request,
                &json!({"answers":{"Color":"Other","Many":"One, Other"}})
            )
            .unwrap(),
            vec![
                json!({"questionId":"color","freeText":"Other"}),
                json!({"questionId":"many","freeText":"One, Other"})
            ]
        );
        assert!(question_answers(&request, &json!({"answers":{"Color":"Blue"}})).is_err());
    }
}
