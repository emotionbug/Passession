//! Remote path helpers (Flutter `remote_path.dart` and the explorer path helpers of
//! `remote_directory_browser.dart`). The host's path style can differ from the client's OS, so
//! the style is chosen from the cwd: a drive letter or UNC prefix is Windows, everything else is
//! POSIX. Explorer entry paths are always `/`-separated and relative to the listed cwd.

/// The path style of a remote cwd or path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Style {
    Posix,
    Windows,
}

impl Style {
    pub fn of(path: &str) -> Style {
        let bytes = path.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if drive || path.starts_with("\\\\") {
            Style::Windows
        } else {
            Style::Posix
        }
    }

    fn is_separator(self, c: char) -> bool {
        match self {
            Style::Posix => c == '/',
            Style::Windows => c == '/' || c == '\\',
        }
    }

    fn separator(self) -> char {
        match self {
            Style::Posix => '/',
            Style::Windows => '\\',
        }
    }

    /// The root prefix of an absolute path (`/`, `C:\`, `\\server\share\`), or `None`.
    fn root(self, path: &str) -> Option<String> {
        match self {
            Style::Posix => path.starts_with('/').then(|| "/".to_owned()),
            Style::Windows => {
                let bytes = path.as_bytes();
                if path.starts_with("\\\\") || path.starts_with("//") {
                    // \\server\share\ is the root of a UNC path.
                    let rest = &path[2..];
                    let mut parts = rest.splitn(3, |c| self.is_separator(c));
                    let server = parts.next().unwrap_or("");
                    let share = parts.next().unwrap_or("");
                    if share.is_empty() {
                        return Some(format!("\\\\{server}"));
                    }
                    return Some(format!("\\\\{server}\\{share}\\"));
                }
                if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                    if bytes.len() >= 3 && self.is_separator(bytes[2] as char) {
                        return Some(format!("{}\\", &path[..2]));
                    }
                    // `C:relative` is drive-relative, not absolute.
                    return None;
                }
                if path.starts_with('\\') || path.starts_with('/') {
                    return Some("\\".to_owned());
                }
                None
            }
        }
    }

    pub fn is_absolute(self, path: &str) -> bool {
        self.root(path).is_some()
    }

    /// Splits a path into its root (if absolute) and its non-empty segments.
    fn split(self, path: &str) -> (Option<String>, Vec<String>) {
        let root = self.root(path);
        let body = match &root {
            Some(root) => {
                // Skip the root's characters (the UNC root normalizes separators).
                let mut consumed = 0;
                let root_chars: Vec<char> = root.chars().collect();
                for (matched, c) in path.chars().enumerate() {
                    if matched >= root_chars.len() {
                        break;
                    }
                    let expected = root_chars[matched];
                    let same = c == expected || (self.is_separator(c) && self.is_separator(expected));
                    if !same {
                        break;
                    }
                    consumed += c.len_utf8();
                }
                &path[consumed..]
            }
            None => path,
        };
        let segments = body
            .split(|c| self.is_separator(c))
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        (root, segments)
    }

    fn join_parts(self, root: Option<&str>, segments: &[String]) -> String {
        let separator = self.separator().to_string();
        let body = segments.join(&separator);
        match root {
            Some(root) => format!("{root}{body}"),
            None if body.is_empty() => ".".to_owned(),
            None => body,
        }
    }

    /// `package:path` `normalize`: resolves `.` and `..` (keeping leading `..` of relative paths).
    pub fn normalize(self, path: &str) -> String {
        let (root, segments) = self.split(path);
        let mut out: Vec<String> = Vec::new();
        for segment in segments {
            match segment.as_str() {
                "." => {}
                ".." => {
                    if out.last().is_some_and(|last| last != "..") {
                        out.pop();
                    } else if root.is_none() {
                        out.push(segment);
                    }
                }
                _ => out.push(segment),
            }
        }
        self.join_parts(root.as_deref(), &out)
    }

    pub fn join(self, base: &str, path: &str) -> String {
        if self.is_absolute(path) || base.is_empty() {
            return path.to_owned();
        }
        let mut joined = base.to_owned();
        if !joined.ends_with(|c| self.is_separator(c)) {
            joined.push(self.separator());
        }
        joined.push_str(path);
        joined
    }

    pub fn basename(self, path: &str) -> String {
        let trimmed = path.trim_end_matches(|c| self.is_separator(c));
        if trimmed.is_empty() {
            return if path.is_empty() { String::new() } else { self.separator().to_string() };
        }
        match trimmed.rfind(|c| self.is_separator(c)) {
            Some(index) => trimmed[index + 1..].to_owned(),
            None => trimmed.to_owned(),
        }
    }

    pub fn dirname(self, path: &str) -> String {
        let (root, mut segments) = self.split(path);
        if segments.is_empty() {
            return root.unwrap_or_else(|| ".".to_owned());
        }
        segments.pop();
        self.join_parts(root.as_deref(), &segments)
    }

    /// `package:path` `relative(path, from:)` for two absolute paths.
    pub fn relative(self, path: &str, from: &str) -> String {
        let (path_root, path_segments) = self.split(&self.normalize(path));
        let (from_root, from_segments) = self.split(&self.normalize(from));
        let same_root = match (&path_root, &from_root) {
            (Some(a), Some(b)) => match self {
                Style::Windows => a.eq_ignore_ascii_case(b),
                Style::Posix => a == b,
            },
            (None, None) => true,
            _ => false,
        };
        if !same_root {
            return self.normalize(path);
        }
        let equal = |a: &String, b: &String| match self {
            Style::Windows => a.eq_ignore_ascii_case(b),
            Style::Posix => a == b,
        };
        let mut common = 0;
        while common < path_segments.len()
            && common < from_segments.len()
            && equal(&path_segments[common], &from_segments[common])
        {
            common += 1;
        }
        let mut out: Vec<String> = Vec::new();
        for _ in common..from_segments.len() {
            out.push("..".to_owned());
        }
        out.extend(path_segments[common..].iter().cloned());
        if out.is_empty() {
            ".".to_owned()
        } else {
            out.join(&self.separator().to_string())
        }
    }
}

