#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, SystemTime},
};

pub(super) fn session_roots(
    env: &std::collections::BTreeMap<String, String>,
    cwd: &Path,
    resolved: PathBuf,
) -> Vec<PathBuf> {
    let mut roots = vec![resolved];
    if let Some(home) = env.get("HOME").map(PathBuf::from).or_else(dirs::home_dir) {
        roots.push(home.join(".omp/agent/sessions"));
        if let Some(configured) = env
            .get("PI_CODING_AGENT_DIR")
            .filter(|value| !value.trim().is_empty())
        {
            let agent = if configured == "~" {
                home
            } else if let Some(relative) = configured.strip_prefix("~/") {
                home.join(relative)
            } else {
                cwd.join(configured)
            };
            roots.push(agent.join("sessions"));
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Aliases are accepted only when they identify exactly one current catalog tool.
#[derive(Default)]
pub(super) struct Catalog {
    identities: HashMap<String, String>,
    aliases: HashMap<String, Option<String>>,
}

impl Catalog {
    pub fn insert(&mut self, name: &str, server_tool: Option<(&str, &str)>) {
        let identity = match server_tool {
            Some((server, tool)) => serde_json::json!(["mcp", server, tool]).to_string(),
            None => serde_json::json!(["native", name]).to_string(),
        };
        self.identities.insert(name.to_owned(), identity.clone());
        self.alias(name, &identity);
        if let Some((server, tool)) = server_tool {
            self.alias(&format!("mcp__{server}__{tool}"), &identity);
            self.alias(&format!("mcp__{server}_{tool}"), &identity);
            self.alias(&format!("{server} / {tool}"), &identity);
            self.alias(&format!("{server}.{tool}"), &identity);
        } else {
            // These are the daemon's existing internal MCP namespace aliases.
            self.alias(&format!("mcp__paseo__{name}"), &identity);
            self.alias(&format!("mcp__paseo_{name}"), &identity);
            self.alias(&format!("paseo.{name}"), &identity);
        }
    }

    fn alias(&mut self, name: &str, identity: &str) {
        let alias = self
            .aliases
            .entry(normalize(name).to_owned())
            .or_insert_with(|| Some(identity.to_owned()));
        if alias.as_deref() != Some(identity) {
            *alias = None;
        }
    }

    pub fn identity(&self, name: &str) -> Option<&str> {
        self.identities.get(name).map(String::as_str)
    }

    pub fn observed<'a>(&'a self, history: &'a History) -> HashSet<&'a str> {
        if !history.complete {
            // Incomplete coverage cannot prove that a tool was never used.
            return self.identities.values().map(String::as_str).collect();
        }
        history
            .identities
            .iter()
            .map(String::as_str)
            .chain(
                history
                    .names
                    .iter()
                    .filter_map(|name| self.aliases.get(name).and_then(Option::as_deref)),
            )
            .collect()
    }

    #[cfg(test)]
    fn essential(&self, name: &str, history: &History) -> bool {
        self.identity(name)
            .is_some_and(|identity| self.observed(history).contains(identity))
    }
}

fn normalize(name: &str) -> &str {
    name.strip_prefix("functions.").unwrap_or(name)
}

type SharedIndex = Arc<Mutex<Option<Index>>>;

pub(super) struct History {
    path: PathBuf,
    names: Arc<HashSet<String>>,
    identities: Arc<HashSet<String>>,
    index: SharedIndex,
    complete: bool,
}

impl History {
    pub async fn load(home: &Path, mut roots: Vec<PathBuf>) -> Self {
        static INDEXES: LazyLock<Mutex<HashMap<PathBuf, SharedIndex>>> =
            LazyLock::new(Mutex::default);
        let path = home.join("omp-host-tool-history.sqlite3");
        let timelines = home.join("agent-timelines");
        roots.sort();
        roots.dedup();
        let index = INDEXES
            .lock()
            .unwrap()
            .entry(path.clone())
            .or_default()
            .clone();
        let shared = index.clone();
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut slot = shared.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|index| index.replaced(&target).unwrap_or(true))
            {
                *slot = None;
            }
            if slot.is_none() {
                *slot = Some(Index::new(&target)?);
            }
            let index = slot.as_mut().unwrap();
            match index.update(&timelines, &roots) {
                Ok(complete) => Ok((index.names.clone(), index.identities.clone(), complete)),
                Err(error) => {
                    *slot = None;
                    Err(error)
                }
            }
        })
        .await;
        let (names, identities, complete) = match result {
            Ok(Ok(snapshot)) => snapshot,
            error => {
                tracing::warn!(
                    ?error,
                    "OMP host tool history unavailable; retaining essential catalog until coverage recovers"
                );
                (Arc::default(), Arc::default(), false)
            }
        };
        Self {
            path,
            names,
            identities,
            index,
            complete,
        }
    }

    pub async fn record(&self, identity: &str) -> Result<()> {
        let path = self.path.clone();
        let shared = self.index.clone();
        let owned = identity.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut slot = shared.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|index| index.replaced(&path).unwrap_or(true))
            {
                *slot = None;
            }
            if slot.is_none() {
                *slot = Some(Index::new(&path)?);
            }
            let index = slot.as_mut().unwrap();
            if !index.identities.contains(&owned) {
                let transaction = index
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute(
                    "INSERT OR IGNORE INTO identities(name) VALUES (?1)",
                    [&owned],
                )?;
                transaction.commit()?;
                index.refresh()?;
            }
            Ok(())
        })
        .await??;
        Ok(())
    }
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS names(name TEXT PRIMARY KEY NOT NULL);
         CREATE TABLE IF NOT EXISTS identities(name TEXT PRIMARY KEY NOT NULL);
         CREATE TABLE IF NOT EXISTS sources(path TEXT PRIMARY KEY NOT NULL, cursor TEXT NOT NULL);",
    )?;
    Ok(connection)
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    created: Option<SystemTime>,
    length: u64,
    #[cfg(unix)]
    identity: (u64, u64),
}

