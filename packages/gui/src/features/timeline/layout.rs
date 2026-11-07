//! Port of `stream/stream_layout.dart` (agent-stream/turn-membership.ts,
//! spacing.ts, layout.ts footer hosts and tool-calls/detail-level grouping).

use super::json::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use makepad_widgets::makepad_draw::text::substr::Substr;

/// One rendered stream row: a timeline entry, or (overview detail level) a
/// run of consecutive tool calls hosted by its latest call.
#[derive(Clone, Debug)]
pub struct StreamRow {
    /// The rendered entry; for a group, the run's latest call.
    pub entry: Rc<Value>,
    /// Overview group: every call of the run, oldest first.
    pub group: Option<Vec<Rc<Value>>>,
}

impl StreamRow {
    pub fn kind(&self) -> &str {
        stream_kind(Some(&self.entry))
    }

    /// grouping.ts createRun: a run is identified by its first call.
    pub fn group_id(&self) -> String {
        match &self.group {
            Some(group) => str_of(get(get(&group[0], "item"), "callId")).to_owned(),
            None => String::new(),
        }
    }
}

pub fn stream_kind(entry: Option<&Value>) -> &str {
    entry.map(|e| str_of(get(get(e, "item"), "type"))).unwrap_or("")
}

/// projection.ts visibleToolCallItems: approval UI owns pending plans.
pub fn is_visible_stream_entry(entry: &Value) -> bool {
    let item = get(entry, "item");
    if get(item, "type") != "tool_call" {
        return true;
    }
    let name = get(item, "name");
    name != "ExitPlanMode" && !(name == "plan_approval" && get(item, "status") == "running")
}

/// grouping.ts isGroupableToolCall: plans and speak calls stay standalone.
pub fn is_groupable_tool_call(entry: &Value) -> bool {
    let item = get(entry, "item");
    if get(item, "type") != "tool_call" {
        return false;
    }
    get(get(item, "detail"), "type") != "plan" && str_of(get(item, "name")).trim().to_lowercase() != "speak"
}

/// stream.ts reduceTimelineToolCall: a Claude `TodoWrite` call becomes a task
/// list entry; None when its input does not parse (the original drops it).
fn todo_write_as_todo(entry: &Rc<Value>, fallback_provider: &str) -> Option<Rc<Value>> {
    let item = get(entry, "item");
    if get(item, "type") != "tool_call" {
        return Some(entry.clone());
    }
    let provider = str_or(get(entry, "provider"), fallback_provider);
    let name = str_of(get(item, "name")).trim().to_lowercase();
    let mut normalized = String::with_capacity(name.len());
    let mut last_sep = false;
    for c in name.chars() {
        if c == '.' || c == '-' || c.is_whitespace() {
            if !last_sep {
                normalized.push('_');
            }
            last_sep = true;
        } else {
            normalized.push(c);
            last_sep = false;
        }
    }
    if provider != "claude" || (normalized != "todowrite" && normalized != "todo_write") {
        return Some(entry.clone());
    }
    let detail = get(item, "detail");
    let input = if get(detail, "type") == "unknown" { get(detail, "input") } else { &Value::Null };
    let todos = get(input, "todos").as_array()?;
    let mut tasks = Vec::new();
    for todo in todos {
        let content = get(todo, "content").as_str()?;
        let status = str_of(get(todo, "status"));
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return None;
        }
        let active = get(todo, "activeForm").as_str().map(str::trim).unwrap_or("");
        let text = if !active.is_empty() { active.to_owned() } else { content.trim().to_owned() };
        let text = if text.is_empty() { content.to_owned() } else { text };
        tasks.push(json!({"text": text, "completed": status == "completed"}));
    }
    let mut next = map(entry).clone();
    next.insert("item".into(), json!({"type": "todo", "provider": provider, "items": tasks}));
    Some(Rc::new(Value::Object(next)))
}

/// Detailed level: visible entries one per row. Overview level: every run of
/// consecutive groupable calls collapses into one summary row.
pub fn build_stream_rows(entries: &[Rc<Value>], overview: bool, provider: &str) -> Vec<StreamRow> {
    let mut rows = Vec::new();
    let mut pending: Vec<Rc<Value>> = Vec::new();
    let flush = |rows: &mut Vec<StreamRow>, pending: &mut Vec<Rc<Value>>| {
        if pending.is_empty() {
            return;
        }
        let group = std::mem::take(pending);
        rows.push(StreamRow { entry: group.last().expect("non-empty").clone(), group: Some(group) });
    };
    for source in entries {
        let Some(entry) = todo_write_as_todo(source, provider) else { continue };
        if !is_visible_stream_entry(&entry) {
            continue;
        }
        if overview && is_groupable_tool_call(&entry) {
            pending.push(entry);
            continue;
        }
        flush(&mut rows, &mut pending);
        rows.push(StreamRow { entry, group: None });
    }
    flush(&mut rows, &mut pending);
    rows
}

