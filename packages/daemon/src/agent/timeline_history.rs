use super::Timeline;
use crate::providers::api::HistoryCoverage;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

fn keys(row: &Value) -> Vec<String> {
    let item = &row["item"];
    let mut keys = Vec::new();
    for value in [
        &row["providerMessageId"],
        &item["messageId"],
        &item["clientMessageId"],
    ] {
        if let Some(id) = value.as_str().filter(|id| !id.is_empty()) {
            let key = format!("message:{}:{id}", item["type"].as_str().unwrap_or(""));
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    if item["type"] == "tool_call" {
        if let Some(id) = item["callId"].as_str() {
            keys.push(format!("tool:{id}"));
        }
    } else if item["type"] == "plugin"
        && let (Some(plugin), Some(id)) = (item["pluginId"].as_str(), item["id"].as_str())
    {
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
        keys.push(format!(
            "content:{}",
            serde_json::to_string(&content).expect("JSON item")
        ));
    }
    keys
}

/// Durable identity fields a matched previous row restores onto its replacement.
struct Restore {
    index: usize,
    row: Vec<(&'static str, Value)>,
    item: Vec<(&'static str, Value)>,
}

impl Timeline {
    /// Non-destructive refresh requires unique, ordered matches for conversation
    /// rows. Complete provider history may omit daemon notifications: retain those
    /// beside the next matched row rather than dropping them or refusing recovery.
    /// `previous` is streamed once; stored rows are decoded one at a time.
    pub(in crate::agent) fn reconcile_history(
        &mut self,
        previous: &Self,
        coverage: HistoryCoverage,
        allow_removal: bool,
    ) -> Result<bool> {
        if allow_removal && coverage != HistoryCoverage::Complete {
            return Ok(false);
        }
        if previous.len() == 0 {
            return Ok(true);
        }
        let mut candidates: HashMap<String, Option<usize>> = HashMap::new();
        for (index, row) in self.rows.iter().enumerate() {
            for key in keys(row) {
                candidates
                    .entry(key)
                    .and_modify(|candidate| {
                        if *candidate != Some(index) {
                            *candidate = None;
                        }
                    })
                    .or_insert(Some(index));
            }
        }
        let mut used = HashSet::new();
        let mut matches = Vec::new();
        let mut previous_index = None;
        let mut pending_notices = Vec::new();
        let mut retained_notices = Vec::new();
        let mut accepted = true;
        let rows = &self.rows;
        previous.scan(&[], |row| {
            let resolved: HashSet<_> = keys(&row)
                .iter()
                .filter_map(|key| candidates.get(key).copied().flatten())
                .collect();
            let matched = (resolved.len() == 1).then(|| *resolved.iter().next().unwrap());
            if !allow_removal
                && matched
                    .is_some_and(|index| previous_index.is_some_and(|previous| index <= previous))
            {
                accepted = false;
                return Ok(false);
            }
            // A non-authoritative snapshot may lag live text even when IDs match.
            if !allow_removal
                && let Some(index) = matched
                && let Some(text) = row["item"]["text"].as_str().filter(|text| !text.is_empty())
                && !rows[index]["item"]["text"]
                    .as_str()
                    .is_some_and(|next| next.starts_with(text))
            {
                accepted = false;
                return Ok(false);
            }
            if let Some(index) = matched {
                previous_index = Some(index);
            }
            match matched.filter(|index| used.insert(*index)) {
                Some(index) => {
                    retained_notices.extend(pending_notices.drain(..).map(|old| (index, old)));
                    let restored = |source: &Value, fields: &[&'static str]| {
                        fields
                            .iter()
                            .filter_map(|&key| {
                                source
                                    .get(key)
                                    .filter(|value| !value.is_null())
                                    .map(|value| (key, value.clone()))
                            })
                            .collect::<Vec<_>>()
                    };
                    matches.push(Restore {
                        index,
                        row: restored(&row, &["turnId", "providerMessageId"]),
                        item: restored(
                            &row["item"],
                            &["messageId", "clientMessageId", "images", "attachments"],
                        ),
                    });
                }
                None if allow_removal => {}
                None if coverage == HistoryCoverage::Complete
                    && row["item"]["type"] == "notification" =>
                {
                    pending_notices.push(row.into_owned());
                }
                None => {
                    accepted = false;
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        if !accepted {
            return Ok(false);
        }
        retained_notices.extend(
            pending_notices
                .into_iter()
                .map(|old| (self.rows.len(), old)),
        );
        for restore in matches {
            let row = &mut self.rows[restore.index];
            for (key, value) in restore.row {
                row[key] = value;
            }
            for (key, value) in restore.item {
                row["item"][key] = value;
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
                let (_, mut notice) = notices.next().unwrap();
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
        for (_, mut notice) in notices {
            notice["seq"] = json!(self.next_seq + shift);
            for key in ["seqStart", "seqEnd", "sourceSeqRanges", "collapsed"] {
                notice.as_object_mut().unwrap().remove(key);
            }
            self.append_row(notice);
            shift += 1;
        }
        self.next_seq += shift;
        Ok(true)
    }
}
