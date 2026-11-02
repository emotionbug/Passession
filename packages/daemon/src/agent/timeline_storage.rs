use super::Timeline;
use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use serde_json::{Value, json};
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};

const UPSERT_ROW: &str = "INSERT INTO timeline_rows(position, row_json) VALUES (?1, ?2) \
    ON CONFLICT(position) DO UPDATE SET row_json = excluded.row_json";
const UPDATE_METADATA: &str = "UPDATE timeline_metadata SET epoch = ?1, next_seq = ?2, min_seq = ?3 WHERE id = 1";
const READ_METADATA: &str = "SELECT version, agent_id, epoch, next_seq, min_seq FROM timeline_metadata WHERE id = 1";
/// Everything `Timeline::fetch` selection reads from a row, without its item.
const ROW_PAGE_METADATA: &str = "SELECT position, json_extract(row_json, '$.seqStart'), \
    json_extract(row_json, '$.seqEnd'), json_extract(row_json, '$.sourceSeqRanges') \
    FROM timeline_rows ORDER BY position";

#[derive(Debug)]
pub(super) struct Store {
    connection: Connection,
    path: PathBuf,
    agent_id: String,
}

impl Store {
    fn open(path: &Path, agent_id: &str) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let (version, stored_agent): (i64, String) = connection.query_row(
            "SELECT version, agent_id FROM timeline_metadata WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(version == 1, "Unsupported timeline database version");
        ensure!(stored_agent == agent_id, "Timeline checkpoint agent mismatch");
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self { connection, path: path.to_owned(), agent_id: agent_id.to_owned() })
    }

    fn check_target(&self, path: &Path, agent_id: &str) -> Result<()> {
        ensure!(self.path == path && self.agent_id == agent_id, "Timeline storage target mismatch");
        Ok(())
    }

    fn replace(&mut self, timeline: &Timeline) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM timeline_rows", [])?;
        write_rows(&transaction, &timeline.rows)?;
        write_metadata(&transaction, timeline)?;
        transaction.commit()?;
        Ok(())
    }
}

fn write_metadata(transaction: &Transaction<'_>, timeline: &Timeline) -> Result<()> {
    transaction.prepare_cached(UPDATE_METADATA)?.execute(params![
        timeline.epoch, timeline.next_seq, timeline.min_seq
    ])?;
    Ok(())
}

fn write_rows(transaction: &Transaction<'_>, rows: &[Value]) -> Result<()> {
    let mut statement = transaction.prepare_cached(UPSERT_ROW)?;
    for (index, row) in rows.iter().enumerate() {
        statement.execute(params![index as i64, serde_json::to_string(row)?])?;
    }
    Ok(())
}

// Build the complete initial database off to the side, so a failed creation
// never leaves a partial database behind.
fn create(path: &Path, agent_id: &str, timeline: &Timeline) -> Result<Store> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let file = tempfile::NamedTempFile::new_in(parent)?;
    let mut connection = Connection::open(file.path())?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE timeline_metadata (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            version INTEGER NOT NULL,
            agent_id TEXT NOT NULL,
            epoch TEXT NOT NULL,
            next_seq INTEGER NOT NULL CHECK(next_seq > 0),
            min_seq INTEGER NOT NULL CHECK(min_seq >= 0)
        );
        CREATE TABLE timeline_rows (
            position INTEGER PRIMARY KEY CHECK(position >= 0),
            row_json TEXT NOT NULL
        );"
    )?;
    transaction.execute(
        "INSERT INTO timeline_metadata VALUES (1, 1, ?1, ?2, ?3, ?4)",
        params![agent_id, timeline.epoch, timeline.next_seq, timeline.min_seq],
    )?;
    write_rows(&transaction, &timeline.rows)?;
    transaction.commit()?;
    drop(connection);
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Store::open(path, agent_id)
}

/// `path` names the SQLite database. An absent database is an empty timeline;
/// it is created on the first commit.
pub(super) fn load(path: &Path, agent_id: &str) -> Result<Timeline> {
    load_measured(path, agent_id).map(|(timeline, _)| timeline)
}

/// Serialized JSON length of a row: the stored size, without allocating it.
pub(super) fn encoded_bytes(row: &Value) -> u64 {
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, row).expect("JSON value serializes");
    count.0
}

fn valid_row(row: &Value) -> bool {
    row["seq"].as_i64().is_some_and(|seq| seq > 0) && row["item"].is_object() && row["timestamp"].is_string()
}

