//! Port of `stream/stream_layout.dart` (agent-stream/turn-membership.ts,
//! spacing.ts, layout.ts footer hosts and tool-calls/detail-level grouping).

use super::json::*;
use makepad_widgets::makepad_draw::text::substr::Substr;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

/// One rendered stream row: a timeline entry, (overview detail level) a run of
/// consecutive tool calls hosted by its latest call, or (turns level) the
/// summary of a finished turn's work.
#[derive(Clone, Debug)]
pub struct StreamRow {
    /// The rendered entry; for a group, the run's latest call; for a turn
    /// summary, a `turn_summary` item in the turn.
    pub entry: Rc<Value>,
    /// Overview group: every call of the run, oldest first.
    pub group: Option<Vec<Rc<Value>>>,
    /// Turn summary: the work it stands for.
    pub fold: Option<Rc<TurnFold>>,
}

/// A finished turn's work before its last reply, folded behind one row under
/// the prompt (agent-stream/turn-collapse/model.ts collapseTurns).
#[derive(Clone, Debug, PartialEq)]
pub struct TurnFold {
    /// The prompt's identity, which keeps the turn open or closed.
    pub key: String,
    /// The folded entries, oldest first: messages, reasoning and tool calls.
    pub entries: Vec<Rc<Value>>,
    /// The folded rows show below the summary.
    pub open: bool,
}

/// How the timeline shows the agent's work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailLevel {
    /// Every entry on its own row.
    Detailed,
    /// Consecutive tool calls share one row.
    Overview,
    /// A finished turn shows its prompt, one summary row and its last reply.
    Turns,
}

impl DetailLevel {
    /// The `toolCallDetailLevel` preference; finished turns fold unless set.
    pub fn from_preference(value: &Value) -> Self {
        match value.as_str() {
            Some("detailed") => Self::Detailed,
            Some("overview") => Self::Overview,
            _ => Self::Turns,
        }
    }
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
    entry
        .map(|e| str_of(get(get(e, "item"), "type")))
        .unwrap_or("")
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
    get(get(item, "detail"), "type") != "plan"
        && str_of(get(item, "name")).trim().to_lowercase() != "speak"
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
    let input = if get(detail, "type") == "unknown" {
        get(detail, "input")
    } else {
        &Value::Null
    };
    let todos = get(input, "todos").as_array()?;
    let mut tasks = Vec::new();
    for todo in todos {
        let content = get(todo, "content").as_str()?;
        let status = str_of(get(todo, "status"));
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return None;
        }
        let active = get(todo, "activeForm")
            .as_str()
            .map(str::trim)
            .unwrap_or("");
        let text = if !active.is_empty() {
            active.to_owned()
        } else {
            content.trim().to_owned()
        };
        let text = if text.is_empty() {
            content.to_owned()
        } else {
            text
        };
        tasks.push(json!({"text": text, "completed": status == "completed"}));
    }
    let mut next = map(entry).clone();
    next.insert(
        "item".into(),
        json!({"type": "todo", "provider": provider, "items": tasks}),
    );
    Some(Rc::new(Value::Object(next)))
}

/// Detailed level: visible entries one per row. Overview level: every run of
/// consecutive groupable calls collapses into one summary row. Turns level:
/// detailed rows, with each finished turn folded (`fold_turns`); `open` holds
/// the keys of turns shown unfolded and `turn_active` keeps the latest turn
/// unfolded while it runs.
pub fn build_stream_rows(
    entries: &[Rc<Value>],
    level: DetailLevel,
    provider: &str,
    open: &HashSet<String>,
    turn_active: bool,
) -> Vec<StreamRow> {
    let overview = level == DetailLevel::Overview;
    let mut rows = Vec::new();
    let mut pending: Vec<Rc<Value>> = Vec::new();
    let flush = |rows: &mut Vec<StreamRow>, pending: &mut Vec<Rc<Value>>| {
        if pending.is_empty() {
            return;
        }
        let group = std::mem::take(pending);
        rows.push(StreamRow {
            entry: group.last().expect("non-empty").clone(),
            group: Some(group),
            fold: None,
        });
    };
    for source in entries {
        let Some(entry) = todo_write_as_todo(source, provider) else {
            continue;
        };
        if !is_visible_stream_entry(&entry) {
            continue;
        }
        if overview && is_groupable_tool_call(&entry) {
            pending.push(entry);
            continue;
        }
        flush(&mut rows, &mut pending);
        rows.push(StreamRow {
            entry,
            group: None,
            fold: None,
        });
    }
    flush(&mut rows, &mut pending);
    if level == DetailLevel::Turns {
        fold_turns(rows, open, turn_active)
    } else {
        rows
    }
}

