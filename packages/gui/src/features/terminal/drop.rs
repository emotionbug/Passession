//! How a dropped file reaches a terminal (original `terminal/drop` and `terminal/file-paste`).
//!
//! `routeTerminalDrop`: a file pastes its own path only when the host is the machine the
//! file sits on. Anywhere else the host cannot read the client's disk, so the bytes upload
//! and the host path is pasted instead.

#![cfg(not(target_arch = "wasm32"))]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use paseo_client::DaemonClient;
use serde_json::{Value, json};

use crate::features::composer::attachments::{FILE_UPLOAD_MAX_BYTES, mime_type_for_path};
use crate::features::composer::ops;

/// The envelope error code of a file over the upload limit; its message is the file name
/// (the pane words it for the user, `TerminalFileTooLargeError`).
pub const FILE_TOO_LARGE: &str = "FILE_TOO_LARGE";

/// `terminal-path.ts` `prepareTerminalPath`: quotes a path for the shell of the machine it
/// belongs to, identified by the path's shape.
pub fn prepare_terminal_path(path: &str) -> String {
    let mut quoted = String::with_capacity(path.len() + 2);
    append_terminal_path(&mut quoted, path);
    quoted
}

fn append_terminal_path(escaped: &mut String, path: &str) {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    if drive || path.starts_with("\\\\") {
        append_cmd_path(escaped, path);
        return;
    }
    // Terminal paste can strip or reinterpret control bytes; spell them out
    // with shell escapes rather than sending the corresponding keystrokes.
    let ansi = bytes.iter().any(u8::is_ascii_control);
    if ansi {
        escaped.push('$');
    }
    escaped.push('\'');
    for c in path.chars() {
        match c {
            '\'' if !ansi => escaped.push_str("'\\''"),
            '\'' | '\\' if ansi => {
                escaped.push('\\');
                escaped.push(c);
            }
            c if c.is_ascii_control() => write!(escaped, "\\{:03o}", c as u8).unwrap(),
            c => escaped.push(c),
        }
    }
    escaped.push('\'');
}

fn append_cmd_path(quoted: &mut String, path: &str) {
    // Windows terminals default to interactive cmd.exe, not batch syntax.
    const META: &str = " \t()&<>|^!;,=";
    let escape = path.contains(['%', '"']);
    if !escape && !path.contains(|c| META.contains(c)) {
        quoted.push_str(path);
        return;
    }

    // With careted outer quotes cmd leaves quote handling to the receiving
    // program's argument parser. Keep carets outside cmd's quote mode: a
    // caret within %NAME% stops the usual interactive environment expansion.
    let delimiter = if escape { "^\"" } else { "\"" };
    quoted.push_str(delimiter);
    let mut slashes = 0;
    let mut after_percent = false;
    for c in path.chars() {
        if c == '"' {
            // Windows argv treats backslashes before a quote as escapes.
            quoted.extend(std::iter::repeat_n('\\', slashes + 1));
        }
        if escape && (after_percent || c == '%' || c == '"' || META.contains(c)) {
            quoted.push('^');
        }
        quoted.push(c);
        slashes = if c == '\\' { slashes + 1 } else { 0 };
        after_percent = c == '%';
    }
    quoted.extend(std::iter::repeat_n('\\', slashes));
    quoted.push_str(delimiter);
}

/// `prepareDroppedPathsForTerminal`: every path of one drop, as one terminal input.
pub fn prepare_dropped_paths(paths: &[PathBuf]) -> String {
    let mut quoted = String::new();
    for (i, path) in paths.iter().enumerate() {
        if i != 0 {
            quoted.push(' ');
        }
        #[cfg(unix)]
        match path.to_str() {
            Some(path) => append_terminal_path(&mut quoted, path),
            None => {
                use std::os::unix::ffi::OsStrExt;

                quoted.push_str("$'");
                for byte in path.as_os_str().as_bytes() {
                    write!(quoted, "\\{byte:03o}").unwrap();
                }
                quoted.push('\'');
            }
        }
        #[cfg(not(unix))]
        append_terminal_path(&mut quoted, &path.to_string_lossy());
    }
    quoted
}

