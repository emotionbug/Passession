//! Diff document model shared by the Changes and commit views (original
//! diff_view.dart / changes_panel.dart helpers and git/*.ts presentation).

use super::json::{array, flag, int, text};
use serde_json::{Value, json};
use std::cmp::Ordering;

/// One numbered diff row. `hunk` is the index of the hunk header row the line
/// belongs to, so context never crosses hunks.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub content: String,
    pub old_number: Option<i64>,
    pub new_number: Option<i64>,
    pub hunk: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Header,
    Add,
    Remove,
    Context,
}

impl LineKind {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "add" => Self::Add,
            "remove" => Self::Remove,
            "context" => Self::Context,
            "header" => Self::Header,
            _ => return None,
        })
    }
}

/// Original `numberDiff`: each hunk contributes its `@@ -a,b +c,d @@` header
/// followed by its non-header lines numbered on each side.
pub fn number_diff(file: &Value) -> Vec<DiffLine> {
    let mut result = Vec::new();
    for hunk in array(file, "hunks") {
        let mut old = int(hunk, "oldStart");
        let mut next = int(hunk, "newStart");
        let header = format!(
            "@@ -{old},{} +{next},{} @@",
            int(hunk, "oldCount"),
            int(hunk, "newCount")
        );
        let hunk_index = result.len();
        result.push(DiffLine {
            kind: LineKind::Header,
            content: header,
            old_number: None,
            new_number: None,
            hunk: hunk_index,
        });
        for line in array(hunk, "lines") {
            // Unknown line kinds render as context, the original's fallthrough.
            let kind = LineKind::parse(text(line, "type")).unwrap_or(LineKind::Context);
            if kind == LineKind::Header {
                continue;
            }
            let old_number = if kind == LineKind::Add {
                None
            } else {
                old += 1;
                Some(old - 1)
            };
            let new_number = if kind == LineKind::Remove {
                None
            } else {
                next += 1;
                Some(next - 1)
            };
            result.push(DiffLine {
                kind,
                content: text(line, "content").to_owned(),
                old_number,
                new_number,
                hunk: hunk_index,
            });
        }
    }
    result
}

/// The original single ordering authority: directories before files within a
/// level, then plain code-unit order (never locale dependent).
pub fn compare_diff_paths(a: &str, b: &str) -> Ordering {
    let left: Vec<&str> = a.split('/').collect();
    let right: Vec<&str> = b.split('/').collect();
    let mut i = 0;
    loop {
        let left_file = i + 1 == left.len();
        let right_file = i + 1 == right.len();
        if left_file != right_file {
            return if left_file {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let order = utf16_cmp(left[i], right[i]);
        if order != Ordering::Equal {
            return order;
        }
        if left_file {
            return Ordering::Equal;
        }
        i += 1;
    }
}

/// Dart `String.compareTo` order (UTF-16 code units).
pub fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Original `normalizeCheckoutDiffCompare`: the uncommitted comparison never
/// carries a base ref; other comparisons carry the trimmed one when present.
pub fn normalize_compare(mode: &str, base_ref: &str, ignore_whitespace: bool, detail: Option<&str>) -> Value {
    let mut normalized = json!({ "mode": mode, "ignoreWhitespace": ignore_whitespace });
    if let Some(detail) = detail {
        normalized["detail"] = json!(detail);
    }
    if mode == "uncommitted" {
        return normalized;
    }
    let trimmed = base_ref.trim();
    if !trimmed.is_empty() {
        normalized["baseRef"] = json!(trimmed);
    }
    normalized
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
}

/// Original `fileChangeKind`.
pub fn change_kind(file: &Value) -> ChangeKind {
    if flag(file, "isNew") || text(file, "status") == "added" {
        ChangeKind::Added
    } else if flag(file, "isDeleted") || text(file, "status") == "deleted" {
        ChangeKind::Deleted
    } else {
        ChangeKind::Modified
    }
}

/// Original `formatDiffCount`: compact en-US notation, one fraction digit.
pub fn format_diff_count(value: i64) -> String {
    const SUFFIXES: [&str; 5] = ["", "k", "m", "b", "t"];
    let mut magnitude = 0;
    let mut scaled = value.unsigned_abs() as f64;
    while scaled >= 1000.0 && magnitude < SUFFIXES.len() - 1 {
        scaled /= 1000.0;
        magnitude += 1;
    }
    let mut rounded = (scaled * 10.0).round() / 10.0;
    if rounded >= 1000.0 && magnitude < SUFFIXES.len() - 1 {
        rounded = (rounded / 100.0).round() / 10.0;
        magnitude += 1;
    }
    let text = if rounded == rounded.trunc() {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded:.1}")
    };
    format!("{}{text}{}", if value < 0 { "-" } else { "" }, SUFFIXES[magnitude])
}

/// Original `formatBaseRefLabel` / `computeBaseRefLabel`.
pub fn format_base_ref_label(base_ref: &str, fallback: &str) -> String {
    if base_ref.is_empty() {
        return fallback.to_owned();
    }
    let mut trimmed = base_ref;
    for prefix in ["refs/heads/", "refs/remotes/"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            trimmed = rest;
            break;
        }
    }
    let trimmed = trimmed.trim();
    trimmed.strip_prefix("origin/").unwrap_or(trimmed).to_owned()
}