/// Messages the provider accepted but has not read yet (the agent's `queuedMessages`), as
/// pending user rows, leaving out those already `recorded`. The provider records each one in
/// the timeline where the agent reads it, which also takes it off the queue.
pub fn queued_messages(agent: &Value, recorded: &HashSet<String>) -> Vec<Value> {
    list(get(agent, "queuedMessages"))
        .iter()
        .filter_map(|message| {
            let client = get(message, "clientMessageId").as_str();
            if client.is_some_and(|id| recorded.contains(id)) {
                return None;
            }
            let mut item = json!({"type": "user_message", "text": str_of(get(message, "text"))});
            let mut entry = json!({"pending": true, "queued": true});
            if let Some(client) = client {
                item["clientMessageId"] = json!(client);
                entry["clientMessageId"] = json!(client);
            }
            entry["item"] = item;
            Some(entry)
        })
        .collect()
}

/// The identity a turn's fold is kept under: its prompt's sequence, else its
/// client message id.
pub fn turn_key(prompt: &Value) -> Option<String> {
    if let Some(seq) = get(prompt, "seqStart").as_i64() {
        return Some(format!("seq:{seq}"));
    }
    get(get(prompt, "item"), "clientMessageId")
        .as_str()
        .map(|client| format!("client:{client}"))
}

/// agent-stream/turn-collapse/model.ts collapseTurns: everything a finished
/// turn did before its last message folds into a summary row under its
/// prompt, and the last message stays. A turn folds once it is finished and
/// did something before its reply. Compaction markers and messages the user
/// sent into the running turn stay visible. While the agent runs, the turn of
/// the latest prompt the timeline recorded is unfinished, even with queued
/// messages after it.
fn fold_turns(rows: Vec<StreamRow>, open: &HashSet<String>, turn_active: bool) -> Vec<StreamRow> {
    let prompts: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|&(index, row)| {
            row.kind() == "user_message"
                && (index == 0 || !continues_turn(Some(&rows[index - 1].entry), Some(&row.entry)))
        })
        .map(|(index, _)| index)
        .collect();
    let Some(&first) = prompts.first() else {
        return rows;
    };
    let active_from = if turn_active {
        prompts
            .iter()
            .rev()
            .find(|&&prompt| get(&rows[prompt].entry, "seqStart").is_i64())
            .or(prompts.last())
            .copied()
            .unwrap_or(first)
    } else {
        rows.len()
    };
    let stays = |row: &StreamRow| matches!(row.kind(), "compaction" | "user_message");
    let mut out = Vec::with_capacity(rows.len());
    out.extend(rows[..first].iter().cloned());
    for (n, &prompt) in prompts.iter().enumerate() {
        out.push(rows[prompt].clone());
        let end = prompts.get(n + 1).copied().unwrap_or(rows.len());
        let run = &rows[prompt + 1..end];
        let finished = prompt < active_from;
        let reply = run
            .iter()
            .rposition(|row| row.kind() == "assistant_message");
        let key = turn_key(&rows[prompt].entry);
        let (Some(reply), Some(key), true) = (reply.filter(|&r| r > 0), key, finished) else {
            out.extend(run.iter().cloned());
            continue;
        };
        let folded = &run[..reply];
        let entries: Vec<Rc<Value>> = folded
            .iter()
            .filter(|row| !stays(row))
            .map(|row| row.entry.clone())
            .collect();
        let Some(last) = entries.last() else {
            out.extend(run.iter().cloned());
            continue;
        };
        let mut summary = json!({"item": {"type": "turn_summary"}});
        for field in ["turnId", "timestamp"] {
            let source = if field == "turnId" { &entries[0] } else { last };
            if !get(source, field).is_null() {
                summary[field] = get(source, field).clone();
            }
        }
        let open = open.contains(&key);
        out.push(StreamRow {
            entry: Rc::new(summary),
            group: None,
            fold: Some(Rc::new(TurnFold { key, entries, open })),
        });
        out.extend(folded.iter().filter(|row| open || stays(row)).cloned());
        out.extend(run[reply..].iter().cloned());
    }
    out
}