fn stamp(path: &Path) -> Result<Option<Stamp>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(Stamp {
        modified: metadata.modified()?,
        length: metadata.len(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        identity: (metadata.dev(), metadata.ino()),
    }))
}

struct Directory {
    stamp: Option<Stamp>,
    files: Vec<PathBuf>,
    children: Vec<PathBuf>,
}

#[derive(PartialEq, Eq)]
struct SourceStamp {
    file: Stamp,
    wal: Option<Stamp>,
}

struct Source {
    stamp: SourceStamp,
    complete: bool,
}

/// One index per daemon home serializes launches and durable calls, while each
/// runtime keeps immutable load-mode snapshots. Only metadata is polled: source
/// bodies and directory listings are revisited when their stamps change.
struct Index {
    connection: Connection,
    names: Arc<HashSet<String>>,
    identities: Arc<HashSet<String>>,
    name_row: i64,
    identity_row: i64,
    database: Stamp,
    data_version: Option<i64>,
    directories: HashMap<PathBuf, Arc<Directory>>,
    sources: HashMap<PathBuf, Source>,
    #[cfg(test)]
    source_scans: usize,
    #[cfg(test)]
    refreshes: usize,
}

impl Index {
    fn new(path: &Path) -> Result<Self> {
        let mut index = Self {
            connection: open(path)?,
            database: stamp(path)?.context("OMP host tool history database disappeared")?,
            names: Arc::default(),
            identities: Arc::default(),
            name_row: 0,
            identity_row: 0,
            data_version: None,
            directories: HashMap::new(),
            sources: HashMap::new(),
            #[cfg(test)]
            source_scans: 0,
            #[cfg(test)]
            refreshes: 0,
        };
        index.refresh()?;
        Ok(index)
    }

    fn replaced(&self, path: &Path) -> Result<bool> {
        let Some(current) = stamp(path)? else {
            return Ok(true);
        };
        #[cfg(unix)]
        return Ok(current.identity != self.database.identity);
        #[cfg(not(unix))]
        Ok(current.created != self.database.created)
    }