/// Flutter `remoteBasename(path, cwd:)`.
pub fn remote_basename(path: &str, cwd: Option<&str>) -> String {
    Style::of(cwd.unwrap_or(path)).basename(path)
}

/// Flutter `remoteParent(path)`.
pub fn remote_parent(path: &str) -> String {
    Style::of(path).dirname(path)
}

/// Flutter `remoteAbsolute(cwd, path)`.
pub fn remote_absolute(cwd: &str, path: &str) -> String {
    let style = Style::of(cwd);
    if style.is_absolute(path) {
        style.normalize(path)
    } else {
        style.normalize(&style.join(cwd, path))
    }
}

/// Flutter `remoteRelative(cwd, path)`.
pub fn remote_relative(cwd: &str, path: &str) -> String {
    let style = Style::of(cwd);
    if style.is_absolute(path) {
        style.relative(path, cwd)
    } else {
        style.normalize(path)
    }
}

/// Whether `cwd` is a Windows-style host path (drive letter or UNC).
pub fn is_windows_path(cwd: &str) -> bool {
    Style::of(cwd) == Style::Windows
}

/// Original `isHiddenExplorerPath`: any segment other than `.`/`..` starting with a dot.
pub fn is_hidden_explorer_path(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment != "." && segment != ".." && segment.starts_with('.'))
}

/// Original `isExplorerPathWithin`.
pub fn is_explorer_path_within(candidate: &str, parent: &str) -> bool {
    candidate == parent
        || (candidate.len() > parent.len()
            && candidate.starts_with(parent)
            && candidate.as_bytes()[parent.len()] == b'/')
}

/// Original `parentExplorerPath`.
pub fn parent_explorer_path(entry_path: &str) -> String {
    match entry_path.rfind('/') {
        Some(index) if index > 0 => entry_path[..index].to_owned(),
        _ => ".".to_owned(),
    }
}

/// The client-side file name for a downloaded copy: the remote basename with characters invalid
/// on common file systems replaced (Flutter `_temporaryFile`).
pub fn safe_local_name(name: &str) -> String {
    let base = remote_basename(name, None);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "download".to_owned()
    } else {
        cleaned
    }
}

/// A file location carried by a file target (`line`, `lineEnd`, `column`), 1-based.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileLocation {
    pub line: usize,
    pub line_end: Option<usize>,
    pub column: Option<usize>,
}

