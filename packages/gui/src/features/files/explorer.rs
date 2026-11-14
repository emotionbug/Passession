//! The explorer tree (Flutter `remote_directory_browser.dart` state): listings, expansion,
//! restoring persisted expanded folders, sorting, hidden files, reveal, rename remapping and
//! the folder picker's flat, history-navigated directory listing. Sans-IO: listings and
//! preference writes are emitted as [`ExplorerCmd`]s and results return through
//! [`ExplorerState::on_listed`].

use crate::features::files::path::{
    Style, is_explorer_path_within, is_hidden_explorer_path, remote_absolute,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Original `MAX_AUTO_EXPANDED_DIRECTORY_DEPTH`.
pub const MAX_AUTO_EXPANDED_DIRECTORY_DEPTH: usize = 5;

/// One listed entry (`{name, path, kind, size, modifiedAt}`), with the raw row kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub modified_at: String,
    pub raw: Value,
}

impl Entry {
    pub fn from_json(json: &Value) -> Entry {
        let text = |key: &str| json.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
        Entry {
            name: text("name"),
            path: text("path"),
            kind: text("kind"),
            size: json.get("size").and_then(Value::as_u64).unwrap_or(0),
            modified_at: text("modifiedAt"),
            raw: json.clone(),
        }
    }

    pub fn is_directory(&self) -> bool {
        self.kind == "directory"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sort {
    Name,
    Modified,
    Size,
}

impl Sort {
    pub fn id(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Modified => "modified",
            Sort::Size => "size",
        }
    }

    fn from_id(id: &str) -> Option<Sort> {
        match id {
            "name" => Some(Sort::Name),
            "modified" => Some(Sort::Modified),
            "size" => Some(Sort::Size),
            _ => None,
        }
    }

    fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::Modified,
            Sort::Modified => Sort::Size,
            Sort::Size => Sort::Name,
        }
    }
}

/// Original `sortExplorerEntries`: directories first, then name A→Z, newest first or largest
/// first, ties by name.
fn compare(sort: Sort, a: &Entry, b: &Entry) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ad, bd) = (a.is_directory(), b.is_directory());
    if ad != bd {
        return if ad { Ordering::Less } else { Ordering::Greater };
    }
    let result = match sort {
        Sort::Size => b.size.cmp(&a.size),
        Sort::Modified => b.modified_at.cmp(&a.modified_at),
        Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    };
    if result == Ordering::Equal { a.name.cmp(&b.name) } else { result }
}

/// Original `reconcileRestoredExpandedPaths`: the restored paths, minus what the user
/// collapsed meanwhile, plus what the user expanded meanwhile.
pub fn reconcile_restored_expanded_paths(
    persisted: &HashSet<String>,
    current: &HashSet<String>,
    restored: &[String],
) -> Vec<String> {
    let mut reconciled: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for path in restored {
        if seen.insert(path.clone()) {
            reconciled.push(path.clone());
        }
    }
    for path in persisted {
        if !current.contains(path) {
            reconciled.retain(|candidate| candidate != path);
        }
    }
    let mut added: Vec<&String> = current.iter().filter(|path| !persisted.contains(*path)).collect();
    added.sort();
    for path in added {
        if seen.insert(path.clone()) {
            reconciled.push(path.clone());
        }
    }
    reconciled
}

/// The parent of a picker directory, or `None` at a root (`/`, `C:\`, `\\server\share\`) and
/// at the top of a text path the host has not resolved yet (`~`, `.`).
pub fn parent_directory(path: &str) -> Option<String> {
    let style = Style::of(path);
    let normalized = style.normalize(path);
    if !style.is_absolute(&normalized) && (normalized == "." || normalized == "~" || normalized.starts_with("..")) {
        return None;
    }
    let parent = style.dirname(&normalized);
    (parent != normalized).then_some(parent)
}

/// The host home directory implied by a `~` path (`~`, `~/work`) and the absolute directory the
/// host listed for it.
fn home_of(requested: &str, absolute: &str) -> Option<String> {
    if requested == "~" {
        return Some(absolute.to_owned());
    }
    let rest = Style::Posix.normalize(requested.strip_prefix("~/")?);
    if rest == "." {
        return Some(absolute.to_owned());
    }
    if rest.split('/').any(|segment| segment == "..") {
        return None;
    }
    let style = Style::of(absolute);
    let mut home = absolute.to_owned();
    for _ in rest.split('/') {
        let parent = style.dirname(&home);
        if parent == home {
            return None;
        }
        home = parent;
    }
    let suffix = style.relative(absolute, &home);
    (suffix.replace('\\', "/") == rest).then_some(home)
}

/// Expand only the host-home alias; never consult the client's environment or resolve symlinks.
fn expand_home(path: &str, home: Option<&str>) -> Option<String> {
    let rest = if path == "~" { "" } else { path.strip_prefix("~/")? };
    let home = home?;
    let style = Style::of(home);
    Some(style.normalize(&style.join(home, rest)))
}

/// One visible row of the flattened tree.
#[derive(Clone, Debug)]
pub struct Row {
    pub entry: Entry,
    pub depth: usize,
}

