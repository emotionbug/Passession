use super::Timeline;
use crate::providers::api::HistoryCoverage;
use serde_json::{Value, json};
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
    } else if item["type"] == "plugin"
        && let (Some(plugin), Some(id)) = (item["pluginId"].as_str(), item["id"].as_str()) {
            keys.push(format!("plugin:{plugin}:{id}"));
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
    /// Non-destructive refresh requires unique, ordered matches for conversation
    /// rows. Complete provider history may omit daemon notifications: retain those
    /// beside the next matched row rather than dropping them or refusing recovery.
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
        let mut pending_notices = Vec::new();
        let mut retained_notices = Vec::new();
        for (old_index, row) in previous.rows.iter().enumerate() {
            let resolved: HashSet<_> = keys(row).iter()
                .filter_map(|key| candidates.get(key).copied().flatten()).collect();
            let matched = (resolved.len() == 1).then(|| *resolved.iter().next().unwrap());
            if !allow_removal && matched.is_some_and(|index| previous_index.is_some_and(|previous| index <= previous)) {
                return false;
            }
            // A non-authoritative snapshot may lag live text even when IDs match.
            if !allow_removal
                && let Some(index) = matched
                && let Some(text) = row["item"]["text"].as_str().filter(|text| !text.is_empty())
                && !self.rows[index]["item"]["text"].as_str().is_some_and(|next| next.starts_with(text))
            {
                return false;
            }
            if let Some(index) = matched { previous_index = Some(index); }
            match matched.filter(|index| used.insert(*index)) {
                Some(index) => {
                    retained_notices.extend(pending_notices.drain(..).map(|old| (index, old)));
                    matches.push((old_index, index));
                }
                None if allow_removal => {}
                None if coverage == HistoryCoverage::Complete && row["item"]["type"] == "notification" => {
                    pending_notices.push(old_index);
                }
                None => return false,
            }
        }
        retained_notices.extend(pending_notices.into_iter().map(|old| (self.rows.len(), old)));
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
        let mut notices = retained_notices.into_iter().peekable();
        let mut shift = 0i64;
        for (index, mut row) in rows.into_iter().enumerate() {
            while notices.peek().is_some_and(|(before, _)| *before == index) {
                let (_, old) = notices.next().unwrap();
                let mut notice = previous.rows[old].clone();
                notice["seq"] = json!(super::num(&row, "seqStart") + shift);
                for key in ["seqStart", "seqEnd", "sourceSeqRanges", "collapsed"] {
                    notice.as_object_mut().unwrap().remove(key);
                }
                self.append_row(notice);
                shift += 1;
            }
            if shift != 0 {
                for key in ["seq", "seqStart", "seqEnd"] {
                    row[key] = json!(super::num(&row, key) + shift);
                }
                for range in row["sourceSeqRanges"].as_array_mut().unwrap() {
                    for key in ["startSeq", "endSeq"] {
                        range[key] = json!(super::num(range, key) + shift);
                    }
                }
            }
            self.append_row(row);
        }
        for (_, old) in notices {
            let mut notice = previous.rows[old].clone();
            notice["seq"] = json!(self.next_seq + shift);
            for key in ["seqStart", "seqEnd", "sourceSeqRanges", "collapsed"] {
                notice.as_object_mut().unwrap().remove(key);
            }
            self.append_row(notice);
            shift += 1;
        }
        self.next_seq += shift;
        true
    }
}
