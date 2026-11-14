use super::{Timeline, identity, kind};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;

const MARKER: &str = "ompStreamMessageId";

impl Timeline {
    /// Message-local authority is deliberately narrower than a history replay.
    /// Build without touching the owner, commit the complete checkpoint, then swap.
    pub(in crate::agent) fn reconcile_message(
        &mut self,
        event: &Value,
        destination: Option<(&Path, &str)>,
    ) -> Result<Option<String>> {
        let Some(mut replacement) = self.message_replacement(event)? else {
            return Ok(None);
        };
        if let Some((path, agent_id)) = destination {
            replacement.persist(path, agent_id)?;
        }
        let epoch = replacement.epoch.clone();
        *self = replacement;
        Ok(Some(epoch))
    }

    fn message_replacement(&self, event: &Value) -> Result<Option<Self>> {
        let message = event["messageId"].as_str().filter(|id| !id.is_empty())
            .context("Missing OMP stream message identity")?;
        let turn = match event.get("turnId") {
            None | Some(Value::Null) => None,
            Some(Value::String(turn)) if !turn.is_empty() => Some(turn.as_str()),
            _ => bail!("Invalid reconciliation turn identity"),
        };
        let items = event["items"].as_array().context("Missing authoritative message items")?;
        for item in items {
            if !matches!(kind(item), "reasoning" | "assistant_message")
                || !item["text"].is_string()
                || item["metadata"][MARKER].as_str() != Some(message)
            {
                bail!("Invalid authoritative text item or message marker");
            }
            for field in ["messageId", "clientMessageId"] {
                if item.get(field).is_some_and(|value| !value.is_string()) {
                    bail!("Invalid authoritative message identity");
                }
            }
        }
        let mut matches = self.rows.iter().enumerate().filter(|(_, row)| {
            row["item"]["metadata"][MARKER].as_str() == Some(message)
        });
        let Some((start, first)) = matches.next() else {
            // Re-delivery of an already removed, empty message is harmless. Never
            // create a new row from a correction that has no existing anchor.
            if items.is_empty() { return Ok(None); }
            bail!("No canonical rows match the OMP stream message");
        };
        let mut end = start + 1;
        for (index, _) in matches {
            if index != end { bail!("Message rows are interleaved with unrelated history"); }
            end += 1;
        }
        let old = &self.rows[start..end];
        for row in old {
            if !matches!(kind(&row["item"]), "reasoning" | "assistant_message")
                || row.get("turnId") != first.get("turnId")
                || turn.is_some_and(|turn| row["turnId"].as_str() != Some(turn))
            {
                bail!("Message marker does not identify one text run in the requested turn");
            }
        }
        let first_seq = old.iter().map(|row| super::num(row, "seqStart")).min().unwrap();
        let last_seq = old.iter().map(|row| super::num(row, "seqEnd")).max().unwrap();
        if self.rows[..start].iter().chain(&self.rows[end..]).any(|row| {
            row["sourceSeqRanges"].as_array().is_some_and(|ranges| ranges.iter().any(|range| {
                super::num(range, "startSeq") <= last_seq
                    && super::num(range, "endSeq") >= first_seq
            }))
        }) {
            bail!("Message source events are interleaved with unrelated history");
        }

        // Match the normal projection's adjacent-block merging before comparing or
        // restoring durable identities. Native content blocks need not equal rows.
        let mut authoritative = Self::new();
        for item in items {
            authoritative.append(item.clone(), &json!({}));
        }
        let mut used = vec![false; old.len()];
        let mut sources = vec![None; authoritative.rows.len()];
        for (slot, row) in sources.iter_mut().zip(&authoritative.rows) {
            if let Some(index) = old.iter().enumerate().position(|(index, previous)| {
                !used[index] && kind(&previous["item"]) == kind(&row["item"])
            }) {
                used[index] = true;
                *slot = Some(index);
            }
        }
        // A changed block kind still belongs to this native message. Reserve
        // same-kind identities first so reordering cannot steal another block's ID.
        for slot in sources.iter_mut().filter(|slot| slot.is_none()) {
            if let Some(index) = used.iter().position(|used| !used) {
                used[index] = true;
                *slot = Some(index);
            }
        }
        for (row, source) in authoritative.rows.iter_mut().zip(sources) {
            let source = source.map(|index| &old[index]);
            row["timestamp"] = source.unwrap_or(first)["timestamp"].clone();
            if let Some(turn) = first.get("turnId") { row["turnId"] = turn.clone(); }
            if let Some(source) = source {
                for field in ["messageId", "clientMessageId"] {
                    if let Some(id) = source["item"].get(field) {
                        if row["item"].get(field).is_some_and(|next| next != id) {
                            bail!("Authoritative message changes an existing durable identity");
                        }
                        row["item"][field] = id.clone();
                    }
                }
                if let Some(id) = source.get("providerMessageId") {
                    row["providerMessageId"] = id.clone();
                }
            }
        }
        if old.len() == authoritative.rows.len()
            && old.iter().zip(&authoritative.rows).all(|(old, new)| old["item"] == new["item"])
        {
            return Ok(None);
        }

        let mut replacement = Self::new();
        replacement.rows.reserve(self.rows.len() - old.len() + authoritative.rows.len());
        for row in self.rows[..start].iter().cloned()
            .chain(authoritative.rows)
            .chain(self.rows[end..].iter().cloned())
        {
            replacement.push_rebased(row);
        }
        replacement.storage_dirty = true;
        Ok(Some(replacement))
    }

    fn push_rebased(&mut self, mut row: Value) {
        // Positions belong to the new epoch. Do not run projection again: removing
        // this message must not merge or otherwise rewrite its unrelated neighbors.
        let seq = self.next_seq;
        row["seq"] = json!(seq);
        row["seqStart"] = json!(seq);
        row["seqEnd"] = json!(seq);
        row["sourceSeqRanges"] = json!([{"startSeq":seq,"endSeq":seq}]);
        row["collapsed"] = json!([]);
        if let Some(id) = identity(&row["item"]) { self.identities.insert(id, self.rows.len()); }
        if kind(&row["item"]) == "user_message"
            && let Some(id) = row["item"]["clientMessageId"].as_str().filter(|id| !id.is_empty())
        {
            self.submitted.entry(id.to_owned()).or_insert(self.rows.len());
        }
        self.rows.push(row);
        self.min_seq = 1;
        self.next_seq += 1;
    }
}

#[cfg(test)]
#[path = "timeline_reconcile_tests.rs"]
mod tests;