/// Original `buildAbsoluteExplorerPath`: Windows roots keep their separator.
pub fn absolute_path(cwd: &str, entry: &str) -> String {
    let root = cwd.trim().trim_end_matches(['/', '\\']);
    let path = entry.trim();
    if root.is_empty() {
        return path.to_owned();
    }
    if path.is_empty() || path == "." {
        return root.to_owned();
    }
    let bytes = path.as_bytes();
    let windows_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\');
    if path.starts_with('/') || windows_absolute {
        return path.to_owned();
    }
    let separator = if root.contains('\\') { "\\" } else { "/" };
    let mut parts = vec![root.to_owned()];
    parts.extend(
        path.split(['/', '\\'])
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned),
    );
    parts.join(separator)
}

// ---------------------------------------------------------------------------
// Changed files tree (original `ChangedFilesTree`).

#[derive(Clone, Debug)]
pub struct ChangeFolder {
    pub path: String,
    pub name: String,
    pub folders: Vec<ChangeFolder>,
    /// Indices into the controller's file list.
    pub files: Vec<usize>,
    pub additions: i64,
    pub deletions: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TreeRow {
    Folder {
        path: String,
        /// Compressed single-child chain, e.g. `src/lib`.
        display: String,
        depth: usize,
        additions: i64,
        deletions: i64,
    },
    File {
        index: usize,
        depth: usize,
    },
}

impl ChangeFolder {
    fn new(path: &str, name: &str) -> Self {
        Self {
            path: path.to_owned(),
            name: name.to_owned(),
            folders: Vec::new(),
            files: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }

    fn insert_folder(&mut self, path: &str) -> &mut ChangeFolder {
        if path.is_empty() {
            return self;
        }
        let (parent, name) = match path.rfind('/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => ("", path),
        };
        let parent = self.insert_folder(parent);
        let position = parent.folders.iter().position(|folder| folder.path == path);
        let index = match position {
            Some(index) => index,
            None => {
                parent.folders.push(ChangeFolder::new(path, name));
                parent.folders.len() - 1
            }
        };
        &mut parent.folders[index]
    }

    fn stats(&mut self, files: &[Value]) {
        self.additions = 0;
        self.deletions = 0;
        for folder in &mut self.folders {
            folder.stats(files);
            self.additions += folder.additions;
            self.deletions += folder.deletions;
        }
        for &index in &self.files {
            self.additions += int(&files[index], "additions");
            self.deletions += int(&files[index], "deletions");
        }
    }
}

/// Original tree rows: compressed single-child folder chains, folders before
/// files (each sorted by code unit), collapsed folders hide their subtree.
pub fn changed_tree_rows(files: &[Value], collapsed: &std::collections::HashSet<String>) -> Vec<TreeRow> {
    let mut root = ChangeFolder::new("", "");
    for (index, file) in files.iter().enumerate() {
        let path = text(file, "path");
        let folder = match path.rfind('/') {
            Some(slash) => &path[..slash],
            None => "",
        };
        root.insert_folder(folder).files.push(index);
    }
    root.stats(files);
    let mut rows = Vec::new();
    fn walk(
        node: &mut ChangeFolder,
        depth: usize,
        files: &[Value],
        collapsed: &std::collections::HashSet<String>,
        rows: &mut Vec<TreeRow>,
    ) {
        node.folders.sort_by(|a, b| utf16_cmp(&a.name, &b.name));
        node.files
            .sort_by(|&a, &b| utf16_cmp(text(&files[a], "path"), text(&files[b], "path")));
        for folder in &mut node.folders {
            let mut display = folder.name.clone();
            let mut current: &mut ChangeFolder = folder;
            while current.folders.len() == 1 && current.files.is_empty() {
                current = &mut current.folders[0];
                display = format!("{display}/{}", current.name);
            }
            rows.push(TreeRow::Folder {
                path: current.path.clone(),
                display,
                depth,
                additions: current.additions,
                deletions: current.deletions,
            });
            if !collapsed.contains(&current.path) {
                walk(current, depth + 1, files, collapsed, rows);
            }
        }
        for &index in &node.files {
            rows.push(TreeRow::File { index, depth });
        }
    }
    walk(&mut root, 0, files, collapsed, &mut rows);
    rows
}

/// Original `handleCollapseFolder`: the folder and every folder under it.
pub fn collapse_folder_recursive(
    files: &[Value],
    path: &str,
    collapsed: &mut std::collections::HashSet<String>,
) {
    collapsed.insert(path.to_owned());
    let prefix = format!("{path}/");
    for file in files {
        let segments: Vec<&str> = text(file, "path").split('/').collect();
        for i in 1..segments.len() {
            let folder = segments[..i].join("/");
            if folder.starts_with(&prefix) {
                collapsed.insert(folder);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Time formatting.

/// Original commit row `_timeAgo` (relative to `now_ms`, both epoch ms).
pub fn commit_time_ago(date_ms: i64, now_ms: i64) -> String {
    let elapsed = (now_ms - date_ms).max(0) / 1000;
    let minutes = elapsed / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        return "just now".into();
    }
    if hours < 1 {
        return format!("{minutes}m ago");
    }
    if days < 1 {
        return format!("{hours}h ago");
    }
    if days < 7 {
        return format!("{days}d ago");
    }
    use chrono::{Datelike, Local, TimeZone};
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match Local.timestamp_millis_opt(date_ms).single() {
        Some(local) => format!("{} {}", MONTHS[local.month0() as usize], local.day()),
        None => String::new(),
    }
}

/// Original PR activity `formatAge`.
pub fn format_age(created_at_ms: i64, now_ms: i64) -> String {
    if created_at_ms <= 0 {
        return String::new();
    }
    let seconds = (now_ms - created_at_ms).max(0) / 1000;
    if seconds < 60 {
        return "just now".into();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    if days < 365 {
        return format!("{}mo ago", days / 30);
    }
    format!("{}y ago", days / 365)
}

/// Original `formatDuration` for pipeline job durations.
pub fn format_duration(total_seconds: i64) -> String {
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    if minutes < 60 {
        let seconds = total_seconds % 60;
        return if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {seconds}s")
        };
    }
    let hours = minutes / 60;
    let rem = minutes % 60;
    if rem == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rem}m")
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `DateTime.tryParse` for the RFC 3339 timestamps the daemon sends
/// (offsets, fractional seconds, `Z`), into epoch milliseconds.
pub fn parse_iso8601_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(parsed.timestamp_millis());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().timestamp_millis());
    }
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn numbering_skips_wire_headers_and_counts_each_side() {
        let file = json!({"hunks":[{"oldStart":3,"oldCount":2,"newStart":3,"newCount":3,"lines":[
            {"type":"header","content":"@@"},
            {"type":"context","content":"a"},
            {"type":"remove","content":"b"},
            {"type":"add","content":"c"},
            {"type":"add","content":"d"}
        ]}]});
        let lines = number_diff(&file);
        assert_eq!(lines[0].content, "@@ -3,2 +3,3 @@");
        assert_eq!((lines[1].old_number, lines[1].new_number), (Some(3), Some(3)));
        assert_eq!((lines[2].old_number, lines[2].new_number), (Some(4), None));
        assert_eq!((lines[3].old_number, lines[3].new_number), (None, Some(4)));
        assert_eq!((lines[4].old_number, lines[4].new_number), (None, Some(5)));
    }

    #[test]
    fn paths_order_directories_before_files_per_level() {
        let mut paths = vec!["b.txt", "a/z.txt", "a/b/c.txt", "A.txt"];
        paths.sort_by(|a, b| compare_diff_paths(a, b));
        assert_eq!(paths, ["a/b/c.txt", "a/z.txt", "A.txt", "b.txt"]);
    }

    #[test]
    fn uncommitted_comparison_never_sends_a_base_ref() {
        assert_eq!(
            normalize_compare("uncommitted", "main", false, None),
            json!({"mode":"uncommitted","ignoreWhitespace":false})
        );
        assert_eq!(
            normalize_compare("base", " origin/main ", true, Some("summary")),
            json!({"mode":"base","ignoreWhitespace":true,"detail":"summary","baseRef":"origin/main"})
        );
        assert_eq!(
            normalize_compare("all", "  ", false, None),
            json!({"mode":"all","ignoreWhitespace":false})
        );
    }

    #[test]
    fn diff_counts_use_compact_notation() {
        assert_eq!(format_diff_count(999), "999");
        assert_eq!(format_diff_count(156_200), "156.2k");
        assert_eq!(format_diff_count(999_950), "1m");
        assert_eq!(format_diff_count(1000), "1k");
    }

    #[test]
    fn tree_compresses_single_child_chains_and_sums_stats() {
        let files = vec![
            json!({"path":"src/lib/a.rs","additions":1,"deletions":2}),
            json!({"path":"src/lib/b.rs","additions":3,"deletions":0}),
            json!({"path":"README.md","additions":1,"deletions":1}),
        ];
        let rows = changed_tree_rows(&files, &HashSet::new());
        assert_eq!(
            rows[0],
            TreeRow::Folder {
                path: "src/lib".into(),
                display: "src/lib".into(),
                depth: 0,
                additions: 4,
                deletions: 2
            }
        );
        assert_eq!(rows[1], TreeRow::File { index: 0, depth: 1 });
        assert_eq!(rows[3], TreeRow::File { index: 2, depth: 0 });
        let collapsed: HashSet<String> = ["src/lib".to_owned()].into();
        assert_eq!(changed_tree_rows(&files, &collapsed).len(), 2);
    }

    #[test]
    fn absolute_paths_keep_windows_separators() {
        assert_eq!(absolute_path("/repo/", "a/b.rs"), "/repo/a/b.rs");
        assert_eq!(absolute_path("C:\\repo", "a/b.rs"), "C:\\repo\\a\\b.rs");
        assert_eq!(absolute_path("/repo", "."), "/repo");
        assert_eq!(absolute_path("/repo", "/etc/x"), "/etc/x");
    }

    #[test]
    fn iso_timestamps_parse_with_offsets() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_iso8601_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_iso8601_ms("1970-01-02"), Some(86_400_000));
        assert_eq!(parse_iso8601_ms("nope"), None);
    }

    #[test]
    fn base_ref_labels_strip_ref_prefixes_and_origin() {
        assert_eq!(format_base_ref_label("refs/remotes/origin/main", "base"), "main");
        assert_eq!(format_base_ref_label("", "base"), "base");
    }
}
