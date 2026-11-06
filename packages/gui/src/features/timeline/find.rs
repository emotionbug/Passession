//! Port of `chrome/chat_find.dart` ChatFindController (chat-find/model.ts):
//! debounced host search (120ms) over the whole chat through
//! `agent.timeline.search.request`, a position across every match, next /
//! previous stepping through each message's rendered occurrences, older
//! history loaded to reveal a hit, and named failures (connection, history
//! changed, reveal). The controller is a state machine: it emits `Request`s
//! the panel runs and consumes their outcomes.

use super::json::*;
use super::markdown::model::{find_match_count, find_pattern};
use serde_json::Value;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Connection,
    HistoryChanged,
    Reveal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Searching,
    Loading,
    Ready,
    Error,
}

/// Work the panel performs for the controller.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// After 120ms, call `search_due(token)`.
    Debounce { token: u64 },
    Search { token: u64, query: String, cursor: Option<i64> },
    /// Load older history until `seq` is loaded (session.reveal).
    Load { token: u64, seq: i64 },
    /// Wait for the loaded target up to 5s; then `wait_expired(token)`.
    WaitDeadline { token: u64 },
    /// Bring the row starting at `row` on screen with its match.
    ScrollTo { row: i64 },
}

#[derive(Clone, Debug)]
struct Location {
    seq: i64,
    role: String,
    count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Target {
    id: i64,
    seq: i64,
    before: i64,
}

#[derive(Clone, Debug)]
enum Pending {
    None,
    Debounce,
    Search,
    /// Navigation: resolving `index` going `direction`, `remaining` tries left.
    Load { index: usize, direction: i32, remaining: usize },
    Wait { index: usize, direction: i32, remaining: usize },
}

pub struct FindController {
    pub open: bool,
    pub query: String,
    pub phase: Phase,
    pub selected_row: Option<i64>,
    pub occurrence: usize,
    pub count: usize,
    pub failure: Option<Failure>,
    /// Bumped by every `show` (the find bar takes focus and selects the query).
    pub focus_requests: u64,
    locations: Vec<Location>,
    resolved: Vec<(i64, Target)>,
    result_epoch: Option<String>,
    current: Option<usize>,
    in_message: (usize, usize),
    highlighted: bool,
    token: u64,
    pending: Pending,
    pub requests: Vec<Request>,
}

fn row_seq(row: &Value) -> i64 {
    int_or(get(row, "seqEnd"), int_of(get(row, "seqStart")))
}

/// ranges.web.ts findMessageMatches over one loaded row.
pub fn row_match_count(row: &Value, query: &str) -> usize {
    let item = get(row, "item");
    let text = str_of(get(item, "text"));
    match str_of(get(item, "type")) {
        "assistant_message" => find_match_count(text, query),
        "user_message" => find_pattern(query).map(|p| p.find_iter(text).count()).unwrap_or(0),
        _ => 0,
    }
}

impl Default for FindController {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            phase: Phase::Idle,
            selected_row: None,
            occurrence: 0,
            count: 0,
            failure: None,
            focus_requests: 0,
            locations: Vec::new(),
            resolved: Vec::new(),
            result_epoch: None,
            current: None,
            in_message: (0, 0),
            highlighted: false,
            token: 0,
            pending: Pending::None,
            requests: Vec::new(),
        }
    }
}

impl FindController {
    /// The occurrence to highlight in `selected_row`.
    pub fn highlight(&self) -> Option<(String, i64, usize)> {
        (self.open && self.highlighted).then_some(())?;
        self.selected_row.map(|row| (self.query.clone(), row, self.in_message.0))
    }

    fn cancel(&mut self) {
        self.token += 1;
        self.pending = Pending::None;
        self.highlighted = false;
    }

    pub fn show(&mut self) {
        self.focus_requests += 1;
        if self.open {
            return;
        }
        self.open = true;
        if !self.query.is_empty() {
            let query = self.query.clone();
            self.set_query(&query);
        }
    }

    pub fn close(&mut self) {
        self.cancel();
        self.open = false;
        self.phase = Phase::Idle;
        self.selected_row = None;
        self.count = 0;
        self.failure = None;
    }

    pub fn retry(&mut self) {
        let query = self.query.clone();
        self.set_query(&query);
    }

    pub fn set_query(&mut self, value: &str) {
        self.cancel();
        self.locations.clear();
        self.result_epoch = None;
        self.resolved.clear();
        self.current = None;
        self.in_message = (0, 0);
        self.query = value.to_owned();
        self.phase = if value.trim().is_empty() { Phase::Idle } else { Phase::Searching };
        self.selected_row = None;
        self.occurrence = 0;
        self.count = 0;
        self.failure = None;
        if value.trim().is_empty() {
            return;
        }
        self.pending = Pending::Debounce;
        self.requests.push(Request::Debounce { token: self.token });
    }

