use super::super::storage;
use super::{SubagentStore, Timeline, limit_item_content};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::{fmt, fs::{self, File}, io::BufReader, path::{Path, PathBuf}};

/// Child timelines kept hydrated after an operation. Each holds one WAL
/// connection (database, `-wal` and `-shm` descriptors).
const RESIDENT_CHILDREN: usize = 16;
/// Stored JSON bytes of all hydrated children kept after an operation. A child
/// larger than this is hydrated only for the write or replay that needs it.
const RESIDENT_BYTES: u64 = 32 << 20;
/// Open parent catalog connections. Descriptor indexes stay resident without one.
const OPEN_CATALOGS: usize = 8;
const INSERT_CHILD: &str = "INSERT INTO children(id, descriptor) VALUES (?1, NULL) ON CONFLICT(id) DO NOTHING";
const UPSERT_CHILD: &str = "INSERT INTO children(id, descriptor) VALUES (?1, ?2) \
    ON CONFLICT(id) DO UPDATE SET descriptor=excluded.descriptor";

/// Persisted subagent state that cannot be read or written without replacing or
/// discarding stored bytes. Scoped to one parent catalog (`child: None`) or one
/// child; the files are left exactly as found.
#[derive(Clone, Debug)]
pub struct SubagentStorageUnavailable {
    pub parent: String,
    pub child: Option<String>,
    pub reason: String,
}
impl fmt::Display for SubagentStorageUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.child {
            Some(child) => write!(formatter, "Subagent '{child}' of agent '{}' is unavailable: {}", self.parent, self.reason),
            None => write!(formatter, "Subagents of agent '{}' are unavailable: {}", self.parent, self.reason),
        }
    }
}
impl std::error::Error for SubagentStorageUnavailable {}
impl SubagentStorageUnavailable {
    fn new(parent: &str, child: Option<&str>, reason: String) -> Self {
        Self { parent: parent.to_owned(), child: child.map(str::to_owned), reason }
    }
}
fn unavailable(parent: &str, child: Option<&str>, reason: String) -> anyhow::Error {
    anyhow::Error::new(SubagentStorageUnavailable::new(parent, child, reason))
}

/// Index of one parent's catalog. Children are hydrated separately.
pub(super) struct Catalog {
    /// Cataloged child ids; `Some` records why the row's descriptor is unreadable.
    children: HashMap<String, Option<String>>,
    connection: Option<Connection>,
    used: u64,
}

struct Resident {
    used: u64,
    /// Stored JSON bytes; appends add their row size (an upper bound when an
    /// append merges into an existing row) until the next reload re-measures.
    bytes: u64,
}

/// Hydrated cataloged children. Memory-only parents are never tracked here.
pub(super) struct Residency {
    clock: u64,
    children: HashMap<(String, String), Resident>,
    bytes: u64,
    max_children: usize,
    max_bytes: u64,
    max_catalogs: usize,
}
impl Default for Residency {
    fn default() -> Self {
        Self {
            clock: 0, children: HashMap::new(), bytes: 0,
            max_children: RESIDENT_CHILDREN, max_bytes: RESIDENT_BYTES, max_catalogs: OPEN_CATALOGS,
        }
    }
}
impl Residency {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
    fn admit(&mut self, key: (String, String), bytes: u64) {
        let used = self.tick();
        if let Some(previous) = self.children.insert(key, Resident { used, bytes }) {
            self.bytes -= previous.bytes;
        }
        self.bytes += bytes;
    }
    fn touch(&mut self, key: &(String, String)) {
        let used = self.tick();
        if let Some(resident) = self.children.get_mut(key) {
            resident.used = used;
        }
    }
    fn grow(&mut self, key: &(String, String), bytes: u64) {
        if let Some(resident) = self.children.get_mut(key) {
            resident.bytes += bytes;
            self.bytes += bytes;
        }
    }
    fn evict(&mut self, key: &(String, String)) {
        if let Some(resident) = self.children.remove(key) {
            self.bytes -= resident.bytes;
        }
    }
    fn exceeded(&self) -> bool {
        self.children.len() > self.max_children || self.bytes > self.max_bytes
    }
    pub(super) fn forget_parent(&mut self, parent: &str) {
        let keys: Vec<_> = self.children.keys().filter(|(owner, _)| owner == parent).cloned().collect();
        for key in keys {
            self.evict(&key);
        }
    }
}