/// `load`, plus the stored JSON bytes of the hydrated rows.
pub(super) fn load_measured(path: &Path, agent_id: &str) -> Result<(Timeline, u64)> {
    if !path.try_exists()? {
        return Ok((Timeline::new(), 0));
    }
    let mut store = Store::open(path, agent_id)?;
    let transaction = store.connection.transaction()?;
    let (epoch, next_seq, min_seq): (String, i64, i64) = transaction.query_row(
        "SELECT epoch, next_seq, min_seq FROM timeline_metadata WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let mut timeline = Timeline::new();
    timeline.epoch = epoch;
    timeline.next_seq = next_seq;
    timeline.min_seq = min_seq;
    let mut bytes = 0;
    {
        let mut statement = transaction.prepare("SELECT position, row_json FROM timeline_rows ORDER BY position")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let index = timeline.rows.len();
            ensure!(row.get::<_, i64>(0)? == index as i64, "Invalid timeline row position");
            let encoded: String = row.get(1)?;
            bytes += encoded.len() as u64;
            let row: Value = serde_json::from_str(&encoded)?;
            ensure!(valid_row(&row), "Invalid timeline row");
            if let Some(id) = super::identity(&row["item"]) {
                timeline.identities.insert(id, index);
            }
            if row["item"]["type"] == "user_message" {
                if let Some(id) = row["item"]["clientMessageId"].as_str().filter(|id| !id.is_empty()) {
                    timeline.submitted.entry(id.to_owned()).or_insert(index);
                }
            }
            timeline.rows.push(row);
        }
    }
    transaction.commit()?;
    timeline.storage = Some(store);
    Ok((timeline, bytes))
}

/// One page of a stored timeline without hydrating it. `Timeline::fetch` first
/// selects over metadata-only rows (`seqStart`, `seqEnd`, `sourceSeqRanges`:
/// all that page selection reads), then runs again with only the selected rows'
/// stored JSON in place, so epochs, cursors, gaps, limits and source ranges
/// behave exactly as for a hydrated timeline. Transient memory is one small
/// metadata stub per row plus the selected rows; `limit: 0` explicitly selects
/// the whole history. Only reads; never alters the database.
pub(super) fn fetch_page(path: &Path, agent_id: &str, options: &Value) -> Result<Value> {
    ensure!(path.try_exists()?, "Timeline database missing: {}", path.display());
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let transaction = connection.transaction()?;
    let (version, stored_agent, epoch, next_seq, min_seq): (i64, String, String, i64, i64) =
        transaction.query_row(READ_METADATA, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })?;
    ensure!(version == 1, "Unsupported timeline database version");
    ensure!(stored_agent == agent_id, "Timeline checkpoint agent mismatch");
    let mut timeline = Timeline::new();
    timeline.epoch = epoch;
    timeline.next_seq = next_seq;
    timeline.min_seq = min_seq;
    {
        let mut statement = transaction.prepare(ROW_PAGE_METADATA)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let index = timeline.rows.len();
            ensure!(row.get::<_, i64>(0)? == index as i64, "Invalid timeline row position");
            let (Some(start), Some(end), Some(ranges)) =
                (row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?, row.get::<_, Option<String>>(3)?)
            else {
                bail!("Invalid timeline row metadata");
            };
            let ranges: Value = serde_json::from_str(&ranges)?;
            ensure!(ranges.is_array(), "Invalid timeline row metadata");
            timeline.rows.push(json!({"seqStart":start,"seqEnd":end,"sourceSeqRanges":ranges,"position":index}));
        }
    }
    let selected: Vec<usize> = timeline.fetch(options)["rows"].as_array().expect("page rows").iter()
        .map(|stub| stub["position"].as_u64().expect("stub position") as usize)
        .collect();
    {
        let mut statement = transaction.prepare("SELECT row_json FROM timeline_rows WHERE position = ?1")?;
        for index in selected {
            let encoded: String = statement.query_row([index as i64], |row| row.get(0))?;
            let row: Value = serde_json::from_str(&encoded)?;
            ensure!(valid_row(&row), "Invalid timeline row");
            timeline.rows[index] = row;
        }
    }
    transaction.commit()?;
    Ok(timeline.fetch(options))
}

