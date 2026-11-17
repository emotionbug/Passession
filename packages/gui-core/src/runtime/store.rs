//! In-memory mirror of the Flutter client stores and the write scheduling of
//! `AppStore.savePreferences` / `_scheduleSave` and `SharedPreferences` writes.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use serde_json::{Map, Value, json};

use super::Core;
use crate::{
    CoreError, RequestId,
    storage::{self, MAIN_WINDOW, Paths, prefs},
};

pub(crate) struct Store {
    pub paths: Paths,
    /// False after an unreadable/corrupt/newer `state.json`: it is never overwritten.
    pub writable: bool,
    pub error: Option<CoreError>,
    pub preferences: Map<String, Value>,
    /// What this store last merged into `state.json` (three-way merge base).
    pub saved_preferences: Map<String, Value>,
    pub saved_hosts: Map<String, Value>,
    pub suppressed: Vec<String>,
    pub layouts: BTreeMap<String, Value>,
    dirty_layouts: BTreeSet<String>,
    written_state: Option<String>,
    /// Contents last written or loaded, by file. Replicas drop out once their revision is
    /// known.
    written_files: HashMap<PathBuf, String>,
    /// Directory revision of the host each replica file was last written from.
    replica_revisions: HashMap<PathBuf, u64>,
    pub prefs: prefs::Prefs,
    pub prefs_revisions: Map<String, Value>,
    pub layouts_revision: u64,
    saving: bool,
    again: bool,
    scheduled: bool,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            paths: Paths::new(state_dir),
            writable: false,
            error: None,
            preferences: Map::new(),
            saved_preferences: Map::new(),
            saved_hosts: Map::new(),
            suppressed: Vec::new(),
            layouts: BTreeMap::new(),
            dirty_layouts: BTreeSet::new(),
            written_state: None,
            written_files: HashMap::new(),
            replica_revisions: HashMap::new(),
            prefs: prefs::Prefs::default(),
            prefs_revisions: Map::new(),
            layouts_revision: 0,
            saving: false,
            again: false,
            scheduled: false,
        }
    }

    pub fn bump_pref(&mut self, key: &str) {
        self.prefs_revisions
            .insert(key.to_owned(), super::directory::next_revision().into());
    }

    pub fn apply_loaded(&mut self, loaded: &mut storage::Loaded, prefs: prefs::Prefs) {
        self.writable = loaded.writable;
        self.error = loaded.error.take().or_else(|| prefs.error.clone());
        self.preferences = std::mem::take(&mut loaded.preferences);
        self.saved_preferences = self.preferences.clone();
        self.saved_hosts = std::mem::take(&mut loaded.saved_hosts);
        self.suppressed = std::mem::take(&mut loaded.suppressed_routes);
        self.layouts = std::mem::take(&mut loaded.layouts);
        self.written_files = std::mem::take(&mut loaded.written_files);
        self.prefs = prefs;
        self.prefs_revisions = self
            .prefs
            .values
            .keys()
            .map(|key| (key.clone(), super::directory::next_revision().into()))
            .collect();
        self.layouts_revision = super::directory::next_revision();
    }
}

fn storage_failed(core: &Rc<Core>, error: CoreError) {
    core.store.borrow_mut().error = Some(error.clone());
    core.error(None, error);
    core.touch();
}

/// Loads `state.json`, layouts, replicas and preferences off the core thread.
pub(crate) async fn load(paths: Paths) -> (storage::Loaded, prefs::Prefs) {
    tokio::task::spawn_blocking(move || (storage::load(&paths), prefs::load(&paths)))
        .await
        .unwrap_or_else(|error| {
            let failed = CoreError::new("STORAGE_IO", format!("State load failed: {error}"), false);
            (
                storage::Loaded {
                    error: Some(failed.clone()),
                    writable: false,
                    ..storage::Loaded::default()
                },
                prefs::Prefs {
                    error: Some(failed),
                    ..prefs::Prefs::default()
                },
            )
        })
}

