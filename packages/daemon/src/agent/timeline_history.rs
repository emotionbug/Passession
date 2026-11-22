use super::Timeline;
use crate::providers::api::HistoryCoverage;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

struct ContentDigest(Sha256);

impl std::io::Write for ContentDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn content_digest(item: &Value) -> String {
    use serde::{
        Serialize,
        ser::{SerializeMap, Serializer},
    };
    let mut digest = ContentDigest(Sha256::new());
    let mut serializer = serde_json::Serializer::new(&mut digest);
    if let Some(object) = item.as_object() {
        let mut map = serializer.serialize_map(None).expect("JSON item");
        for (key, value) in object {
            if !matches!(key.as_str(), "messageId" | "clientMessageId") {
                map.serialize_entry(key, value).expect("JSON item field");
            }
        }
        map.end().expect("JSON item");
    } else {
        item.serialize(&mut serializer).expect("JSON item");
    }
    hex::encode(digest.0.finalize())
}

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
        keys.push(format!("content:{}", content_digest(item)));
    }
    keys
}

fn append_reconciled(timeline: &mut Timeline, row: Value, staged: bool) -> Result<()> {
    if staged {
        timeline.append_history_row(row)
    } else {
        timeline.append_row(row);
        Ok(())
    }
}

fn same_content(left: &Value, right: &Value) -> bool {
    let ignored = |key: &str| matches!(key, "messageId" | "clientMessageId");
    match (left.as_object(), right.as_object()) {
        (Some(left), Some(right)) => {
            left.iter().filter(|(key, _)| !ignored(key)).count()
                == right.iter().filter(|(key, _)| !ignored(key)).count()
                && left
                    .iter()
                    .filter(|(key, _)| !ignored(key))
                    .all(|(key, value)| right.get(key) == Some(value))
        }
        _ => left == right,
    }
}

fn moderated_message_id(item: &Value) -> Option<&str> {
    (item["type"] == "notification" && item["metadata"]["codexModeration"] == true)
        .then(|| item["messageId"].as_str())
        .flatten()
        .filter(|id| !id.is_empty())
}

impl Timeline {
    pub(in crate::agent) fn native_revision(&self) -> Option<&Value> {
        self.native_revision.as_ref()
    }

    pub(in crate::agent) fn matches_native_revision(&self, revision: Option<&Value>) -> bool {
        self.native_revision
            .as_ref()
            .zip(revision)
            .is_some_and(|(saved, current)| saved == current)
    }

    pub(in crate::agent) fn certify_native_revision(
        &mut self,
        revision: Option<Value>,
    ) -> Result<()> {
        super::storage::certify(self, revision)
    }