/// Splits an optional `#L<line>[-<lineEnd>][:<column>]` suffix off a route path (the convention
/// for `Route::Files { path }` file links). A path without a valid suffix is returned unchanged.
pub fn parse_file_location(path: &str) -> (String, Option<FileLocation>) {
    let Some(hash) = path.rfind("#L") else {
        return (path.to_owned(), None);
    };
    let suffix = &path[hash + 2..];
    let mut chars = suffix.char_indices().peekable();
    let number = |chars: &mut std::iter::Peekable<std::str::CharIndices>| -> Option<usize> {
        let mut value: usize = 0;
        let mut any = false;
        while let Some((_, c)) = chars.peek().copied() {
            if let Some(digit) = c.to_digit(10) {
                value = value.checked_mul(10)?.checked_add(digit as usize)?;
                any = true;
                chars.next();
            } else {
                break;
            }
        }
        any.then_some(value)
    };
    let Some(line) = number(&mut chars) else {
        return (path.to_owned(), None);
    };
    let mut location = FileLocation { line: line.max(1), line_end: None, column: None };
    if chars.peek().map(|(_, c)| *c) == Some('-') {
        chars.next();
        if chars.peek().map(|(_, c)| *c) == Some('L') {
            chars.next();
        }
        match number(&mut chars) {
            Some(end) => location.line_end = Some(end),
            None => return (path.to_owned(), None),
        }
    }
    if chars.peek().map(|(_, c)| *c) == Some(':') {
        chars.next();
        match number(&mut chars) {
            Some(column) => location.column = Some(column),
            None => return (path.to_owned(), None),
        }
    }
    if chars.next().is_some() {
        return (path.to_owned(), None);
    }
    (path[..hash].to_owned(), Some(location))
}

/// Original markdown link fragment `^L?(\d+)(?::(\d+))?` (prefix match).
pub fn parse_link_fragment(fragment: &str) -> Option<(usize, Option<usize>)> {
    let rest = fragment.strip_prefix('L').unwrap_or(fragment);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let line = digits.parse().ok()?;
    let after = &rest[digits.len()..];
    let column = after.strip_prefix(':').and_then(|value| {
        let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    });
    Some((line, column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_and_windows_paths_follow_the_cwd_style() {
        assert_eq!(remote_absolute("/repo", "src/a.rs"), "/repo/src/a.rs");
        assert_eq!(remote_absolute("/repo", "../other/a.rs"), "/other/a.rs");
        assert_eq!(remote_absolute("/repo", "/etc/hosts"), "/etc/hosts");
        assert_eq!(remote_absolute("C:\\repo", "src/a.rs"), "C:\\repo\\src\\a.rs");
        assert_eq!(remote_relative("/repo", "/repo/src/a.rs"), "src/a.rs");
        assert_eq!(remote_relative("/repo", "/other/b"), "../other/b");
        assert_eq!(remote_relative("C:\\Repo", "c:\\repo\\x.txt"), "x.txt");
        assert_eq!(remote_relative("/repo", "./src//a.rs"), "src/a.rs");
        assert_eq!(remote_basename("src/a.rs", Some("/repo")), "a.rs");
        assert_eq!(remote_basename("src\\a.rs", Some("C:\\repo")), "a.rs");
        assert_eq!(remote_parent("/repo/src/a.rs"), "/repo/src");
        assert_eq!(remote_parent("/a"), "/");
        assert_eq!(remote_absolute("\\\\server\\share\\dir", "x"), "\\\\server\\share\\dir\\x");
    }

    #[test]
    fn explorer_paths() {
        assert!(is_hidden_explorer_path(".git"));
        assert!(is_hidden_explorer_path("src/.cache/x"));
        assert!(!is_hidden_explorer_path("./src"));
        assert!(!is_hidden_explorer_path("../x"));
        assert!(is_explorer_path_within("src/a", "src"));
        assert!(is_explorer_path_within("src", "src"));
        assert!(!is_explorer_path_within("srcx", "src"));
        assert_eq!(parent_explorer_path("src/a/b.rs"), "src/a");
        assert_eq!(parent_explorer_path("b.rs"), ".");
    }

    #[test]
    fn file_location_suffix() {
        assert_eq!(parse_file_location("src/a.rs"), ("src/a.rs".into(), None));
        assert_eq!(
            parse_file_location("src/a.rs#L12"),
            ("src/a.rs".into(), Some(FileLocation { line: 12, line_end: None, column: None }))
        );
        assert_eq!(
            parse_file_location("a#b.rs#L3-7:2"),
            ("a#b.rs".into(), Some(FileLocation { line: 3, line_end: Some(7), column: Some(2) }))
        );
        assert_eq!(parse_file_location("a.rs#Lx"), ("a.rs#Lx".into(), None));
        assert_eq!(parse_link_fragment("L10:4"), Some((10, Some(4))));
        assert_eq!(parse_link_fragment("5"), Some((5, None)));
        assert_eq!(parse_link_fragment("intro"), None);
    }
}
