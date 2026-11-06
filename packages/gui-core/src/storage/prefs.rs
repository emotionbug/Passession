//! The Flutter `SharedPreferences` values the native client keeps (`paseo.clientId`,
//! `paseo.chat.drafts.v1`, `paseo.review.*`, `paseo.relayProtected.*`, `mobile.*`).
//!
//! The store is `shared_preferences.json` in the application-support directory, the exact
//! file and `flutter.` key prefix `shared_preferences_linux`/`_windows` use, so existing desktop
//! data is read and written in place. On macOS/iOS (NSUserDefaults plist) and Android
//! (`FlutterSharedPreferences.xml`) the file is seeded once from the native store.
use std::{fs, path::Path};

use serde_json::{Map, Value};

use super::{Paths, atomic_write, io_error};
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
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(file)) => {
                let (values, foreign) = split(file);
                Prefs {
                    values,
                    foreign,
                    error: None,
                    writable: true,
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let imported = native::import(&paths.app).unwrap_or_default();
            let (values, foreign) = split(imported);
            let prefs = Prefs {
                values,
                foreign,
                error: None,
                writable: true,
            };
            if !prefs.values.is_empty()
                && let Err(error) = save(paths, &prefs.values, &prefs.foreign) {
                    return Prefs {
                        error: Some(error),
                        ..prefs
                    };
                }
            prefs
        }
        Err(error) => Prefs {
            error: Some(io_error("Cannot read", &path, error)),
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

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
fn prefixed(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .filter(|(key, _)| key.starts_with(PREFIX))
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod native {
    use super::*;

    /// NSUserDefaults of the Flutter app: `Library/Preferences/<bundle id>.plist` next to the
    /// `Library/Application Support[/<bundle id>]` application-support directory.
    pub(super) fn import(app: &Path) -> Option<Map<String, Value>> {
        // iOS returns the container's bare `Application Support`; macOS appends the bundle id.
        let bare = app.file_name().is_some_and(|n| n == "Application Support");
        let support = if bare { app } else { app.parent()? };
        let library = support.parent()?;
        let bundle = if bare {
            "sh.paseo.sessions".to_owned()
        } else {
            app.file_name()?.to_str()?.to_owned()
        };
        let path = library.join("Preferences").join(format!("{bundle}.plist"));
        let bytes = fs::read(path).ok()?;
        match super::super::bplist::parse(&bytes)? {
            Value::Object(map) => Some(super::prefixed(map)),
            _ => None,
        }
    }
}

#[cfg(target_os = "android")]
mod native {
    use super::*;

    /// `shared_preferences_android` legacy store: `<data dir>/shared_prefs/FlutterSharedPreferences.xml`
    /// beside the `files` application-support directory.
    pub(super) fn import(app: &Path) -> Option<Map<String, Value>> {
        let path = app
            .parent()?
            .join("shared_prefs")
            .join("FlutterSharedPreferences.xml");
        let text = fs::read_to_string(path).ok()?;
        Some(super::prefixed(super::super::android_xml::parse(&text)))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
mod native {
    use super::*;

    /// Linux and Windows already use `shared_preferences.json`; nothing to import.
    pub(super) fn import(_app: &Path) -> Option<Map<String, Value>> {
        None
    }
}

#[cfg(test)]
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