/// `file-paste` `toShellSafeFileName`: the host path is pasted into a shell or TUI; its last
/// segment stays unquotable.
fn shell_safe_file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stores one dropped file on the host (`TerminalFilePasteQueue.paste`): size-checked, then
/// uploaded under a shell-safe name. `{"ok": true, "payload": {"files": [{"path", …}]}}`.
pub async fn upload_dropped_file(
    client: Arc<DaemonClient>,
    path: PathBuf,
) -> anyhow::Result<Value> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let stat_path = path.clone();
    let size = tokio::task::spawn_blocking(move || {
        std::fs::metadata(&stat_path).map(|metadata| metadata.len())
    })
    .await;
    let error = |code: &str, message: String| -> anyhow::Result<Value> {
        Ok(json!({"ok": false, "error": {"code": code, "message": message}}))
    };
    let size = match size {
        Ok(Ok(size)) if size > FILE_UPLOAD_MAX_BYTES => return error(FILE_TOO_LARGE, name),
        Ok(Ok(size)) => size,
        Ok(Err(io)) => return error("LOCAL_ERROR", format!("{name}: {io}")),
        Err(join) => return error("LOCAL_ERROR", join.to_string()),
    };
    let mime = mime_type_for_path(&name).to_owned();
    ops::upload_files(
        client,
        vec![ops::UploadFile::from_path(
            path,
            shell_safe_file_name(&name),
            mime,
            size,
        )],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_paths_keep_shell_metacharacters_and_unicode() {
        for (path, quoted) in [
            ("a#b", "'a#b'"),
            ("it's", "'it'\\''s'"),
            ("$HOME", "'$HOME'"),
            ("/tmp/my image.png", "'/tmp/my image.png'"),
            ("*.png", "'*.png'"),
            ("/tmp/a'b\"c.png", "'/tmp/a'\\''b\"c.png'"),
            ("a\\b`$|&>~#!^*;<", "'a\\b`$|&>~#!^*;<'"),
            ("/home/u/한글 파일 이름.txt", "'/home/u/한글 파일 이름.txt'"),
        ] {
            assert_eq!(prepare_terminal_path(path), quoted);
        }
    }

    #[test]
    fn posix_path_controls_are_encoded_as_printable_shell_input() {
        assert_eq!(
            prepare_terminal_path("a\nb\tc\\'\x1b"),
            "$'a\\012b\\011c\\\\\\'\\033'"
        );
    }

    #[test]
    fn cmd_paths_keep_drive_and_unc_prefixes() {
        for (path, quoted) in [
            (r"C:\Users\me\photo.png", r"C:\Users\me\photo.png"),
            (
                r"C:\Users\me\photo one.png",
                r#""C:\Users\me\photo one.png""#,
            ),
            (r"C:\photo & music\", r#""C:\photo & music\\""#),
            (
                r"\\server\share\photo one.png",
                r#""\\server\share\photo one.png""#,
            ),
            (
                r"C:\%PATH%\photo one.png",
                r#"^"C:\^%^PATH^%^\photo^ one.png^""#,
            ),
            (
                r"\\server\share\100%.png",
                r#"^"\\server\share\100^%^.png^""#,
            ),
            (r#"C:/a"b.png"#, r#"^"C:/a\^"b.png^""#),
        ] {
            assert_eq!(prepare_terminal_path(path), quoted);
        }
    }

    #[test]
    fn one_drop_is_one_terminal_input() {
        let paths = [PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b c.png")];
        assert_eq!(prepare_dropped_paths(&paths), "'/tmp/a.png' '/tmp/b c.png'");
    }

    #[test]
    #[cfg(unix)]
    fn dropped_posix_path_keeps_non_utf8_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = PathBuf::from(OsStr::from_bytes(b"/a\xff"));
        assert_eq!(prepare_dropped_paths(&[path]), "$'\\057\\141\\377'");
    }

    #[test]
    fn uploaded_names_need_no_quoting() {
        assert_eq!(shell_safe_file_name("한글 파일 (1).txt"), "_______1_.txt");
        assert_eq!(
            shell_safe_file_name("notes-v2_final.md"),
            "notes-v2_final.md"
        );
    }
}