/// The assistant messages of `rows`, oldest first, including those a closed
/// turn summary folds away.
pub fn response_messages(rows: &[StreamRow]) -> impl Iterator<Item = &Rc<Value>> + Clone {
    rows.iter()
        .flat_map(|row| {
            let folded = row
                .fold
                .as_deref()
                .filter(|fold| !fold.open)
                .map_or(&[][..], |fold| fold.entries.as_slice());
            folded.iter().chain(std::iter::once(&row.entry))
        })
        .filter(|entry| stream_kind(Some(entry)) == "assistant_message")
}

pub fn is_tool_sequence_entry(entry: Option<&Value>) -> bool {
    matches!(
        stream_kind(entry),
        "tool_call" | "reasoning" | "todo" | "turn_summary"
    )
}

/// turn-membership.ts continuesTurn.
pub fn continues_turn(previous: Option<&Value>, next: Option<&Value>) -> bool {
    let (Some(previous), Some(next)) = (previous, next) else {
        return false;
    };
    if !get(previous, "turnId").is_null() && !get(next, "turnId").is_null() {
        return get(previous, "turnId") == get(next, "turnId");
    }
    stream_kind(Some(next)) != "user_message"
}

/// A visible response spans turns whose prompts are absent from the timeline.
pub fn continues_response(previous: Option<&Value>, next: Option<&Value>) -> bool {
    let (Some(_), Some(n)) = (previous, next) else {
        return false;
    };
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
        if self.host == host
            && Weak::ptr_eq(&self.snapshot, &snapshot)
            && let Some(text) = &self.text
        {
            return text.clone();
        }
        let response = if host < rows.len() {
            let mut first = host;
            while first > 0
                && continues_response(Some(&rows[first - 1].entry), Some(&rows[first].entry))
            {
                first -= 1;
            }
            &rows[first..=host]
        } else {
            &rows[..0]
        };
        let entries = response_messages(response);
        let mut previous = self.sources.iter();
        let unchanged = entries
            .clone()
            .all(|entry| previous.next().is_some_and(|old| Rc::ptr_eq(old, entry)))
            && previous.next().is_none();
        if !unchanged || self.text.is_none() {
            self.sources.clear();
            self.sources.extend(entries.cloned());
            let bytes = self
                .sources
                .iter()
                .map(|entry| str_of(get(get(entry, "item"), "text")).len())
                .sum::<usize>()
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
        self.text
            .as_ref()
            .expect("response text initialized above")
            .clone()
    }
}

