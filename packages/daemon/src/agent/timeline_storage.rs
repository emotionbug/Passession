use super::{RESIDENT_BYTES, RESIDENT_ROWS, Timeline};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const UPSERT_ROW: &str = "INSERT INTO timeline_rows(position, row_json) VALUES (?1, ?2) \
    ON CONFLICT(position) DO UPDATE SET row_json = excluded.row_json";
const UPDATE_METADATA: &str =
    "UPDATE timeline_metadata SET epoch = ?1, next_seq = ?2, min_seq = ?3 WHERE id = 1";
const READ_METADATA: &str =
    "SELECT version, agent_id, epoch, next_seq, min_seq FROM timeline_metadata WHERE id = 1";
// Row mutations invalidate native revision certificates in the same transaction.
const REVISION_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS timeline_native_revision (
    id INTEGER PRIMARY KEY CHECK(id = 1), revision_json TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS timeline_revision_insert AFTER INSERT ON timeline_rows
BEGIN DELETE FROM timeline_native_revision; END;
CREATE TRIGGER IF NOT EXISTS timeline_revision_update AFTER UPDATE ON timeline_rows
BEGIN DELETE FROM timeline_native_revision; END;
CREATE TRIGGER IF NOT EXISTS timeline_revision_delete AFTER DELETE ON timeline_rows
BEGIN DELETE FROM timeline_native_revision; END;";
const CREATE_PAGE_INDEX: &str = "CREATE INDEX timeline_rows_page ON timeline_rows(\
    position, json_extract(row_json, '$.seqStart'), \
    json_extract(row_json, '$.seqEnd'), json_extract(row_json, '$.sourceSeqRanges'))";
/// Page metadata of the stored rows before a resident tail.
const STORED_PAGE_METADATA: &str = "SELECT position, json_extract(row_json, '$.seqStart'), \
    json_extract(row_json, '$.seqEnd'), json_extract(row_json, '$.sourceSeqRanges') \
    FROM timeline_rows INDEXED BY timeline_rows_page WHERE position < ?1 ORDER BY position";
const STORED_SEQ_END: &str = "SELECT MAX(json_extract(row_json, '$.seqEnd')) \
    FROM timeline_rows INDEXED BY timeline_rows_page WHERE position < ?1";
const ROW_AT: &str = "SELECT position, row_json FROM timeline_rows WHERE position = ?1";
const SCAN_ROWS: &str = "SELECT row_json FROM timeline_rows WHERE position < ?1 ORDER BY position";
const SCAN_KINDS: &str = "SELECT row_json FROM timeline_rows WHERE position < ?1 \
    AND json_extract(row_json, '$.item.type') IN (?2, ?3) ORDER BY position";
const LAST_OF_KIND: &str = "SELECT position, row_json FROM timeline_rows WHERE position < ?1 \
    AND json_extract(row_json, '$.item.type') = ?2 ORDER BY position DESC LIMIT 1";
const ENDING_AT: &str = "SELECT position, row_json FROM timeline_rows WHERE position < ?1 \
    AND json_extract(row_json, '$.seqEnd') = ?2 ORDER BY position LIMIT 1";
/// `super::identity` in SQL: a tool call's string `callId`, or `pluginId/id`.
const IDENTITY_KEY: &str = "CASE json_extract(row_json, '$.item.type') \
    WHEN 'tool_call' THEN CASE json_type(row_json, '$.item.callId') \
    WHEN 'text' THEN json_extract(row_json, '$.item.callId') END \
    WHEN 'plugin' THEN CASE WHEN json_type(row_json, '$.item.pluginId') = 'text' \
    AND json_type(row_json, '$.item.id') = 'text' \
    THEN json_extract(row_json, '$.item.pluginId') || '/' || json_extract(row_json, '$.item.id') \
    END END";
/// A submitted user message's string `clientMessageId`.
const SUBMITTED_KEY: &str = "CASE WHEN json_extract(row_json, '$.item.type') = 'user_message' \
    AND json_type(row_json, '$.item.clientMessageId') = 'text' \
    THEN json_extract(row_json, '$.item.clientMessageId') END";
/// Identity lookups for rows outside the resident tail, maintained by SQLite.
static LOOKUP_INDEXES: LazyLock<[(&str, String); 2]> = LazyLock::new(|| {
    [
        (
            "timeline_rows_identity",
            format!(
                "CREATE INDEX timeline_rows_identity ON timeline_rows({IDENTITY_KEY}, position)"
            ),
        ),
        (
            "timeline_rows_submitted",
            format!(
                "CREATE INDEX timeline_rows_submitted ON timeline_rows({SUBMITTED_KEY}, position)"
            ),
        ),
    ]
});
/// The latest stored row with an identity: `identities` keeps the latest.
static FIND_IDENTITY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT position, row_json FROM timeline_rows WHERE {IDENTITY_KEY} = ?1 \
        AND position < ?2 ORDER BY position DESC LIMIT 1"
    )
});
/// The first stored submission of a client message: `submitted` keeps the first.
static FIND_SUBMITTED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT position, row_json FROM timeline_rows WHERE {SUBMITTED_KEY} = ?1 \
        AND position < ?2 ORDER BY position LIMIT 1"
    )
});

