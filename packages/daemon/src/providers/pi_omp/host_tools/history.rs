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
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, SystemTime},
};
use tokio::sync::watch;

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

/// What one OMP launch registers with: the tool names and identities seen so far, and
/// whether the scan that found them covered every source.
#[derive(Clone, Default)]
struct Snapshot {
    names: Arc<HashSet<String>>,
    identities: Arc<HashSet<String>>,
    complete: bool,
}

/// The sources the background scanner covers and whether it is running. `requested`
/// counts scan requests; the scanner reports the last one it served in `Home::scanned`.
#[derive(Default)]
struct Scan {
    roots: BTreeSet<PathBuf>,
    requested: u64,
    running: bool,
}

/// One per daemon home. A launch reads the latest snapshot without waiting; one
/// background scanner brings it up to date for the next launch. Tool calls record
/// through their own connection, so neither waits for a scan, however long the
/// session history is.
struct Home {
    path: PathBuf,
    timelines: PathBuf,
    /// Held by the scanner for the whole scan.
    index: Mutex<Option<Index>>,
    /// The latest snapshot: read from the database on first use, then published by
    /// each scan and extended by recorded calls.
    snapshot: Mutex<Option<Snapshot>>,
    scan: Mutex<Scan>,
    scanned: watch::Sender<u64>,
}

/// Every home this process launched OMP sessions for, by history database.
static HOMES: LazyLock<Mutex<HashMap<PathBuf, Arc<Home>>>> = LazyLock::new(Mutex::default);

impl Home {
    fn of(home: &Path) -> Arc<Self> {
        let path = home.join("omp-host-tool-history.sqlite3");
        HOMES
            .lock()
            .unwrap()
            .entry(path.clone())
            .or_insert_with(|| {
                Arc::new(Self {
                    path,
                    timelines: home.join("agent-timelines"),
                    index: Mutex::default(),
                    snapshot: Mutex::default(),
                    scan: Mutex::default(),
                    scanned: watch::Sender::new(0),
                })
            })
            .clone()
    }

    fn current(&self) -> Snapshot {
        let mut snapshot = self.snapshot.lock().unwrap();
        snapshot
            .get_or_insert_with(|| {
                stored(&self.path).unwrap_or_else(|error| {
                    tracing::warn!(%error, "OMP host tool history unreadable; retaining essential catalog until a scan recovers it");
                    Snapshot::default()
                })
            })
            .clone()
    }

    /// Adds `roots` to the scanned sources and starts the scanner unless it runs;
    /// returns the request the scanner reports once it covered it.
    fn request(self: &Arc<Self>, roots: Vec<PathBuf>) -> u64 {
        let mut scan = self.scan.lock().unwrap();
        scan.roots.extend(roots);
        scan.requested += 1;
        if !scan.running {
            scan.running = true;
            let home = self.clone();
            tokio::task::spawn_blocking(move || home.run());
        }
        scan.requested
    }

    fn run(&self) {
        struct Running<'a>(&'a Mutex<Scan>);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    self.0.lock().unwrap().running = false;
                }
            }
        }
        let _running = Running(&self.scan);
        loop {
            let (roots, served) = {
                let scan = self.scan.lock().unwrap();
                (
                    scan.roots.iter().cloned().collect::<Vec<_>>(),
                    scan.requested,
                )
            };
            self.scan_once(&roots);
            self.scanned.send_replace(served);
            let mut scan = self.scan.lock().unwrap();
            if scan.requested == served {
                scan.running = false;
                return;
            }
        }
    }

    fn scan_once(&self, roots: &[PathBuf]) {
        let mut slot = self.index.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|index| index.replaced(&self.path).unwrap_or(true))
        {
            *slot = None;
        }
        let result = (|| -> Result<()> {
            if slot.is_none() {
                *slot = Some(Index::new(&self.path)?);
            }
            let index = slot.as_mut().unwrap();
            let (complete, scanned) = index.update(&self.timelines, roots)?;
            index.save_coverage(complete)?;
            // Under the snapshot lock, so a call recorded meanwhile is either read
            // here or added to the snapshot published here.
            let mut snapshot = self.snapshot.lock().unwrap();
            index.sync(scanned)?;
            *snapshot = Some(Snapshot {
                names: index.names.clone(),
                identities: index.identities.clone(),
                complete,
            });
            Ok(())
        })();
        if let Err(error) = result {
            *slot = None;
            tracing::warn!(
                ?error,
                "OMP host tool history unavailable; retaining essential catalog until coverage recovers"
            );
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.get_or_insert_with(Snapshot::default).complete = false;
        }
    }

    fn record(&self, identity: &str) -> Result<()> {
        let mut snapshot = self.snapshot.lock().unwrap();
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.identities.contains(identity))
        {
            return Ok(());
        }
        open(&self.path)?.execute(
            "INSERT OR IGNORE INTO identities(name) VALUES (?1)",
            [identity],
        )?;
        if let Some(snapshot) = snapshot.as_mut() {
            Arc::make_mut(&mut snapshot.identities).insert(identity.to_owned());
        }
        Ok(())
    }
}

