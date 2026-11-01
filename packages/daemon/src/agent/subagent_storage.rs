use super::{SubagentStore, Timeline, limit_item_content};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::{self, File}, io::BufReader, path::{Path, PathBuf}};

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
    /// Replay each child as a guarded replacement, never append the same native
    /// transcript to a restored checkpoint a second time.
    pub fn replay_history(
        &mut self, root: Option<&Path>, parent: &str, events: Vec<Value>,
    ) -> Result<Vec<Value>> {
        use crate::providers::api::HistoryCoverage;
        let mut timelines = std::collections::BTreeMap::<String, (String, Timeline, bool)>::new();
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
            if let Some(root) = root {
                if !self.catalogs.contains_key(parent) { self.load_parent(root, parent, true)?; }
            }
            let key = (parent.to_owned(), id.clone());
            let coverage = if complete { HistoryCoverage::Complete } else { HistoryCoverage::Unverified };
            if self.timelines.get(&key).is_some_and(|old| !replacement.reconcile_history(old, coverage, false)) {
                tracing::warn!(parent_id=parent, subagent_id=id, "Retaining canonical subagent history: replay does not cover stored rows");
                continue;
            }
            if let Some(root) = root {
                replacement.persist(&timeline_path(root, parent, &id), &identity(parent, &id))?;
                if !self.timelines.contains_key(&key) {
                    self.catalogs.get_mut(parent).unwrap().prepare_cached(
                        "INSERT INTO children(id, descriptor) VALUES (?1, NULL) ON CONFLICT(id) DO NOTHING"
                    )?.execute([&id])?;
                }
            }
            // Existing projected-timeline consumers detect the new epoch and
            // refetch. No new protocol kind or thousands of duplicate rows.
            if let Some(row) = replacement.rows.last() {
                outputs.push(json!({"type":"timeline","parentAgentId":parent,"subagentId":id,
                    "provider":provider,"row":row,"epoch":replacement.epoch()}));
            }
            self.timelines.insert(key, replacement);
        }
        Ok(outputs)
    }

    /// The legacy checkpoint is imported once and left untouched. The catalog is
    /// published only after every child database is durable.
    pub fn load_parent(&mut self, root: &Path, parent: &str, create: bool) -> Result<()> {
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
                    if !create { return Ok(()); }
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
        let mut timelines = Vec::new();
        {
            let mut statement = connection.prepare("SELECT id, descriptor FROM children ORDER BY rowid")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                if let Some(encoded) = row.get::<_, Option<String>>(1)? {
                    let descriptor: Value = serde_json::from_str(&encoded)?;
                    ensure!(descriptor["id"] == id && descriptor["parentAgentId"] == parent,
                        "Subagent descriptor identity mismatch");
                    descriptors.push(descriptor);
                }
                let path = timeline_path(root, parent, &id);
                ensure!(path.try_exists()?, "Subagent timeline database missing");
                let timeline = Timeline::load(&path, &identity(parent, &id))?;
                timelines.push(((parent.to_owned(), id), timeline));
            }
        }
        self.delete_parent(parent);
        self.descriptors.extend(descriptors);
        self.timelines.extend(timelines);
        self.catalogs.insert(parent.to_owned(), connection);
        Ok(())
    }

    /// Called on a blocking worker. Parent event commits serialize these changes;
    /// only the touched projected row or descriptor is written, never its siblings.
    pub fn apply_persisted(&mut self, root: &Path, parent: &str, provider: &str, event: &Value) -> Result<Value> {
        if !self.catalogs.contains_key(parent) { self.load_parent(root, parent, true)?; }
        let id = event["id"].as_str().context("Provider subagent event missing id")?;
        let key = (parent.to_owned(), id.to_owned());
        let path = timeline_path(root, parent, id);
        let owner = identity(parent, id);
        match event["type"].as_str() {
            Some("timeline") => {
                if !self.timelines.contains_key(&key) {
                    let mut timeline = Timeline::new();
                    timeline.persist(&path, &owner)?;
                    self.catalogs.get_mut(parent).unwrap().prepare_cached(
                        "INSERT INTO children(id, descriptor) VALUES (?1, NULL) ON CONFLICT(id) DO NOTHING"
                    )?.execute([id])?;
                    self.timelines.insert(key.clone(), timeline);
                }
                let timeline = self.timelines.get_mut(&key).unwrap();
                let row = timeline.append_persisted(&path, &owner, limit_item_content(event["item"].clone()), event)?;
                Ok(json!({"type":"timeline","parentAgentId":parent,"subagentId":id,"provider":provider,"row":row,"epoch":timeline.epoch()}))
            }
            Some("upsert") => {
                let previous = self.descriptors.iter().position(|row| row["parentAgentId"] == parent && row["id"] == id)
                    .map(|index| (index, self.descriptors[index].clone()));
                let existed = self.timelines.contains_key(&key);
                let output = self.apply(parent, provider, event)?;
                let persist = (|| -> Result<()> {
                    if !existed { self.timelines.get_mut(&key).unwrap().persist(&path, &owner)?; }
                    self.catalogs.get_mut(parent).unwrap().prepare_cached(
                        "INSERT INTO children(id, descriptor) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET descriptor=excluded.descriptor"
                    )?.execute(params![id, serde_json::to_string(&output["subagent"])?])?;
                    Ok(())
                })();
                if let Err(error) = persist {
                    if let Some((index, descriptor)) = previous {
                        self.descriptors[index] = descriptor;
                    } else {
                        self.descriptors.retain(|row| row["parentAgentId"] != parent || row["id"] != id);
                    }
                    if !existed { self.timelines.remove(&key); }
                    return Err(error);
                }
                Ok(output)
            }
            Some("remove") => {
                self.catalogs.get_mut(parent).unwrap().prepare_cached("DELETE FROM children WHERE id=?1")?.execute([id])?;
                let output = self.apply(parent, provider, event)?;
                for file in [path.clone(), PathBuf::from(format!("{}-wal", path.display())), PathBuf::from(format!("{}-shm", path.display()))] {
                    if let Err(error) = fs::remove_file(file) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(%error, "Failed to remove unreferenced subagent storage");
                        }
                    }
                }
                Ok(output)
            }
            _ => anyhow::bail!("Unknown provider subagent event"),
        }
    }
}