fn timeline_path(root: &Path, parent: &str, id: &str) -> PathBuf {
    root.join(parent).join(format!("{}.sqlite3", hex::encode(Sha256::digest(id.as_bytes()))))
}
fn identity(parent: &str, id: &str) -> String {
    serde_json::to_string(&[parent, id]).expect("string identity")
}
fn open_catalog(path: &Path, parent: &str) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let (version, stored): (i64, String) = connection.query_row(
        "SELECT version, parent_id FROM identity", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    ensure!(version == 1 && stored == parent, "Subagent catalog identity mismatch");
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(connection)
}

impl SubagentStore {
    #[cfg(test)]
    fn with_residency(max_children: usize, max_bytes: u64, max_catalogs: usize) -> Self {
        assert!(max_children > 0 && max_catalogs > 0);
        let residency = Residency { max_children, max_bytes, max_catalogs, ..Residency::default() };
        Self { residency, ..Self::default() }
    }

    /// Replay each child as a guarded replacement, never append the same native
    /// transcript to a restored checkpoint a second time. Stored children are
    /// hydrated first so reconciliation always sees canonical rows; an
    /// unavailable child keeps its stored bytes and is skipped.
    pub fn replay_history(
        &mut self, root: Option<&Path>, parent: &str, events: Vec<Value>,
    ) -> Result<Vec<Value>> {
        use crate::providers::api::HistoryCoverage;
        let mut timelines = BTreeMap::<String, (String, Timeline, bool)>::new();
        let mut outputs = Vec::new();
        for envelope in events {
            let provider = envelope["provider"].as_str().context("Subagent provider missing")?;
            let event = &envelope["event"];
            let id = event["id"].as_str().context("Subagent id missing")?;
            if event["type"] == "timeline" {
                let (_, timeline, complete) = timelines.entry(id.to_owned())
                    .or_insert_with(|| (provider.to_owned(), Timeline::new(), true));
                *complete &= event["historyComplete"] == true;
                timeline.append(event["item"].clone(), event);
            } else if event["type"] == "upsert" {
                outputs.push(match root {
                    Some(root) => self.apply_persisted(root, parent, provider, event)?,
                    None => self.apply(parent, provider, event)?,
                });
            }
        }
        for (id, (provider, mut replacement, complete)) in timelines {
            if root.is_some() {
                self.release_excess();
            }
            let key = (parent.to_owned(), id.clone());
            let stored = match root {
                Some(root) => {
                    self.ensure_parent(root, parent, true)?;
                    match self.ensure_child(root, parent, &id) {
                        Ok(stored) => stored,
                        Err(error) => {
                            tracing::warn!(parent_id=parent, subagent_id=id, %error, "Retaining unavailable subagent history: replay not applied");
                            continue;
                        }
                    }
                }
                None => self.timelines.contains_key(&key),
            };
            let coverage = if complete { HistoryCoverage::Complete } else { HistoryCoverage::Unverified };
            if stored && !replacement.reconcile_history(&self.timelines[&key], coverage, false) {
                tracing::warn!(parent_id=parent, subagent_id=id, "Retaining canonical subagent history: replay does not cover stored rows");
                continue;
            }
            if let Some(root) = root {
                replacement.persist(&timeline_path(root, parent, &id), &identity(parent, &id))?;
                if !stored {
                    self.catalog(root, parent)?.prepare_cached(INSERT_CHILD)?.execute([&id])?;
                    self.index_child(parent, &id);
                }
            }
            // Existing projected-timeline consumers detect the new epoch and
            // refetch. No new protocol kind or thousands of duplicate rows.
            if let Some(row) = replacement.rows.last() {
                outputs.push(json!({"type":"timeline","parentAgentId":parent,"subagentId":id,
                    "provider":provider,"row":row,"epoch":replacement.epoch()}));
            }
            let bytes: u64 = match root {
                Some(_) => replacement.rows.iter().map(storage::encoded_bytes).sum(),
                None => 0,
            };
            self.timelines.insert(key.clone(), replacement);
            if root.is_some() {
                self.residency.admit(key, bytes);
            }
        }
        if root.is_some() {
            self.release_excess();
        }
        Ok(outputs)
    }