/// Checkpoint projections are disposable; only the complete current schema is reusable.
fn current_schema(connection: &Connection, agent_id: &str) -> Result<bool> {
    let (current, metadata): (bool, bool) = connection.query_row(
        "SELECT count(*) = 9, COALESCE(max(type = 'table' AND name = 'timeline_metadata'), 0)
         FROM sqlite_schema WHERE
         (type = 'table' AND name IN ('timeline_metadata', 'timeline_rows', 'timeline_native_revision'))
         OR (type = 'index' AND name IN ('timeline_rows_page', 'timeline_rows_identity', 'timeline_rows_submitted'))
         OR (type = 'trigger' AND name IN ('timeline_revision_insert', 'timeline_revision_update', 'timeline_revision_delete'))",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if metadata && !current {
        let (version, stored_agent): (i64, String) = connection.query_row(
            "SELECT version, agent_id FROM timeline_metadata WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(version == 1, "Unsupported timeline database version");
        ensure!(
            stored_agent == agent_id,
            "Timeline checkpoint agent mismatch"
        );
    }
    Ok(current)
}

/// Read-only callers treat an obsolete projection like an absent checkpoint.
pub(super) fn checkpoint_exists(path: &Path, agent_id: &str) -> Result<bool> {
    if !path.try_exists()? {
        return Ok(false);
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    current_schema(&connection, agent_id)
}

#[derive(Debug)]
pub(super) struct Store {
    connection: Connection,
    path: PathBuf,
    agent_id: String,
    /// A saved reader's snapshot; it never writes.
    writable: bool,
    /// Owns disposable replay storage; it never becomes a durable checkpoint.
    temporary: Option<tempfile::TempDir>,
}

impl Store {
    fn open(path: &Path, agent_id: &str) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        if !current_schema(&connection, agent_id)? {
            drop(connection);
            for suffix in ["-wal", "-shm", ""] {
                let mut file = path.as_os_str().to_os_string();
                file.push(suffix);
                match fs::remove_file(Path::new(&file)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            return create(path, agent_id, &Timeline::new()).map(|(store, _)| store);
        }
        let (version, stored_agent): (i64, String) = connection.query_row(
            "SELECT version, agent_id FROM timeline_metadata WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(version == 1, "Unsupported timeline database version");
        ensure!(
            stored_agent == agent_id,
            "Timeline checkpoint agent mismatch"
        );
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self {
            connection,
            path: path.to_owned(),
            agent_id: agent_id.to_owned(),
            writable: true,
            temporary: None,
        })
    }

    fn check_target(&self, path: &Path, agent_id: &str) -> Result<()> {
        ensure!(
            self.path == path && self.agent_id == agent_id,
            "Timeline storage target mismatch"
        );
        ensure!(self.writable, "Saved timeline snapshots are read-only");
        Ok(())
    }

    fn replace(&mut self, timeline: &Timeline) -> Result<Vec<u64>> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM timeline_rows", [])?;
        let sizes = write_rows(&transaction, &timeline.rows)?;
        write_metadata(&transaction, timeline)?;
        transaction.commit()?;
        Ok(sizes)
    }

    /// One stored row selected by `sql` with `params`, and its position.
    fn find(&self, sql: &str, params: impl rusqlite::Params) -> Result<Option<(usize, Value)>> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let mut rows = statement.query(params)?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let position = usize::try_from(row.get::<_, i64>(0)?)?;
        Ok(Some((position, decode(row.get_ref(1)?.as_str()?)?)))
    }

    /// Stored rows before `end` in position order, filtered to at most two
    /// item `kinds` (none: every row), until `visit` returns false.
    fn scan(
        &self,
        end: usize,
        kinds: &[&str],
        visit: &mut dyn FnMut(Cow<'_, Value>) -> Result<bool>,
    ) -> Result<bool> {
        debug_assert!(kinds.len() <= 2);
        let mut statement;
        let mut rows = if kinds.is_empty() {
            statement = self.connection.prepare_cached(SCAN_ROWS)?;
            statement.query([end as i64])?
        } else {
            statement = self.connection.prepare_cached(SCAN_KINDS)?;
            statement.query(params![end as i64, kinds[0], kinds[kinds.len() - 1]])?
        };
        while let Some(row) = rows.next()? {
            if !visit(Cow::Owned(decode(row.get_ref(0)?.as_str()?)?))? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn decode(encoded: &str) -> Result<Value> {
    let row: Value = serde_json::from_str(encoded)?;
    ensure!(valid_row(&row), "Invalid timeline row");
    Ok(row)
}

fn read_page_metadata(row: &rusqlite::Row<'_>, position: usize) -> Result<Value> {
    ensure!(
        row.get::<_, i64>(0)? == position as i64,
        "Invalid timeline row position"
    );
    let (Some(start), Some(end), Some(ranges)) = (
        row.get::<_, Option<i64>>(1)?,
        row.get::<_, Option<i64>>(2)?,
        row.get::<_, Option<String>>(3)?,
    ) else {
        bail!("Invalid timeline row metadata");
    };
    let ranges: Value = serde_json::from_str(&ranges)?;
    ensure!(
        valid_page_metadata(start, end, &ranges),
        "Invalid timeline row metadata"
    );
    Ok(json!({"seqStart":start,"seqEnd":end,"sourceSeqRanges":ranges,"position":position}))
}

fn valid_page_metadata(start: i64, end: i64, ranges: &Value) -> bool {
    start > 0
        && end >= start
        && ranges.as_array().is_some_and(|ranges| {
            !ranges.is_empty()
                && ranges.iter().all(|range| {
                    matches!((range["startSeq"].as_i64(), range["endSeq"].as_i64()),
                (Some(a), Some(b)) if a >= start && b >= a && b <= end)
                })
        })
}

fn write_metadata(transaction: &Transaction<'_>, timeline: &Timeline) -> Result<()> {
    transaction
        .prepare_cached(UPDATE_METADATA)?
        .execute(params![timeline.epoch, timeline.next_seq, timeline.min_seq])?;
    write_revision(transaction, timeline.native_revision.as_ref())?;
    Ok(())
}

fn write_revision(transaction: &Transaction<'_>, revision: Option<&Value>) -> Result<()> {
    transaction.execute("DELETE FROM timeline_native_revision", [])?;
    if let Some(revision) = revision {
        transaction.execute(
            "INSERT INTO timeline_native_revision VALUES (1, ?1)",
            [serde_json::to_string(revision)?],
        )?;
    }
    Ok(())
}

/// Writes every row and returns each one's stored JSON length.
fn write_rows(transaction: &Transaction<'_>, rows: &[Value]) -> Result<Vec<u64>> {
    let mut statement = transaction.prepare_cached(UPSERT_ROW)?;
    let mut sizes = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let encoded = serde_json::to_string(row)?;
        statement.execute(params![index as i64, encoded])?;
        sizes.push(encoded.len() as u64);
    }
    Ok(sizes)
}

/// Copies disk-backed projections one row at a time; sizes describe only the
/// resident tail, just as they do after a normal bounded load.
fn write_projection(transaction: &Transaction<'_>, timeline: &Timeline) -> Result<Vec<u64>> {
    let mut statement = transaction.prepare_cached(UPSERT_ROW)?;
    let mut sizes = Vec::with_capacity(timeline.rows.len());
    let mut position = 0usize;
    timeline.scan(&[], |row| {
        let encoded = serde_json::to_string(row.as_ref())?;
        statement.execute(params![position as i64, encoded])?;
        if position >= timeline.base {
            sizes.push(encoded.len() as u64);
        }
        position += 1;
        Ok(true)
    })?;
    Ok(sizes)
}

/// Rows are contiguous from position 0: the stored count.
fn row_count(connection: &Connection) -> Result<usize> {
    let (count, last): (i64, Option<i64>) = connection.query_row(
        "SELECT COUNT(*), MAX(position) FROM timeline_rows",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        last.unwrap_or(-1) == count - 1,
        "Invalid timeline row position"
    );
    Ok(usize::try_from(count)?)
}

// Build the complete initial database off to the side, so a failed creation
// never leaves a partial database behind.
fn create(path: &Path, agent_id: &str, timeline: &Timeline) -> Result<(Store, Vec<u64>)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
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
        );",
    )?;
    transaction.execute(
        "INSERT INTO timeline_metadata VALUES (1, 1, ?1, ?2, ?3, ?4)",
        params![
            agent_id,
            timeline.epoch,
            timeline.next_seq,
            timeline.min_seq
        ],
    )?;
    transaction.execute_batch(CREATE_PAGE_INDEX)?;
    for (_, sql) in LOOKUP_INDEXES.iter() {
        transaction.execute_batch(sql)?;
    }
    let sizes = write_projection(&transaction, timeline)?;
    transaction.execute_batch(REVISION_SCHEMA)?;
    write_revision(&transaction, timeline.native_revision.as_ref())?;
    transaction.commit()?;
    drop(connection);
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok((Store::open(path, agent_id)?, sizes))
}

/// `path` names the SQLite database. An absent database is an empty timeline;
/// it is created on the first commit. Only the bounded recent tail is read;
/// older rows are read on demand.
pub(super) fn load(path: &Path, agent_id: &str) -> Result<Timeline> {
    hydrate(path, agent_id, true).map(|(timeline, _)| timeline)
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
    row["seq"].as_i64().is_some_and(|seq| seq > 0)
        && row["item"].is_object()
        && row["timestamp"].is_string()
        && matches!((row["seqStart"].as_i64(), row["seqEnd"].as_i64()),
            (Some(start), Some(end)) if valid_page_metadata(start, end, &row["sourceSeqRanges"]))
}

/// Every row of the checkpoint, plus the stored JSON bytes of the hydrated
/// rows. Provider child timelines bound their residency as whole timelines.
pub(super) fn load_measured(path: &Path, agent_id: &str) -> Result<(Timeline, u64)> {
    hydrate(path, agent_id, false)
}

/// Reads rows newest first: all of them, or the bounded resident tail.
fn hydrate(path: &Path, agent_id: &str, bounded: bool) -> Result<(Timeline, u64)> {
    let mut timeline = Timeline::new();
    timeline.bounded = bounded;
    if !path.try_exists()? {
        return Ok((timeline, 0));
    }
    let mut store = Store::open(path, agent_id)?;
    let transaction = store.connection.transaction()?;
    let (epoch, next_seq, min_seq): (String, i64, i64) = transaction.query_row(
        "SELECT epoch, next_seq, min_seq FROM timeline_metadata WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    timeline.epoch = epoch;
    timeline.next_seq = next_seq;
    timeline.min_seq = min_seq;
    let encoded: Option<String> = transaction
        .query_row(
            "SELECT revision_json FROM timeline_native_revision WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    timeline.native_revision = encoded.and_then(|encoded| serde_json::from_str(&encoded).ok());
    let count = row_count(&transaction)?;
    let mut bytes = 0;
    {
        let mut statement = transaction
            .prepare("SELECT position, row_json FROM timeline_rows ORDER BY position DESC")?;
        let mut rows = statement.query([])?;
        while !bounded || timeline.rows.len() < RESIDENT_ROWS {
            let Some(row) = rows.next()? else {
                break;
            };
            ensure!(
                row.get::<_, i64>(0)? == count as i64 - 1 - timeline.rows.len() as i64,
                "Invalid timeline row position"
            );
            let encoded = row.get_ref(1)?.as_str()?;
            let size = encoded.len() as u64;
            if bounded && !timeline.rows.is_empty() && bytes + size > RESIDENT_BYTES {
                break;
            }
            timeline.rows.push(decode(encoded)?);
            timeline.sizes.push(size);
            bytes += size;
        }
    }
    timeline.base = count - timeline.rows.len();
    if timeline.base > 0 {
        let end: Option<i64> =
            transaction.query_row(STORED_SEQ_END, [timeline.base as i64], |row| row.get(0))?;
        timeline.evicted_seq_end = end.context("Invalid timeline row metadata")?;
    }
    transaction.commit()?;
    timeline.rows.reverse();
    timeline.sizes.reverse();
    for (offset, row) in timeline.rows.iter().enumerate() {
        let position = timeline.base + offset;
        if let Some(id) = super::identity(&row["item"]) {
            timeline.identities.insert(id, position);
        }
        if row["item"]["type"] == "user_message"
            && let Some(id) = row["item"]["clientMessageId"]
                .as_str()
                .filter(|id| !id.is_empty())
        {
            timeline.submitted.entry(id.to_owned()).or_insert(position);
        }
    }
    timeline.storage = Some(store);
    Ok((timeline, bytes))
}

/// An existing checkpoint as one read-only snapshot: header only, no rows resident.
pub(super) fn open_saved(path: &Path, agent_id: &str) -> Result<Timeline> {
    ensure!(
        path.try_exists()?,
        "Timeline database missing: {}",
        path.display()
    );
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Every later read through this handle sees the same committed state.
    connection.execute_batch("BEGIN")?;
    ensure!(
        current_schema(&connection, agent_id)?,
        "Timeline checkpoint schema is obsolete"
    );
    let (version, stored_agent, epoch, next_seq, min_seq): (i64, String, String, i64, i64) =
        connection.query_row(READ_METADATA, [], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
    ensure!(version == 1, "Unsupported timeline database version");
    ensure!(
        stored_agent == agent_id,
        "Timeline checkpoint agent mismatch"
    );
    let count = row_count(&connection)?;
    let mut timeline = Timeline::new();
    timeline.epoch = epoch;
    timeline.next_seq = next_seq;
    timeline.min_seq = min_seq;
    timeline.base = count;
    timeline.evicted_seq_end = i64::MAX;
    timeline.storage = Some(Store {
        connection,
        path: path.to_owned(),
        agent_id: agent_id.to_owned(),
        writable: false,
        temporary: None,
    });
    Ok(timeline)
}

/// One page of a stored timeline without hydrating it.
pub(super) fn fetch_page(path: &Path, agent_id: &str, options: &Value) -> Result<Value> {
    open_saved(path, agent_id)?.page(options)
}

impl Timeline {
    /// A private projection database with the normal bounded resident tail.
    /// Source-event writes are disposable: no WAL or fsync until final commit.
    pub(in crate::agent) fn history_stage() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("history.sqlite3");
        let mut timeline = Self::new();
        let (mut store, sizes) = create(&path, "history-stage", &timeline)?;
        store.connection.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
             PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;",
        )?;
        store.temporary = Some(directory);
        timeline.storage = Some(store);
        timeline.sizes = sizes;
        timeline.bounded = true;
        Ok(timeline)
    }

    pub(in crate::agent) fn append_history_item(
        &mut self,
        item: Value,
        options: &Value,
    ) -> Result<()> {
        let row = self.source_row(item, options);
        project_row(self, row)?;
        Ok(())
    }

    /// Reprojects canonical rows during reconciliation without inventing source
    /// sequence numbers. Only the affected row and bounded tail are resident.
    pub(super) fn append_history_row(&mut self, row: Value) -> Result<()> {
        let next = self.next_seq;
        project_row(self, row)?;
        self.next_seq = next;
        Ok(())
    }

    pub(super) fn history_row(&self, position: usize) -> Result<Value> {
        if position >= self.base {
            return self
                .rows
                .get(position - self.base)
                .cloned()
                .context("Missing history row");
        }
        self.store()?
            .find(ROW_AT, [position as i64])?
            .map(|(_, row)| row)
            .context("Missing history row")
    }

    /// Copies the disposable projection into the real checkpoint atomically.
    /// Do not retarget the in-memory timeline until its transaction commits.
    pub(in crate::agent) fn commit_history(&mut self, path: &Path, agent_id: &str) -> Result<()> {
        if !path.try_exists()? {
            let (destination, sizes) = create(path, agent_id, self)?;
            self.storage = Some(destination);
            self.sizes = sizes;
            self.storage_dirty = false;
            return Ok(());
        }
        let mut destination = Store::open(path, agent_id)?;
        let transaction = destination.connection.transaction()?;
        transaction.execute("DELETE FROM timeline_rows", [])?;
        let sizes = write_projection(&transaction, self)?;
        write_metadata(&transaction, self)?;
        transaction.commit()?;
        self.storage = Some(destination);
        self.sizes = sizes;
        self.storage_dirty = false;
        Ok(())
    }

    fn store(&self) -> Result<&Store> {
        self.storage.as_ref().context("Timeline storage missing")
    }

    /// One page with exactly the selection of a fully resident timeline.
    /// Pages the resident tail answers alone never touch SQLite. Otherwise
    /// `fetch` selection runs over metadata-only stubs of the stored rows
    /// (`seqStart`, `seqEnd`, `sourceSeqRanges`: all that selection reads)
    /// followed by the resident rows, then again with the selected rows in
    /// place, so epochs, cursors, gaps, limits and source ranges are
    /// unchanged. Transient memory is one small stub per stored row plus the
    /// selected rows; `limit: 0` explicitly selects the whole history.
    pub fn page(&self, options: &Value) -> Result<Value> {
        if self.base == 0 {
            return Ok(self.fetch_resident(options));
        }
        if let Some(page) = self.resident_page(options) {
            return Ok(page);
        }
        let store = self.store()?;
        let mut stubs = Timeline::new();
        stubs.epoch = self.epoch.clone();
        stubs.next_seq = self.next_seq;
        stubs.min_seq = self.min_seq;
        stubs.rows.reserve(self.len());
        {
            let mut statement = store.connection.prepare_cached(STORED_PAGE_METADATA)?;
            let mut rows = statement.query([self.base as i64])?;
            while let Some(row) = rows.next()? {
                stubs.rows.push(read_page_metadata(row, stubs.rows.len())?);
            }
        }
        ensure!(
            stubs.rows.len() == self.base,
            "Invalid timeline row position"
        );
        for (offset, row) in self.rows.iter().enumerate() {
            stubs
                .rows
                .push(json!({"seqStart":row["seqStart"],"seqEnd":row["seqEnd"],
                "sourceSeqRanges":row["sourceSeqRanges"],"position":self.base + offset}));
        }
        let selected: Vec<usize> = stubs.fetch_resident(options)["rows"]
            .as_array()
            .expect("page rows")
            .iter()
            .map(|stub| stub["position"].as_u64().expect("stub position") as usize)
            .collect();
        for position in selected {
            if position >= self.base {
                stubs.rows[position] = self.rows[position - self.base].clone();
                continue;
            }
            let (_, row) = store
                .find(ROW_AT, [position as i64])?
                .context("Missing timeline row")?;
            for field in ["seqStart", "seqEnd", "sourceSeqRanges"] {
                ensure!(
                    row[field] == stubs.rows[position][field],
                    "Invalid timeline row metadata"
                );
            }
            stubs.rows[position] = row;
        }
        Ok(stubs.fetch_resident(options))
    }

    /// Visits rows in position order, stored rows before the resident tail,
    /// until `visit` returns false. `kinds` (at most two item types) filters
    /// the rows; empty visits every row. Stored rows are decoded one at a time.
    pub(in crate::agent) fn scan(
        &self,
        kinds: &[&str],
        mut visit: impl FnMut(Cow<'_, Value>) -> Result<bool>,
    ) -> Result<()> {
        if self.base > 0 && !self.store()?.scan(self.base, kinds, &mut visit)? {
            return Ok(());
        }
        for row in &self.rows {
            if (kinds.is_empty() || kinds.iter().any(|kind| *kind == super::kind(&row["item"])))
                && !visit(Cow::Borrowed(row))?
            {
                break;
            }
        }
        Ok(())
    }

    /// Every row of the history.
    pub fn all_rows(&self) -> Result<Vec<Value>> {
        let mut rows = Vec::with_capacity(self.len());
        self.scan(&[], |row| {
            rows.push(row.into_owned());
            Ok(true)
        })?;
        Ok(rows)
    }

    /// Every item of the history.
    pub fn all_items(&self) -> Result<Vec<Value>> {
        let mut items = Vec::with_capacity(self.len());
        self.scan(&[], |row| {
            items.push(match row {
                Cow::Owned(mut row) => row["item"].take(),
                Cow::Borrowed(row) => row["item"].clone(),
            });
            Ok(true)
        })?;
        Ok(items)
    }

    /// The first row of item `kinds` matching `predicate`.
    pub fn find_row(
        &self,
        kinds: &[&str],
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<Option<Value>> {
        let mut found = None;
        self.scan(kinds, |row| {
            if predicate(&row) {
                found = Some(row.into_owned());
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    /// The item of the row holding the latest sequence number.
    pub fn last_item(&self) -> Result<Option<Value>> {
        let seq = self.next_seq - 1;
        if self.base > 0
            && self.evicted_seq_end >= seq
            && let Some((_, row)) = self
                .store()?
                .find(ENDING_AT, params![self.base as i64, seq])?
        {
            return Ok(Some(row["item"].clone()));
        }
        Ok(self
            .rows
            .iter()
            .find(|row| super::num(row, "seqEnd") == seq)
            .map(|row| row["item"].clone()))
    }

    /// The text of the latest assistant message row, if it has text.
    pub fn last_assistant_message(&self) -> Result<Option<String>> {
        let resident = self
            .rows
            .iter()
            .rev()
            .find(|row| row["item"]["type"] == "assistant_message");
        let row = match resident {
            Some(row) => Cow::Borrowed(row),
            None if self.base > 0 => match self
                .store()?
                .find(LAST_OF_KIND, params![self.base as i64, "assistant_message"])?
            {
                Some((_, row)) => Cow::Owned(row),
                None => return Ok(None),
            },
            None => return Ok(None),
        };
        Ok(row["item"]["text"].as_str().map(str::to_owned))
    }

    /// The first user row submitted with client message `id`.
    pub fn submitted_user_message(&self, id: &str) -> Result<Option<Value>> {
        if let Some((_, row)) = self.stored_submission(id)? {
            return Ok(Some(row));
        }
        Ok(self
            .submitted
            .get(id)
            .map(|&position| self.rows[position - self.base].clone()))
    }

    /// The first stored, non-resident submission of client message `id`.
    fn stored_submission(&self, id: &str) -> Result<Option<(usize, Value)>> {
        if self.base == 0 || id.is_empty() {
            return Ok(None);
        }
        self.store()?
            .find(&FIND_SUBMITTED, params![id, self.base as i64])
    }
}

/// Full-history hydration/replacement is one transaction. Keep the connection
/// with its owner so subsequent token commits touch only one projected row.
pub(super) fn persist(timeline: &mut Timeline, path: &Path, agent_id: &str) -> Result<()> {
    // Replacing rows from a partial resident tail would drop stored history.
    ensure!(timeline.base == 0, "Timeline rows are not all resident");
    if let Some(store) = &timeline.storage {
        store.check_target(path, agent_id)?;
    }
    if timeline.storage.is_none() {
        if !path.try_exists()? {
            let (store, sizes) = create(path, agent_id, timeline)?;
            timeline.storage = Some(store);
            timeline.sizes = sizes;
            timeline.storage_dirty = false;
            timeline.trim();
            return Ok(());
        }
        timeline.storage = Some(Store::open(path, agent_id)?);
    }
    let mut store = timeline.storage.take().unwrap();
    timeline.storage_dirty = true;
    let result = store.replace(timeline);
    timeline.storage = Some(store);
    timeline.sizes = result?;
    timeline.storage_dirty = false;
    timeline.trim();
    Ok(())
}

/// Certificate metadata updates never rewrite history. Row mutations clear the
/// certificate in their own transaction.
pub(super) fn certify(timeline: &mut Timeline, revision: Option<Value>) -> Result<()> {
    let Some(store) = timeline.storage.as_mut() else {
        return Ok(());
    };
    let transaction = store.connection.transaction()?;
    write_revision(&transaction, revision.as_ref())?;
    transaction.commit()?;
    timeline.native_revision = revision;
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

/// Commits one projected row with the sequence metadata. Serialization is
/// bounded by the affected projected row, not history size.
fn commit_row(timeline: &mut Timeline, position: usize, encoded: &str) -> Result<()> {
    let store = timeline
        .storage
        .as_mut()
        .context("Timeline storage missing")?;
    if store.temporary.is_some() {
        store
            .connection
            .prepare_cached(UPSERT_ROW)?
            .execute(params![position as i64, encoded])?;
        timeline.native_revision = None;
        return Ok(());
    }
    let transaction = store.connection.transaction()?;
    transaction
        .prepare_cached(UPSERT_ROW)?
        .execute(params![position as i64, encoded])?;
    transaction
        .prepare_cached(UPDATE_METADATA)?
        .execute(params![timeline.epoch, timeline.next_seq, timeline.min_seq])?;
    transaction.commit()?;
    timeline.native_revision = None;
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
    let row = timeline.source_row(item, options);
    project_row(timeline, row)
}

fn project_row(timeline: &mut Timeline, row: Value) -> Result<Value> {
    let id = super::identity(&row["item"]);
    // A lifecycle update of a tool or plugin row older than the resident tail
    // merges into its stored row in place; the tail is unchanged.
    let stored = match &id {
        Some(id) if timeline.base > 0 && !timeline.identities.contains_key(id) => timeline
            .store()?
            .find(&FIND_IDENTITY, params![id, timeline.base as i64])?,
        _ => None,
    };
    if let Some((position, mut stored)) = stored
        && super::merge_identity(&mut stored, &super::canonical(row.clone()))
    {
        timeline.next_seq += 1;
        let committed = serde_json::to_string(&stored)
            .map_err(anyhow::Error::from)
            .and_then(|encoded| commit_row(timeline, position, &encoded));
        if let Err(error) = committed {
            timeline.next_seq -= 1;
            return Err(error);
        }
        timeline.evicted_seq_end = timeline.evicted_seq_end.max(super::num(&stored, "seqEnd"));
        return Ok(row);
    }
    let old_next = timeline.next_seq;
    let old_min = timeline.min_seq;
    let old_len = timeline.rows.len();
    let item_kind = super::kind(&row["item"]).to_owned();
    let old_identity = id
        .as_ref()
        .and_then(|id| timeline.identities.get(id).copied());
    let candidate = old_identity
        .map(|position| position - timeline.base)
        .or_else(|| {
            if matches!(item_kind.as_str(), "assistant_message" | "reasoning") {
                old_len
                    .checked_sub(1)
                    .filter(|&index| super::kind(&timeline.rows[index]["item"]) == item_kind)
            } else {
                None
            }
        });
    let old_row = candidate.map(|index| (index, timeline.rows[index].clone()));
    let submitted_id = (row["item"]["type"] == "user_message")
        .then(|| row["item"]["clientMessageId"].as_str().map(str::to_owned))
        .flatten();
    let old_submitted = submitted_id
        .as_ref()
        .and_then(|id| timeline.submitted.get(id).copied());
    timeline.next_seq += 1;
    let position = timeline.append_row(row.clone());
    let index = position - timeline.base;
    let committed = serde_json::to_string(&timeline.rows[index])
        .map_err(anyhow::Error::from)
        .and_then(|encoded| {
            commit_row(timeline, position, &encoded).map(|()| encoded.len() as u64)
        });
    let size = match committed {
        Ok(size) => size,
        Err(error) => {
            timeline.rows.truncate(old_len);
            if let Some((index, row)) = old_row {
                timeline.rows[index] = row;
            }
            timeline.next_seq = old_next;
            timeline.min_seq = old_min;
            if let Some(id) = id {
                if let Some(position) = old_identity {
                    timeline.identities.insert(id, position);
                } else {
                    timeline.identities.remove(&id);
                }
            }
            if let Some(id) = submitted_id {
                if let Some(position) = old_submitted {
                    timeline.submitted.insert(id, position);
                } else {
                    timeline.submitted.remove(&id);
                }
            }
            return Err(error);
        }
    };
    if timeline.sizes.len() == old_len {
        if index == old_len {
            timeline.sizes.push(size);
        } else {
            timeline.sizes[index] = size;
        }
    }
    timeline.trim();
    Ok(row)
}

pub(super) fn enrich(
    timeline: &mut Timeline,
    path: &Path,
    agent_id: &str,
    message_id: &str,
    provider_id: &str,
) -> Result<Option<Value>> {
    if timeline.base > 0 {
        timeline.store()?.check_target(path, agent_id)?;
    }
    if let Some((position, mut row)) = timeline.stored_submission(message_id)? {
        row["providerMessageId"] = Value::String(provider_id.to_owned());
        commit_row(timeline, position, &serde_json::to_string(&row)?)?;
        return Ok(Some(row));
    }
    let Some(&position) = timeline.submitted.get(message_id) else {
        return Ok(None);
    };
    ensure_storage(timeline, path, agent_id)?;
    let index = position - timeline.base;
    let previous = timeline.rows[index].get("providerMessageId").cloned();
    timeline.rows[index]["providerMessageId"] = Value::String(provider_id.to_owned());
    let committed = serde_json::to_string(&timeline.rows[index])
        .map_err(anyhow::Error::from)
        .and_then(|encoded| {
            commit_row(timeline, position, &encoded).map(|()| encoded.len() as u64)
        });
    match committed {
        Ok(size) => {
            if timeline.sizes.len() == timeline.rows.len() {
                timeline.sizes[index] = size;
            }
        }
        Err(error) => {
            if let Some(previous) = previous {
                timeline.rows[index]["providerMessageId"] = previous;
            } else {
                timeline.rows[index]
                    .as_object_mut()
                    .unwrap()
                    .remove("providerMessageId");
            }
            return Err(error);
        }
    }
    Ok(Some(timeline.rows[index].clone()))
}
