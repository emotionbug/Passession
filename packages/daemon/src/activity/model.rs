//! Bounded MCP views over the existing authoritative timeline and agent snapshot.
use std::io::{self, Write};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;
const PREVIEW_BYTES: usize = 256;
const CHUNK_BYTES: usize = 4096;
const PERMISSIONS_SHOWN: usize = 3;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cursor {
    epoch: String,
    max_seq: i64,
    min_seq: i64,
    before_seq: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ItemDetail {
    epoch: String,
    seq_end: i64,
    seq_start: i64,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotDetail {
    path: String,
    revision: Option<String>,
    #[serde(default)]
    offset: usize,
}

fn scope(input: &Value) -> Result<Value> {
    let id = input["agentId"].as_str().context("agentId is required")?;
    ensure!(
        !id.is_empty() && id.len() <= 256,
        "agentId must be 1..256 bytes"
    );
    let mut scope = json!({"agentId": id});
    if let Some(server) = input.get("serverId") {
        let server = server.as_str().context("serverId must be a string")?;
        ensure!(
            !server.is_empty() && server.len() <= 256,
            "serverId must be 1..256 bytes"
        );
        scope["serverId"] = json!(server);
    }
    Ok(scope)
}

fn epoch(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256,
        "Invalid timeline epoch"
    );
    Ok(())
}

fn limit(input: &Value) -> Result<usize> {
    match input.get("limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(value) => {
            let value = value
                .as_u64()
                .context("limit must be an integer from 1 to 20")?;
            ensure!(
                (1..=MAX_LIMIT as u64).contains(&value),
                "limit must be from 1 to 20"
            );
            Ok(value as usize)
        }
    }
}

fn failures_only(input: &Value) -> Result<bool> {
    match input.get("view").and_then(Value::as_str) {
        None if input.get("view").is_none() => Ok(false),
        Some("recent") => Ok(false),
        Some("failures") => Ok(true),
        _ => bail!("view must be recent or failures"),
    }
}

/// `before` pages count canonical rows without tail's lifecycle-range closure.
/// Continuations retain their initial high-water mark but allow unrelated tail
/// growth. A projected row changed after that mark has no reconstructible old
/// payload and must be rejected rather than mixed into the earlier snapshot.
pub(crate) fn timeline_options(input: &Value) -> Result<Value> {
    scope(input)?;
    failures_only(input)?;
    let count = limit(input)?;
    if let Some(detail) = input.get("detail") {
        ensure!(
            input.get("cursor").is_none(),
            "detail and cursor are mutually exclusive"
        );
        let detail: ItemDetail = serde_json::from_value(detail.clone())?;
        epoch(&detail.epoch)?;
        ensure!(
            detail.seq_start > 0 && detail.seq_end >= detail.seq_start,
            "Invalid item position"
        );
        let before = detail
            .seq_start
            .checked_add(1)
            .context("Invalid item position")?;
        return Ok(json!({"direction":"before", "limit":1,
            "cursor":{"epoch":detail.epoch,"seq":before}}));
    }
    let mut options = json!({"direction":"before", "limit":count});
    if let Some(cursor) = input.get("cursor") {
        let cursor: Cursor = serde_json::from_value(cursor.clone())?;
        epoch(&cursor.epoch)?;
        ensure!(
            cursor.min_seq > 0
                && cursor.before_seq >= cursor.min_seq
                && cursor.max_seq >= cursor.before_seq,
            "Invalid activity cursor"
        );
        options["cursor"] = json!({"epoch":cursor.epoch,"seq":cursor.before_seq});
    }
    Ok(options)
}