/// layout.ts resolveCompletedFooter.
pub fn completed_footer_host(
    rows: &[StreamRow],
    index: usize,
    auxiliary_host: Option<usize>,
) -> Option<usize> {
    let row = &rows[index];
    let below = rows.get(index + 1).map(|r| &*r.entry);
    if row.kind() == "user_message"
        || below.is_none()
        || continues_response(Some(&row.entry), below)
    {
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
    let close = |timing: &mut HashMap<usize, TurnTiming>,
                 user_at: Option<i64>,
                 last: Option<i64>,
                 assistants: &[usize]| {
        let Some(completed_at) = last else { return };
        if assistants.is_empty() {
            return;
        }
        for &index in assistants {
            timing.insert(
                index,
                TurnTiming {
                    started_at: user_at,
                    completed_at,
                },
            );
        }
    };
    let mut previous: Option<&Value> = None;
    for (index, row) in rows.iter().enumerate() {
        let entry: &Value = &row.entry;
        let at = parse_time(get(entry, "timestamp"));
        if previous.is_none() || !continues_turn(previous, Some(entry)) {
            close(&mut timing, user_at, last_item_at, &assistants);
            user_at = if stream_kind(Some(entry)) == "user_message" {
                at
            } else {
                None
            };
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
        StreamRow {
            entry: Rc::new(json!({"turnId": turn, "item": {"type": kind, "text": text}})),
            group: None,
            fold: None,
        }
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

#[cfg(test)]
mod turn_fold_tests {
    use super::*;

    fn entry(seq: Option<i64>, turn: &str, kind: &str, text: &str) -> Rc<Value> {
        let mut value = json!({"turnId": turn, "item": {"type": kind, "text": text}});
        if let Some(seq) = seq {
            value["seqStart"] = json!(seq);
        }
        Rc::new(value)
    }

    /// Each row as `kind:text`, a summary as `fold:<key>:<folded count>`.
    fn shape(rows: &[StreamRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match &row.fold {
                Some(fold) => format!("fold:{}:{}", fold.key, fold.entries.len()),
                None => format!(
                    "{}:{}",
                    row.kind(),
                    str_of(get(get(&row.entry, "item"), "text"))
                ),
            })
            .collect()
    }

    fn build(entries: &[Rc<Value>], open: &[&str], running: bool) -> Vec<StreamRow> {
        let open = open.iter().map(|key| key.to_string()).collect();
        build_stream_rows(entries, DetailLevel::Turns, "omp", &open, running)
    }

    fn finished_turn() -> Vec<Rc<Value>> {
        vec![
            entry(Some(1), "a", "user_message", "ask"),
            entry(Some(2), "a", "assistant_message", "looking"),
            entry(Some(3), "a", "tool_call", "read"),
            entry(Some(4), "a", "compaction", ""),
            entry(Some(5), "a", "user_message", "steer"),
            entry(Some(6), "a", "tool_call", "edit"),
            entry(Some(7), "a", "assistant_message", "done"),
        ]
    }

    #[test]
    fn a_finished_turn_folds_its_work_but_keeps_markers_steering_and_the_reply() {
        assert_eq!(
            shape(&build(&finished_turn(), &[], false)),
            [
                "user_message:ask",
                "fold:seq:1:3",
                "compaction:",
                "user_message:steer",
                "assistant_message:done"
            ]
        );
        // Opened, the folded rows follow the summary in order.
        assert_eq!(
            shape(&build(&finished_turn(), &["seq:1"], false)),
            [
                "user_message:ask",
                "fold:seq:1:3",
                "assistant_message:looking",
                "tool_call:read",
                "compaction:",
                "user_message:steer",
                "tool_call:edit",
                "assistant_message:done"
            ]
        );
    }

    #[test]
    fn the_running_turn_stays_unfolded_even_behind_a_queued_message() {
        let mut entries = finished_turn();
        entries.push(entry(None, "", "user_message", "queued"));
        let rows = build(&entries, &[], true);
        assert!(
            rows.iter().all(|row| row.fold.is_none()),
            "{:?}",
            shape(&rows)
        );
        // The next recorded prompt finishes the turn before it.
        let mut entries = finished_turn();
        entries.push(entry(Some(8), "b", "user_message", "next"));
        entries.push(entry(Some(9), "b", "tool_call", "bash"));
        entries.push(entry(Some(10), "b", "assistant_message", "partial"));
        let rows = build(&entries, &[], true);
        assert_eq!(
            shape(&rows)[..3],
            ["user_message:ask", "fold:seq:1:3", "compaction:"]
        );
        assert!(rows[rows.len() - 3..].iter().all(|row| row.fold.is_none()));
    }

    #[test]
    fn a_turn_without_work_before_its_reply_or_without_a_reply_stays_as_is() {
        let entries = [
            entry(Some(1), "a", "user_message", "hi"),
            entry(Some(2), "a", "assistant_message", "hello"),
            entry(Some(3), "b", "user_message", "go"),
            entry(Some(4), "b", "tool_call", "bash"),
        ];
        assert_eq!(
            shape(&build(&entries, &[], false)),
            [
                "user_message:hi",
                "assistant_message:hello",
                "user_message:go",
                "tool_call:bash"
            ]
        );
    }

    #[test]
    fn copying_a_folded_response_includes_its_folded_messages() {
        let rows = Rc::new(build(&finished_turn(), &[], false));
        let reply = rows.len() - 1;
        let mut cache = ResponseCopyCache::default();
        assert_eq!(&*cache.text(&rows, reply), "looking\n\ndone");
    }
}
