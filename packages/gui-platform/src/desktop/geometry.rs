//! Window geometry `window-<key>.json` (original DesktopHost `_restoreGeometry` /
//! `_saveGeometry`). Files are read once at start; saves are debounced 250 ms
//! and written on a worker.

use crate::platform::{Shared, WindowGeometry};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
pub(crate) struct GeometryStore {
    saved: Mutex<HashMap<String, WindowGeometry>>,
    generations: Mutex<HashMap<String, u64>>,
}

fn file(key: &str) -> PathBuf {
    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    crate::paths::app_paths().support_dir.join(format!("window-{safe}.json"))
}

fn parse(value: &Value) -> Option<WindowGeometry> {
    let width = value.get("width")?.as_f64()?;
    let height = value.get("height")?.as_f64()?;
    // The original window has no minimum size; narrow windows use the compact layout.
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(WindowGeometry {
        x: value.get("x").and_then(Value::as_f64),
        y: value.get("y").and_then(Value::as_f64),
        width,
        height,
        maximized: value.get("maximized") == Some(&Value::Bool(true)),
    })
}

impl GeometryStore {
    /// Blocking: loads every saved window geometry in the support directory.
    pub fn load() -> Self {
        let store = Self::default();
        if let Ok(entries) = std::fs::read_dir(&crate::paths::app_paths().support_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(key) = name.strip_prefix("window-").and_then(|n| n.strip_suffix(".json")) else { continue };
                let parsed = std::fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|value| parse(&value));
                if let Some(geometry) = parsed {
                    store.saved.lock().unwrap().insert(key.to_owned(), geometry);
                }
            }
        }
        store
    }

    pub fn get(&self, key: &str) -> Option<WindowGeometry> {
        self.saved.lock().unwrap().get(key).copied()
    }

    fn remember(&self, key: &str, geometry: WindowGeometry) -> WindowGeometry {
        let mut saved = self.saved.lock().unwrap();
        let next = match (geometry.maximized, saved.get(key)) {
            (true, Some(previous)) => WindowGeometry { maximized: true, ..*previous },
            _ => geometry,
        };
        saved.insert(key.to_owned(), next);
        next
    }

    /// A maximized window keeps its previous normal bounds.
    pub fn record(&self, shared: &Arc<Shared>, key: &str, geometry: WindowGeometry) {
        let geometry = self.remember(key, geometry);
        let generation = {
            let mut generations = self.generations.lock().unwrap();
            let value = generations.entry(key.to_owned()).or_default();
            *value += 1;
            *value
        };
        let key = key.to_owned();
        let shared_for_task = shared.clone();
        shared.spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let current = shared_for_task.desktop.geometry.generations.lock().unwrap().get(&key).copied();
            if current != Some(generation) {
                return;
            }
            let _ = write(&key, geometry);
        });
    }

    /// Blocking: writes everything now (quit / window close).
    pub fn flush(&self) {
        let saved = self.saved.lock().unwrap().clone();
        for (key, geometry) in saved {
            let _ = write(&key, geometry);
        }
    }
}

fn write(key: &str, geometry: WindowGeometry) -> std::io::Result<()> {
    let mut value = json!({"width": geometry.width, "height": geometry.height, "maximized": geometry.maximized});
    if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
        value["x"] = x.into();
        value["y"] = y.into();
    }
    let path = file(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f64, y: f64, width: f64, height: f64, maximized: bool) -> WindowGeometry {
        WindowGeometry { x: Some(x), y: Some(y), width, height, maximized }
    }

    #[test]
    fn maximize_keeps_normal_bounds_until_unzoom() {
        let store = GeometryStore::default();
        let normal = bounds(80.0, 120.0, 900.0, 650.0, false);
        let zoomed = bounds(0.0, 24.0, 1440.0, 876.0, true);
        store.remember("main", normal);
        assert_eq!(store.remember("main", zoomed), WindowGeometry { maximized: true, ..normal });
        // A second zoomed resize (e.g. a display change) must not erode bounds.
        store.remember("main", bounds(1440.0, 0.0, 1920.0, 1080.0, true));
        assert_eq!(store.get("main"), Some(WindowGeometry { maximized: true, ..normal }));
        let restored = bounds(100.0, 140.0, 950.0, 700.0, false);
        assert_eq!(store.remember("main", restored), restored);
        assert_eq!(store.get("main"), Some(restored));
    }

    #[test]
    fn loaded_zoomed_geometry_keeps_normal_bounds_and_windows_are_independent() {
        let store = GeometryStore::default();
        let loaded = parse(&json!({"x": 40, "y": 80, "width": 1000, "height": 720, "maximized": true})).unwrap();
        store.remember("main", loaded);
        let other = bounds(300.0, 200.0, 500.0, 400.0, false);
        store.remember("second", other);
        store.remember("main", bounds(0.0, 0.0, 1920.0, 1080.0, true));
        assert_eq!(store.get("main"), Some(loaded));
        assert_eq!(store.get("second"), Some(other));
    }
}