    pub(in crate::agent) fn set_replay_revision(&mut self, revision: Option<Value>) {
        self.native_revision = revision;
    }
    /// Non-destructive refresh requires unique, ordered matches for conversation
    /// rows. Complete provider history can omit daemon-owned rows: retain those
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
        // Only identity/digest indices live in RAM. Restored attachments and
        // daemon notices can themselves be large, so stage those on disk too.
        let file = tempfile::NamedTempFile::new()?;
        let edits = Connection::open(file.path())?;
        edits.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
             PRAGMA cache_size=-2048; PRAGMA temp_store=FILE;
             CREATE TABLE restores(position INTEGER PRIMARY KEY, row_json TEXT NOT NULL);
             CREATE TABLE notices(ordinal INTEGER PRIMARY KEY, position INTEGER, row_json TEXT NOT NULL);
             CREATE INDEX notice_position ON notices(position, ordinal);",
        )?;
        let mut candidates: HashMap<String, Option<usize>> = HashMap::new();
        let mut moderated = HashMap::<String, usize>::new();
        let mut index = 0;
        self.scan(&[], |row| {
            if let Some(id) = moderated_message_id(&row["item"]) {
                moderated.entry(id.to_owned()).or_insert(index);
            }
            for key in keys(&row) {
                candidates
                    .entry(key)
                    .and_modify(|candidate| {
                        if *candidate != Some(index) {
                            *candidate = None;
                        }
                    })
                    .or_insert(Some(index));
            }
            index += 1;
            Ok(true)
        })?;
        let mut used = HashSet::new();
        let mut previous_index = None;
        let mut accepted = true;
        previous.scan(&[], |row| {
            let item = &row["item"];
            if let Some(id) = moderated_message_id(item)
                && candidates.contains_key(&format!("message:assistant_message:{id}"))
            {
                // A replay lacking the directive cannot resurrect text Codex
                // already withdrew for this exact provider message.
                accepted = false;
                return Ok(false);
            }
            let redaction = if item["type"] == "assistant_message" {
                item["messageId"]
                    .as_str()
                    .and_then(|id| moderated.get(id).copied())
            } else {
                None
            };
            if redaction.is_some_and(|index| used.contains(&index)) {
                // Later interleaved chunks of the same withdrawn message.
                return Ok(true);
            }
            let mut resolved = HashSet::new();
            for key in keys(&row) {
                if let Some(index) = candidates.get(&key).copied().flatten() {
                    // Digests bound key residency, but never authorize a match
                    // without exact content equality.
                    if !key.starts_with("content:")
                        || same_content(&row["item"], &self.history_row(index)?["item"])
                    {
                        resolved.insert(index);
                    }
                }
            }
            let matched =
                redaction.or_else(|| (resolved.len() == 1).then(|| *resolved.iter().next().unwrap()));
            if !allow_removal
                && matched.is_some_and(|index| previous_index.is_some_and(|old| index <= old))
            {
                accepted = false;
                return Ok(false);
            }
            if !allow_removal
                && redaction.is_none()
                && let Some(index) = matched
                && let Some(text) = row["item"]["text"].as_str().filter(|text| !text.is_empty())
                && !self.history_row(index)?["item"]["text"]
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
                    edits.execute(
                        "UPDATE notices SET position = ?1 WHERE position IS NULL",
                        [index as i64],
                    )?;
                    let mut restore = json!({"item":{}});
                    for key in ["turnId", "providerMessageId"] {
                        if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
                            restore[key] = value.clone();
                        }
                    }
                    for key in ["messageId", "clientMessageId", "images", "attachments"] {
                        if let Some(value) = row["item"].get(key).filter(|value| !value.is_null()) {
                            restore["item"][key] = value.clone();
                        }
                    }
                    edits.execute(
                        "INSERT INTO restores VALUES (?1, ?2)",
                        params![index as i64, serde_json::to_string(&restore)?],
                    )?;
                }
                None if allow_removal => {}
                None if coverage == HistoryCoverage::Complete
                    && (row["item"]["type"] == "notification"
                        || row["item"]["daemonOwned"] == true) =>
                {
                    edits.execute(
                        "INSERT INTO notices(position, row_json) VALUES (NULL, ?1)",
                        [serde_json::to_string(row.as_ref())?],
                    )?;
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
        drop(candidates);
        drop(used);
        edits.execute(
            "UPDATE notices SET position = ?1 WHERE position IS NULL",
            [self.len() as i64],
        )?;
        let staged = self.storage.is_some();
        let mut rebuilt = if staged {
            Self::history_stage()?
        } else {
            Self::new()
        };
        rebuilt.epoch = self.epoch.clone();
        rebuilt.next_seq = self.next_seq;
        rebuilt.storage_dirty = !staged;
        let mut shift = 0i64;
        let mut index = 0;
        let mut notices =
            edits.prepare("SELECT row_json FROM notices WHERE position = ?1 ORDER BY ordinal")?;
        self.scan(&[], |source| {
            let mut row = source.into_owned();
            let restore: Option<String> = edits
                .query_row(
                    "SELECT row_json FROM restores WHERE position = ?1",
                    [index as i64],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(restore) = restore {
                let restore: Value = serde_json::from_str(&restore)?;
                for key in ["turnId", "providerMessageId"] {
                    if let Some(value) = restore.get(key) {
                        row[key] = value.clone();
                    }
                }
                for (key, value) in restore["item"].as_object().unwrap() {
                    row["item"][key] = value.clone();
                }
            }
            let mut retained = notices.query([index as i64])?;
            while let Some(notice) = retained.next()? {
                let mut notice: Value = serde_json::from_str(notice.get_ref(0)?.as_str()?)?;
                notice["seq"] = json!(super::num(&row, "seqStart") + shift);
                for key in ["seqStart", "seqEnd", "sourceSeqRanges", "collapsed"] {
                    notice.as_object_mut().unwrap().remove(key);
                }
                append_reconciled(&mut rebuilt, notice, staged)?;
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
            append_reconciled(&mut rebuilt, row, staged)?;
            index += 1;
            Ok(true)
        })?;
        let mut retained = notices.query([self.len() as i64])?;
        while let Some(notice) = retained.next()? {
            let mut notice: Value = serde_json::from_str(notice.get_ref(0)?.as_str()?)?;
            notice["seq"] = json!(self.next_seq + shift);
            for key in ["seqStart", "seqEnd", "sourceSeqRanges", "collapsed"] {
                notice.as_object_mut().unwrap().remove(key);
            }
            append_reconciled(&mut rebuilt, notice, staged)?;
            shift += 1;
        }
        rebuilt.next_seq += shift;
        *self = rebuilt;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_revision_requires_exact_match_and_is_invalidated_by_every_writer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("timeline.sqlite3");
        let marker = json!({"native":{"path":"session.jsonl","length":42}});
        let mut timeline = Timeline::new();
        timeline.append(
            json!({"type":"user_message","text":"hello","messageId":"one"}),
            &json!({}),
        );
        timeline.set_replay_revision(Some(marker.clone()));
        timeline.persist(&path, "agent").unwrap();
        drop(timeline);
        let timeline = Timeline::load(&path, "agent").unwrap();
        assert!(timeline.matches_native_revision(Some(&marker)));
        assert!(!timeline.matches_native_revision(None));
        assert!(!timeline.matches_native_revision(Some(&json!({"native":{"length":43}}))));
        drop(timeline);
        // An older writer knows only timeline_rows, not certificate metadata.
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute(
                "UPDATE timeline_rows SET row_json = row_json WHERE position = 0",
                [],
            )
            .unwrap();
        drop(writer);
        let mut timeline = Timeline::load(&path, "agent").unwrap();
        assert!(!timeline.matches_native_revision(Some(&marker)));
        timeline
            .certify_native_revision(Some(marker.clone()))
            .unwrap();
        timeline
            .append_persisted(
                &path,
                "agent",
                json!({"type":"assistant_message","text":"reply","messageId":"two"}),
                &json!({}),
            )
            .unwrap();
        assert!(!timeline.matches_native_revision(Some(&marker)));
        drop(timeline);
        assert!(
            Timeline::load(&path, "agent")
                .unwrap()
                .native_revision()
                .is_none()
        );
    }

    #[test]
    fn streamed_history_stage_bounds_residency_and_rolls_back_failed_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("timeline.sqlite3");
        let mut current = Timeline::new();
        current.append(
            json!({"type":"user_message","text":"hello","messageId":"one"}),
            &json!({}),
        );
        current.append(
            json!({"type":"notification","text":"daemon notice"}),
            &json!({}),
        );
        let marker = json!({"native":{"length":42}});
        current.set_replay_revision(Some(marker.clone()));
        current.persist(&path, "agent").unwrap();
        let before = current.all_rows().unwrap();
        let epoch = current.epoch().to_owned();

        let mut staged = Timeline::history_stage().unwrap();
        staged
            .append_history_item(
                json!({"type":"user_message","text":"hello","messageId":"one"}),
                &json!({}),
            )
            .unwrap();
        for index in 1..600 {
            staged.append_history_item(
                json!({"type":"user_message","text":"x".repeat(16384),"messageId":format!("message-{index}")}),
                &json!({}),
            ).unwrap();
        }
        assert!(staged.base > 0);
        assert!(staged.rows.len() <= super::super::RESIDENT_ROWS);
        assert!(staged.sizes.iter().sum::<u64>() <= super::super::RESIDENT_BYTES);
        assert!(
            staged
                .reconcile_history(&current, HistoryCoverage::Complete, false)
                .unwrap()
        );
        staged.set_replay_revision(Some(json!({"native":{"length":999}})));

        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "CREATE TRIGGER reject_history BEFORE INSERT ON timeline_rows
             WHEN NEW.position = 200 BEGIN SELECT RAISE(ABORT, 'injected replacement failure'); END;",
        ).unwrap();
        assert!(staged.commit_history(&path, "agent").is_err());
        let saved = Timeline::load(&path, "agent").unwrap();
        assert_eq!(saved.all_rows().unwrap(), before);
        assert_eq!(saved.epoch(), epoch);
        assert!(saved.matches_native_revision(Some(&marker)));
        drop(saved);
        writer.execute_batch("DROP TRIGGER reject_history").unwrap();

        staged.commit_history(&path, "agent").unwrap();
        let saved = Timeline::load(&path, "agent").unwrap();
        assert_eq!(saved.len(), 601);
        assert_eq!(saved.next_seq(), 602);
        assert_eq!(saved.last_item().unwrap().unwrap()["text"], "daemon notice");
        assert!(saved.matches_native_revision(Some(&json!({"native":{"length":999}}))));
    }
}
