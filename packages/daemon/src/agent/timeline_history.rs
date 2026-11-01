use super::Timeline;
use crate::providers::api::HistoryCoverage;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn keys(row: &Value) -> Vec<String> {
    let item = &row["item"];
    let mut keys = Vec::new();
    for value in [&row["providerMessageId"], &item["messageId"], &item["clientMessageId"]] {
        if let Some(id) = value.as_str().filter(|id| !id.is_empty()) {
            let key = format!("message:{}:{id}", item["type"].as_str().unwrap_or(""));
            if !keys.contains(&key) { keys.push(key); }
        }
    }
    if item["type"] == "tool_call" {
        if let Some(id) = item["callId"].as_str() { keys.push(format!("tool:{id}")); }
    } else if item["type"] == "plugin" {
        if let (Some(plugin), Some(id)) = (item["pluginId"].as_str(), item["id"].as_str()) {
            keys.push(format!("plugin:{plugin}:{id}"));
        }
    }
    // Exact unique content may retain presentation identity for non-user rows.
    // It never correlates a submitted user message or settles a delivery receipt.
    if item["type"] != "user_message" {
        let mut content = item.clone();
        if let Some(object) = content.as_object_mut() {
            object.remove("messageId");
            object.remove("clientMessageId");
        }
        keys.push(format!("content:{}", serde_json::to_string(&content).expect("JSON item")));
    }
    keys
}

impl Timeline {
    /// Non-destructive refresh requires every existing row to survive with a
    /// unique match. Unknown/torn replay can never authorize a rewind. Ambiguous
    /// matches preserve the entire old timeline instead of guessing associations.
    pub(in crate::agent) fn reconcile_history(
        &mut self, previous: &Self, coverage: HistoryCoverage, allow_removal: bool,
    ) -> bool {
        if allow_removal && coverage != HistoryCoverage::Complete { return false; }
        if previous.rows.is_empty() { return true; }
        let mut candidates: HashMap<String, Option<usize>> = HashMap::new();
        for (index, row) in self.rows.iter().enumerate() {
            for key in keys(row) {
                candidates.entry(key).and_modify(|candidate| {
                    if *candidate != Some(index) { *candidate = None; }
                }).or_insert(Some(index));
            }
        }
        let mut used = HashSet::new();
        let mut matches = Vec::new();
        let mut previous_index = None;
        for (old_index, row) in previous.rows.iter().enumerate() {
            let resolved: HashSet<_> = keys(row).iter()
                .filter_map(|key| candidates.get(key).copied().flatten()).collect();
            let matched = (resolved.len() == 1).then(|| *resolved.iter().next().unwrap());
            if !allow_removal && matched.is_some_and(|index| previous_index.is_some_and(|previous| index <= previous)) {
                return false;
            }
            if let Some(index) = matched { previous_index = Some(index); }
            match matched.filter(|index| used.insert(*index)) {
                Some(index) => matches.push((old_index, index)),
                None if allow_removal => {}
                None => return false,
            }
        }
        for (old_index, index) in matches {
            let old = &previous.rows[old_index];
            let row = &mut self.rows[index];
            for key in ["turnId", "providerMessageId"] {
                if let Some(value) = old.get(key).filter(|value| !value.is_null()) { row[key] = value.clone(); }
            }
            for key in ["messageId", "clientMessageId", "images", "attachments"] {
                if let Some(value) = old["item"].get(key).filter(|value| !value.is_null()) { row["item"][key] = value.clone(); }
            }
        }
        // Identity restoration can change the submitted-message index. Rebuild
        // projection indexes before any later live event is admitted.
        let rows = std::mem::take(&mut self.rows);
        self.identities.clear();
        self.submitted.clear();
        self.min_seq = 0;
        for row in rows { self.append_row(row); }
        true
    }
}
