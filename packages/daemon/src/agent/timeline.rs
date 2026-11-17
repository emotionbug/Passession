use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

#[path = "timeline_stream.rs"]
mod stream;
pub use stream::{StreamCoalescer, SubagentStorageUnavailable, SubagentStore, public_subagent};
#[path = "timeline_history.rs"]
mod history;
#[path = "timeline_reconcile.rs"]
mod reconcile;
#[path = "timeline_search.rs"]
mod search;
#[path = "timeline_storage.rs"]
mod storage;

pub(super) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn num(row: &Value, key: &str) -> i64 {
    row[key].as_i64().unwrap_or(0)
}
fn kind(item: &Value) -> &str {
    item["type"].as_str().unwrap_or("")
}
fn identity(item: &Value) -> Option<String> {
    match kind(item) {
        "tool_call" => item["callId"].as_str().map(str::to_owned),
        "plugin" => Some(format!(
            "{}/{}",
            item["pluginId"].as_str()?,
            item["id"].as_str()?
        )),
        _ => None,
    }
}

pub fn merge_tool_items(existing: &Value, incoming: &Value) -> Value {
    let mut result = existing.clone();
    if let (Some(out), Some(next)) = (result.as_object_mut(), incoming.as_object()) {
        out.extend(next.iter().map(|(k, v)| (k.clone(), v.clone())));
        if incoming["detail"]["type"] == "unknown" && existing["detail"]["type"] != "unknown" {
            out.insert("detail".into(), existing["detail"].clone());
        }
        if existing["metadata"].is_object() || incoming["metadata"].is_object() {
            let mut metadata = existing["metadata"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            if let Some(next) = incoming["metadata"].as_object() {
                metadata.extend(next.clone());
            }
            out.insert("metadata".into(), Value::Object(metadata));
        }
        match incoming["status"].as_str() {
            Some("completed" | "canceled") => {
                out.insert("error".into(), Value::Null);
            }
            Some("failed") if incoming.get("error").is_none() => {
                out.remove("error");
            }
            _ => {}
        }
    }
    result
}

fn canonical(mut row: Value) -> Value {
    if row.get("seqStart").is_none() {
        let seq = row["seq"].clone();
        row["seqStart"] = seq.clone();
        row["seqEnd"] = seq.clone();
        row["sourceSeqRanges"] = json!([{"startSeq":seq,"endSeq":seq}]);
        row["collapsed"] = json!([]);
    }
    row["seq"] = row["seqEnd"].clone();
    row
}
fn merge_metadata(target: &mut Value, incoming: &Value, collapse: &str) {
    let ranges = target["sourceSeqRanges"]
        .as_array_mut()
        .expect("canonical ranges");
    if let Some(next) = incoming["sourceSeqRanges"].as_array() {
        for range in next {
            if let Some(last) = ranges.last_mut()
                && num(range, "startSeq") <= num(last, "endSeq") + 1
            {
                last["endSeq"] = json!(num(last, "endSeq").max(num(range, "endSeq")));
                continue;
            }
            ranges.push(range.clone());
        }
    }
    let collapsed = target["collapsed"]
        .as_array_mut()
        .expect("canonical collapse");
    if let Some(next) = incoming["collapsed"].as_array() {
        for value in next {
            if !collapsed.contains(value) {
                collapsed.push(value.clone());
            }
        }
    }
    if !collapsed.iter().any(|v| v == collapse) {
        collapsed.push(json!(collapse));
    }
}
fn merge_identity(existing: &mut Value, incoming: &Value) -> bool {
    let collapse = match kind(&incoming["item"]) {
        "tool_call"
            if kind(&existing["item"]) == "tool_call"
                && existing.get("turnId") == incoming.get("turnId") =>
        {
            existing["item"] = merge_tool_items(&existing["item"], &incoming["item"]);
            existing["timestamp"] = incoming["timestamp"].clone();
            existing["seqEnd"] = json!(num(existing, "seqEnd").max(num(incoming, "seqEnd")));
            "tool_lifecycle"
        }
        "plugin" if kind(&existing["item"]) == "plugin" => {
            let start = existing["seqStart"].clone();
            let ranges = existing["sourceSeqRanges"].clone();
            let collapsed = existing["collapsed"].clone();
            *existing = incoming.clone();
            existing["seqStart"] = start;
            existing["sourceSeqRanges"] = ranges;
            existing["collapsed"] = collapsed;
            "identity"
        }
        _ => return false,
    };
    merge_metadata(existing, incoming, collapse);
    existing["seq"] = existing["seqEnd"].clone();
    true
}
fn merge_text(previous: &mut Value, next: &Value) -> bool {
    let ty = kind(&previous["item"]);
    if !matches!(ty, "assistant_message" | "reasoning")
        || ty != kind(&next["item"])
        || num(previous, "seqEnd") + 1 != num(next, "seqStart")
        || previous.get("turnId") != next.get("turnId")
        || previous["item"]["metadata"].get("ompStreamMessageId")
            != next["item"]["metadata"].get("ompStreamMessageId")
    {
        return false;
    }
    if ty == "assistant_message"
        && next["item"].get("messageId").is_some()
        && previous["item"].get("messageId") != next["item"].get("messageId")
    {
        return false;
    }
    let collapse = if ty == "reasoning" {
        "reasoning_merge"
    } else {
        "assistant_merge"
    };
    let text = next["item"]["text"].as_str().unwrap_or("");
    if let Some(Value::String(previous_text)) = previous["item"].get_mut("text") {
        previous_text.push_str(text);
    }
    previous["timestamp"] = next["timestamp"].clone();
    previous["seqEnd"] = next["seqEnd"].clone();
    previous["seq"] = next["seqEnd"].clone();
    merge_metadata(previous, next, collapse);
    true
}

/// Batch projection uses identity collapse before adjacent text merges, matching the SDK curator.
pub fn project_rows(rows: &[Value]) -> Vec<Value> {
    let mut collapsed: Vec<Value> = Vec::with_capacity(rows.len());
    let mut identities = HashMap::new();
    for row in rows {
        let entry = canonical(row.clone());
        if let Some(id) = identity(&entry["item"]) {
            if let Some(&index) = identities.get(&id)
                && merge_identity(&mut collapsed[index], &entry)
            {
                continue;
            }
            identities.insert(id, collapsed.len());
        }
        collapsed.push(entry);
    }
    let mut output: Vec<Value> = Vec::with_capacity(collapsed.len());
    for entry in collapsed {
        if output
            .last_mut()
            .is_some_and(|previous| merge_text(previous, &entry))
        {
            continue;
        }
        output.push(entry);
    }
    output
}

/// Rows a loaded agent keeps in memory. A client opens a chat with a 40-item
/// tail page and model observations default to 200-row pages, so 256 rows
/// answer the live tail, catch-up pages and in-place updates of the current
/// turn without SQLite. The byte budget caps a tail of rows near the 64 KiB
/// tool-output limit. Older rows stay in SQLite and are read on demand; the
/// newest row always stays resident because streamed text extends it.
const RESIDENT_ROWS: usize = 256;
const RESIDENT_BYTES: u64 = 4 << 20;

struct PageRequest<'a> {
    direction: &'a str,
    stale: bool,
    gap: bool,
    mode: &'a str,
    cursor: Option<i64>,
    limit: usize,
}

