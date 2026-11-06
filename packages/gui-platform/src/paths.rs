//! The Flutter client's storage locations (path_provider), kept so an upgrade
//! finds existing state, desktop settings, geometry and updater identity.

use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// Desktop application identity used by path_provider and the OS integrations.
pub const DESKTOP_APP_ID: &str = "sh.paseo.sessions.desktop";
/// Android package / iOS bundle identifier.
pub const MOBILE_APP_ID: &str = "sh.paseo.sessions";

#[derive(Debug, Clone)]
pub struct AppPaths {
    /// path_provider `getApplicationSupportDirectory()`.
    pub support_dir: PathBuf,
    /// path_provider `getTemporaryDirectory()`.
    pub temp_dir: PathBuf,
    /// Private cache (Android `getCacheDir()`, iOS `Library/Caches`, desktop temp).
    pub cache_dir: PathBuf,
}

static PATHS: LazyLock<AppPaths> = LazyLock::new(|| {
    let mut paths = resolve();
    // `cargo xtask dev app` isolates development runs from the installed app.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Some(dev) = std::env::var_os("PASEO_GUI_STATE_DIR").filter(|v| !v.is_empty()) {
        paths.support_dir = PathBuf::from(dev);
    }
    paths
});

/// The Flutter client's directories for this platform (mobile values come from the OS).
pub fn app_paths() -> &'static AppPaths {
    &PATHS
}

#[cfg(target_os = "linux")]
fn resolve() -> AppPaths {
    // path_provider_linux: $XDG_DATA_HOME/<GApplication id>, falling back to an
    // existing directory named after the executable (the original plugin's layout).
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".local").join("share"));
    let primary = data_home.join(DESKTOP_APP_ID);
    let legacy = data_home.join("paseo_sessions");
    let support_dir = if !primary.is_dir() && legacy.is_dir() { legacy } else { primary };
    let temp_dir = std::env::temp_dir();
    AppPaths { support_dir, cache_dir: temp_dir.clone(), temp_dir }
}

#[cfg(target_os = "macos")]
fn resolve() -> AppPaths {
    // Not sandboxed: NSApplicationSupportDirectory/<bundle identifier>.
    let support_dir = home_dir()
        .join("Library")
        .join("Application Support")
        .join(DESKTOP_APP_ID);
    let temp_dir = std::env::temp_dir();
    let cache_dir = home_dir().join("Library").join("Caches").join(DESKTOP_APP_ID);
    AppPaths { support_dir, temp_dir, cache_dir }
}

#[cfg(windows)]
fn resolve() -> AppPaths {
    // path_provider_windows: FOLDERID_RoamingAppData\<CompanyName>\<ProductName>
    // from the original executable's version resource ("Paseo", "Paseo Sessions").
    let roaming = crate::desktop::windows_util::roaming_app_data()
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"));
    let support_dir = roaming.join("Paseo").join("Paseo Sessions");
    let temp_dir = std::env::temp_dir();
    AppPaths { support_dir, cache_dir: temp_dir.clone(), temp_dir }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn resolve() -> AppPaths {
    crate::native::os_app_paths()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// original native_file_actions.dart `_temporaryFile`: a unique
/// `paseo-files/transfer-*` directory holding the sanitized name.
pub fn transfer_staging_file(name: &str) -> std::io::Result<PathBuf> {
    let root = app_paths().temp_dir.join("paseo-files");
    std::fs::create_dir_all(&root)?;
    let directory = loop {
        let candidate = root.join(format!("transfer-{}", uuid::Uuid::new_v4().simple()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let base = remote_basename(name);
    let safe: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    Ok(directory.join(if safe.is_empty() { "download".to_owned() } else { safe }))
}

/// Removes a staging directory created by [`transfer_staging_file`].
pub fn remove_transfer_staging(file: &Path) -> std::io::Result<()> {
    match file.parent() {
        Some(parent) if parent.file_name().is_some_and(|n| n.to_string_lossy().starts_with("transfer-")) => {
            std::fs::remove_dir_all(parent)
        }
        _ => Ok(()),
    }
}

fn remote_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}
pub(crate) fn mime_for_name(name: &str) -> Option<String> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(
        match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "pdf" => "application/pdf",
            "txt" | "log" => "text/plain",
            "md" => "text/markdown",
            "json" => "application/json",
            "html" | "htm" => "text/html",
            "csv" => "text/csv",
            "zip" => "application/zip",
            _ => return None,
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn remote_basename_uses_last_segment() {
        assert_eq!(super::remote_basename("a/b/c.txt"), "c.txt");
        assert_eq!(super::remote_basename("a\\b\\"), "b");
    }
}

