//! Mobile platform preferences (push installation id, per-host Expo tokens).
//! The first load imports the Flutter client's `mobile.*` shared_preferences
//! values so existing push subscriptions can still be revoked.

use serde_json::{Map, Value};
use std::{path::PathBuf, sync::Mutex};

pub(crate) struct PlatformPrefs {
    file: PathBuf,
    values: Mutex<Option<Map<String, Value>>>,
}

impl PlatformPrefs {
    pub fn new() -> Self {
        Self {
            file: crate::paths::app_paths().support_dir.join("platform-preferences.json"),
            values: Mutex::new(None),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
        let mut guard = self.values.lock().unwrap();
        let values = guard.get_or_insert_with(|| self.load());
        f(values)
    }

    fn load(&self) -> Map<String, Value> {
        if let Ok(bytes) = std::fs::read(&self.file) {
            if let Ok(Value::Object(map)) = serde_json::from_slice(&bytes) {
                return map;
            }
        }
        crate::legacy_prefs::load_legacy_preferences()
            .into_iter()
            .filter(|(key, _)| key.starts_with("mobile."))
            .collect()
    }

    pub fn get_string(&self, key: &str) -> Option<String> {
        self.with(|values| values.get(key).and_then(Value::as_str).map(str::to_owned))
    }

    /// String values whose key starts with `prefix`.
    pub fn keys_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        self.with(|values| {
            values
                .iter()
                .filter(|(key, _)| key.starts_with(prefix))
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
    }

    /// Blocking write; call from a worker thread.
    pub fn set_string(&self, key: &str, value: &str) -> std::io::Result<()> {
        self.with(|values| {
            values.insert(key.to_owned(), Value::String(value.to_owned()));
            self.save(values)
        })
    }

    /// Blocking write; call from a worker thread.
    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        self.with(|values| {
            if values.remove(key).is_some() {
                self.save(values)
            } else {
                Ok(())
            }
        })
    }

    fn save(&self, values: &Map<String, Value>) -> std::io::Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.file.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec(values)?)?;
        std::fs::rename(&temporary, &self.file)
    }
}