/// The history a previous scan left in the database. A database without a recorded
/// complete scan proves nothing about unused tools.
fn stored(path: &Path) -> Result<Snapshot> {
    if stamp(path)?.is_none() {
        return Ok(Snapshot::default());
    }
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    let transaction = connection.transaction()?;
    let (names, _) = additions(
        &transaction,
        "SELECT rowid,name FROM names WHERE rowid > ?1 ORDER BY rowid",
        0,
    )?;
    let (identities, _) = additions(
        &transaction,
        "SELECT rowid,name FROM identities WHERE rowid > ?1 ORDER BY rowid",
        0,
    )?;
    let recorded: i64 = transaction.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='coverage'",
        [],
        |row| row.get(0),
    )?;
    let complete = recorded > 0
        && transaction
            .query_row("SELECT complete FROM coverage WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?
            .unwrap_or(false);
    Ok(Snapshot {
        names: Arc::new(names.into_iter().collect()),
        identities: Arc::new(identities.into_iter().collect()),
        complete,
    })
}

pub(super) struct History {
    names: Arc<HashSet<String>>,
    identities: Arc<HashSet<String>>,
    complete: bool,
    home: Arc<Home>,
}

impl History {
    /// The history as of the last finished scan, without waiting for one. A scan of
    /// what changed since then runs in the background for the next launch.
    pub async fn load(home: &Path, roots: Vec<PathBuf>) -> Self {
        let home = Home::of(home);
        let snapshot = Self::current(&home).await;
        home.request(roots);
        Self::from(home, snapshot)
    }

    /// The history once a scan covering `roots` finished.
    #[cfg(test)]
    pub async fn settled(home: &Path, roots: Vec<PathBuf>) -> Self {
        let home = Home::of(home);
        let request = home.request(roots);
        home.scanned
            .subscribe()
            .wait_for(|served| *served >= request)
            .await
            .expect("history scanner stopped");
        let snapshot = Self::current(&home).await;
        Self::from(home, snapshot)
    }