/// `CoreHandle::save_state`.
pub(crate) async fn save_state(core: &Rc<Core>, key: String, value: Value, ack: Option<RequestId>) {
    if let Some(name) = key.strip_prefix("prefs:").filter(|name| !name.is_empty()) {
        let changed = {
            let mut store = core.store.borrow_mut();
            let value = prefs::stored_value(value);
            let changed = store.prefs.values.get(name) != value.as_ref();
            if changed {
                match value {
                    Some(value) => {
                        store.prefs.values.insert(name.to_owned(), value);
                    }
                    None => {
                        store.prefs.values.remove(name);
                    }
                }
                store.bump_pref(name);
            }
            changed
        };
        if changed {
            core.touch();
        }
        if !changed && ack.is_none() && core.store.borrow().prefs.writable {
            return;
        }
        match queue_prefs(core, ack.is_some()) {
            Ok(Some(completed)) => {
                let core = core.clone();
                tokio::task::spawn_local(async move {
                    let result = prefs_completed(completed).await.map(|()| Value::Null);
                    core.respond("", &ack.expect("flushed prefs acknowledgement"), result);
                });
            }
            Ok(None) => {}
            Err(error) => match ack {
                Some(ack) => core.respond("", &ack, Err(error)),
                None => core.error(None, error),
            },
        }
        return;
    }
    let result = apply(core, &key, value).await;
    match (result, ack) {
        (Ok(()), Some(ack)) => core.respond("", &ack, Ok(Value::Null)),
        (Err(error), Some(ack)) => core.respond("", &ack, Err(error)),
        (Err(error), None) => core.error(None, error),
        (Ok(()), None) => {}
    }
}

async fn apply(core: &Rc<Core>, key: &str, value: Value) -> Result<(), CoreError> {
    if key == "preferences" {
        let Value::Object(preferences) = value else {
            return Err(CoreError::new(
                "INVALID_STATE_KEY",
                "preferences must be an object",
                false,
            ));
        };
        core.store.borrow_mut().preferences = preferences;
        core.touch();
        return save_now(core).await;
    }
    if let Some(window) = key
        .strip_prefix("layout:")
        .or((key == "layout").then_some(MAIN_WINDOW))
    {
        if window.is_empty() || !value.is_object() {
            return Err(CoreError::new(
                "INVALID_STATE_KEY",
                "A layout is a JSON object for a named window",
                false,
            ));
        }
        {
            let mut store = core.store.borrow_mut();
            if store.layouts.get(window) != Some(&value) {
                store.layouts.insert(window.to_owned(), value);
                store.layouts_revision = super::directory::next_revision();
                store.dirty_layouts.insert(window.to_owned());
            }
        }
        core.touch();
        return save_now(core).await;
    }
    Err(CoreError::new(
        "INVALID_STATE_KEY",
        format!("Unknown state key {key}"),
        false,
    ))
}

type PrefsCompletion = tokio::sync::oneshot::Receiver<Result<(), CoreError>>;

/// Ordinary saves only mark the latest in-memory values dirty. Flushed saves seal a
/// barrier: subsequent commands may overwrite a key, but cannot skip that durable value.
fn queue_prefs(core: &Rc<Core>, flushed: bool) -> Result<Option<PrefsCompletion>, CoreError> {
    let (completed, start) = {
        let mut store = core.store.borrow_mut();
        if !store.prefs.writable {
            return Err(CoreError::new(
                "STORAGE_READ_ONLY",
                "Saved client preferences are unreadable; changes are kept only in memory.",
                false,
            ));
        }
        let completed = if flushed {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let write = prefs::Write {
                values: store.prefs.values.clone(),
                foreign: store.prefs.foreign.clone(),
                report_error: std::mem::take(&mut store.prefs.writer.dirty),
                completed: Some(sender),
            };
            store.prefs.writer.barriers.push_back(write);
            Some(receiver)
        } else {
            store.prefs.writer.dirty = true;
            None
        };
        let start = !store.prefs.writer.running;
        store.prefs.writer.running = true;
        (completed, start)
    };
    if start {
        let core = core.clone();
        tokio::task::spawn_local(async move {
            loop {
                let next = {
                    let mut store = core.store.borrow_mut();
                    let write = store.prefs.writer.barriers.pop_front().or_else(|| {
                        if !std::mem::take(&mut store.prefs.writer.dirty) {
                            return None;
                        }
                        Some(prefs::Write {
                            values: store.prefs.values.clone(),
                            foreign: store.prefs.foreign.clone(),
                            report_error: true,
                            completed: None,
                        })
                    });
                    match write {
                        Some(write) => Some((store.paths.clone(), write)),
                        None => {
                            store.prefs.writer.running = false;
                            None
                        }
                    }
                };
                let Some((paths, write)) = next else { break };
                let prefs::Write {
                    values,
                    foreign,
                    report_error,
                    completed,
                } = write;
                // Exactly one replacement is in flight, including its fsync and rename.
                let result =
                    tokio::task::spawn_blocking(move || prefs::save(&paths, &values, &foreign))
                        .await
                        .map_err(|error| CoreError::new("STORAGE_IO", error.to_string(), false))
                        .and_then(|result| result);
                if report_error && let Err(error) = &result {
                    core.error(None, error.clone());
                }
                if let Some(completed) = completed {
                    let _ = completed.send(result);
                }
            }
        });
    }
    Ok(completed)
}