pub fn is_tool_sequence_entry(entry: Option<&Value>) -> bool {
    matches!(stream_kind(entry), "tool_call" | "reasoning" | "todo")
}

/// turn-membership.ts continuesTurn.
pub fn continues_turn(previous: Option<&Value>, next: Option<&Value>) -> bool {
    let (Some(previous), Some(next)) = (previous, next) else { return false };
    if !get(previous, "turnId").is_null() && !get(next, "turnId").is_null() {
        return get(previous, "turnId") == get(next, "turnId");
    }
    stream_kind(Some(next)) != "user_message"
}

/// A visible response spans turns whose prompts are absent from the timeline.
pub fn continues_response(previous: Option<&Value>, next: Option<&Value>) -> bool {
    let (Some(_), Some(n)) = (previous, next) else { return false };
    continues_turn(previous, next) || stream_kind(Some(n)) != "user_message"
}

/// spacing.ts getGapBetweenStreamItems.
pub fn gap_between_stream_items(entry: Option<&Value>, below: Option<&Value>) -> f64 {
    if entry.is_none() || below.is_none() {
        return 0.0;
    }
    let kind = stream_kind(entry);
    let next = stream_kind(below);
    if kind == "user_message" && next == "user_message" {
        return 4.0;
    }
    if kind == "user_message" && next == "assistant_message" {
        return 0.0;
    }
    if is_tool_sequence_entry(entry) && is_tool_sequence_entry(below) {
        return 0.0;
    }
    if kind == "user_message" && is_tool_sequence_entry(below) {
        return 16.0;
    }
    if kind == "assistant_message" && is_tool_sequence_entry(below) {
        return 4.0;
    }
    if is_tool_sequence_entry(entry) && next == "assistant_message" {
        return 4.0;
    }
    16.0
}

/// layout.ts findLatestAssistantInResponse.
pub fn latest_assistant_in_response(rows: &[StreamRow], start: usize) -> Option<usize> {
    let mut later: Option<&Value> = None;
    let mut index = start as isize;
    while index >= 0 && (index as usize) < rows.len() {
        let entry: &Value = &rows[index as usize].entry;
        if later.is_some() && !continues_response(Some(entry), later) {
            return None;
        }
        if stream_kind(Some(entry)) == "assistant_message" {
            return Some(index as usize);
        }
        later = Some(entry);
        index -= 1;
    }
    None
}

/// Retain clipboard text by immutable response sources, not by each frame.
/// A weak snapshot fast path avoids retaining complete historical row vectors.
#[derive(Default)]
pub struct ResponseCopyCache {
    snapshot: Weak<Vec<StreamRow>>,
    host: usize,
    sources: Vec<Rc<Value>>,
    text: Option<Substr>,
}

impl ResponseCopyCache {
    pub fn text(&mut self, rows: &Rc<Vec<StreamRow>>, host: usize) -> Substr {
        let snapshot = Rc::downgrade(rows);
        if self.host == host && Weak::ptr_eq(&self.snapshot, &snapshot)
            && let Some(text) = &self.text
        {
            return text.clone();
        }
        let response = if host < rows.len() {
            let mut first = host;
            while first > 0 && continues_response(Some(&rows[first - 1].entry), Some(&rows[first].entry)) {
                first -= 1;
            }
            &rows[first..=host]
        } else {
            &rows[..0]
        };
        let entries = response.iter().filter(|row| row.kind() == "assistant_message").map(|row| &row.entry);
        let mut previous = self.sources.iter();
        let unchanged = entries.clone().all(|entry| previous.next().is_some_and(|old| Rc::ptr_eq(old, entry)))
            && previous.next().is_none();
        if !unchanged || self.text.is_none() {
            self.sources.clear();
            self.sources.extend(entries.cloned());
            let bytes = self.sources.iter().map(|entry| str_of(get(get(entry, "item"), "text")).len()).sum::<usize>()
                + self.sources.len().saturating_sub(1) * 2;
            let mut text = String::with_capacity(bytes);
            for (index, entry) in self.sources.iter().enumerate() {
                if index > 0 {
                    text.push_str("\n\n");
                }
                text.push_str(str_of(get(get(entry, "item"), "text")));
            }
            self.text = Some(text.into());
        }
        self.snapshot = snapshot;
        self.host = host;
        self.text.as_ref().expect("response text initialized above").clone()
    }
}