    /// Descriptors of one persisted parent, indexing its catalog on first use,
    /// plus every cataloged child whose descriptor is unreadable. A non-empty
    /// second list means the descriptor list is incomplete. Child histories are
    /// not hydrated.
    pub fn list_persisted(
        &mut self, root: &Path, parent: &str,
    ) -> Result<(Vec<Value>, Vec<SubagentStorageUnavailable>)> {
        self.ensure_parent(root, parent, false)?;
        self.release_excess();
        Ok((self.list(parent), self.unreadable_children(parent)))
    }

    /// Descriptors across persisted parents, plus every parent catalog or child
    /// descriptor that could not be read. A non-empty second list means the
    /// descriptor list is incomplete; one failure never hides other parents.
    /// A recorded catalog failure is reported again without re-reading it; the
    /// parent's own access (listing, fetch, write) retries.
    pub fn list_persisted_parents<'a>(
        &mut self, root: &Path, parents: impl IntoIterator<Item = &'a str>,
    ) -> (Vec<Value>, Vec<SubagentStorageUnavailable>) {
        let mut listed = HashSet::new();
        let mut failures = Vec::new();
        for parent in parents {
            let failure = self.unavailable.get(parent).cloned().or_else(|| {
                self.ensure_parent(root, parent, false).err().map(|error| {
                    error.downcast_ref::<SubagentStorageUnavailable>()
                        .map_or_else(|| format!("{error:#}"), |error| error.reason.clone())
                })
            });
            match failure {
                Some(reason) => failures.push(SubagentStorageUnavailable::new(parent, None, reason)),
                None => {
                    failures.extend(self.unreadable_children(parent));
                    listed.insert(parent);
                }
            }
        }
        self.release_excess();
        let subagents = self.descriptors.iter()
            .filter(|descriptor| descriptor["parentAgentId"].as_str().is_some_and(|parent| listed.contains(parent)))
            .cloned().collect();
        (subagents, failures)
    }

    /// One page of a persisted child's history. A hydrated child is paged in
    /// memory; otherwise only row metadata and the selected rows are read from
    /// SQLite, and nothing is hydrated or retained.
    pub fn fetch_persisted(&mut self, root: &Path, parent: &str, id: &str, options: &Value) -> Result<Value> {
        if !(self.ensure_parent(root, parent, false)? && self.cataloged(parent, id)?) {
            bail!("Unknown agent '{parent}\0{id}'");
        }
        let key = (parent.to_owned(), id.to_owned());
        let page = if self.residency.children.contains_key(&key) {
            self.residency.touch(&key);
            self.timelines[&key].fetch(options)
        } else {
            storage::fetch_page(&timeline_path(root, parent, id), &identity(parent, id), options)
                .map_err(|error| {
                    let reason = format!("{error:#}");
                    tracing::warn!(parent_id = parent, subagent_id = id, error = %reason, "Subagent history unavailable; stored files retained");
                    unavailable(parent, Some(id), reason)
                })?
        };
        self.release_excess();
        Ok(page)
    }

    /// Inactive release: drop a parent's hydrated children and its catalog
    /// connection. Descriptors stay indexed; history reloads on demand.
    pub fn release_parent(&mut self, parent: &str) {
        let keys: Vec<_> = self.residency.children.keys()
            .filter(|(owner, _)| owner == parent).cloned().collect();
        for key in keys {
            if self.timelines.get(&key).is_some_and(|timeline| timeline.storage_dirty) { continue; }
            self.residency.evict(&key);
            self.timelines.remove(&key);
        }
        if let Some(catalog) = self.catalogs.get_mut(parent) {
            catalog.connection = None;
        }
    }

    /// Called on a blocking worker. Parent event commits serialize these changes;
    /// only the touched projected row or descriptor is written, never its siblings.
    pub fn apply_persisted(&mut self, root: &Path, parent: &str, provider: &str, event: &Value) -> Result<Value> {
        self.ensure_parent(root, parent, true)?;
        let id = event["id"].as_str().context("Provider subagent event missing id")?;
        let output = match event["type"].as_str() {
            Some("timeline") => self.append_child(root, parent, provider, id, event),
            Some("upsert") => self.upsert_child(root, parent, provider, id, event),
            Some("remove") => self.remove_child(root, parent, provider, id, event),
            _ => Err(anyhow!("Unknown provider subagent event")),
        };
        self.release_excess();
        output
    }

    /// Appending needs the hydrated child (identity merges, sequence window).
    /// A child over the residency budget is released again after this write.
    fn append_child(&mut self, root: &Path, parent: &str, provider: &str, id: &str, event: &Value) -> Result<Value> {
        if !self.ensure_child(root, parent, id)? {
            self.create_child(root, parent, id, None)?;
        }
        let key = (parent.to_owned(), id.to_owned());
        let timeline = self.timelines.get_mut(&key).expect("hydrated subagent timeline");
        let row = timeline.append_persisted(
            &timeline_path(root, parent, id), &identity(parent, id), limit_item_content(event["item"].clone()), event,
        )?;
        let output = json!({"type":"timeline","parentAgentId":parent,"subagentId":id,"provider":provider,"row":row,"epoch":timeline.epoch()});
        self.residency.grow(&key, storage::encoded_bytes(&output["row"]));
        Ok(output)
    }

    /// Descriptor changes never hydrate or rewrite the child's history. The
    /// catalog row is durable before the descriptor is published in memory.
    fn upsert_child(&mut self, root: &Path, parent: &str, provider: &str, id: &str, event: &Value) -> Result<Value> {
        let cataloged = self.cataloged(parent, id)?;
        let descriptor = self.descriptor(parent, provider, id, event);
        let encoded = serde_json::to_string(&descriptor)?;
        if cataloged {
            self.catalog(root, parent)?.prepare_cached(UPSERT_CHILD)?.execute(params![id, encoded])?;
        } else {
            self.create_child(root, parent, id, Some(&encoded))?;
        }
        self.store_descriptor(descriptor.clone());
        Ok(json!({"type":"upsert","subagent":descriptor}))
    }

    /// Explicit provider removal deletes the catalog row first, then the files.
    fn remove_child(&mut self, root: &Path, parent: &str, provider: &str, id: &str, event: &Value) -> Result<Value> {
        self.catalog(root, parent)?.prepare_cached("DELETE FROM children WHERE id=?1")?.execute([id])?;
        if let Some(catalog) = self.catalogs.get_mut(parent) {
            catalog.children.remove(id);
        }
        self.residency.evict(&(parent.to_owned(), id.to_owned()));
        let output = self.apply(parent, provider, event)?;
        let path = timeline_path(root, parent, id);
        for file in [path.clone(), PathBuf::from(format!("{}-wal", path.display())), PathBuf::from(format!("{}-shm", path.display()))] {
            if let Err(error) = fs::remove_file(file) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, "Failed to remove unreferenced subagent storage");
                }
            }
        }
        Ok(output)
    }

    /// A child first seen now gets a durable empty database before its catalog row.
    fn create_child(&mut self, root: &Path, parent: &str, id: &str, descriptor: Option<&str>) -> Result<()> {
        let mut timeline = Timeline::new();
        timeline.persist(&timeline_path(root, parent, id), &identity(parent, id))?;
        let catalog = self.catalog(root, parent)?;
        match descriptor {
            Some(descriptor) => catalog.prepare_cached(UPSERT_CHILD)?.execute(params![id, descriptor])?,
            None => catalog.prepare_cached(INSERT_CHILD)?.execute([id])?,
        };
        self.index_child(parent, id);
        let key = (parent.to_owned(), id.to_owned());
        self.timelines.insert(key.clone(), timeline);
        self.residency.admit(key, 0);
        Ok(())
    }

    /// Index a persisted parent once. Failures are scoped to this parent, leave
    /// no partial state, and never replace or delete the stored files.
    /// `Ok(false)`: the parent has no catalog (and `create` was not requested).
    fn ensure_parent(&mut self, root: &Path, parent: &str, create: bool) -> Result<bool> {
        if self.catalogs.contains_key(parent) {
            return Ok(true);
        }
        match self.index_parent(root, parent, create) {
            Ok(indexed) => {
                self.unavailable.remove(parent);
                Ok(indexed)
            }
            Err(error) => {
                self.delete_parent(parent);
                let reason = format!("{error:#}");
                tracing::warn!(parent_id = parent, error = %reason, "Subagent catalog unavailable; stored files retained");
                self.unavailable.insert(parent.to_owned(), reason.clone());
                Err(unavailable(parent, None, reason))
            }
        }
    }

    /// The legacy checkpoint is imported once and left untouched. The catalog is
    /// published only after every child database is durable. Only descriptors
    /// are retained; child histories hydrate on demand.
    fn index_parent(&mut self, root: &Path, parent: &str, create: bool) -> Result<bool> {
        let directory = root.join(parent);
        let catalog = directory.join("index.sqlite3");
        if !catalog.try_exists()? {
            let legacy = root.join(format!("{parent}.json"));
            match File::open(&legacy) {
                Ok(file) => {
                    let checkpoint: Value = serde_json::from_reader(BufReader::new(file))?;
                    ensure!(checkpoint["parentAgentId"] == parent, "Subagent checkpoint parent mismatch");
                    self.restore_parent(&checkpoint)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if !create { return Ok(false); }
                }
                Err(error) => return Err(error.into()),
            }
            // Legacy descriptors may legitimately have no first timeline item.
            for descriptor in &self.descriptors {
                if descriptor["parentAgentId"] == parent {
                    let id = descriptor["id"].as_str().context("Subagent descriptor missing id")?;
                    self.timelines.entry((parent.to_owned(), id.to_owned())).or_default();
                }
            }
            fs::create_dir_all(&directory)?;
            for ((owner, id), timeline) in &mut self.timelines {
                if owner == parent {
                    timeline.persist(&timeline_path(root, parent, id), &identity(parent, id))?;
                }
            }
            let file = tempfile::NamedTempFile::new_in(&directory)?;
            let mut connection = Connection::open(file.path())?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            let transaction = connection.transaction()?;
            transaction.execute_batch("CREATE TABLE identity(version INTEGER NOT NULL, parent_id TEXT NOT NULL); CREATE TABLE children(id TEXT PRIMARY KEY, descriptor TEXT);")?;
            transaction.execute("INSERT INTO identity VALUES (1, ?1)", [parent])?;
            for ((owner, id), _) in &self.timelines {
                if owner == parent {
                    let descriptor = self.get(parent, id).map(serde_json::to_string).transpose()?;
                    transaction.execute("INSERT INTO children VALUES (?1, ?2)", params![id, descriptor])?;
                }
            }
            transaction.commit()?;
            drop(connection);
            file.as_file().sync_all()?;
            file.persist_noclobber(&catalog).map_err(|error| error.error)?;
            #[cfg(unix)]
            File::open(&directory)?.sync_all()?;
        }
        let connection = open_catalog(&catalog, parent)?;
        let mut descriptors = Vec::new();
        let mut children = HashMap::new();
        {
            let mut statement = connection.prepare("SELECT id, descriptor FROM children ORDER BY rowid")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let encoded: Option<String> = row.get(1)?;
                let descriptor = encoded.map(|encoded| -> Result<Value> {
                    let descriptor: Value = serde_json::from_str(&encoded)?;
                    ensure!(descriptor["id"] == id.as_str() && descriptor["parentAgentId"] == parent,
                        "Subagent descriptor identity mismatch");
                    Ok(descriptor)
                }).transpose();
                match descriptor {
                    Ok(descriptor) => {
                        descriptors.extend(descriptor);
                        children.insert(id, None);
                    }
                    // The row stays as stored; this child alone refuses reads and writes.
                    Err(error) => {
                        let reason = format!("{error:#}");
                        tracing::warn!(parent_id = parent, subagent_id = id, error = %reason, "Subagent descriptor unreadable; catalog row retained");
                        children.insert(id, Some(reason));
                    }
                }
            }
        }
        // A legacy import hydrated every child once; keep only the index.
        self.delete_parent(parent);
        self.descriptors.extend(descriptors);
        let used = self.residency.tick();
        self.catalogs.insert(parent.to_owned(), Catalog { children, connection: Some(connection), used });
        Ok(true)
    }

    /// `Ok(false)`: the indexed catalog has no such child. A child whose
    /// descriptor row is unreadable refuses every read and write.
    fn cataloged(&self, parent: &str, id: &str) -> Result<bool> {
        match self.catalogs.get(parent).context("Subagent catalog not indexed")?.children.get(id) {
            None => Ok(false),
            Some(Some(reason)) => Err(unavailable(parent, Some(id), reason.clone())),
            Some(None) => Ok(true),
        }
    }

    fn unreadable_children(&self, parent: &str) -> Vec<SubagentStorageUnavailable> {
        let mut unreadable: Vec<_> = self.catalogs.get(parent).into_iter()
            .flat_map(|catalog| catalog.children.iter())
            .filter_map(|(id, reason)| reason.as_ref().map(|reason| SubagentStorageUnavailable::new(parent, Some(id.as_str()), reason.clone())))
            .collect();
        unreadable.sort_by(|left, right| left.child.cmp(&right.child));
        unreadable
    }

    /// Hydrate one cataloged child for a write or replay. `Ok(false)`: the
    /// catalog has no such child. A missing or unreadable database is reported,
    /// never replaced by an empty timeline that a later write would persist.
    fn ensure_child(&mut self, root: &Path, parent: &str, id: &str) -> Result<bool> {
        if !self.cataloged(parent, id)? {
            return Ok(false);
        }
        let key = (parent.to_owned(), id.to_owned());
        if self.timelines.contains_key(&key) {
            self.residency.touch(&key);
            return Ok(true);
        }
        let path = timeline_path(root, parent, id);
        let loaded = path.try_exists().map_err(anyhow::Error::from).and_then(|exists| {
            ensure!(exists, "Subagent timeline database missing: {}", path.display());
            storage::load_measured(&path, &identity(parent, id))
        });
        let (timeline, bytes) = loaded.map_err(|error| {
            let reason = format!("{error:#}");
            tracing::warn!(parent_id = parent, subagent_id = id, error = %reason, "Subagent history unavailable; stored files retained");
            unavailable(parent, Some(id), reason)
        })?;
        self.timelines.insert(key.clone(), timeline);
        self.residency.admit(key, bytes);
        Ok(true)
    }

    fn index_child(&mut self, parent: &str, id: &str) {
        self.catalogs.get_mut(parent).expect("indexed subagent catalog").children.insert(id.to_owned(), None);
    }

    /// The parent's catalog connection, reopened (and re-verified) after release.
    fn catalog(&mut self, root: &Path, parent: &str) -> Result<&mut Connection> {
        let used = self.residency.tick();
        let catalog = self.catalogs.get_mut(parent).context("Subagent catalog not indexed")?;
        catalog.used = used;
        if catalog.connection.is_none() {
            let connection = open_catalog(&root.join(parent).join("index.sqlite3"), parent)
                .map_err(|error| unavailable(parent, None, format!("{error:#}")))?;
            catalog.connection = Some(connection);
        }
        Ok(catalog.connection.as_mut().expect("open subagent catalog"))
    }

    /// Evict least-recently used children until both the count and byte limits
    /// hold, including the child just used when it alone exceeds the byte
    /// limit, and close idle catalogs. Every evicted child is durable (never
    /// `storage_dirty`), so it reloads with the same epoch, rows and sequence
    /// window.
    fn release_excess(&mut self) {
        while self.residency.exceeded() {
            let Some(key) = self.residency.children.iter()
                .filter(|(key, _)| self.timelines.get(*key).is_none_or(|timeline| !timeline.storage_dirty))
                .min_by_key(|(_, resident)| resident.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.residency.evict(&key);
            self.timelines.remove(&key);
        }
        let mut open: Vec<_> = self.catalogs.values_mut().filter(|catalog| catalog.connection.is_some()).collect();
        if open.len() > self.residency.max_catalogs {
            open.sort_by_key(|catalog| catalog.used);
            let excess = open.len() - self.residency.max_catalogs;
            for catalog in open.into_iter().take(excess) {
                catalog.connection = None;
            }
        }
    }
}

#[cfg(test)]
#[path = "subagent_storage_tests.rs"]
mod tests;