async fn prefs_completed(completed: PrefsCompletion) -> Result<(), CoreError> {
    completed.await.map_err(|error| {
        CoreError::new(
            "STORAGE_IO",
            format!("Preferences writer stopped: {error}"),
            false,
        )
    })?
}

/// A durable barrier for internal workflows and shutdown; shares the same ordered writer
/// as `save_state`, never races a separate atomic replacement against it.
pub(crate) async fn write_prefs(core: &Rc<Core>) -> Result<(), CoreError> {
    let completed = queue_prefs(core, true)?.expect("flushed prefs completion");
    prefs_completed(completed).await
}

/// Flutter `_scheduleSave`: one save 350 ms after the first change of a burst.
pub(crate) fn schedule_save(core: &Rc<Core>) {
    if core.store.borrow().scheduled {
        return;
    }
    core.store.borrow_mut().scheduled = true;
    let core = core.clone();
    tokio::task::spawn_local(async move {
        tokio::time::sleep(Duration::from_millis(350)).await;
        core.store.borrow_mut().scheduled = false;
        if let Err(error) = save_now(&core).await {
            storage_failed(&core, error);
        }
    });
}

/// Flutter `savePreferences`: content-addressed, serialized saves. A save requested while one
/// runs is folded into one more pass after it.
pub(crate) async fn save_now(core: &Rc<Core>) -> Result<(), CoreError> {
    {
        let mut store = core.store.borrow_mut();
        if store.saving {
            store.again = true;
            return Ok(());
        }
        store.saving = true;
    }
    let mut result = Ok(());
    loop {
        core.store.borrow_mut().again = false;
        if let Err(error) = save_once(core).await {
            result = Err(error);
        }
        if !core.store.borrow().again {
            break;
        }
    }
    core.store.borrow_mut().saving = false;
    result
}

async fn save_once(core: &Rc<Core>) -> Result<(), CoreError> {
    if !core.store.borrow().writable {
        return Ok(());
    }
    let (rows, replicas) = {
        let hosts = core.hosts.borrow();
        let store = core.store.borrow();
        let rows: Vec<Value> = hosts.iter().map(|host| host.record_json()).collect();
        // Only a host whose directory revision moved since its replica was written is
        // serialized again.
        let replicas: Vec<(PathBuf, u64, String)> = hosts
            .iter()
            .filter_map(|host| {
                let path = store.paths.replica(&host.server_id());
                let revision = host.dir.borrow().revision();
                (store.replica_revisions.get(&path) != Some(&revision))
                    .then(|| (path, revision, host.replica_contents()))
            })
            .collect();
        (rows, replicas)
    };
    let (job, encoded, layout_keys, replica_writes, layout_writes) = {
        let mut store = core.store.borrow_mut();
        let snapshot = json!({
            "preferences": store.preferences,
            "hosts": rows,
            "suppressedRoutes": store.suppressed,
        });
        let encoded = snapshot.to_string();
        let state_changed = store.written_state.as_deref() != Some(encoded.as_str());
        let mut files = Vec::new();
        let mut replica_writes = Vec::new();
        for (path, revision, contents) in replicas {
            // Content loaded from disk is only needed until its revision is known.
            if store.written_files.get(&path) == Some(&contents) {
                store.written_files.remove(&path);
                store.replica_revisions.insert(path, revision);
            } else {
                replica_writes.push((path.clone(), revision));
                files.push((path, contents));
            }
        }
        let mut layout_writes = Vec::new();
        let layout_keys: Vec<String> = std::mem::take(&mut store.dirty_layouts)
            .into_iter()
            .collect();
        for window in &layout_keys {
            if let Some(layout) = store.layouts.get(window) {
                let path = store.paths.layout(window);
                let contents = layout.to_string();
                if store.written_files.get(&path) != Some(&contents) {
                    layout_writes.push((path.clone(), contents.clone()));
                    files.push((path, contents));
                }
            }
        }
        if !state_changed && files.is_empty() {
            return Ok(());
        }
        let state = state_changed.then(|| storage::StateSnapshot {
            preferences: store.preferences.clone(),
            hosts: snapshot["hosts"].as_array().cloned().unwrap_or_default(),
            suppressed_routes: store.suppressed.clone(),
        });
        let job = storage::SaveJob {
            state,
            saved_preferences: store.saved_preferences.clone(),
            saved_hosts: store.saved_hosts.clone(),
            files,
        };
        (
            job,
            state_changed.then_some(encoded),
            layout_keys,
            replica_writes,
            layout_writes,
        )
    };
    let paths = core.store.borrow().paths.clone();
    let outcome = tokio::task::spawn_blocking(move || storage::save(&paths, job))
        .await
        .map_err(|error| CoreError::new("STORAGE_IO", error.to_string(), false))?;
    let mut store = core.store.borrow_mut();
    match outcome {
        Ok(saved) => {
            if let Some(saved) = saved {
                store.saved_preferences = saved.preferences;
                store.saved_hosts = saved.hosts;
            }
            if let Some(encoded) = encoded {
                store.written_state = Some(encoded);
            }
            for (path, revision) in replica_writes {
                store.written_files.remove(&path);
                store.replica_revisions.insert(path, revision);
            }
            for (path, contents) in layout_writes {
                store.written_files.insert(path, contents);
            }
            Ok(())
        }
        Err(error) => {
            // Retry the layouts with the next save; unwritten replicas keep their old revision.
            store.dirty_layouts.extend(layout_keys);
            Err(error)
        }
    }
}

