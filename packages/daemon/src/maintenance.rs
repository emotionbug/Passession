use std::{ffi::OsStr, fs, io::ErrorKind, path::Path};
use walkdir::WalkDir;

pub(crate) fn cleanup(home: &Path) {
    let entries = WalkDir::new(home)
        .max_depth(2)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() != 1
                || !entry.file_type().is_dir()
                || entry.file_name() == OsStr::new("agent-timelines")
        });
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, "Cannot inspect retired daemon files");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let retired = match entry.depth() {
            1 => legacy_log_name(entry.file_name()),
            2 if path.extension() == Some(OsStr::new("json")) => {
                // Timeline storage reads only SQLite, but retain unmatched JSON
                // checkpoints rather than deleting the only historical copy.
                let database = path.with_extension("sqlite3");
                match fs::symlink_metadata(&database) {
                    Ok(metadata) => metadata.is_file(),
                    Err(error) => {
                        if error.kind() != ErrorKind::NotFound {
                            tracing::debug!(path = %database.display(), %error, "Cannot inspect timeline database for cleanup");
                        }
                        false
                    }
                }
            }
            _ => false,
        };
        if retired && let Err(error) = fs::remove_file(path) {
            tracing::debug!(path = %path.display(), %error, "Cannot remove retired daemon file");
        }
    }
}

fn legacy_log_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(timestamp) = name.strip_suffix("-daemon.log") else {
        return false;
    };
    timestamp.len() == 16
        && timestamp.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13) {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_retired_checkpoints_and_exact_legacy_logs() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let timelines = home.join("agent-timelines");
        fs::create_dir(&timelines).unwrap();
        let removed = ["agent-timelines/saved.json", "20261117-1200-01-daemon.log"];
        let kept = [
            "agent-timelines/saved.sqlite3",
            "agent-timelines/unmatched.json",
            "daemon.log",
            "daemon.log.1",
            "daemon.log.2",
            "daemon.log.3",
            "20261117-1200-1-daemon.log",
            "20261117-1200-001-daemon.log",
            "20261117-1200-aa-daemon.log",
            "20261117-1200-01-daemon.log.bak",
        ];
        for name in removed.iter().chain(kept.iter()) {
            fs::write(home.join(name), b"retained bytes").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("saved.sqlite3", timelines.join("linked.sqlite3")).unwrap();
            fs::write(timelines.join("linked.json"), b"keep").unwrap();
            std::os::unix::fs::symlink("daemon.log", home.join("20261117-1200-02-daemon.log"))
                .unwrap();
        }
        cleanup(home);
        for name in removed {
            assert!(!home.join(name).exists(), "{name}");
        }
        for name in kept {
            assert_eq!(
                fs::read(home.join(name)).unwrap(),
                b"retained bytes",
                "{name}"
            );
        }
        #[cfg(unix)]
        {
            assert!(timelines.join("linked.json").exists());
            assert!(home.join("20261117-1200-02-daemon.log").is_symlink());
        }
    }
}
