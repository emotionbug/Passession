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
    written_files: HashMap<PathBuf, String>,
    pub prefs: prefs::Prefs,
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
            prefs: prefs::Prefs::default(),
            saving: false,
            again: false,
            scheduled: false,
        }
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
            store.layouts.insert(window.to_owned(), value);
            store.dirty_layouts.insert(window.to_owned());
        }
        core.touch();
        return save_now(core).await;
    }
    if let Some(name) = key.strip_prefix("prefs:").filter(|name| !name.is_empty()) {
        {
            let mut store = core.store.borrow_mut();
            match prefs::stored_value(value) {
                Some(value) => store.prefs.values.insert(name.to_owned(), value),
                None => store.prefs.values.remove(name),
            };
        }
        core.touch();
        return write_prefs(core).await;
    }
    Err(CoreError::new(
        "INVALID_STATE_KEY",
        format!("Unknown state key {key}"),
        false,
    ))
}

/// Persists `shared_preferences.json` (Flutter writes it on every `set*`).
pub(crate) async fn write_prefs(core: &Rc<Core>) -> Result<(), CoreError> {
    let (paths, values, foreign, writable) = {
        let store = core.store.borrow();
        (
            store.paths.clone(),
            store.prefs.values.clone(),
            store.prefs.foreign.clone(),
            store.prefs.writable,
        )
    };
    if !writable {
        return Err(CoreError::new(
            "STORAGE_READ_ONLY",
            "Saved client preferences are unreadable; changes are kept only in memory.",
            false,
        ));
    }
    tokio::task::spawn_blocking(move || prefs::save(&paths, &values, &foreign))
        .await
        .map_err(|error| CoreError::new("STORAGE_IO", error.to_string(), false))?
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
    let (rows, replicas) = {
        let hosts = core.hosts.borrow();
        let rows: Vec<Value> = hosts.iter().map(|host| host.record_json()).collect();
        let replicas: Vec<(String, String)> = hosts
            .iter()
            .map(|host| (host.server_id(), host.replica_json().to_string()))
            .collect();
        (rows, replicas)
    };
    let (job, encoded, layout_keys) = {
        let mut store = core.store.borrow_mut();
        if !store.writable {
            return Ok(());
        }
        let snapshot = json!({
            "preferences": store.preferences,
            "hosts": rows,
            "suppressedRoutes": store.suppressed,
        });
        let encoded = snapshot.to_string();
        let state_changed = store.written_state.as_deref() != Some(encoded.as_str());
        let mut files = Vec::new();
        for (server_id, contents) in replicas {
            let path = store.paths.replica(&server_id);
            if store.written_files.get(&path) != Some(&contents) {
                files.push((path, contents));
            }
        }
        let layout_keys: Vec<String> = std::mem::take(&mut store.dirty_layouts).into_iter().collect();
        for window in &layout_keys {
            if let Some(layout) = store.layouts.get(window) {
                let path = store.paths.layout(window);
                let contents = layout.to_string();
                if store.written_files.get(&path) != Some(&contents) {
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
        (job, state_changed.then_some(encoded), layout_keys)
    };
    let paths = core.store.borrow().paths.clone();
    let files: Vec<(PathBuf, String)> = job.files.clone();
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
            for (path, contents) in files {
                store.written_files.insert(path, contents);
            }
            Ok(())
        }
        Err(error) => {
            // Retry the layouts with the next save.
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