/// Flutter `_reloadSharedHosts`: rows another window/process added to `state.json`.
pub(crate) async fn shared_hosts(core: &Rc<Core>) -> Result<Vec<storage::LoadedHost>, CoreError> {
    let (paths, writable) = {
        let store = core.store.borrow();
        (store.paths.clone(), store.writable)
    };
    if !writable {
        return Ok(Vec::new());
    }
    tokio::task::spawn_blocking(move || {
        let loaded = storage::load(&paths);
        match loaded.error {
            Some(error) => Err(error),
            None => Ok(loaded.hosts),
        }
    })
    .await
    .map_err(|error| CoreError::new("STORAGE_IO", error.to_string(), false))?
}

/// Reads the cached directory of a host that joined after startup.
pub(crate) async fn read_replica(
    core: &Rc<Core>,
    server_id: String,
) -> Result<Option<storage::Replica>, CoreError> {
    let paths = core.store.borrow().paths.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut written = HashMap::new();
        storage::read_replica(&paths, &server_id, &mut written).map(|replica| (replica, written))
    })
    .await
    .map_err(|error| CoreError::new("STORAGE_IO", error.to_string(), false))?;
    let (replica, written) = result?;
    core.store.borrow_mut().written_files.extend(written);
    Ok(replica)
}

/// Records a host's saved row as the merge base after loading it from disk.
pub(crate) fn remember_saved_host(core: &Rc<Core>, config_id: &str, row: Value) {
    core.store
        .borrow_mut()
        .saved_hosts
        .insert(config_id.to_owned(), row);
}

#[cfg(test)]
mod prefs_writer_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn flushed_prefs_seal_exact_values_and_complete_after_write() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let core = Core::new(
                    crate::CoreConfig {
                        state_dir: dir.path().to_owned(),
                        daemon_home: None,
                        default_endpoint: None,
                        tailnet_key_store: None,
                    },
                    Arc::new(crate::events::EventQueue::new(Arc::new(|| {}))),
                );
                core.store.borrow_mut().prefs.writable = true;
                save_state(&core, "prefs:draft".into(), json!("first"), None).await;
                let mut first = queue_prefs(&core, true).unwrap().unwrap();
                assert!(matches!(
                    first.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ));
                assert!(!core.store.borrow().paths.prefs().exists());
                save_state(&core, "prefs:draft".into(), json!("final"), None).await;
                let final_write = queue_prefs(&core, true).unwrap().unwrap();
                {
                    let store = core.store.borrow();
                    assert_eq!(store.prefs.writer.barriers[0].values["draft"], "first");
                    assert_eq!(store.prefs.writer.barriers[1].values["draft"], "final");
                }
                prefs_completed(first).await.unwrap();
                prefs_completed(final_write).await.unwrap();
                let saved = prefs::load(&core.store.borrow().paths);
                assert_eq!(saved.values["draft"], "final");
                assert!(!core.store.borrow().prefs.writer.running);
            })
            .await;
    }
}
