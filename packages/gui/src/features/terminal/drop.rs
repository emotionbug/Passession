//! How a dropped file reaches a terminal (original `terminal/drop` and `terminal/file-paste`).
//!
//! `routeTerminalDrop`: a file pastes its own path only when the host is the machine the
//! file sits on. Anywhere else the host cannot read the client's disk, so the bytes upload
//! and the host path is pasted instead.

use std::path::PathBuf;
use std::sync::Arc;

use paseo_client::DaemonClient;
use serde_json::{json, Value};

use crate::features::composer::attachments::{mime_type_for_path, FILE_UPLOAD_MAX_BYTES};
use crate::features::composer::ops;

/// The envelope error code of a file over the upload limit; its message is the file name
/// (the pane words it for the user, `TerminalFileTooLargeError`).
pub const FILE_TOO_LARGE: &str = "FILE_TOO_LARGE";

/// `terminal-path.ts` `prepareTerminalPath`: quotes a path for the shell of the machine it
/// belongs to, identified by the path's shape.
pub fn prepare_terminal_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    if drive || path.starts_with("\\\\") {
        return if path.contains(' ') { format!("\"{path}\"") } else { path.to_owned() };
    }
    let mut escaped = String::with_capacity(path.len() + 2);
    for c in path.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '`' | '$' | '|' | '&' | '>' | '~' | '#' | '!' | '^' | '*' | ';' | '<' => {}
            '\'' => escaped.push_str("\\'"),
            c => escaped.push(c),
        }
    }
    if path.contains('\'') && path.contains('"') { format!("$'{escaped}'") } else { format!("'{escaped}'") }
}

/// `prepareDroppedPathsForTerminal`: every path of one drop, as one terminal input.
pub fn prepare_dropped_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|path| prepare_terminal_path(&path.to_string_lossy())).collect::<Vec<_>>().join(" ")
}

/// `file-paste` `toShellSafeFileName`: the host path is pasted into a shell or TUI; its last
/// segment stays unquotable.
fn shell_safe_file_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '_' }).collect()
}

/// Stores one dropped file on the host (`TerminalFilePasteQueue.paste`): size-checked, then
/// uploaded under a shell-safe name. `{"ok": true, "payload": {"files": [{"path", …}]}}`.
pub async fn upload_dropped_file(client: Arc<DaemonClient>, path: PathBuf) -> anyhow::Result<Value> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned());
    let stat_path = path.clone();
    let size = tokio::task::spawn_blocking(move || std::fs::metadata(&stat_path).map(|metadata| metadata.len())).await;
    let error = |code: &str, message: String| -> anyhow::Result<Value> { Ok(json!({"ok": false, "error": {"code": code, "message": message}})) };
    match size {
        Ok(Ok(size)) if size > FILE_UPLOAD_MAX_BYTES => return error(FILE_TOO_LARGE, name),
        Ok(Ok(_)) => {}
        Ok(Err(io)) => return error("LOCAL_ERROR", format!("{name}: {io}")),
        Err(join) => return error("LOCAL_ERROR", join.to_string()),
    }
    let mime = mime_type_for_path(&name).to_owned();
    ops::upload_files(client, vec![(path, shell_safe_file_name(&name), mime)]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // terminal-path.test.ts
    #[test]
    fn posix_paths_are_quoted_with_conservative_escaping() {
        assert_eq!(prepare_terminal_path("/tmp/my image.png"), "'/tmp/my image.png'");
        assert_eq!(prepare_terminal_path("/tmp/a$(touch bad).png"), "'/tmp/a(touch bad).png'");
        assert_eq!(prepare_terminal_path("/tmp/it's.png"), "'/tmp/it\\'s.png'");
        assert_eq!(prepare_terminal_path("/tmp/a'b\"c.png"), "$'/tmp/a\\'b\"c.png'");
        assert_eq!(prepare_terminal_path("/home/u/한글 파일 이름.txt"), "'/home/u/한글 파일 이름.txt'");
    }

    #[test]
    fn windows_paths_are_quoted_only_for_spaces() {
        assert_eq!(prepare_terminal_path("C:\\Users\\me\\photo.png"), "C:\\Users\\me\\photo.png");
        assert_eq!(prepare_terminal_path("C:\\Users\\me\\photo one.png"), "\"C:\\Users\\me\\photo one.png\"");
        assert_eq!(prepare_terminal_path("\\\\server\\share\\photo one.png"), "\"\\\\server\\share\\photo one.png\"");
    }

    #[test]
    fn one_drop_is_one_terminal_input() {
        let paths = [PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b c.png")];
        assert_eq!(prepare_dropped_paths(&paths), "'/tmp/a.png' '/tmp/b c.png'");
    }

    #[test]
    fn uploaded_names_need_no_quoting() {
        assert_eq!(shell_safe_file_name("한글 파일 (1).txt"), "_______1_.txt");
        assert_eq!(shell_safe_file_name("notes-v2_final.md"), "notes-v2_final.md");
    }
}
