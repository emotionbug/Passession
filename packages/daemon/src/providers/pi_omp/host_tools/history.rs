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
    sync::Mutex,
    time::Duration,
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

pub(super) struct History {
    path: PathBuf,
    names: HashSet<String>,
    identities: HashSet<String>,
    recorded: Mutex<HashSet<String>>,
    complete: bool,
}

impl History {
    pub async fn load(home: &Path, roots: Vec<PathBuf>) -> Self {
        let path = home.join("omp-host-tool-history.sqlite3");
        let timelines = home.join("agent-timelines");
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || load(&target, &timelines, &roots)).await;
        let (names, identities, complete) = match result {
            Ok(Ok(snapshot)) => snapshot,
            error => {
                tracing::warn!(
                    ?error,
                    "OMP host tool history unavailable; retaining essential catalog until coverage recovers"
                );
                (HashSet::new(), HashSet::new(), false)
            }
        };
        Self {
            path,
            names,
            recorded: Mutex::new(identities.clone()),
            identities,
            complete,
        }
    }

    pub async fn record(&self, identity: &str) -> Result<()> {
        if self.recorded.lock().unwrap().contains(identity) {
            return Ok(());
        }
        let path = self.path.clone();
        let owned = identity.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let connection = open(&path)?;
            connection.execute(
                "INSERT OR IGNORE INTO identities(name) VALUES (?1)",
                [&owned],
            )?;
            Ok(())
        })
        .await??;
        self.recorded.lock().unwrap().insert(identity.to_owned());
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

type Snapshot = (HashSet<String>, HashSet<String>, bool);

fn load(path: &Path, timelines: &Path, roots: &[PathBuf]) -> Result<Snapshot> {
    let mut connection = open(path)?;
    // One transaction serializes concurrent launches and commits the union with
    // its source cursors. Readers of source databases use independent snapshots.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut complete = true;
    let mut files = Vec::new();
    discover(timelines, "sqlite3", false, &mut files, &mut complete);
    discover(timelines, "json", false, &mut files, &mut complete);
    for root in roots {
        discover(root, "jsonl", true, &mut files, &mut complete);
    }
    files.sort();
    files.dedup();
    for file in files {
        let result = match file.extension().and_then(|extension| extension.to_str()) {
            Some("sqlite3") => scan_timeline(&transaction, &file),
            Some("json") => scan_legacy_timeline(&transaction, &file),
            _ => scan_session(&transaction, &file),
        };
        if let Err(error) = result {
            complete = false;
            tracing::warn!(path = %file.display(), %error, "OMP host tool history is partial; source will be retried");
        }
    }
    let names = read_set(&transaction, "SELECT name FROM names")?;
    let identities = read_set(&transaction, "SELECT name FROM identities")?;
    transaction.commit()?;
    if !complete {
        tracing::warn!(
            "OMP host tool history coverage is partial; retaining essential catalog until coverage recovers"
        );
    }
    Ok((names, identities, complete))
}

fn discover(
    root: &Path,
    extension: &str,
    recursive: bool,
    files: &mut Vec<PathBuf>,
    complete: &mut bool,
) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            *complete = false;
            tracing::warn!(path = %root.display(), %error, "OMP host tool history directory unreadable");
            return;
        }
    };
    for entry in entries {
        let result = entry.and_then(|entry| Ok((entry.file_type()?, entry.path())));
        match result {
            Ok((kind, path)) if recursive && kind.is_dir() => {
                discover(&path, extension, true, files, complete)
            }
            Ok((kind, path))
                if kind.is_file() && path.extension().is_some_and(|ext| ext == extension) =>
            {
                files.push(path)
            }
            Ok(_) => {}
            Err(error) => {
                *complete = false;
                tracing::warn!(path = %root.display(), %error, "OMP host tool history entry unreadable");
            }
        }
    }
}

fn read_set(transaction: &Transaction<'_>, sql: &str) -> Result<HashSet<String>> {
    let mut statement = transaction.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
