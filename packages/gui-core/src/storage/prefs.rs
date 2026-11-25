//! The Flutter `SharedPreferences` values the native client keeps (`paseo.clientId`,
//! `paseo.chat.drafts.v1`, `paseo.review.*`, `paseo.relayProtected.*`, `mobile.*`).
//!
//! The store is `shared_preferences.json` in the application-support directory, using the
//! `flutter.` key prefix and preserving unrelated plugin values.
use std::collections::VecDeque;
#[cfg(not(target_arch = "wasm32"))]
use std::fs;

use serde_json::{Map, Value};

#[cfg(not(target_arch = "wasm32"))]
use super::io_error;
use super::{Paths, atomic_write, read_file};
use crate::CoreError;

const PREFIX: &str = "flutter.";

#[derive(Default)]
pub(crate) struct Prefs {
    /// Values by Flutter key (without the `flutter.` prefix).
    pub values: Map<String, Value>,
    /// Keys of other plugins in the same file, written back unchanged.
    pub foreign: Map<String, Value>,
    pub error: Option<CoreError>,
    pub writable: bool,
    pub writer: Writer,
}

/// Only the core task touches this queue. A flushed write seals a snapshot so a later
/// overwrite of the same key cannot acknowledge a value that never reached disk.
#[derive(Default)]
pub(crate) struct Writer {
    pub running: bool,
    pub dirty: bool,
    pub barriers: VecDeque<Write>,
}

pub(crate) struct Write {
    pub values: Map<String, Value>,
    pub foreign: Map<String, Value>,
    pub report_error: bool,
    pub completed: Option<tokio::sync::oneshot::Sender<Result<(), CoreError>>>,
}

fn split(file: Map<String, Value>) -> (Map<String, Value>, Map<String, Value>) {
    let mut values = Map::new();
    let mut foreign = Map::new();
    for (key, value) in file {
        match key.strip_prefix(PREFIX) {
            Some(name) => {
                values.insert(name.to_owned(), value);
            }
            None => {
                foreign.insert(key, value);
            }
        }
    }
    (values, foreign)
}

pub(crate) fn load(paths: &Paths) -> Prefs {
    let path = paths.prefs();
    match read_file(&path) {
        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(file)) => {
                let (values, foreign) = split(file);
                Prefs {
                    values,
                    foreign,
                    error: None,
                    writable: true,
                    writer: Writer::default(),
                }
            }
            _ => Prefs {
                error: Some(CoreError::new(
                    "STORAGE_INVALID",
                    "Saved client preferences are unreadable; they will not be overwritten.",
                    false,
                )),
                writable: false,
                ..Prefs::default()
            },
        },
        Ok(None) => Prefs {
            writable: true,
            ..Prefs::default()
        },
        Err(error) => Prefs {
            error: Some(error),
            writable: false,
            ..Prefs::default()
        },
    }
}

pub(crate) fn save(
    paths: &Paths,
    values: &Map<String, Value>,
    foreign: &Map<String, Value>,
) -> Result<(), CoreError> {
    let mut file = foreign.clone();
    for (key, value) in values {
        file.insert(format!("{PREFIX}{key}"), value.clone());
    }
    #[cfg(not(target_arch = "wasm32"))]
    fs::create_dir_all(&paths.app).map_err(|e| io_error("Cannot create", &paths.app, e))?;
    let encoded = serde_json::to_string(&Value::Object(file))
        .map_err(|e| CoreError::new("STORAGE_ENCODE", e.to_string(), false))?;
    atomic_write(&paths.prefs(), &encoded)
}

/// The Flutter key-value convention: objects/arrays are stored as their JSON string.
pub(crate) fn stored_value(value: Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Object(_) | Value::Array(_) => Some(Value::String(value.to_string())),
        scalar => Some(scalar),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flutter_keys_round_trip_and_foreign_keys_survive() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::write(
            paths.prefs(),
            br#"{"flutter.paseo.clientId":"abc","flutter.paseo.chat.drafts.v1":"{}","other.plugin":1}"#,
        )
        .unwrap();
        let mut prefs = load(&paths);
        assert_eq!(prefs.values["paseo.clientId"], "abc");
        prefs.values.insert(
            "paseo.relayProtected.key".into(),
            stored_value(json!(true)).unwrap(),
        );
        prefs.values.insert(
            "paseo.review.x".into(),
            stored_value(json!({"comments":[]})).unwrap(),
        );
        save(&paths, &prefs.values, &prefs.foreign).unwrap();
        let file: Value = serde_json::from_slice(&fs::read(paths.prefs()).unwrap()).unwrap();
        assert_eq!(file["other.plugin"], 1);
        assert_eq!(file["flutter.paseo.relayProtected.key"], true);
        assert_eq!(file["flutter.paseo.review.x"], "{\"comments\":[]}");
        assert_eq!(file["flutter.paseo.chat.drafts.v1"], "{}");
    }

    #[test]
    fn unreadable_preferences_are_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::write(paths.prefs(), b"not json").unwrap();
        let prefs = load(&paths);
        assert!(!prefs.writable && prefs.error.is_some());
    }
}