    pub fn search_due(&mut self, token: u64) {
        if token != self.token || !matches!(self.pending, Pending::Debounce) {
            return;
        }
        self.pending = Pending::Search;
        self.requests.push(Request::Search { token, query: self.query.clone(), cursor: None });
    }

    fn fail(&mut self, failure: Failure) {
        self.cancel();
        self.phase = Phase::Error;
        self.selected_row = None;
        self.failure = Some(failure);
    }

    /// One `agent.timeline.search.response` page (or its failure).
    pub fn on_search(&mut self, token: u64, result: Result<Value, String>, loaded_epoch: Option<&str>, rows: &[Rc<Value>]) {
        if token != self.token || !matches!(self.pending, Pending::Search) {
            return;
        }
        let Ok(page) = result else {
            self.fail(Failure::Connection);
            return;
        };
        let epoch = str_of(get(&page, "epoch")).to_owned();
        if loaded_epoch.is_some_and(|e| e != epoch) || self.result_epoch.as_deref().is_some_and(|e| e != epoch) {
            self.fail(Failure::HistoryChanged);
            return;
        }
        self.result_epoch = Some(epoch);
        for location in list(get(&page, "locations")) {
            self.locations.push(Location {
                seq: int_of(get(location, "seq")),
                role: str_or(get(location, "role"), "assistant").to_owned(),
                // COMPAT(timelineSearchCount): hosts before v0.9.0 send no count.
                count: get(location, "count").as_u64().map(|c| c as usize).unwrap_or(1),
            });
        }
        if let Some(cursor) = get(&page, "nextCursor").as_i64() {
            self.requests.push(Request::Search { token, query: self.query.clone(), cursor: Some(cursor) });
            return;
        }
        self.pending = Pending::None;
        self.navigate(0, 1, self.locations.len(), loaded_epoch, rows);
    }

    /// model.ts updateHistory: the loaded timeline changed.
    pub fn history_changed(&mut self, epoch: Option<&str>, rows: &[Rc<Value>]) {
        if let (Some(result), Some(epoch)) = (self.result_epoch.as_deref(), epoch)
            && result != epoch
            && self.open
            && self.phase != Phase::Error
        {
            self.fail(Failure::HistoryChanged);
            return;
        }
        if let Pending::Wait { index, direction, remaining } = self.pending.clone()
            && let Some(location) = self.locations.get(index).cloned()
            && let Some(target) = self.target(&location, false, epoch, rows)
        {
            self.pending = Pending::None;
            if let Err(next) = self.resolved_target(index, direction, target, rows) {
                self.navigate(next, direction, remaining, epoch, rows);
            }
        }
    }

    pub fn wait_expired(&mut self, token: u64) {
        if token == self.token && matches!(self.pending, Pending::Wait { .. }) {
            self.fail(Failure::Reveal);
        }
    }

    /// The older-history load for a location finished.
    pub fn on_load(&mut self, token: u64, ok: bool, epoch: Option<&str>, rows: &[Rc<Value>]) {
        if token != self.token {
            return;
        }
        let Pending::Load { index, direction, remaining } = self.pending.clone() else { return };
        if !ok {
            self.fail(Failure::Connection);
            return;
        }
        self.pending = Pending::Wait { index, direction, remaining };
        self.requests.push(Request::WaitDeadline { token });
        self.history_changed(epoch, rows);
    }

    /// model.ts target: the loaded row of the location's role holding it.
    fn target(&self, location: &Location, exact: bool, epoch: Option<&str>, rows: &[Rc<Value>]) -> Option<Target> {
        if epoch != self.result_epoch.as_deref() {
            return None;
        }
        let mut before = 0;
        for row in rows {
            let start = int_of(get(row, "seqStart"));
            let seq = row_seq(row);
            if seq < location.seq {
                before = before.max(seq);
                continue;
            }
            let kind = str_of(get(get(row, "item"), "type"));
            if kind == format!("{}_message", location.role) && (!exact || start <= location.seq) {
                return Some(Target { id: start, seq, before });
            }
            if kind == "user_message" {
                return None;
            }
        }
        None
    }