#[derive(Debug)]
pub struct Timeline {
    epoch: String,
    /// Resident projected rows, at positions `base..base + rows.len()`.
    rows: Vec<Value>,
    /// Stored JSON length of each resident row while storage is in sync.
    sizes: Vec<u64>,
    /// Leading rows that live only in `storage`.
    base: usize,
    /// Largest `seqEnd` of a non-resident row; `i64::MAX` when unknown.
    evicted_seq_end: i64,
    /// Latest resident position of each tool or plugin identity.
    identities: HashMap<String, usize>,
    /// First resident position of each submitted client message.
    submitted: HashMap<String, usize>,
    next_seq: i64,
    min_seq: i64,
    storage: Option<storage::Store>,
    storage_dirty: bool,
    /// Certified native revision for these exact committed rows; never public.
    native_revision: Option<Value>,
    /// Agent projections keep only a bounded tail resident once durable.
    bounded: bool,
}
impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}
impl Timeline {
    pub fn new() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            rows: Vec::new(),
            sizes: Vec::new(),
            base: 0,
            evicted_seq_end: 0,
            identities: HashMap::new(),
            submitted: HashMap::new(),
            next_seq: 1,
            min_seq: 0,
            storage: None,
            storage_dirty: false,
            native_revision: None,
            bounded: false,
        }
    }
    pub fn seed(options: &Value) -> Result<Self> {
        let mut timeline = Self::new();
        if let Some(epoch) = options["epoch"].as_str() {
            timeline.epoch = epoch.into();
        }
        timeline.next_seq = options["nextSeq"].as_i64().unwrap_or(1);
        if timeline.next_seq < 1 {
            bail!("Timeline nextSeq must be positive");
        }
        if let Some(rows) = options["rows"].as_array().filter(|r| !r.is_empty()) {
            for row in rows {
                let seq = row["seq"].as_i64().context("Timeline row missing seq")?;
                if seq < 1 || !row["item"].is_object() || !row["timestamp"].is_string() {
                    bail!("Invalid timeline row");
                }
                timeline.next_seq = timeline.next_seq.max(seq + 1);
                timeline.append_row(row.clone());
            }
        } else if let Some(items) = options["items"].as_array() {
            let timestamp = options["timestamp"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(now);
            let append_options = json!({"timestamp":timestamp});
            for item in items {
                timeline.append(item.clone(), &append_options);
            }
        }
        Ok(timeline)
    }
    pub fn reset(&mut self, options: &Value) -> Result<()> {
        *self = Self::seed(options)?;
        Ok(())
    }
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    pub fn next_seq(&self) -> i64 {
        self.next_seq
    }
    fn len(&self) -> usize {
        self.base + self.rows.len()
    }
    /// Resident items: the whole history of a timeline that never released
    /// rows. Agent readers use `all_items`.
    pub fn items(&self) -> Vec<Value> {
        debug_assert_eq!(self.base, 0, "timeline rows are not all resident");
        self.rows.iter().map(|r| r["item"].clone()).collect()
    }
    /// Resident rows; see `items`. Agent readers use `all_rows`.
    pub fn rows(&self) -> Vec<Value> {
        debug_assert_eq!(self.base, 0, "timeline rows are not all resident");
        self.rows.clone()
    }
    pub fn enrich_submitted_user_message(&mut self, id: &str, provider_id: &str) -> Option<Value> {
        let index = *self.submitted.get(id)? - self.base;
        let row = &mut self.rows[index];
        self.storage_dirty = true;
        self.native_revision = None;
        row["providerMessageId"] = json!(provider_id);
        Some(row.clone())
    }
    pub fn enrich_submitted_persisted(
        &mut self,
        path: &Path,
        agent_id: &str,
        message_id: &str,
        provider_id: &str,
    ) -> Result<Option<Value>> {
        storage::enrich(self, path, agent_id, message_id, provider_id)
    }
    pub fn append(&mut self, item: Value, options: &Value) -> Value {
        self.storage_dirty = true;
        self.native_revision = None;
        let row = self.source_row(item, options);
        self.next_seq += 1;
        self.append_row(row.clone());
        row
    }
    /// The source row for the next sequence number; it is not yet applied.
    fn source_row(&self, item: Value, options: &Value) -> Value {
        let item = limit_item_content(item);
        let mut row = json!({"seq":self.next_seq,"timestamp":options["timestamp"].as_str().map(str::to_owned).unwrap_or_else(now),"item":item});
        for field in ["turnId", "providerMessageId"] {
            if let Some(value) = options[field].as_str().filter(|s| !s.is_empty()) {
                row[field] = json!(value);
            }
        }
        row
    }
    /// Projects `row` into the resident tail and returns the affected
    /// position. A stored identity outside the tail is merged by `storage`.
    fn append_row(&mut self, row: Value) -> usize {
        let entry = canonical(row);
        if self.min_seq == 0 {
            self.min_seq = num(&entry, "seqStart");
        }
        if let Some(id) = identity(&entry["item"]) {
            if let Some(&position) = self.identities.get(&id)
                && merge_identity(&mut self.rows[position - self.base], &entry)
            {
                return position;
            }
            self.identities.insert(id, self.len());
        }
        if self
            .rows
            .last_mut()
            .is_some_and(|previous| merge_text(previous, &entry))
        {
            return self.len() - 1;
        }
        if kind(&entry["item"]) == "user_message"
            && let Some(id) = entry["item"]["clientMessageId"]
                .as_str()
                .filter(|s| !s.is_empty())
        {
            let position = self.len();
            self.submitted.entry(id.into()).or_insert(position);
        }
        self.rows.push(entry);
        self.len() - 1
    }
    /// Releases the oldest durable rows beyond the resident budget. Rows that
    /// are not yet committed, or whose stored size is unknown, stay resident.
    fn trim(&mut self) {
        if !self.bounded
            || self.storage.is_none()
            || self.storage_dirty
            || self.sizes.len() != self.rows.len()
        {
            return;
        }
        let mut bytes: u64 = self.sizes.iter().sum();
        let mut evict = 0;
        while self.rows.len() - evict > 1
            && (self.rows.len() - evict > RESIDENT_ROWS || bytes > RESIDENT_BYTES)
        {
            bytes -= self.sizes[evict];
            evict += 1;
        }
        if evict == 0 {
            return;
        }
        for (offset, row) in self.rows.drain(..evict).enumerate() {
            let position = self.base + offset;
            self.evicted_seq_end = self.evicted_seq_end.max(num(&row, "seqEnd"));
            if let Some(id) = identity(&row["item"])
                && self.identities.get(&id) == Some(&position)
            {
                self.identities.remove(&id);
            }
            if kind(&row["item"]) == "user_message"
                && let Some(id) = row["item"]["clientMessageId"].as_str()
                && self.submitted.get(id) == Some(&position)
            {
                self.submitted.remove(id);
            }
        }
        self.sizes.drain(..evict);
        self.base += evict;
    }
    /// One page selected from resident rows: the whole history of a timeline
    /// that never released rows. Agent readers use `page`.
    pub fn fetch(&self, options: &Value) -> Value {
        debug_assert_eq!(self.base, 0, "timeline rows are not all resident");
        self.fetch_resident(options)
    }
    fn page_request<'a>(&self, options: &'a Value) -> PageRequest<'a> {
        let direction = options["direction"].as_str().unwrap_or("tail");
        let cursor = options.get("cursor").filter(|c| !c.is_null());
        let stale = cursor.is_some_and(|c| c["epoch"].as_str() != Some(&self.epoch));
        let cursor_seq = cursor.and_then(|c| c["seq"].as_i64());
        let gap = !stale
            && direction == "after"
            && self.min_seq != 0
            && cursor_seq.is_some_and(|s| s < self.min_seq - 1);
        let mode = if stale || gap { "tail" } else { direction };
        let limit = options["limit"]
            .as_f64()
            .map(|n| n.floor().max(0.0) as usize)
            .unwrap_or(200);
        PageRequest {
            direction,
            stale,
            gap,
            mode,
            cursor: cursor_seq,
            limit,
        }
    }
    fn fetch_resident(&self, options: &Value) -> Value {
        let request = self.page_request(options);
        let max = self.next_seq - 1;
        let min = self.min_seq;
        let (rows, start, end, older, newer) =
            self.select_page(request.mode, request.cursor, request.limit, min, max);
        json!({"epoch":self.epoch,"direction":request.direction,"reset":request.stale||request.gap,"staleCursor":request.stale,"gap":request.gap,
            "window":{"minSeq":min,"maxSeq":max,"nextSeq":self.next_seq},"hasOlder":older,"hasNewer":newer,"startSeq":start,"endSeq":end,"rows":rows})
    }
    /// The page answered by resident rows alone, when no stored row can be
    /// selected or change its bounds; otherwise `None`.
    fn resident_page(&self, options: &Value) -> Option<Value> {
        if self.rows.is_empty() {
            return None;
        }
        let request = self.page_request(options);
        let (min, max) = (self.min_seq, self.next_seq - 1);
        let tail = match request.mode {
            "after" => {
                // A stored row is eligible only through a range reaching `start`.
                let start = min.max(request.cursor.unwrap_or(min - 1) + 1);
                if self.evicted_seq_end >= start {
                    return None;
                }
                false
            }
            "before" => {
                let end = max.min(request.cursor.unwrap_or(max + 1) - 1);
                if end >= min {
                    // Stored rows precede every resident row; more resident
                    // candidates than the limit leave them unselected.
                    let eligible = self
                        .rows
                        .iter()
                        .filter(|row| num(row, "seqStart") <= end)
                        .count();
                    if request.limit == 0 || eligible <= request.limit {
                        return None;
                    }
                }
                false
            }
            _ => {
                if request.limit == 0 || self.rows.len() < request.limit {
                    return None;
                }
                let mut begin = self.rows.len() - request.limit;
                for i in (0..begin).rev() {
                    if num(&self.rows[i], "seqEnd") >= num(&self.rows[begin], "seqStart") {
                        begin = i;
                    }
                }
                // A stored row joins the tail only when it ends inside it.
                if self.evicted_seq_end >= num(&self.rows[begin], "seqStart") {
                    return None;
                }
                true
            }
        };
        let mut page = self.fetch_resident(options);
        if tail {
            page["hasOlder"] = json!(true);
        }
        Some(page)
    }
    fn select_page(
        &self,
        direction: &str,
        cursor: Option<i64>,
        limit: usize,
        min: i64,
        max: i64,
    ) -> (Vec<&Value>, Option<i64>, Option<i64>, bool, bool) {
        if self.rows.is_empty() {
            let (older, newer) = match direction {
                "after" => {
                    let c = cursor.unwrap_or(min - 1);
                    (c >= min, c < max)
                }
                "before" => {
                    let c = cursor.unwrap_or(max + 1);
                    (c > min, c <= max)
                }
                _ => (false, false),
            };
            return (vec![], None, None, older, newer);
        }
        if direction == "after" {
            let start = min.max(cursor.unwrap_or(min - 1) + 1);
            let mut eligible: Vec<(usize, i64)> = self
                .rows
                .iter()
                .enumerate()
                .filter_map(|(index, row)| {
                    row["sourceSeqRanges"].as_array()?.iter().find_map(|r| {
                        let first = num(r, "startSeq").max(start);
                        (first <= num(r, "endSeq").min(max)).then_some((index, first))
                    })
                })
                .collect();
            eligible.sort_by_key(|&(i, s)| (s, i));
            if limit > 0 {
                eligible.truncate(limit);
            }
            eligible.sort_by_key(|&(i, _)| i);
            let selected: Vec<&Value> = eligible.iter().map(|&(i, _)| &self.rows[i]).collect();
            let mut ranges: Vec<(i64, i64)> = selected
                .iter()
                .flat_map(|r| r["sourceSeqRanges"].as_array().into_iter().flatten())
                .map(|r| (num(r, "startSeq"), num(r, "endSeq")))
                .collect();
            ranges.sort_unstable();
            let mut end = start - 1;
            for (a, b) in ranges {
                if b <= end {
                    continue;
                }
                if a > end + 1 {
                    break;
                }
                end = b.min(max);
            }
            let end = (end >= start).then_some(end);
            return (
                selected,
                end.map(|_| start),
                end,
                start > min,
                end.is_some_and(|e| e < max),
            );
        }
        if direction == "before" {
            let end = max.min(cursor.unwrap_or(max + 1) - 1);
            if end < min {
                return (vec![], None, None, false, end < max);
            }
            let eligible: Vec<&Value> = self
                .rows
                .iter()
                .filter(|r| num(r, "seqStart") <= end)
                .collect();
            let begin = if limit == 0 {
                0
            } else {
                eligible.len().saturating_sub(limit)
            };
            let selected = eligible[begin..].to_vec();
            let start = selected.first().map(|r| num(r, "seqStart"));
            return (selected, start, Some(end), begin > 0, end < max);
        }
        let mut begin = if limit == 0 {
            0
        } else {
            self.rows.len().saturating_sub(limit)
        };
        for i in (0..begin).rev() {
            if num(&self.rows[i], "seqEnd") >= num(&self.rows[begin], "seqStart") {
                begin = i;
            }
        }
        (
            self.rows[begin..].iter().collect(),
            Some(num(&self.rows[begin], "seqStart")),
            Some(max),
            begin > 0,
            false,
        )
    }
    /// Each projected sub-agent card's recorded log, and its log-entry
    /// identities where recorded, by call ID. A resumed provider keeps these
    /// instead of reading child transcripts.
    pub fn subagent_card_logs(
        &self,
    ) -> Result<(
        serde_json::Map<String, Value>,
        serde_json::Map<String, Value>,
    )> {
        let mut logs = serde_json::Map::new();
        let mut entries = serde_json::Map::new();
        self.scan(&["tool_call"], |row| {
            let item = &row["item"];
            if item["detail"]["type"] != "sub_agent" {
                return Ok(true);
            }
            let (Some(call), Some(log)) = (
                item["callId"].as_str(),
                item["detail"]["log"].as_str().filter(|log| !log.is_empty()),
            ) else {
                return Ok(true);
            };
            logs.insert(call.to_owned(), json!(log));
            if let Some(recorded) = item["metadata"]["subagentLog"].as_array() {
                entries.insert(call.to_owned(), json!(recorded));
            }
            Ok(true)
        })?;
        Ok((logs, entries))
    }
    /// An agent's projection: the bounded recent tail of its checkpoint.
    pub fn load(path: &Path, agent_id: &str) -> Result<Self> {
        storage::load(path, agent_id)
    }
    /// Whether a reusable current-format checkpoint exists, without changing it.
    pub(super) fn checkpoint_exists(path: &Path, agent_id: &str) -> Result<bool> {
        storage::checkpoint_exists(path, agent_id)
    }
    /// An existing checkpoint as one read-only snapshot with no resident rows.
    /// Saved readers page, scan and search it without hydrating history.
    pub fn open_saved(path: &Path, agent_id: &str) -> Result<Self> {
        storage::open_saved(path, agent_id)
    }
    /// One page of an existing checkpoint, read without hydrating it.
    pub fn fetch_saved(path: &Path, agent_id: &str, options: &Value) -> Result<Value> {
        storage::fetch_page(path, agent_id, options)
    }
    pub fn persist(&mut self, path: &Path, agent_id: &str) -> Result<()> {
        storage::persist(self, path, agent_id)
    }
    pub fn append_persisted(
        &mut self,
        path: &Path,
        agent_id: &str,
        item: Value,
        options: &Value,
    ) -> Result<Value> {
        storage::append(self, path, agent_id, item, options)
    }
    /// Atomically commits one coalescer flush before any source row is published.
    pub(super) fn append_batch_persisted(
        &mut self,
        path: &Path,
        agent_id: &str,
        entries: Vec<(Value, Value)>,
    ) -> Result<Vec<Value>> {
        storage::append_batch(self, path, agent_id, entries)
    }
}

pub fn limit_item_content(mut item: Value) -> Value {
    if item["type"] != "tool_call" {
        return item;
    }
    let shell = item["detail"]["type"] == "shell";
    let plain = item["detail"]["type"] == "plain_text";
    if shell {
        truncate_field(&mut item["detail"], "output", 65536, "");
        if item["status"] == "failed" {
            truncate_field(&mut item["error"], "content", 65536, "");
        }
    } else if plain {
        truncate_field(
            &mut item["detail"],
            "text",
            65536,
            "\n\n[Content truncated]",
        );
    }
    item
}
fn truncate_field(value: &mut Value, key: &str, limit: usize, suffix: &str) {
    if let Some(text) = value[key]
        .as_str()
        .filter(|s| s.encode_utf16().count() > limit)
    {
        let shortened =
            crate::activity::utf16_prefix(text, limit - suffix.encode_utf16().count()) + suffix;
        value[key] = json!(shortened);
    }
}

#[cfg(test)]
#[path = "timeline_tests.rs"]
mod tests;