    async fn current(home: &Arc<Home>) -> Snapshot {
        let home = home.clone();
        tokio::task::spawn_blocking(move || home.current())
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "OMP host tool history read failed; retaining essential catalog");
                Snapshot::default()
            })
    }

    fn from(home: Arc<Home>, snapshot: Snapshot) -> Self {
        Self {
            names: snapshot.names,
            identities: snapshot.identities,
            complete: snapshot.complete,
            home,
        }
    }

    pub async fn record(&self, identity: &str) -> Result<()> {
        let home = self.home.clone();
        let owned = identity.to_owned();
        tokio::task::spawn_blocking(move || home.record(&owned)).await?
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
         CREATE TABLE IF NOT EXISTS sources(path TEXT PRIMARY KEY NOT NULL, cursor TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS coverage(id INTEGER PRIMARY KEY CHECK (id = 1), complete INTEGER NOT NULL);",
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

/// The scanner's view of one home's history database. Only metadata is polled:
/// source bodies and directory listings are revisited when their stamps change.
struct Index {
    connection: Connection,
    names: Arc<HashSet<String>>,
    identities: Arc<HashSet<String>>,
    name_row: i64,
    identity_row: i64,
    database: Stamp,
    data_version: Option<i64>,
    /// Whether the last scan covered every source, as stored.
    coverage: Option<bool>,
    directories: HashMap<PathBuf, Arc<Directory>>,
    sources: HashMap<PathBuf, Source>,
    #[cfg(test)]
    source_scans: usize,
    #[cfg(test)]
    refreshes: usize,
}

impl Index {
    fn new(path: &Path) -> Result<Self> {
        let connection = open(path)?;
        let coverage = connection
            .query_row("SELECT complete FROM coverage WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let mut index = Self {
            connection,
            database: stamp(path)?.context("OMP host tool history database disappeared")?,
            names: Arc::default(),
            identities: Arc::default(),
            name_row: 0,
            identity_row: 0,
            data_version: None,
            coverage,
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

    /// Reads every changed source, each in its own transaction so calls recorded
    /// meanwhile wait for one source at most and a stopped scan keeps its progress.
    /// Returns whether every source was covered and whether any was read.
    fn update(&mut self, timelines: &Path, roots: &[PathBuf]) -> Result<(bool, bool)> {
        let mut changed = Vec::new();
        let mut complete = true;
        self.discover(timelines, false, &mut changed, &mut complete);
        for root in roots {
            self.discover(root, true, &mut changed, &mut complete);
        }
        changed.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        changed.dedup_by(|left, right| left.0 == right.0);
        let scanned = !changed.is_empty();
        for (file, stamp) in changed {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = match file.extension().and_then(|extension| extension.to_str()) {
                Some("sqlite3") => scan_timeline(&transaction, &file),
                _ => scan_session(&transaction, &file),
            };
            // Calls read before a failure stay counted.
            transaction.commit()?;
            let succeeded = result.is_ok();
            if let Err(error) = result {
                complete = false;
                tracing::warn!(path = %file.display(), %error, "OMP host tool history is partial; source will be retried");
            }
            #[cfg(test)]
            {
                self.source_scans += 1;
            }
            self.sources.insert(
                file,
                Source {
                    stamp,
                    complete: succeeded,
                },
            );
        }
        if !complete {
            tracing::warn!(
                "OMP host tool history coverage is partial; retaining essential catalog until coverage recovers"
            );
        }
        Ok((complete, scanned))
    }

    /// Reads rows this scan wrote (`scanned`) or another connection committed.
    fn sync(&mut self, scanned: bool) -> Result<()> {
        if !scanned {
            let version: i64 = self
                .connection
                .pragma_query_value(None, "data_version", |row| row.get(0))?;
            if Some(version) == self.data_version {
                return Ok(());
            }
        }
        self.refresh()
    }

    /// Stores whether the last scan covered every source, for the first launch
    /// of the next daemon process.
    fn save_coverage(&mut self, complete: bool) -> Result<()> {
        if self.coverage != Some(complete) {
            self.connection.execute(
                "INSERT INTO coverage(id,complete) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET complete=excluded.complete",
                [complete],
            )?;
            self.coverage = Some(complete);
        }
        Ok(())
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
                extension == Some("sqlite3")
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
    let mut statement = snapshot.prepare("SELECT row_json FROM timeline_rows WHERE json_extract(row_json,'$.seqEnd') >= ?1 ORDER BY position")?;
    let mut rows = statement.query([since])?;
    let mut names = HashSet::new();
    while let Some(row) = rows.next()? {
        let encoded: String = row.get(0)?;
        // Only tool call rows name tools; most rows are messages and outputs.
        if !encoded.contains(r#""tool_call""#) {
            continue;
        }
        let row: Value = serde_json::from_str(&encoded)?;
        timeline_names(&row["item"], &mut names);
    }
    save_names(transaction, names)?;
    save_cursor(transaction, path, &serde_json::to_string(&(epoch, next))?)
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
        // Only a record with a tool call block can name a tool, and most of the
        // history is messages and tool output: parse only the records that could.
        if line.contains(r#""toolCall""#) {
            match serde_json::from_str::<Value>(&line).context("Invalid OMP session record") {
                Ok(entry) => session_names(&entry, &mut names),
                Err(error) => {
                    problem = Some(error);
                }
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

fn timeline_names(item: &Value, names: &mut HashSet<String>) {
    if item["type"] != "tool_call" || !item["callId"].is_string() {
        return;
    }
    let Some(name) = item["name"].as_str() else {
        return;
    };
    let input = &item["detail"]["input"];
    if item["detail"]["type"] == "read"
        && item["detail"]["filePath"]
            .as_str()
            .is_some_and(|path| path.starts_with("xd://"))
    {
        names.insert("read".into());
        return;
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
