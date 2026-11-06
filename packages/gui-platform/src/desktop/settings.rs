//! `<support>/desktop-settings.json` (original desktop_settings.dart).

use serde_json::{json, Map, Value};
use std::{path::PathBuf, sync::Mutex};

pub(crate) struct DesktopSettings {
    file: PathBuf,
    document: Mutex<Map<String, Value>>,
    /// Serializes writes so a later patch never lands before an earlier one.
    write: Mutex<()>,
}

fn defaults() -> Map<String, Value> {
    match json!({
        "releaseChannel": "stable",
        "notifications": {"playSound": true},
        "daemon": {"manageBuiltInDaemon": true, "keepRunningAfterQuit": false},
    }) {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

/// Whitelisted merge: only known keys with the right types are accepted.
fn merge(settings: &mut Map<String, Value>, patch: &Map<String, Value>) {
    if let Some(channel @ ("stable" | "beta")) = patch.get("releaseChannel").and_then(Value::as_str) {
        settings.insert("releaseChannel".into(), channel.into());
    }
    for (section, keys) in [
        ("notifications", &["playSound"][..]),
        ("daemon", &["manageBuiltInDaemon", "keepRunningAfterQuit"][..]),
    ] {
        let source = patch.get(section).and_then(Value::as_object);
        let mut target = settings.get(section).and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(source) = source {
            for key in keys {
                if let Some(value @ Value::Bool(_)) = source.get(*key) {
                    target.insert((*key).to_owned(), value.clone());
                }
            }
        }
        settings.insert(section.to_owned(), Value::Object(target));
    }
}

impl DesktopSettings {
    /// Blocking: reads, normalizes and rewrites the document (worker or startup).
    pub fn load() -> anyhow::Result<Self> {
        let file = crate::paths::app_paths().support_dir.join("desktop-settings.json");
        let mut document = match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes)? {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(error.into()),
        };
        let mut settings = defaults();
        if let Some(saved) = document.get("settings").and_then(Value::as_object).cloned() {
            merge(&mut settings, &saved);
        }
        document.insert("version".into(), 1.into());
        document.insert("settings".into(), Value::Object(settings));
        let this = Self { file, document: Mutex::new(document), write: Mutex::new(()) };
        this.save()?;
        Ok(this)
    }

    pub fn value(&self) -> Map<String, Value> {
        self.document
            .lock()
            .unwrap()
            .get("settings")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    fn flag(&self, section: &str, key: &str, default: bool) -> bool {
        self.value()
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(default)
    }

    pub fn managed(&self) -> bool {
        self.flag("daemon", "manageBuiltInDaemon", true)
    }
    pub fn keep_running(&self) -> bool {
        self.flag("daemon", "keepRunningAfterQuit", false)
    }
    pub fn play_sound(&self) -> bool {
        self.flag("notifications", "playSound", true)
    }
    pub fn release_channel(&self) -> String {
        self.value()
            .get("releaseChannel")
            .and_then(Value::as_str)
            .unwrap_or("stable")
            .to_owned()
    }

    /// Blocking.
    pub fn patch(&self, patch: &Map<String, Value>) -> anyhow::Result<Map<String, Value>> {
        {
            let mut document = self.document.lock().unwrap();
            let mut settings = document
                .get("settings")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(defaults);
            merge(&mut settings, patch);
            document.insert("settings".into(), Value::Object(settings));
        }
        self.save()?;
        Ok(self.value())
    }

    fn save(&self) -> anyhow::Result<()> {
        let _guard = self.write.lock().unwrap();
        let bytes = serde_json::to_vec(&*self.document.lock().unwrap())?;
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = PathBuf::from(format!("{}.{}.tmp", self.file.display(), std::process::id()));
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &self.file)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_accepts_only_known_typed_keys() {
        let mut settings = defaults();
        let patch = json!({"releaseChannel":"nightly","daemon":{"keepRunningAfterQuit":true,"manageBuiltInDaemon":"no","other":true},"notifications":{"playSound":false}});
        merge(&mut settings, patch.as_object().unwrap());
        assert_eq!(settings["releaseChannel"], "stable");
        assert_eq!(settings["daemon"], json!({"manageBuiltInDaemon":true,"keepRunningAfterQuit":true}));
        assert_eq!(settings["notifications"], json!({"playSound":false}));
    }
}