    fn refresh(&mut self) -> Result<()> {
        // Sample before the read transaction: a concurrent commit after the
        // snapshot must still invalidate the next registration.
        self.data_version = None;
        let version = self
            .connection
            .pragma_query_value(None, "data_version", |row| row.get(0))?;
        let transaction = self.connection.transaction()?;
        let (names, name_row) = additions(
            &transaction,
            "SELECT rowid,name FROM names WHERE rowid > ?1 ORDER BY rowid",
            self.name_row,
        )?;
        let (identities, identity_row) = additions(
            &transaction,
            "SELECT rowid,name FROM identities WHERE rowid > ?1 ORDER BY rowid",
            self.identity_row,
        )?;
        transaction.commit()?;
        if !names.is_empty() {
            Arc::make_mut(&mut self.names).extend(names);
        }
        if !identities.is_empty() {
            Arc::make_mut(&mut self.identities).extend(identities);
        }
        self.name_row = name_row;
        self.identity_row = identity_row;
        self.data_version = Some(version);
        #[cfg(test)]
        {
            self.refreshes += 1;
        }
        Ok(())
    }

    fn update(&mut self, timelines: &Path, roots: &[PathBuf]) -> Result<bool> {
        let mut changed = Vec::new();
        let mut complete = true;
        self.discover(timelines, false, &mut changed, &mut complete);
        for root in roots {
            self.discover(root, true, &mut changed, &mut complete);
        }
        changed.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        changed.dedup_by(|left, right| left.0 == right.0);
        if !changed.is_empty() {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut scanned = Vec::with_capacity(changed.len());
            for (file, stamp) in changed {
                let result = match file.extension().and_then(|extension| extension.to_str()) {
                    Some("sqlite3") => scan_timeline(&transaction, &file),
                    Some("json") => scan_legacy_timeline(&transaction, &file),
                    _ => scan_session(&transaction, &file),
                };
                let succeeded = result.is_ok();
                if let Err(error) = result {
                    complete = false;
                    tracing::warn!(path = %file.display(), %error, "OMP host tool history is partial; source will be retried");
                }
                scanned.push((
                    file,
                    Source {
                        stamp,
                        complete: succeeded,
                    },
                ));
            }
            transaction.commit()?;
            #[cfg(test)]
            {
                self.source_scans += scanned.len();
            }
            self.sources.extend(scanned);
            self.refresh()?;
        } else {
            let version: i64 = self
                .connection
                .pragma_query_value(None, "data_version", |row| row.get(0))?;
            if Some(version) != self.data_version {
                self.refresh()?;
            }
        }
        if !complete {
            tracing::warn!(
                "OMP host tool history coverage is partial; retaining essential catalog until coverage recovers"
            );
        }
        Ok(complete)
    }

    fn discover(
        &mut self,
        root: &Path,
        recursive: bool,
        changed: &mut Vec<(PathBuf, SourceStamp)>,
        complete: &mut bool,
    ) {
        let result = self.directory(root);
        let directory = match result {
            Ok(directory) => directory,
            Err(error) => {
                *complete = false;
                tracing::warn!(path = %root.display(), %error, "OMP host tool history directory unreadable");
                return;
            }
        };
        for path in &directory.files {
            let extension = path.extension().and_then(|value| value.to_str());
            let included = if recursive {
                extension == Some("jsonl")
            } else {
                matches!(extension, Some("sqlite3" | "json"))
            };
            if !included {
                continue;
            }
            let result = (|| -> Result<Option<SourceStamp>> {
                let Some(file) = stamp(path)? else {
                    return Ok(None);
                };
                let wal = if extension == Some("sqlite3") {
                    stamp(&path.with_extension("sqlite3-wal"))?
                } else {
                    None
                };
                Ok(Some(SourceStamp { file, wal }))
            })();
            match result {
                Ok(Some(stamp)) => {
                    let unchanged = self
                        .sources
                        .get(path)
                        .is_some_and(|source| source.complete && source.stamp == stamp);
                    if !unchanged {
                        changed.push((path.clone(), stamp));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    *complete = false;
                    tracing::warn!(path = %path.display(), %error, "OMP host tool history source unreadable");
                }
            }
        }
        if recursive {
            for child in &directory.children {
                self.discover(child, true, changed, complete);
            }
        }
    }

    fn directory(&mut self, path: &Path) -> Result<Arc<Directory>> {
        let stamp = stamp(path)?;
        if let Some(directory) = self.directories.get(path)
            && directory.stamp == stamp
        {
            return Ok(directory.clone());
        }
        let mut directory = Directory {
            stamp,
            files: Vec::new(),
            children: Vec::new(),
        };
        if directory.stamp.is_some() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    directory.children.push(entry.path());
                } else if kind.is_file() {
                    directory.files.push(entry.path());
                }
            }
        }
        let directory = Arc::new(directory);
        self.directories.insert(path.to_owned(), directory.clone());
        Ok(directory)
    }
}