/// layout.ts resolveCompletedFooter.
pub fn completed_footer_host(rows: &[StreamRow], index: usize, auxiliary_host: Option<usize>) -> Option<usize> {
    let row = &rows[index];
    let below = rows.get(index + 1).map(|r| &*r.entry);
    if row.kind() == "user_message" || below.is_none() || continues_response(Some(&row.entry), below) {
        return None;
    }
    let host = latest_assistant_in_response(rows, index)?;
    (Some(host) != auxiliary_host).then_some(host)
}

/// layout.ts resolveAuxiliaryTurnFooter.
pub fn auxiliary_footer_host(rows: &[StreamRow], turn_active: bool) -> Option<usize> {
    if turn_active || rows.is_empty() {
        return None;
    }
    latest_assistant_in_response(rows, rows.len() - 1)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TurnTiming {
    /// Unix milliseconds; None for a turn without a visible prompt.
    pub started_at: Option<i64>,
    pub completed_at: i64,
}

pub fn parse_time(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|t| t.timestamp_millis())
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|t| t.and_utc().timestamp_millis())
        })
}

/// timeline/turn-time.ts deriveStreamTurnTiming.
pub fn derive_turn_timing(rows: &[StreamRow], turn_active: bool) -> HashMap<usize, TurnTiming> {
    let mut timing = HashMap::new();
    let mut user_at: Option<i64> = None;
    let mut last_item_at: Option<i64> = None;
    let mut assistants: Vec<usize> = Vec::new();
    let close = |timing: &mut HashMap<usize, TurnTiming>, user_at: Option<i64>, last: Option<i64>, assistants: &[usize]| {
        let Some(completed_at) = last else { return };
        if assistants.is_empty() {
            return;
        }
        for &index in assistants {
            timing.insert(index, TurnTiming { started_at: user_at, completed_at });
        }
    };
    let mut previous: Option<&Value> = None;
    for (index, row) in rows.iter().enumerate() {
        let entry: &Value = &row.entry;
        let at = parse_time(get(entry, "timestamp"));
        if previous.is_none() || !continues_turn(previous, Some(entry)) {
            close(&mut timing, user_at, last_item_at, &assistants);
            user_at = if stream_kind(Some(entry)) == "user_message" { at } else { None };
            last_item_at = None;
            assistants.clear();
        }
        last_item_at = at.or(last_item_at);
        if stream_kind(Some(entry)) == "assistant_message" {
            assistants.push(index);
        }
        previous = Some(entry);
    }
    if !turn_active {
        close(&mut timing, user_at, last_item_at, &assistants);
    }
    timing
}

#[cfg(test)]
mod response_copy_tests {
    use super::*;

    fn row(kind: &str, text: &str, turn: &str) -> StreamRow {
        StreamRow { entry: Rc::new(json!({"turnId": turn, "item": {"type": kind, "text": text}})), group: None }
    }

    #[test]
    fn response_copy_tracks_prepends_replacements_and_response_boundaries() {
        let tail = row("assistant_message", "계속", "current");
        let initial = Rc::new(vec![tail.clone()]);
        let mut cache = ResponseCopyCache::default();
        let original = cache.text(&initial, 0);
        assert_eq!(&*original, "계속");

        let expanded = Rc::new(vec![
            row("assistant_message", "다른 응답", "older"),
            row("user_message", "질문", "current"),
            row("assistant_message", "앞부분", "current"),
            tail,
        ]);
        assert_eq!(&*cache.text(&expanded, 3), "앞부분\n\n계속");
        assert_eq!(&*original, "계속");

        let mut changed = (*expanded).clone();
        changed[3] = row("assistant_message", "수정된 끝", "current");
        let changed = Rc::new(changed);
        assert_eq!(&*cache.text(&changed, 3), "앞부분\n\n수정된 끝");
        assert_eq!(&*cache.text(&changed, 0), "다른 응답");
        assert_eq!(&*cache.text(&changed, changed.len()), "");
    }
}