/// Full-history hydration/replacement is one transaction. Keep the connection
/// with its owner so subsequent token commits touch only one projected row.
pub(super) fn persist(timeline: &mut Timeline, path: &Path, agent_id: &str) -> Result<()> {
    if let Some(store) = &timeline.storage {
        store.check_target(path, agent_id)?;
    }
    if timeline.storage.is_none() {
        if !path.try_exists()? {
            timeline.storage = Some(create(path, agent_id, timeline)?);
            timeline.storage_dirty = false;
            return Ok(());
        }
        timeline.storage = Some(Store::open(path, agent_id)?);
    }
    let mut store = timeline.storage.take().unwrap();
    timeline.storage_dirty = true;
    let result = store.replace(timeline);
    timeline.storage = Some(store);
    result?;
    timeline.storage_dirty = false;
    Ok(())
}

fn ensure_storage(timeline: &mut Timeline, path: &Path, agent_id: &str) -> Result<()> {
    if let Some(store) = &timeline.storage {
        store.check_target(path, agent_id)?;
    }
    if timeline.storage.is_none() || timeline.storage_dirty {
        persist(timeline, path, agent_id)?;
    }
    Ok(())
}

fn persist_row(timeline: &mut Timeline, index: usize) -> Result<()> {
    // Serialization is bounded by the affected projected row, not history size.
    let encoded = serde_json::to_string(&timeline.rows[index])?;
    let store = timeline.storage.as_mut().unwrap();
    let transaction = store.connection.transaction()?;
    transaction.prepare_cached(UPSERT_ROW)?.execute(params![index as i64, encoded])?;
    transaction.prepare_cached(UPDATE_METADATA)?.execute(params![
        timeline.epoch, timeline.next_seq, timeline.min_seq
    ])?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn append(
    timeline: &mut Timeline,
    path: &Path,
    agent_id: &str,
    item: Value,
    options: &Value,
) -> Result<Value> {
    ensure_storage(timeline, path, agent_id)?;
    let old_next = timeline.next_seq;
    let old_min = timeline.min_seq;
    let old_len = timeline.rows.len();
    let id = super::identity(&item);
    let old_identity = id.as_ref().and_then(|id| timeline.identities.get(id).copied());
    let candidate = old_identity.or_else(|| {
        if matches!(super::kind(&item), "assistant_message" | "reasoning") {
            old_len.checked_sub(1).filter(|&index| {
                super::kind(&timeline.rows[index]["item"]) == super::kind(&item)
            })
        } else {
            None
        }
    });
    let old_row = candidate.map(|index| (index, timeline.rows[index].clone()));
    let submitted_id = (item["type"] == "user_message")
        .then(|| item["clientMessageId"].as_str().map(str::to_owned)).flatten();
    let old_submitted = submitted_id.as_ref().and_then(|id| timeline.submitted.get(id).copied());
    let (row, index) = timeline.append_indexed(item, options);
    if let Err(error) = persist_row(timeline, index) {
        timeline.rows.truncate(old_len);
        if let Some((index, row)) = old_row {
            timeline.rows[index] = row;
        }
        timeline.next_seq = old_next;
        timeline.min_seq = old_min;
        if let Some(id) = id {
            if let Some(index) = old_identity {
                timeline.identities.insert(id, index);
            } else {
                timeline.identities.remove(&id);
            }
        }
        if let Some(id) = submitted_id {
            if let Some(index) = old_submitted {
                timeline.submitted.insert(id, index);
            } else {
                timeline.submitted.remove(&id);
            }
        }
        return Err(error);
    }
    Ok(row)
}

pub(super) fn enrich(
    timeline: &mut Timeline,
    path: &Path,
    agent_id: &str,
    message_id: &str,
    provider_id: &str,
) -> Result<Option<Value>> {
    let Some(&index) = timeline.submitted.get(message_id) else {
        return Ok(None);
    };
    ensure_storage(timeline, path, agent_id)?;
    let previous = timeline.rows[index].get("providerMessageId").cloned();
    timeline.rows[index]["providerMessageId"] = Value::String(provider_id.to_owned());
    if let Err(error) = persist_row(timeline, index) {
        if let Some(previous) = previous {
            timeline.rows[index]["providerMessageId"] = previous;
        } else {
            timeline.rows[index].as_object_mut().unwrap().remove("providerMessageId");
        }
        return Err(error);
    }
    Ok(Some(timeline.rows[index].clone()))
}
