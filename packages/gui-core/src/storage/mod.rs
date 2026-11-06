//! Flutter-compatible client state under the application-support directory
//! (`app_store.dart` `_initialize`, `savePreferences`, `_mergeState`, replicas and per-window
//! layouts). Everything here is blocking and runs on Tokio's blocking pool.
#[cfg(any(target_os = "android", test))]
mod android_xml;
#[cfg(any(target_os = "macos", target_os = "ios", test))]
mod bplist;
pub(crate) mod prefs;

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};

use crate::{CoreError, ids::encode_component};

pub(crate) const MAIN_WINDOW: &str = "main";

#[derive(Clone)]
pub(crate) struct Paths {
    pub app: PathBuf,
    pub dir: PathBuf,
}

impl Paths {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            app: state_dir.to_path_buf(),
            dir: state_dir.join("paseo-native"),
        }
    }
    fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    fn lock(&self) -> PathBuf {
        self.dir.join("state.lock")
    }
    pub fn replica(&self, server_id: &str) -> PathBuf {
        self.dir.join(format!(
            "replica-{}.json",
            URL_SAFE_NO_PAD.encode(server_id.as_bytes())
        ))
    }
    pub fn layout(&self, window: &str) -> PathBuf {
        self.dir
            .join(format!("layout-{}.json", encode_component(window)))
    }
    pub fn prefs(&self) -> PathBuf {
        self.app.join("shared_preferences.json")
    }
}

pub(crate) fn io_error(action: &str, path: &Path, error: std::io::Error) -> CoreError {
    CoreError::new(
        "STORAGE_IO",
        format!("{action} {}: {error}", path.display()),
        false,
    )
}

pub(crate) struct Replica {
    pub projects: Vec<Value>,
    pub workspaces: Vec<Value>,
    pub agents: Vec<Value>,
    pub cached_at: Option<String>,
}

pub(crate) struct LoadedHost {
    pub server_id: String,
    pub origin_server_id: Option<String>,
    pub config: Value,
    pub connections: Vec<Value>,
    pub replica: Option<Replica>,
    pub replica_error: Option<CoreError>,
}

#[derive(Default)]
pub(crate) struct Loaded {
    pub retained: Map<String, Value>,
    pub preferences: Map<String, Value>,
    pub saved_hosts: Map<String, Value>,
    pub hosts: Vec<LoadedHost>,
    pub layouts: BTreeMap<String, Value>,
    pub suppressed_routes: Vec<String>,
    pub error: Option<CoreError>,
    pub writable: bool,
    pub written_files: HashMap<PathBuf, String>,
}

/// Restricts private state to its owner (Flutter `_restrict`: Linux and macOS only; mobile
/// sandboxes and Windows profile ACLs already do).
pub(crate) fn restrict(path: &Path, directory: bool) -> Result<(), CoreError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            CoreError::new(
                "STORAGE_PERMISSIONS",
                format!("Cannot protect private app state {}: {error}", path.display()),
                false,
            )
        })?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (path, directory);
    Ok(())
}

fn structure_error() -> CoreError {
    CoreError::new(
        "STORAGE_INVALID",
        "App state has an invalid structure",
        false,
    )
}

/// Flutter `_readStateFile`: absent is empty; an invalid document is an error, never defaults.
fn read_state(path: &Path) -> Result<Map<String, Value>, CoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(io_error("Cannot read", path, error)),
    };
    let decoded: Value = serde_json::from_slice(&bytes).map_err(|error| {
        CoreError::new(
            "STORAGE_INVALID",
            format!("App state is not valid JSON: {error}"),
            false,
        )
    })?;
    let Value::Object(map) = decoded else {
        return Err(structure_error());
    };
    let wrong = |key: &str, ok: fn(&Value) -> bool| map.get(key).is_some_and(|v| !ok(v));
    if wrong("preferences", Value::is_object)
        || wrong("hosts", Value::is_array)
        || wrong("layout", Value::is_object)
    {
        return Err(structure_error());
    }
    Ok(map)
}

fn version(map: &Map<String, Value>) -> i64 {
    map.get("version")
        .and_then(Value::as_f64)
        .map(|v| v as i64)
        .unwrap_or(0)
}