fn read_ref(scope: &Value, detail: Value) -> Value {
    let mut reference = scope.clone();
    reference["detail"] = detail;
    reference
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// Serialize through a bounded window instead of allocating a complete snapshot
// or tool result just to return its prefix. The digest covers the full value.
struct JsonWindow {
    offset: usize,
    length: usize,
    total: usize,
    bytes: Vec<u8>,
    hash: Option<Sha256>,
}

impl Write for JsonWindow {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(hash) = &mut self.hash {
            hash.update(bytes);
        }
        let start = self.offset.saturating_sub(self.total).min(bytes.len());
        let remaining = self.length.saturating_sub(self.bytes.len());
        let end = start + remaining.min(bytes.len() - start);
        self.bytes.extend_from_slice(&bytes[start..end]);
        self.total += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Serialized {
    text: String,
    total: usize,
    revision: Option<String>,
}

fn serialize(
    value: &Value,
    offset: usize,
    length: usize,
    with_revision: bool,
) -> Result<Serialized> {
    let mut window = JsonWindow {
        offset,
        length,
        total: 0,
        bytes: Vec::with_capacity(length),
        hash: with_revision.then(Sha256::new),
    };
    serde_json::to_writer(&mut window, value)?;
    ensure!(
        offset <= window.total,
        "Detail offset exceeds the JSON length"
    );
    match std::str::from_utf8(&window.bytes) {
        Ok(_) => {}
        Err(error) if error.error_len().is_none() => window.bytes.truncate(error.valid_up_to()),
        Err(_) => bail!("Detail offset must be a UTF-8 character boundary"),
    }
    Ok(Serialized {
        text: String::from_utf8(window.bytes)?,
        total: window.total,
        revision: window.hash.map(|hash| format!("{:x}", hash.finalize())),
    })
}

fn preview(value: &Value, path: &str, pin: bool) -> Result<Value> {
    let (mut summary, revision) = if let Some(text) = value.as_str() {
        if text.len() <= PREVIEW_BYTES {
            return Ok(value.clone());
        }
        let revision = if pin {
            serialize(value, 0, 0, true)?.revision
        } else {
            None
        };
        (
            json!({"preview":prefix(text, PREVIEW_BYTES),"truncated":true,"path":path}),
            revision,
        )
    } else {
        let encoded = serialize(value, 0, PREVIEW_BYTES, pin)?;
        if encoded.text.len() == encoded.total {
            return Ok(value.clone());
        }
        (
            json!({"preview":encoded.text,"encoding":"json","truncated":true,"path":path}),
            encoded.revision,
        )
    };
    if let Some(revision) = revision {
        summary["revision"] = json!(revision);
    }
    Ok(summary)
}

fn chunk(encoded: Serialized, offset: usize, mut reference: Value) -> Value {
    let end = offset + encoded.text.len();
    let next = if end < encoded.total {
        reference["detail"]["offset"] = json!(end);
        reference
    } else {
        Value::Null
    };
    json!({"encoding":"json-utf8", "offset":offset,"totalBytes":encoded.total,
        "chunk":encoded.text,"nextRead":next})
}

fn is_failure(item: &Value) -> bool {
    item["type"] == "error"
        || item["status"] == "failed"
        || item["status"] == "error"
        || ["error", "errorText"].iter().any(|field| {
            item.get(*field)
                .is_some_and(|value| !value.is_null() && value != "")
        })
        || item["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("[System Error]"))
}

pub(crate) fn activity(page: &Value, snapshot: &Value, input: &Value) -> Result<Value> {
    let scope = scope(input)?;
    let count = limit(input)?;
    let failures = failures_only(input)?;
    let page_epoch = page["epoch"]
        .as_str()
        .context("Timeline page has no epoch")?;
    epoch(page_epoch)?;
    let max_seq = page["window"]["maxSeq"]
        .as_i64()
        .context("Timeline page has no high-water mark")?;
    let min_seq = page["window"]["minSeq"]
        .as_i64()
        .context("Timeline page has no retention boundary")?;
    let expected = input.get("detail").or_else(|| input.get("cursor"));
    if let Some(expected) = expected {
        ensure!(
            page["reset"] != true && expected["epoch"] == page["epoch"],
            "Activity epoch changed; restart without cursor/detail. Do not combine different snapshots"
        );
    }
    let rows = page
        .get("rows")
        .or_else(|| page.get("entries"))
        .and_then(Value::as_array)
        .context("Timeline page has no rows")?;
    if let Some(detail) = input.get("detail") {
        let detail: ItemDetail = serde_json::from_value(detail.clone())?;
        let row = rows
            .iter()
            .find(|row| row["seqStart"].as_i64() == Some(detail.seq_start))
            .context("Activity item no longer exists; restart without detail")?;
        ensure!(
            row["seqEnd"].as_i64() == Some(detail.seq_end),
            "Activity item changed; restart its detail read from a new activity page"
        );
        let encoded = serialize(&row["item"], detail.offset, CHUNK_BYTES, false)?;
        let reference = read_ref(&scope, input["detail"].clone());
        let mut result = chunk(encoded, detail.offset, reference);
        result["agentId"] = scope["agentId"].clone();
        return Ok(result);
    }
    ensure!(
        rows.len() <= count,
        "Timeline server returned an expanded page; bounded before pagination is required"
    );
    let (max_seq, min_seq) = if let Some(cursor) = input.get("cursor") {
        let cursor: Cursor = serde_json::from_value(cursor.clone())?;
        ensure!(
            max_seq >= cursor.max_seq && min_seq <= cursor.min_seq,
            "Activity retention changed; restart without cursor"
        );
        ensure!(
            rows.iter().all(|row| row["seqEnd"]
                .as_i64()
                .is_some_and(|end| end <= cursor.max_seq)),
            "Activity snapshot changed in the requested page; restart without cursor instead of mixing snapshots"
        );
        (cursor.max_seq, cursor.min_seq)
    } else {
        (max_seq, min_seq)
    };
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let item = &row["item"];
        if failures && !is_failure(item) {
            continue;
        }
        let seq = row["seqStart"]
            .as_i64()
            .context("Timeline row has no start sequence")?;
        let end = row["seqEnd"]
            .as_i64()
            .context("Timeline row has no end sequence")?;
        let mut entry = json!({"seqStart":seq,"seqEnd":end,
            "detail":read_ref(&scope, json!({"epoch":page_epoch,"seqEnd":end,"seqStart":seq,"offset":0}))});
        for field in ["type", "status", "name"] {
            if let Some(value) = item.get(field) {
                entry[field] = preview(value, &format!("/{field}"), false)?;
            }
        }
        if item["type"] == "reasoning" {
            entry["omitted"] = json!("reasoning");
        } else {
            for field in ["text", "message"] {
                if let Some(value) = item.get(field) {
                    entry[field] = preview(value, &format!("/{field}"), false)?;
                }
            }
        }
        // The old curator displayed the tool name but dropped its failure body.
        if let Some(error) = item.get("errorText").filter(|value| !value.is_null()) {
            entry["errorText"] = preview(error, "/errorText", false)?;
        } else if let Some(error) = item.get("error").filter(|value| !value.is_null()) {
            let (error, path) = match error.get("content").filter(|value| value.is_string()) {
                Some(content) => (content, "/error/content"),
                None => (error, "/error"),
            };
            entry["errorText"] = preview(error, path, false)?;
        }
        entries.push(entry);
    }
    let has_older = page["hasOlder"]
        .as_bool()
        .context("Timeline page has no coverage")?;
    let next_cursor = if has_older {
        let first = rows
            .first()
            .context("Timeline page cannot advance its older cursor")?;
        json!({"epoch":page_epoch,"maxSeq":max_seq,"minSeq":min_seq,"beforeSeq":first["seqStart"]})
    } else {
        Value::Null
    };
    let mut failure_read = scope.clone();
    failure_read["view"] = json!("failures");
    let agent_status = status(snapshot, &scope)?;
    let mut result = scope;
    result["entries"] = json!(entries);
    result["agentStatus"] = agent_status;
    result["coverage"] = json!({"epoch":page_epoch,"maxSeq":max_seq,"minSeq":min_seq,
        "authority":"Retained projected timeline, not certification of complete provider history",
        "scanned":rows.len(),"shown":entries.len(),"hasOlder":has_older,
        "failureCoverage":"scanned rows only; older pages may contain failures",
        "order":"oldest to newest within each backward page",
        "detail":"Full item JSON is available through each entry's detail arguments, including omitted reasoning and truncated fields"});
    result["nextCursor"] = next_cursor;
    result["failuresRead"] = failure_read;
    Ok(result)
}

pub(crate) fn status(snapshot: &Value, input: &Value) -> Result<Value> {
    let scope = scope(input)?;
    if let Some(detail) = input.get("detail") {
        let detail: SnapshotDetail = serde_json::from_value(detail.clone())?;
        ensure!(
            detail.path.len() <= 512,
            "Snapshot detail path exceeds 512 bytes"
        );
        ensure!(
            detail.offset == 0 || detail.revision.is_some(),
            "Continuation requires a value revision"
        );
        let value = snapshot
            .pointer(&detail.path)
            .context("Snapshot detail path does not exist")?;
        let encoded = serialize(value, detail.offset, CHUNK_BYTES, true)?;
        ensure!(
            detail.revision.is_none() || detail.revision == encoded.revision,
            "Status value changed; restart this detail read at offset 0 without revision. Do not combine different revisions"
        );
        let reference = read_ref(
            &scope,
            json!({"path":detail.path,
            "revision":encoded.revision,"offset":detail.offset}),
        );
        let mut result = chunk(encoded, detail.offset, reference);
        result["agentId"] = scope["agentId"].clone();
        return Ok(result);
    }
    let mut result = scope.clone();
    let (state, state_path) = if let Some(lifecycle) = snapshot.get("lifecycle") {
        (lifecycle, "/lifecycle")
    } else {
        (&snapshot["status"], "/status")
    };
    result["status"] = preview(state, state_path, true)?;
    for field in [
        "provider",
        "title",
        "currentModeId",
        "lastError",
        "errorText",
        "preparation",
        "snapshotAvailability",
        "requiresAttention",
        "attentionReason",
    ] {
        let mut path = format!("/{field}");
        let value = if matches!(field, "requiresAttention" | "attentionReason")
            && snapshot["attention"].get(field).is_some()
        {
            path = format!("/attention/{field}");
            &snapshot["attention"][field]
        } else if field == "title" && snapshot.get(field).is_none() {
            path = "/config/title".into();
            &snapshot["config"]["title"]
        } else {
            &snapshot[field]
        };
        result[field] = preview(value, &path, true)?;
    }
    let permissions = snapshot["pendingPermissions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut shown = Vec::with_capacity(permissions.len().min(PERMISSIONS_SHOWN));
    for (index, permission) in permissions.iter().take(PERMISSIONS_SHOWN).enumerate() {
        let path = format!("/pendingPermissions/{index}");
        shown.push(json!({"path":path,"request":preview(permission, &path, true)?}));
    }
    result["pendingPermissions"] = json!({"count":permissions.len(),"shown":shown,
        "path":"/pendingPermissions","omitted":permissions.len().saturating_sub(PERMISSIONS_SHOWN)});
    result["detail"] = read_ref(&scope, json!({"path":"","offset":0}));
    result["coverage"] = json!(
        "Selected status fields and first three permissions only. Use detail with a returned path and revision for truncated values. Without revision, offset 0 starts a current-value read (empty path reads the full snapshot). Follow nextRead; concatenate chunks before JSON decoding"
    );
    Ok(result)
}

pub(crate) fn activity_properties() -> Value {
    json!({
        "agentId":{"type":"string"},
        "limit":{"type":"integer","minimum":1,"maximum":MAX_LIMIT,"default":DEFAULT_LIMIT,"description":"Maximum canonical rows inspected per backward page; tool lifecycles never expand this limit."},
        "view":{"type":"string","enum":["recent","failures"],"default":"recent","description":"failures filters each bounded page; follow nextCursor even when entries is empty to discover older failures."},
        "cursor":{"type":"object","additionalProperties":false,"required":["epoch","maxSeq","minSeq","beforeSeq"],"properties":{
            "epoch":{"type":"string"},"maxSeq":{"type":"integer"},"minSeq":{"type":"integer"},"beforeSeq":{"type":"integer"}
        },"description":"Use nextCursor unchanged. Unrelated tail growth is allowed; changed rows in the requested page, retention loss or epoch changes reject continuation. Restart and discard old pages rather than mixing snapshots."},
        "detail":{"type":"object","additionalProperties":false,"required":["epoch","seqEnd","seqStart"],"properties":{
            "epoch":{"type":"string"},"seqEnd":{"type":"integer","description":"Canonical item revision; unrelated tail growth does not invalidate it."},"seqStart":{"type":"integer"},"offset":{"type":"integer","minimum":0,"default":0}
        },"description":"Use entry.detail arguments for exact item JSON, including omitted fields/reasoning. Returns at most 4096 UTF-8 bytes; follow nextRead and concatenate chunks before decoding JSON. Mutually exclusive with cursor."}
    })
}

pub(crate) fn status_properties() -> Value {
    json!({"agentId":{"type":"string"},
        "detail":{"type":"object","additionalProperties":false,"required":["path"],"properties":{
            "path":{"type":"string","maxLength":512,"description":"JSON pointer into the original snapshot, e.g. /pendingPermissions/3, /lastError, or empty for the entire snapshot."},
            "revision":{"type":"string","description":"Use a truncated preview's revision or nextRead unchanged. Pins only the selected value, so unrelated status updates are allowed. Omit only at offset 0 to begin reading the current value."},
            "offset":{"type":"integer","minimum":0,"default":0}
        },"description":"Read exact snapshot JSON in <=4096-byte UTF-8 chunks; follow nextRead and concatenate before decoding. Changed selected values reject continuation. Omit detail for compact status."}
    })
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