/// One successful `file_explorer_response` list result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    pub entries: Vec<Value>,
    /// `directory.absolutePath`: the expanded, normalized listed directory (newer hosts).
    pub absolute: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExplorerCmd {
    List { path: String, generation: u64 },
    /// Persist one explorer preference (`explorerShowHiddenFiles`, `explorerSortOption`).
    SavePreference { key: &'static str, value: Value },
    /// Persist this workspace's expanded directories into `expandedPathsByWorkspace`.
    SaveExpanded { workspace_key: String, paths: Vec<String> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestorePurpose {
    Initialize,
    ToggleHidden,
}

/// Original `restoreExpandedDirectories` as a level-by-level state machine.
struct Restore {
    generation: u64,
    purpose: RestorePurpose,
    persisted: HashSet<String>,
    show_hidden: bool,
    requested: HashSet<String>,
    restored: Vec<String>,
    depth: usize,
    level: Vec<String>,
    pending: HashSet<String>,
    results: HashMap<String, Vec<Entry>>,
}

struct Reveal {
    generation: u64,
    paths: Vec<String>,
    index: usize,
}

/// The persisted explorer preferences of a workspace explorer (absent for a picker).
#[derive(Clone, Debug)]
pub struct ExplorerPreferences {
    /// Original `buildWorkspaceExplorerStateKey`: `workspace:<id>` or `root:<dir>`.
    pub workspace_key: String,
}

pub struct ExplorerState {
    pub cwd: String,
    pub initial_path: String,
    /// The remote folder picker: directories only, one flat level, navigated by absolute path
    /// once the host reports it (`directory.absolutePath`).
    pub picker: bool,
    pub preferences: Option<ExplorerPreferences>,
    directories: HashMap<String, Vec<Entry>>,
    expanded: BTreeSet<String>,
    loading: HashSet<String>,
    inflight: HashSet<String>,
    errors: HashMap<String, String>,
    history: Vec<String>,
    history_index: Option<usize>,
    generation: u64,
    path: String,
    hidden: bool,
    sort: Sort,
    refresh_waiting: HashSet<String>,
    pub selected: Option<String>,
    /// The host's home directory, once a listing of `~` reported its absolute path.
    home: Option<String>,
    rows: Vec<Row>,
    restore: Option<Restore>,
    reveal: Option<Reveal>,
    initialized: bool,
    pub cmds: Vec<ExplorerCmd>,
    /// Bumps on every observable change.
    pub revision: u64,
}

impl ExplorerState {
    /// A workspace explorer (with persisted preferences) or the folder picker (without).
    pub fn new(
        cwd: &str,
        initial_path: &str,
        picker: bool,
        preferences: Option<ExplorerPreferences>,
        stored: &Value,
    ) -> ExplorerState {
        let mut state = ExplorerState {
            cwd: cwd.to_owned(),
            initial_path: initial_path.to_owned(),
            picker,
            preferences,
            directories: HashMap::new(),
            expanded: BTreeSet::new(),
            loading: HashSet::new(),
            inflight: HashSet::new(),
            errors: HashMap::new(),
            history: Vec::new(),
            history_index: None,
            generation: 0,
            path: initial_path.to_owned(),
            hidden: true,
            sort: Sort::Name,
            refresh_waiting: HashSet::new(),
            selected: None,
            home: None,
            rows: Vec::new(),
            restore: None,
            reveal: None,
            initialized: false,
            cmds: Vec::new(),
            revision: 0,
        };
        if state.preferences.is_some() {
            state.hidden = stored.get("explorerShowHiddenFiles") != Some(&Value::Bool(false));
            if let Some(sort) =
                stored.get("explorerSortOption").and_then(Value::as_str).and_then(Sort::from_id)
            {
                state.sort = sort;
            }
        }
        state
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Starts the first listing (original `initState` / `initializeExplorer`).
    pub fn start(&mut self, stored: &Value) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        if self.preferences.is_none() {
            let path = self.path.clone();
            self.navigate(&path, true);
            return;
        }
        self.initialize(stored);
    }

    pub fn started(&self) -> bool {
        self.initialized
    }

    /// The persisted expanded directories of this workspace (always with `.`).
    fn persisted_expanded(&self, stored: &Value) -> HashSet<String> {
        let Some(preferences) = &self.preferences else { return HashSet::new() };
        let list: Vec<String> = stored
            .get("expandedPathsByWorkspace")
            .and_then(|value| value.get(&preferences.workspace_key))
            .and_then(Value::as_array)
            .map(|paths| paths.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        let mut set: HashSet<String> = list.into_iter().collect();
        if set.is_empty() {
            set.insert(".".to_owned());
        }
        set
    }

    fn initialize(&mut self, stored: &Value) {
        let persisted = self.persisted_expanded(stored);
        self.expanded = persisted.iter().filter(|path| *path != ".").cloned().collect();
        let path = self.path.clone();
        self.navigate(&path, true);
        self.restore = Some(Restore {
            generation: self.generation,
            purpose: RestorePurpose::Initialize,
            persisted,
            show_hidden: self.hidden,
            requested: HashSet::from([".".to_owned()]),
            restored: vec![".".to_owned()],
            depth: 0,
            level: Vec::new(),
            pending: HashSet::from([path]),
            results: HashMap::new(),
        });
        self.changed();
    }

    /// The cwd changed (another workspace or an assistant link elsewhere): everything resets.
    pub fn reset_cwd(&mut self, cwd: &str, stored: &Value) {
        self.generation += 1;
        self.cwd = cwd.to_owned();
        self.directories.clear();
        self.expanded.clear();
        self.loading.clear();
        self.inflight.clear();
        self.errors.clear();
        self.history.clear();
        self.history_index = None;
        self.refresh_waiting.clear();
        self.restore = None;
        self.reveal = None;
        self.path = self.initial_path.clone();
        self.rows.clear();
        self.selected = None;
        self.home = None;
        self.cmds.retain(|cmd| !matches!(cmd, ExplorerCmd::List { .. }));
        self.initialized = true;
        if self.preferences.is_some() {
            self.initialize(stored);
        } else {
            let path = self.path.clone();
            self.navigate(&path, true);
        }
    }

    /// The host connected again: pending listings of the lost connection are gone; the tree and
    /// its expansion stay and list again (original client switch).
    pub fn connection_restored(&mut self) {
        if !self.initialized {
            return;
        }
        self.generation += 1;
        self.loading.clear();
        self.inflight.clear();
        self.errors.clear();
        self.restore = None;
        self.reveal = None;
        self.refresh();
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn show_hidden(&self) -> bool {
        self.hidden
    }

    pub fn refreshing(&self) -> bool {
        !self.refresh_waiting.is_empty()
    }

    pub fn is_expanded(&self, path: &str) -> bool {
        self.expanded.contains(path)
    }

    pub fn is_loading(&self, path: &str) -> bool {
        self.loading.contains(path)
    }

    pub fn error(&self, path: &str) -> Option<&str> {
        self.errors.get(path).map(String::as_str)
    }

    pub fn is_listed(&self, path: &str) -> bool {
        self.directories.contains_key(path)
    }

    /// The root failed before it ever listed.
    pub fn root_error(&self) -> Option<&str> {
        if self.directories.contains_key(&self.path) { None } else { self.error(&self.path) }
    }

    /// The root failed a re-list but an older listing is shown.
    pub fn stale_root_error(&self) -> Option<&str> {
        if self.directories.contains_key(&self.path) { self.error(&self.path) } else { None }
    }

    pub fn initial_loading(&self) -> bool {
        self.rows.is_empty() && self.loading.contains(&self.path) && !self.directories.contains_key(&self.path)
    }

    pub fn can_go_back(&self) -> bool {
        self.history_index.is_some_and(|index| index > 0)
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_index.is_some_and(|index| index + 1 < self.history.len())
    }

    fn flatten(&mut self) {
        let mut rows = Vec::new();
        let mut ancestors: Vec<String> = Vec::new();
        self.visit(&self.path.clone(), 0, &mut ancestors, &mut rows);
        self.rows = rows;
    }

    fn visit(&self, path: &str, depth: usize, ancestors: &mut Vec<String>, rows: &mut Vec<Row>) {
        if ancestors.iter().any(|ancestor| ancestor == path) {
            return;
        }
        ancestors.push(path.to_owned());
        if let Some(entries) = self.directories.get(path) {
            for entry in entries {
                if self.picker && !entry.is_directory() {
                    continue;
                }
                if !self.hidden && entry.name.starts_with('.') {
                    continue;
                }
                rows.push(Row { entry: entry.clone(), depth });
                // The picker lists one level: rows open their folder instead of expanding.
                if !self.picker && self.expanded.contains(&entry.path) {
                    self.visit(&entry.path, depth + 1, ancestors, rows);
                }
            }
        }
        ancestors.pop();
    }

    fn set_expanded(&mut self, paths: impl IntoIterator<Item = String>) {
        self.expanded = paths.into_iter().filter(|path| path != ".").collect();
        self.flatten();
        self.changed();
        if let Some(preferences) = &self.preferences {
            let mut paths = vec![".".to_owned()];
            paths.extend(self.expanded.iter().cloned());
            self.cmds.push(ExplorerCmd::SaveExpanded {
                workspace_key: preferences.workspace_key.clone(),
                paths,
            });
        }
    }

    fn save_preference(&mut self, key: &'static str, value: Value) {
        if self.preferences.is_some() {
            self.cmds.push(ExplorerCmd::SavePreference { key, value });
        }
    }

    /// Lists `path`; concurrent requests for one path share the listing.
    fn load(&mut self, path: &str) {
        if self.inflight.contains(path) {
            return;
        }
        self.inflight.insert(path.to_owned());
        self.loading.insert(path.to_owned());
        self.errors.remove(path);
        self.cmds.push(ExplorerCmd::List { path: path.to_owned(), generation: self.generation });
        self.changed();
    }

    /// Re-lists one directory (after an entry changed in it).
    pub fn reload(&mut self, path: &str) {
        self.load(path);
    }

    /// A listing finished: entries (with the host's absolute directory), or the failure message.
    pub fn on_listed(&mut self, path: &str, generation: u64, result: Result<Listing, String>) {
        if generation != self.generation {
            return;
        }
        self.inflight.remove(path);
        self.loading.remove(path);
        let listed = match result {
            Ok(listing) => {
                let mut entries: Vec<Entry> = listing.entries.iter().map(Entry::from_json).collect();
                let sort = self.sort;
                entries.sort_by(|a, b| compare(sort, a, b));
                self.directories.insert(path.to_owned(), entries.clone());
                if self.picker && let Some(absolute) = listing.absolute.filter(|absolute| Style::of(absolute).is_absolute(absolute)) {
                    self.adopt_absolute(path, absolute);
                }
                self.flatten();
                Some(entries)
            }
            Err(error) => {
                self.errors.insert(path.to_owned(), error);
                None
            }
        };
        self.changed();
        self.refresh_waiting.remove(path);
        self.advance_restore(path, listed.clone());
        self.advance_reveal(path, listed.is_some());
    }

    /// The picker: the host reported the absolute directory of `path`. The shown directory (and
    /// its history entry) continues by that absolute path, and a listing under `~` reveals home.
    fn adopt_absolute(&mut self, path: &str, absolute: String) {
        let requested = if path == "." { self.cwd.as_str() } else { path };
        if self.home.is_none()
            && let Some(home) = home_of(requested, &absolute)
        {
            self.home = Some(home);
        }
        // Resolve history even if the user left before this response arrived. Otherwise Back
        // can revisit `.` or `~` as a second entry for the same directory.
        for entry in &mut self.history {
            if entry == path {
                *entry = absolute.clone();
            } else if let Some(resolved) = expand_home(entry, self.home.as_deref()) {
                *entry = resolved;
            }
        }
        if let Some(index) = self.history_index {
            self.history_index = Some(self.history.windows(2).take(index).filter(|pair| pair[0] != pair[1]).count());
        }
        self.history.dedup();
        if path == self.path && absolute != path {
            if let Some(entries) = self.directories.remove(path) {
                self.directories.insert(absolute.clone(), entries);
            }
            self.path = absolute;
        }
    }

    fn advance_restore(&mut self, path: &str, listed: Option<Vec<Entry>>) {
        let Some(restore) = self.restore.as_mut() else { return };
        if restore.generation != self.generation || !restore.pending.remove(path) {
            return;
        }
        if let Some(entries) = listed {
            restore.results.insert(path.to_owned(), entries);
        } else if restore.depth == 0 {
            // The root failed: nothing to restore.
            self.restore = None;
            return;
        }
        if !restore.pending.is_empty() {
            return;
        }
        // The level finished: its successful listings are the next parents.
        let parents: Vec<Vec<Entry>> = if restore.depth == 0 {
            restore.results.drain().map(|(_, entries)| entries).collect()
        } else {
            let level = std::mem::take(&mut restore.level);
            let mut parents = Vec::new();
            for path in level {
                if let Some(entries) = restore.results.remove(&path) {
                    restore.restored.push(path);
                    parents.push(entries);
                }
            }
            parents
        };
        restore.depth += 1;
        if restore.depth <= MAX_AUTO_EXPANDED_DIRECTORY_DEPTH {
            let mut paths = Vec::new();
            for entries in &parents {
                for entry in entries {
                    if !restore.show_hidden && entry.name.starts_with('.') {
                        continue;
                    }
                    if entry.is_directory()
                        && restore.persisted.contains(&entry.path)
                        && restore.requested.insert(entry.path.clone())
                    {
                        paths.push(entry.path.clone());
                    }
                }
            }
            if !paths.is_empty() {
                restore.level = paths.clone();
                restore.pending = paths.iter().cloned().collect();
                for path in paths {
                    self.load(&path);
                }
                return;
            }
        }
        let restore = self.restore.take().unwrap();
        let mut current: HashSet<String> = self.expanded.iter().cloned().collect();
        current.insert(".".to_owned());
        let mut restored = restore.restored;
        if restore.purpose == RestorePurpose::Initialize && !self.hidden {
            let mut hidden: Vec<String> =
                restore.persisted.iter().filter(|path| is_hidden_explorer_path(path)).cloned().collect();
            hidden.sort();
            restored.extend(hidden);
        }
        let reconciled = reconcile_restored_expanded_paths(&restore.persisted, &current, &restored);
        self.set_expanded(reconciled);
    }

    fn advance_reveal(&mut self, path: &str, listed: bool) {
        let Some(reveal) = self.reveal.as_mut() else { return };
        if reveal.generation != self.generation || reveal.paths.get(reveal.index).map(String::as_str) != Some(path) {
            return;
        }
        if !listed {
            self.reveal = None;
            return;
        }
        reveal.index += 1;
        if let Some(next) = reveal.paths.get(reveal.index).cloned() {
            self.load(&next);
            return;
        }
        let reveal = self.reveal.take().unwrap();
        let mut expanded: Vec<String> = self.expanded.iter().cloned().collect();
        expanded.extend(reveal.paths);
        self.set_expanded(expanded);
    }

    /// Original `navigate`: shows `path` as the root, recording history.
    pub fn navigate(&mut self, path: &str, record_history: bool) {
        if path.trim().is_empty() {
            return;
        }
        let resolved = if self.picker { expand_home(path, self.home()) } else { None };
        let path = resolved.as_deref().unwrap_or(path);
        let same_directory = self.picker && self.current_directory() == path;
        self.path = path.to_owned();
        self.selected = None;
        if record_history {
            let same = self.history_index.and_then(|index| self.history.get(index)).is_some_and(|last| last == path);
            if same_directory && let Some(index) = self.history_index {
                self.history[index] = path.to_owned();
            }
            if (!same && !same_directory) || self.history_index.is_none() {
                let keep = self.history_index.map(|index| index + 1).unwrap_or(0);
                self.history.truncate(keep);
                self.history.push(path.to_owned());
                self.history_index = Some(self.history.len() - 1);
            }
        }
        self.flatten();
        self.changed();
        self.load(path);
    }

    pub fn back(&mut self) {
        if let Some(index) = self.history_index.filter(|index| *index > 0) {
            self.history_index = Some(index - 1);
            let path = self.history[index - 1].clone();
            self.navigate(&path, false);
        }
    }

    pub fn forward(&mut self) {
        if let Some(index) = self.history_index.filter(|index| index + 1 < self.history.len()) {
            self.history_index = Some(index + 1);
            let path = self.history[index + 1].clone();
            self.navigate(&path, false);
        }
    }

    /// The picker's "Up": the parent of the shown directory (none at a root).
    pub fn parent(&mut self) {
        if let Some(parent) = self.parent_directory() {
            self.navigate(&parent, true);
        }
    }

    pub fn can_go_up(&self) -> bool {
        self.parent_directory().is_some()
    }

    pub fn parent_directory(&self) -> Option<String> {
        parent_directory(&self.current_directory())
    }

    /// The remote directory currently shown (the picker's selection): absolute once the host
    /// reported it, otherwise the cwd-resolved text path (`~/work` on older hosts).
    pub fn current_directory(&self) -> String {
        let path = if self.path == "." { &self.cwd } else { &self.path };
        if let Some(absolute) = expand_home(path, self.home()) {
            return absolute;
        }
        let style = Style::of(path);
        if path == "~" || path.starts_with("~/") {
            // An older host has not supplied enough metadata to resolve this alias.
            path.clone()
        } else if style.is_absolute(path) {
            style.normalize(path)
        } else {
            remote_absolute(&self.cwd, &self.path)
        }
    }

    /// The host's home directory, when known.
    pub fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }

    /// The picker: the directory a listed row opens.
    pub fn child_directory(&self, entry: &Entry) -> String {
        let current = self.current_directory();
        let style = Style::of(&current);
        if style.is_absolute(&current) {
            style.normalize(&style.join(&current, &entry.name))
        } else {
            remote_absolute(&self.cwd, &entry.path)
        }
    }

    /// The picker: opens a typed location. Absolute and `~` paths stand alone; anything else is
    /// relative to the shown directory.
    pub fn go_to(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let current = self.current_directory();
        let style = Style::of(if Style::of(text) == Style::Windows { text } else { &current });
        let target = if text == "~" || text.starts_with("~/") {
            // Keep unresolved tilde paths intact for the host, including `~/..`. With a known
            // home, navigate expands using that home's style (not the current directory's).
            text.to_owned()
        } else if style.is_absolute(text) {
            style.normalize(text)
        } else {
            style.normalize(&style.join(&current, text))
        };
        self.navigate(&target, true);
    }

    /// Original `refreshExplorerDirectories`: re-lists the root and every visible expanded
    /// directory.
    pub fn refresh(&mut self) {
        let mut paths = vec![self.path.clone()];
        for path in &self.expanded {
            if self.hidden || !is_hidden_explorer_path(path) {
                paths.push(path.clone());
            }
        }
        for path in paths {
            self.refresh_waiting.insert(path.clone());
            self.load(&path);
        }
        self.changed();
    }

    /// Original `handleSortCycle`: Name → Modified → Size.
    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        let sort = self.sort;
        for entries in self.directories.values_mut() {
            entries.sort_by(|a, b| compare(sort, a, b));
        }
        self.flatten();
        self.changed();
        self.save_preference("explorerSortOption", Value::String(sort.id().to_owned()));
    }

    /// Original `handleToggleHiddenFiles`: showing hidden files again lists the expanded hidden
    /// directories the tree now shows.
    pub fn toggle_hidden(&mut self) {
        let will_show = !self.hidden;
        self.hidden = will_show;
        self.flatten();
        self.changed();
        self.save_preference("explorerShowHiddenFiles", Value::Bool(will_show));
        let Some(root) = self.directories.get(&self.path).cloned() else { return };
        if !will_show || self.preferences.is_none() {
            return;
        }
        let mut persisted: HashSet<String> = self.expanded.iter().cloned().collect();
        persisted.insert(".".to_owned());
        let mut restore = Restore {
            generation: self.generation,
            purpose: RestorePurpose::ToggleHidden,
            persisted,
            show_hidden: true,
            requested: HashSet::from([".".to_owned()]),
            restored: vec![".".to_owned()],
            depth: 0,
            level: Vec::new(),
            pending: HashSet::new(),
            results: HashMap::new(),
        };
        // The root is already listed: start from its entries.
        restore.pending.insert(self.path.clone());
        self.restore = Some(restore);
        let path = self.path.clone();
        self.advance_restore(&path, Some(root));
    }

    /// Original `toggleDirectory`: lists a directory the first time it expands.
    pub fn toggle(&mut self, path: &str) {
        self.selected = Some(path.to_owned());
        let expand = !self.expanded.contains(path);
        let mut next: Vec<String> = self.expanded.iter().cloned().collect();
        if expand {
            next.push(path.to_owned());
        } else {
            next.retain(|candidate| candidate != path);
        }
        self.set_expanded(next);
        if expand && !self.directories.contains_key(path) {
            self.load(path);
        }
    }

    /// Original "Collapse folder": collapses the folder and every folder inside it.
    pub fn collapse(&mut self, path: &str) {
        let next: Vec<String> =
            self.expanded.iter().filter(|expanded| !is_explorer_path_within(expanded, path)).cloned().collect();
        self.set_expanded(next);
    }

    /// Original `handleNewEntry`: a new entry's parent folder expands (and lists once).
    pub fn expand_for_create(&mut self, parent: &str) {
        if parent == "." || parent == self.path {
            return;
        }
        let mut next: Vec<String> = self.expanded.iter().cloned().collect();
        next.push(parent.to_owned());
        self.set_expanded(next);
        if !self.directories.contains_key(parent) {
            self.load(parent);
        }
    }

    /// Original `handleRenameCommit`: a renamed folder's expanded paths follow it, and re-list.
    pub fn renamed(&mut self, from: &str, to: &str) {
        let remap = |path: &String| -> String {
            if is_explorer_path_within(path, from) {
                format!("{to}{}", &path[from.len()..])
            } else {
                path.clone()
            }
        };
        let moved: Vec<String> =
            self.expanded.iter().filter(|path| is_explorer_path_within(path, from)).map(remap).collect();
        let next: Vec<String> = self.expanded.iter().map(remap).collect();
        self.set_expanded(next);
        for path in moved {
            self.load(&path);
        }
    }

    /// Original `revealExplorerDirectory`: lists and expands the path to a directory selected
    /// outside the tree, showing hidden files for a hidden one.
    pub fn reveal(&mut self, directory: &str) {
        let directory = directory.trim_end_matches('/');
        let directory = if directory.is_empty() { "." } else { directory };
        if is_hidden_explorer_path(directory) && !self.hidden {
            self.hidden = true;
            self.flatten();
            self.save_preference("explorerShowHiddenFiles", Value::Bool(true));
        }
        let mut paths = vec![".".to_owned()];
        if directory != "." {
            let segments: Vec<&str> = directory.split('/').filter(|segment| !segment.is_empty()).collect();
            for length in 1..=segments.len() {
                paths.push(segments[..length].join("/"));
            }
        }
        self.reveal = Some(Reveal { generation: self.generation, paths: paths.clone(), index: 0 });
        // Every step re-lists (original `_load` per path).
        self.inflight.remove(&paths[0]);
        self.load(&paths[0]);
        self.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(name: &str, path: &str, kind: &str) -> Value {
        json!({"name": name, "path": path, "kind": kind, "size": 1, "modifiedAt": ""})
    }

    /// Answers every listing from `tree` (path → rows).
    fn run(state: &mut ExplorerState, tree: &HashMap<&str, Vec<Value>>) -> Vec<String> {
        let mut listed = Vec::new();
        loop {
            let cmds = std::mem::take(&mut state.cmds);
            let lists: Vec<(String, u64)> = cmds
                .into_iter()
                .filter_map(|cmd| match cmd {
                    ExplorerCmd::List { path, generation } => Some((path, generation)),
                    _ => None,
                })
                .collect();
            if lists.is_empty() {
                return listed;
            }
            for (path, generation) in lists {
                listed.push(path.clone());
                let result = tree.get(path.as_str()).cloned().ok_or_else(|| "missing".to_owned());
                state.on_listed(&path, generation, result.map(|entries| Listing { entries, absolute: None }));
            }
        }
    }

    fn tree() -> HashMap<&'static str, Vec<Value>> {
        HashMap::from([
            (".", vec![entry("src", "src", "directory"), entry(".git", ".git", "directory"), entry("README.md", "README.md", "file")]),
            ("src", vec![entry("lib", "src/lib", "directory"), entry("b.rs", "src/b.rs", "file")]),
            ("src/lib", vec![entry("c.rs", "src/lib/c.rs", "file")]),
            (".git", vec![entry("HEAD", ".git/HEAD", "file")]),
        ])
    }

    #[test]
    fn restores_persisted_expanded_directories_level_by_level() {
        let stored = json!({"expandedPathsByWorkspace": {"workspace:main": [".", "src", "src/lib", "gone"]}});
        let mut state = ExplorerState::new(
            "/repo",
            ".",
            false,
            Some(ExplorerPreferences { workspace_key: "workspace:main".into() }),
            &stored,
        );
        state.start(&stored);
        let listed = run(&mut state, &tree());
        assert_eq!(listed, vec![".", "src", "src/lib"]);
        assert!(state.is_expanded("src") && state.is_expanded("src/lib"));
        assert!(!state.is_expanded("gone"));
        let names: Vec<&str> = state.rows().iter().map(|row| row.entry.name.as_str()).collect();
        assert_eq!(names, vec![".git", "src", "lib", "c.rs", "b.rs", "README.md"]);
    }

    #[test]
    fn hidden_files_off_hides_dot_entries_and_toggle_relists() {
        let stored = json!({"explorerShowHiddenFiles": false, "expandedPathsByWorkspace": {"root:/repo": [".", ".git"]}});
        let mut state = ExplorerState::new(
            "/repo",
            ".",
            false,
            Some(ExplorerPreferences { workspace_key: "root:/repo".into() }),
            &stored,
        );
        state.start(&stored);
        run(&mut state, &tree());
        assert!(state.rows().iter().all(|row| !row.entry.name.starts_with('.')));
        // The hidden persisted directory stays expanded while hidden files are off.
        assert!(state.is_expanded(".git"));
        state.toggle_hidden();
        let listed = run(&mut state, &tree());
        assert_eq!(listed, vec![".git"]);
        assert!(state.rows().iter().any(|row| row.entry.name == "HEAD"));
    }

    #[test]
    fn reveal_collapse_and_rename() {
        let mut state = ExplorerState::new("/repo", ".", false, None, &json!({}));
        state.start(&json!({}));
        run(&mut state, &tree());
        state.reveal("src/lib/");
        run(&mut state, &tree());
        assert!(state.is_expanded("src") && state.is_expanded("src/lib"));
        state.renamed("src", "source");
        assert!(state.is_expanded("source") && state.is_expanded("source/lib"));
        state.collapse("source");
        assert!(!state.is_expanded("source") && !state.is_expanded("source/lib"));
    }

    #[test]
    fn reconcile_keeps_user_changes_made_during_restore() {
        let persisted: HashSet<String> = [".", "a", "b"].iter().map(|s| s.to_string()).collect();
        let current: HashSet<String> = [".", "a", "c"].iter().map(|s| s.to_string()).collect();
        let restored: Vec<String> = [".", "a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(reconcile_restored_expanded_paths(&persisted, &current, &restored), vec![".", "a", "c"]);
    }

    #[test]
    fn picker_history_navigation() {
        let mut state = ExplorerState::new("/home/me", ".", true, None, &json!({}));
        state.start(&json!({}));
        state.navigate("/srv", true);
        assert!(state.can_go_back());
        state.back();
        assert_eq!(state.path(), ".");
        assert_eq!(state.current_directory(), "/home/me");
        assert!(state.can_go_forward());
        state.forward();
        assert_eq!(state.path(), "/srv");
        state.parent();
        assert_eq!(state.path(), "/");
        assert!(!state.can_go_up());
        state.parent();
        assert_eq!(state.path(), "/");
        state.back();
        assert_eq!(state.path(), "/srv");
    }

    /// Answers the picker's pending listings as a host reporting `absolutePath`.
    fn answer_absolute(state: &mut ExplorerState, absolute: &dyn Fn(&str) -> String, rows: Vec<Value>) {
        for cmd in std::mem::take(&mut state.cmds) {
            if let ExplorerCmd::List { path, generation } = cmd {
                let listing = Listing { entries: rows.clone(), absolute: Some(absolute(&path)) };
                state.on_listed(&path, generation, Ok(listing));
            }
        }
    }

    #[test]
    fn picker_adopts_the_reported_absolute_path_and_lists_one_flat_level() {
        let mut state = ExplorerState::new("~", ".", true, None, &json!({}));
        state.start(&json!({}));
        let home = |path: &str| if path == "." { "/home/me".to_owned() } else { path.to_owned() };
        answer_absolute(&mut state, &home, vec![
            entry("work", "work", "directory"),
            entry("notes.md", "notes.md", "file"),
            entry(".config", ".config", "directory"),
        ]);
        assert_eq!(state.path(), "/home/me");
        assert_eq!(state.home(), Some("/home/me"));
        let names: Vec<&str> = state.rows().iter().map(|row| row.entry.name.as_str()).collect();
        assert_eq!(names, vec![".config", "work"]);
        let work = state.rows()[1].entry.clone();
        assert_eq!(state.child_directory(&work), "/home/me/work");
        state.navigate(&state.child_directory(&work), true);
        // A nested listing stays flat: opening never expands rows.
        answer_absolute(&mut state, &home, vec![entry("src", "work/src", "directory")]);
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.parent_directory().as_deref(), Some("/home/me"));
        state.back();
        assert_eq!(state.path(), "/home/me");
        state.go_to("work/../work/src");
        assert_eq!(state.path(), "/home/me/work/src");
        state.go_to("~/x");
        assert_eq!(state.path(), "/home/me/x");
    }

    #[test]
    fn picker_known_home_aliases_keep_absolute_location_and_history_while_loading() {
        let mut state = ExplorerState::new("~", ".", true, None, &json!({}));
        state.start(&json!({}));
        answer_absolute(&mut state, &|_| "/home/me".to_owned(), Vec::new());
        state.go_to("~");
        assert_eq!(state.path(), "/home/me");
        assert_eq!(state.current_directory(), "/home/me");
        assert_eq!(state.history, vec!["/home/me"]);
        assert!(!state.can_go_back());
        assert!(state.is_loading("/home/me"));
        answer_absolute(&mut state, &str::to_owned, Vec::new());
        state.go_to("~/work");
        assert_eq!(state.path(), "/home/me/work");
        answer_absolute(&mut state, &str::to_owned, Vec::new());
        state.go_to("~");
        assert_eq!(state.current_directory(), "/home/me");
        assert_eq!(state.history, vec!["/home/me", "/home/me/work", "/home/me"]);
        state.back();
        assert_eq!(state.path(), "/home/me/work");
        state.forward();
        assert_eq!(state.path(), "/home/me");
        state.parent();
        assert_eq!(state.path(), "/home");
        state.refresh();
        assert!(state.refreshing());
        answer_absolute(&mut state, &str::to_owned, Vec::new());
        assert!(!state.refreshing());
        assert_eq!(state.path(), "/home");
    }

    #[test]
    fn picker_late_initial_response_resolves_history_without_changing_location() {
        let mut state = ExplorerState::new("~", ".", true, None, &json!({}));
        state.start(&json!({}));
        state.go_to("/home/me");
        state.go_to("/srv");
        // Initial `.` resolves after navigation, not while it is the visible location.
        state.on_listed(".", 0, Ok(Listing { entries: Vec::new(), absolute: Some("/home/me".to_owned()) }));
        assert_eq!(state.current_directory(), "/srv");
        assert_eq!(state.history, vec!["/home/me", "/srv"]);
        assert_eq!(state.history_index, Some(1));
        state.back();
        assert_eq!(state.path(), "/home/me");
        assert!(!state.can_go_back());
        state.forward();
        assert_eq!(state.path(), "/srv");
    }

    #[test]
    fn picker_initial_absolute_cwd_does_not_add_history_for_same_typed_path() {
        let mut state = ExplorerState::new("/home/me", ".", true, None, &json!({}));
        state.start(&json!({}));
        state.go_to("/home/me");
        assert_eq!(state.history, vec!["/home/me"]);
        assert!(!state.can_go_back());
        answer_absolute(&mut state, &|_| "/home/me".to_owned(), Vec::new());
        assert_eq!(state.history, vec!["/home/me"]);
        assert_eq!(state.current_directory(), "/home/me");
        assert_eq!(state.home(), None);
        // An absolute cwd is not evidence that this directory is the remote home.
        state.go_to("~");
        assert_eq!(state.path(), "~");
        answer_absolute(&mut state, &|_| "/home/me".to_owned(), Vec::new());
        assert_eq!(state.history, vec!["/home/me"]);
        assert!(!state.can_go_back());
    }

    #[test]
    fn picker_reset_discards_home_and_old_responses() {
        let mut state = ExplorerState::new("~", ".", true, None, &json!({}));
        state.start(&json!({}));
        answer_absolute(&mut state, &|_| "/home/old".to_owned(), Vec::new());
        state.go_to("~/pending");
        state.reset_cwd("/srv/new", &json!({}));
        assert_eq!(state.home(), None);
        assert_eq!(state.current_directory(), "/srv/new");
        assert_eq!(state.cmds.len(), 1);
        state.on_listed("~/pending", 0, Ok(Listing { entries: Vec::new(), absolute: Some("/home/old/pending".to_owned()) }));
        assert_eq!(state.home(), None);
        state.go_to("~");
        assert_eq!(state.current_directory(), "~");
        answer_absolute(&mut state, &|path| if path == "." { "/srv/new" } else { "/users/new" }.to_owned(), Vec::new());
        assert_eq!(state.home(), Some("/users/new"));
        assert_eq!(state.current_directory(), "/users/new");
    }

    #[test]
    fn picker_home_expansion_uses_remote_windows_and_unc_style() {
        for home in ["C:\\Users\\me", "\\\\server\\share\\me"] {
            let mut state = ExplorerState::new("~", ".", true, None, &json!({}));
            state.start(&json!({}));
            answer_absolute(&mut state, &|_| home.to_owned(), Vec::new());
            state.go_to("~");
            assert_eq!(state.history, vec![home]);
            state.go_to("~/work");
            assert_eq!(state.path(), format!("{home}\\work"));
            assert_eq!(state.current_directory(), format!("{home}\\work"));
            state.parent();
            assert_eq!(state.path(), home);
            state.go_to("~/..");
            assert_eq!(state.path(), Style::Windows.dirname(home));
        }
    }

    #[test]
    fn picker_keeps_host_reported_symlink_spelling() {
        let mut state = ExplorerState::new("~/linked", ".", true, None, &json!({}));
        state.start(&json!({}));
        answer_absolute(&mut state, &|_| "/home/me/linked".to_owned(), Vec::new());
        assert_eq!(state.current_directory(), "/home/me/linked");
        assert_eq!(state.home(), Some("/home/me"));
        state.go_to("~/linked");
        assert_eq!(state.history, vec!["/home/me/linked"]);
        assert_eq!(state.child_directory(&Entry::from_json(&entry("child", "child", "directory"))), "/home/me/linked/child");
    }

    #[test]
    fn picker_unresolved_parent_alias_is_not_a_fake_absolute_directory_or_home() {
        let mut state = ExplorerState::new("~/..", ".", true, None, &json!({}));
        state.start(&json!({}));
        assert_eq!(state.current_directory(), "~/..");
        state.on_listed(".", 0, Ok(Listing { entries: Vec::new(), absolute: Some("relative".to_owned()) }));
        assert_eq!(state.current_directory(), "~/..");
        assert_eq!(state.home(), None);
        state.go_to("~");
        assert_eq!(state.path(), "~");
        state.go_to("~/..");
        assert_eq!(state.path(), "~/..");
        answer_absolute(&mut state, &|path| if path == "~" { "/home/me" } else { "/home" }.to_owned(), Vec::new());
        assert_eq!(state.current_directory(), "/home");
        assert_eq!(state.home(), Some("/home/me"));
        assert_eq!(home_of("~/..", "/home"), None);
    }

    #[test]
    fn picker_falls_back_to_text_paths_on_older_hosts() {
        let mut state = ExplorerState::new("~", ".", true, None, &json!({}));
        state.start(&json!({}));
        run(&mut state, &HashMap::from([(".", vec![entry("work", "work", "directory")])]));
        assert_eq!(state.current_directory(), "~");
        assert!(!state.can_go_up());
        let work = state.rows()[0].entry.clone();
        assert_eq!(state.child_directory(&work), "~/work");
        state.navigate("~/work", true);
        assert_eq!(state.parent_directory().as_deref(), Some("~"));
    }

    #[test]
    fn picker_learns_home_from_tilde_paths() {
        assert_eq!(home_of("~", "/home/me").as_deref(), Some("/home/me"));
        assert_eq!(home_of("~/work/x", "/home/me/work/x").as_deref(), Some("/home/me"));
        assert_eq!(home_of("~/work/", "C:\\Users\\me\\work").as_deref(), Some("C:\\Users\\me"));
        assert_eq!(home_of("~/work", "/srv/elsewhere"), None);
        assert_eq!(home_of("/srv", "/srv"), None);
        let mut state = ExplorerState::new("~/work", ".", true, None, &json!({}));
        state.start(&json!({}));
        answer_absolute(&mut state, &|_: &str| "/home/me/work".to_owned(), Vec::new());
        assert_eq!(state.home(), Some("/home/me"));
        assert_eq!(state.path(), "/home/me/work");
    }

    #[test]
    fn parent_of_roots_is_disabled() {
        assert_eq!(parent_directory("/"), None);
        assert_eq!(parent_directory("/srv"), Some("/".to_owned()));
        assert_eq!(parent_directory("/srv/app/"), Some("/srv".to_owned()));
        assert_eq!(parent_directory("C:\\"), None);
        assert_eq!(parent_directory("C:\\Users\\me"), Some("C:\\Users".to_owned()));
        assert_eq!(parent_directory("C:\\Users"), Some("C:\\".to_owned()));
        assert_eq!(parent_directory("\\\\server\\share\\"), None);
        assert_eq!(parent_directory("~"), None);
        assert_eq!(parent_directory("."), None);
    }
}