    fn navigate(&mut self, mut index: usize, direction: i32, mut remaining: usize, epoch: Option<&str>, rows: &[Rc<Value>]) {
        if self.result_epoch.is_none() {
            return;
        }
        self.phase = Phase::Loading;
        self.failure = None;
        while remaining > 0 && !self.locations.is_empty() {
            remaining -= 1;
            let len = self.locations.len() as i64;
            index = (((index as i64) % len + len) % len) as usize;
            let location = self.locations[index].clone();
            let cached = self.resolved.iter().find(|(seq, _)| *seq == location.seq).map(|(_, t)| *t);
            let still_loaded = cached.filter(|t| rows.iter().any(|r| int_of(get(r, "seqStart")) == t.id && row_seq(r) == t.seq));
            match still_loaded.or_else(|| self.target(&location, true, epoch, rows)) {
                Some(target) => match self.resolved_target(index, direction, target, rows) {
                    Ok(()) => return,
                    // The location had no rendered match: continue with the next.
                    Err(next) => index = next,
                },
                None => {
                    self.pending = Pending::Load { index, direction, remaining };
                    self.requests.push(Request::Load { token: self.token, seq: location.seq });
                    return;
                }
            }
        }
        self.highlighted = false;
        self.phase = Phase::Ready;
        self.selected_row = None;
        self.count = 0;
        self.occurrence = 0;
    }

    /// A location resolved to a loaded row: collapse duplicate hits and reveal
    /// the row. Err carries the index to continue from when the row holds no
    /// rendered match (the location is dropped).
    fn resolved_target(&mut self, index: usize, direction: i32, target: Target, rows: &[Rc<Value>]) -> Result<(), usize> {
        let location = self.locations[index].clone();
        self.resolved.retain(|(seq, _)| *seq != location.seq);
        self.resolved.push((location.seq, target));
        // Several host hits may belong to one displayed message.
        let keep_seq = location.seq;
        let role = location.role.clone();
        self.locations.retain(|c| c.seq == keep_seq || c.role != role || !(c.seq > target.before && c.seq <= target.seq));
        let index = self.locations.iter().position(|c| c.seq == keep_seq).unwrap_or(0);
        if self.selected_row != Some(target.id) {
            self.highlighted = false;
        }
        self.selected_row = Some(target.id);
        let reveal = self.reveal(target.id, if direction == 1 { 0 } else { -1 }, rows);
        if reveal.1 > 0 {
            self.current = Some(index);
            self.phase = Phase::Ready;
            self.position(index, reveal);
            return Ok(());
        }
        self.locations.remove(index);
        let next = if direction == -1 { index as i64 - 1 } else { index as i64 };
        let len = self.locations.len().max(1) as i64;
        Err((((next % len) + len) % len) as usize)
    }

    /// viewport.web.ts reveal: the row's rendered occurrence count; a negative
    /// occurrence names the last one.
    fn reveal(&mut self, row: i64, occurrence: i64, rows: &[Rc<Value>]) -> (usize, usize) {
        let Some(entry) = rows.iter().find(|r| int_of(get(r, "seqStart")) == row) else { return (0, 0) };
        self.requests.push(Request::ScrollTo { row });
        let count = row_match_count(entry, &self.query);
        if count == 0 {
            return (0, 0);
        }
        self.highlighted = true;
        let occurrence = if occurrence < 0 { count - 1 } else { (occurrence as usize).min(count - 1) };
        (occurrence, count)
    }

    fn position(&mut self, index: usize, revealed: (usize, usize)) {
        self.in_message = revealed;
        self.locations[index].count = revealed.1;
        let mut before = 0;
        let mut total = 0;
        for (i, location) in self.locations.iter().enumerate() {
            if i == index {
                before = total;
            }
            total += location.count;
        }
        self.occurrence = before + revealed.0;
        self.count = total;
    }

    fn step(&mut self, direction: i32, epoch: Option<&str>, rows: &[Rc<Value>]) {
        if self.phase != Phase::Ready || self.count == 0 {
            return;
        }
        self.cancel();
        let next = self.in_message.0 as i64 + direction as i64;
        if let Some(row) = self.selected_row
            && next >= 0
            && (next as usize) < self.in_message.1
        {
            let result = self.reveal(row, next, rows);
            if result.1 > 0 {
                self.phase = Phase::Ready;
                if let Some(current) = self.current {
                    self.position(current, result);
                }
                return;
            }
        }
        let start = self.current.map(|c| c as i64 + direction as i64).unwrap_or(0);
        let len = self.locations.len().max(1) as i64;
        let start = ((start % len) + len) % len;
        self.navigate(start as usize, direction, self.locations.len(), epoch, rows);
    }

    pub fn next(&mut self, epoch: Option<&str>, rows: &[Rc<Value>]) {
        self.step(1, epoch, rows);
    }

    pub fn previous(&mut self, epoch: Option<&str>, rows: &[Rc<Value>]) {
        self.step(-1, epoch, rows);
    }

    pub fn token(&self) -> u64 {
        self.token
    }
}