fn object(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn array(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Flutter `_hostRecords`: rows keyed by their active config id (legacy flat rows by `id`).
pub(crate) fn host_records(rows: &[Value]) -> Map<String, Value> {
    let mut records = Map::new();
    for row in rows {
        let id = row["config"]["id"]
            .as_str()
            .or_else(|| row["id"].as_str())
            .unwrap_or_default();
        records.insert(id.to_owned(), row.clone());
    }
    records
}

/// Flutter `HostConfig.fromJson` requires string `id`, `label` and `endpoint`.
pub(crate) fn valid_config(config: &Value) -> bool {
    config["id"].is_string() && config["label"].is_string() && config["endpoint"].is_string()
}

pub(crate) fn read_replica(
    paths: &Paths,
    server_id: &str,
    written: &mut HashMap<PathBuf, String>,
) -> Result<Option<Replica>, CoreError> {
    let path = paths.replica(server_id);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("Cannot read", &path, error)),
    };
    let data: Value = serde_json::from_str(&contents).map_err(|error| {
        CoreError::new(
            "STORAGE_INVALID",
            format!("Host cache is not valid JSON: {error}"),
            false,
        )
    })?;
    if data["serverId"] != server_id {
        return Ok(None);
    }
    let rows = |key: &str| array(data.get(key)).into_iter().filter(Value::is_object).collect();
    let replica = Replica {
        projects: rows("projects"),
        workspaces: rows("workspaces"),
        agents: rows("agents"),
        cached_at: data["cachedAt"]
            .as_str()
            .filter(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok() || chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S%.f").is_ok())
            .map(str::to_owned),
    };
    written.insert(path, contents);
    Ok(Some(replica))
}

fn load_layouts(paths: &Paths, loaded: &mut Loaded) {
    let Ok(entries) = fs::read_dir(&paths.dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(encoded) = name
            .strip_prefix("layout-")
            .and_then(|rest| rest.strip_suffix(".json"))
        else {
            continue;
        };
        let Ok(window) = percent_encoding::percent_decode_str(encoded).decode_utf8() else {
            continue;
        };
        let Ok(contents) = fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Ok(layout @ Value::Object(_)) = serde_json::from_str::<Value>(&contents) {
            loaded.written_files.insert(entry.path(), contents);
            loaded.layouts.insert(window.into_owned(), layout);
        }
    }
}

pub(crate) fn load(paths: &Paths) -> Loaded {
    let mut loaded = Loaded {
        writable: true,
        ..Loaded::default()
    };
    let result = (|| -> Result<(), CoreError> {
        fs::create_dir_all(&paths.dir).map_err(|e| io_error("Cannot create", &paths.dir, e))?;
        restrict(&paths.dir, true)?;
        let retained = read_state(&paths.state())?;
        if version(&retained) > 1 {
            return Err(CoreError::new(
                "STORAGE_NEWER",
                "App state belongs to a newer Paseo version; it will not be overwritten.",
                false,
            ));
        }
        loaded.retained = retained;
        if loaded.retained.is_empty() {
            load_layouts(paths, &mut loaded);
            return Ok(());
        }
        loaded.preferences = object(loaded.retained.get("preferences"));
        let rows = array(loaded.retained.get("hosts"));
        loaded.saved_hosts = host_records(&rows);
        for row in rows {
            let config = if row["config"].is_object() {
                row["config"].clone()
            } else {
                row.clone()
            };
            if !valid_config(&config) {
                return Err(structure_error());
            }
            let server_id = row["serverId"]
                .as_str()
                .or_else(|| config["expectedServerId"].as_str())
                .or_else(|| config["id"].as_str())
                .unwrap_or_default()
                .to_owned();
            let mut connections = vec![config.clone()];
            for candidate in array(row.get("connections")) {
                if valid_config(&candidate)
                    && !connections.iter().any(|c| c["id"] == candidate["id"])
                {
                    connections.push(candidate);
                }
            }
            let (replica, replica_error) =
                match read_replica(paths, &server_id, &mut loaded.written_files) {
                    Ok(replica) => (replica, None),
                    Err(error) => (None, Some(error)),
                };
            loaded.hosts.push(LoadedHost {
                server_id,
                origin_server_id: row["originServerId"].as_str().map(str::to_owned),
                config,
                connections,
                replica,
                replica_error,
            });
        }
        load_layouts(paths, &mut loaded);
        if !loaded.layouts.contains_key(MAIN_WINDOW)
            && let Some(layout @ Value::Object(_)) = loaded.retained.get("layout") {
                loaded.layouts.insert(MAIN_WINDOW.to_owned(), layout.clone());
            }
        loaded.suppressed_routes = array(loaded.retained.get("suppressedRoutes"))
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        Ok(())
    })();
    if let Err(error) = result {
        // Preserve corrupt/unreadable state for recovery instead of replacing it.
        loaded.error = Some(error);
        loaded.writable = false;
    }
    loaded
}

/// Flutter `_mergeChanges`: applies this store's edits (`before` → `next`) onto `latest`.
pub(crate) fn merge_changes(
    latest: &Map<String, Value>,
    before: &Map<String, Value>,
    next: &Map<String, Value>,
) -> Map<String, Value> {
    let mut result = latest.clone();
    let keys: Vec<&String> = before
        .keys()
        .chain(next.keys().filter(|key| !before.contains_key(*key)))
        .collect();
    for key in keys {
        if before.get(key) == next.get(key) {
            continue;
        }
        match (next.get(key), before.get(key), result.get(key)) {
            (None, _, _) => {
                result.remove(key);
            }
            (Some(Value::Object(n)), Some(Value::Object(b)), Some(Value::Object(r))) => {
                let merged = merge_changes(r, b, n);
                result.insert(key.clone(), Value::Object(merged));
            }
            (Some(value), _, _) => {
                result.insert(key.clone(), value.clone());
            }
        }
    }
    result
}

/// Writes through a same-directory temporary file and rename.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<(), CoreError> {
    let temporary = PathBuf::from(format!(
        "{}.{}.tmp",
        path.display(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<(), CoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| io_error("Cannot create", &temporary, e))?;
        restrict(&temporary, false)?;
        file.write_all(contents.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| io_error("Cannot write", &temporary, e))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|e| io_error("Cannot replace", path, e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) struct StateSnapshot {
    pub preferences: Map<String, Value>,
    pub hosts: Vec<Value>,
    pub suppressed_routes: Vec<String>,
}

pub(crate) struct SaveJob {
    pub state: Option<StateSnapshot>,
    pub saved_preferences: Map<String, Value>,
    pub saved_hosts: Map<String, Value>,
    pub files: Vec<(PathBuf, String)>,
}

pub(crate) struct Saved {
    pub preferences: Map<String, Value>,
    pub hosts: Map<String, Value>,
}

/// Rust 1.93's File::lock is unsupported on Android. Use the same flock primitive on
/// Unix, holding the descriptor through the transaction; close releases it even on errors.
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: the borrowed File keeps this descriptor open for the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(not(unix))]
    file.lock()
}

/// One locked save (Flutter `savePreferences` after change detection): merge `state.json`, then
/// write each changed file.
pub(crate) fn save(paths: &Paths, job: SaveJob) -> Result<Option<Saved>, CoreError> {
    let lock_path = paths.lock();
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(|e| io_error("Cannot open", &lock_path, e))?;
    restrict(&lock_path, false)?;
    lock_exclusive(&lock).map_err(|e| io_error("Cannot lock", &lock_path, e))?;
    let saved = match job.state {
        Some(snapshot) => Some(merge_state(
            paths,
            snapshot,
            &job.saved_preferences,
            &job.saved_hosts,
        )?),
        None => None,
    };
    for (path, contents) in &job.files {
        atomic_write(path, contents)?;
    }
    Ok(saved)
}

fn merge_state(
    paths: &Paths,
    snapshot: StateSnapshot,
    saved_preferences: &Map<String, Value>,
    saved_hosts: &Map<String, Value>,
) -> Result<Saved, CoreError> {
    let latest = read_state(&paths.state())?;
    if version(&latest) > 1 {
        return Err(CoreError::new(
            "STORAGE_NEWER",
            "A newer client owns the saved state.",
            false,
        ));
    }
    let next_hosts = host_records(&snapshot.hosts);
    let merged_preferences = merge_changes(
        &object(latest.get("preferences")),
        saved_preferences,
        &snapshot.preferences,
    );
    let merged_hosts = merge_changes(
        &host_records(&array(latest.get("hosts"))),
        saved_hosts,
        &next_hosts,
    );
    let mut routes: Vec<String> = array(latest.get("suppressedRoutes"))
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    for route in snapshot.suppressed_routes {
        if !routes.contains(&route) {
            routes.push(route);
        }
    }
    let mut merged = latest;
    merged.insert("version".into(), json!(1));
    merged.insert("preferences".into(), Value::Object(merged_preferences));
    merged.insert(
        "hosts".into(),
        Value::Array(merged_hosts.into_iter().map(|(_, row)| row).collect()),
    );
    merged.insert("suppressedRoutes".into(), json!(routes));
    let encoded = serde_json::to_string(&Value::Object(merged))
        .map_err(|e| CoreError::new("STORAGE_ENCODE", e.to_string(), false))?;
    atomic_write(&paths.state(), &encoded)?;
    Ok(Saved {
        preferences: snapshot.preferences,
        hosts: next_hosts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn try_lock_exclusive(file: &File) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: the borrowed File owns a live descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_lock_excludes_other_handles_until_owner_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.lock");
        let owner = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let other = OpenOptions::new().append(true).open(&path).unwrap();
        lock_exclusive(&owner).unwrap();
        assert_eq!(
            try_lock_exclusive(&other).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(owner);
        try_lock_exclusive(&other).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_save_releases_lock_without_overwriting_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::create_dir_all(&paths.dir).unwrap();
        let original = br#"{"version":2,"preferences":{"future":true}}"#;
        fs::write(paths.state(), original).unwrap();
        let error = save(
            &paths,
            SaveJob {
                state: Some(StateSnapshot {
                    preferences: Map::new(),
                    hosts: Vec::new(),
                    suppressed_routes: Vec::new(),
                }),
                saved_preferences: Map::new(),
                saved_hosts: Map::new(),
                files: Vec::new(),
            },
        ).err().unwrap();
        assert_eq!(error.code, "STORAGE_NEWER");
        assert_eq!(fs::read(paths.state()).unwrap(), original);
        let next = OpenOptions::new().append(true).open(paths.lock()).unwrap();
        try_lock_exclusive(&next).unwrap();
    }

    #[test]
    fn merge_keeps_concurrent_edits_and_applies_ours() {
        let latest = json!({"a":1,"b":{"x":1,"y":2},"other":true});
        let before = json!({"a":1,"b":{"x":1,"y":1}});
        let next = json!({"b":{"x":1,"y":3},"c":4});
        let merged = merge_changes(
            latest.as_object().unwrap(),
            before.as_object().unwrap(),
            next.as_object().unwrap(),
        );
        assert_eq!(
            Value::Object(merged),
            json!({"b":{"x":1,"y":3},"other":true,"c":4})
        );
    }

    #[test]
    fn corrupt_or_newer_state_is_reported_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(paths.state(), b"{partial").unwrap();
        let loaded = load(&paths);
        assert!(loaded.error.is_some() && !loaded.writable);
        assert_eq!(fs::read(paths.state()).unwrap(), b"{partial");
        fs::write(paths.state(), br#"{"version":2}"#).unwrap();
        let loaded = load(&paths);
        assert_eq!(loaded.error.unwrap().code, "STORAGE_NEWER");
    }

    #[test]
    fn saved_state_round_trips_through_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(
            paths.state(),
            br#"{"version":1,"preferences":{"theme":"dark","future":{"k":1}},"hosts":[],"unknownRoot":7}"#,
        )
        .unwrap();
        let loaded = load(&paths);
        assert!(loaded.error.is_none());
        let mut preferences = loaded.preferences.clone();
        preferences.insert("language".into(), json!("ko"));
        let config = json!({"id":"c1","label":"Box","endpoint":"localhost:6767","extra":"kept"});
        save(
            &paths,
            SaveJob {
                state: Some(StateSnapshot {
                    preferences,
                    hosts: vec![json!({"serverId":"srv","originServerId":null,"config":config,"connections":[config]})],
                    suppressed_routes: vec!["a/b".into()],
                }),
                saved_preferences: loaded.preferences.clone(),
                saved_hosts: loaded.saved_hosts.clone(),
                files: vec![(paths.layout(MAIN_WINDOW), "{\"version\":2}".into())],
            },
        )
        .unwrap();
        let reloaded = load(&paths);
        assert_eq!(reloaded.preferences["future"], json!({"k":1}));
        assert_eq!(reloaded.preferences["language"], "ko");
        assert_eq!(reloaded.retained["unknownRoot"], 7);
        assert_eq!(reloaded.hosts[0].config["extra"], "kept");
        assert_eq!(reloaded.layouts[MAIN_WINDOW], json!({"version":2}));
        assert_eq!(reloaded.suppressed_routes, vec!["a/b".to_owned()]);
    }
}