fn additions(transaction: &Transaction<'_>, sql: &str, after: i64) -> Result<(Vec<String>, i64)> {
    let mut statement = transaction.prepare(sql)?;
    let mut rows = statement.query([after])?;
    let mut last = after;
    let mut names = Vec::new();
    while let Some(row) = rows.next()? {
        last = row.get(0)?;
        names.push(row.get(1)?);
    }
    Ok((names, last))
}

fn cursor(transaction: &Transaction<'_>, path: &Path) -> Result<Option<String>> {
    Ok(transaction
        .query_row(
            "SELECT cursor FROM sources WHERE path = ?1",
            [path.to_string_lossy().as_ref()],
            |row| row.get(0),
        )
        .optional()?)
}

fn save_cursor(transaction: &Transaction<'_>, path: &Path, cursor: &str) -> Result<()> {
    transaction.execute("INSERT INTO sources(path,cursor) VALUES (?1,?2) ON CONFLICT(path) DO UPDATE SET cursor=excluded.cursor", params![path.to_string_lossy(), cursor])?;
    Ok(())
}

fn save_names(transaction: &Transaction<'_>, names: HashSet<String>) -> Result<()> {
    let mut insert = transaction.prepare_cached("INSERT OR IGNORE INTO names(name) VALUES (?1)")?;
    for name in names {
        insert.execute([name])?;
    }
    Ok(())
}

fn scan_timeline(transaction: &Transaction<'_>, path: &Path) -> Result<()> {
    let mut source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::from_secs(2))?;
    let snapshot = source.transaction()?;
    let (epoch, next): (String, i64) = snapshot.query_row(
        "SELECT epoch,next_seq FROM timeline_metadata WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let previous: Option<(String, i64)> = cursor(transaction, path)?
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    let since = match previous {
        Some((old_epoch, old_next)) if old_epoch == epoch && old_next <= next => old_next,
        _ => 0,
    };
    if since == next {
        return Ok(());
    }
    let mut statement = snapshot.prepare("SELECT row_json FROM timeline_rows WHERE COALESCE(json_extract(row_json,'$.seqEnd'),json_extract(row_json,'$.seq'),0) >= ?1 ORDER BY position")?;
    let mut rows = statement.query([since])?;
    let mut names = HashSet::new();
    let mut skipped_legacy_rows = 0;
    while let Some(row) = rows.next()? {
        let encoded: String = row.get(0)?;
        let row: Value = serde_json::from_str(&encoded)?;
        if timeline_names(&row["item"], &mut names) {
            skipped_legacy_rows += 1;
        }
    }
    if skipped_legacy_rows > 0 {
        tracing::warn!(path = %path.display(), skipped_legacy_rows,
            "OMP legacy timeline rows lack invocation provenance; skipped rows are not evidence of unused tools");
    }
    save_names(transaction, names)?;
    save_cursor(transaction, path, &serde_json::to_string(&(epoch, next))?)
}

fn scan_legacy_timeline(transaction: &Transaction<'_>, path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let stamp = serde_json::to_string(&(metadata.len(), modified))?;
    if cursor(transaction, path)?.as_deref() == Some(stamp.as_str()) {
        return Ok(());
    }
    let snapshot: Value = serde_json::from_reader(BufReader::new(file))?;
    anyhow::ensure!(
        snapshot["version"] == 1,
        "Unsupported legacy timeline version"
    );
    let rows = snapshot["rows"]
        .as_array()
        .context("Legacy timeline has no rows")?;
    let mut names = HashSet::new();
    let mut skipped_legacy_rows = 0;
    for row in rows {
        if timeline_names(&row["item"], &mut names) {
            skipped_legacy_rows += 1;
        }
    }
    if skipped_legacy_rows > 0 {
        tracing::warn!(path = %path.display(), skipped_legacy_rows,
            "OMP legacy timeline rows lack invocation provenance; skipped rows are not evidence of unused tools");
    }
    save_names(transaction, names)?;
    save_cursor(transaction, path, &stamp)
}

#[derive(Serialize, Deserialize)]
struct SessionCursor {
    offset: u64,
    prefix: String,
    boundary: String,
    modified: Option<u128>,
}

fn fingerprint(file: &mut File, start: u64, length: u64) -> Result<String> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = [0; 512];
    let bytes = &mut bytes[..length as usize];
    file.read_exact(bytes)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn scan_session(transaction: &Transaction<'_>, path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let length = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_nanos());
    let previous: Option<SessionCursor> = cursor(transaction, path)?
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    let mut offset = 0;
    if let Some(previous) = previous.filter(|previous| previous.offset <= length) {
        let sample = previous.offset.min(512);
        let prefix = fingerprint(&mut file, 0, sample)?;
        let boundary = fingerprint(&mut file, previous.offset - sample, sample)?;
        let unchanged = length != previous.offset || modified == previous.modified;
        if unchanged && prefix == previous.prefix && boundary == previous.boundary {
            offset = previous.offset;
            if offset == length {
                return Ok(());
            }
        }
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file.take(length - offset));
    let mut line = String::new();
    let mut names = HashSet::new();
    let mut partial = false;
    let mut problem = None;
    loop {
        line.clear();
        let size = match reader.read_line(&mut line) {
            Ok(size) => size,
            Err(error) => {
                problem = Some(anyhow::Error::from(error));
                break;
            }
        };
        if size == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // A writer may still be completing this record; never checkpoint it.
            partial = true;
            break;
        }
        match serde_json::from_str::<Value>(&line).context("Invalid OMP session record") {
            Ok(entry) => session_names(&entry, &mut names),
            Err(error) => {
                problem = Some(error);
            }
        }
        offset += size as u64;
    }
    save_names(transaction, names)?;
    if let Some(error) = problem {
        return Err(error);
    }
    let mut file = reader.into_inner().into_inner();
    let sample = offset.min(512);
    let cursor = SessionCursor {
        offset,
        prefix: fingerprint(&mut file, 0, sample)?,
        boundary: fingerprint(&mut file, offset - sample, sample)?,
        modified,
    };
    save_cursor(transaction, path, &serde_json::to_string(&cursor)?)?;
    anyhow::ensure!(!partial, "OMP session has an incomplete trailing record");
    Ok(())
}

/// Returns whether this row lacks the provenance needed to classify invocation.
fn timeline_names(item: &Value, names: &mut HashSet<String>) -> bool {
    if item["type"] != "tool_call" || !item["callId"].is_string() {
        return false;
    }
    // Older OMP timelines renamed schema reads to the queried tool and discarded
    // the original operation. Only the native JSONL can disambiguate those rows.
    if item["detail"]["type"] == "plain_text" {
        return true;
    }
    let Some(name) = item["name"].as_str() else {
        return false;
    };
    let input = &item["detail"]["input"];
    if item["detail"]["type"] == "read"
        && item["detail"]["filePath"]
            .as_str()
            .is_some_and(|path| path.starts_with("xd://"))
    {
        names.insert("read".into());
        return false;
    }
    if item["detail"]["type"] == "write" {
        invocation(
            name,
            &serde_json::json!({"path":item["detail"]["filePath"]}),
            names,
        );
    } else {
        invocation(name, input, names);
    }
    false
}

fn session_names(entry: &Value, names: &mut HashSet<String>) {
    if entry["type"] != "message" || entry["message"]["role"] != "assistant" {
        return;
    }
    if let Some(blocks) = entry["message"]["content"].as_array() {
        for block in blocks {
            if block["type"] == "toolCall"
                && block["id"].is_string()
                && let Some(name) = block["name"].as_str()
            {
                invocation(name, &block["arguments"], names);
            }
        }
    }
}

fn invocation(name: &str, args: &Value, names: &mut HashSet<String>) {
    let name = normalize(name);
    names.insert(name.to_owned());
    if name == "write"
        && let Some(target) = args["path"]
            .as_str()
            .and_then(|path| path.strip_prefix("xd://"))
    {
        // Only exact executable devices, not schema reads or subresources.
        if !target.is_empty() && !target.contains(['/', '?', '#', ':']) {
            names.insert(normalize(target).to_owned());
        }
    }
    if name == "multi_tool_use.parallel"
        && let Some(calls) = args["tool_uses"].as_array()
    {
        for call in calls {
            if let Some(name) = call["recipient_name"].as_str() {
                invocation(name, &call["parameters"], names);
            }
        }
    }
}
