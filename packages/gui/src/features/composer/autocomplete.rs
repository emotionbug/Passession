//! Port of `composer/autocomplete.dart`: slash command and @file mention detection, client
//! commands, tiered fuzzy ranking and the popover's option/selection model. Text indices are
//! UTF-8 byte offsets, the unit of Makepad's `TextInput` cursor.

use super::json::*;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommandRange {
    pub start: usize,
    pub end: usize,
    pub query: String,
    pub at_start: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMentionRange {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

fn clamp_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn prev_char(text: &str, index: usize) -> Option<char> {
    text[..index].chars().next_back()
}

/// findActiveSlashCommand: the nearest `/` before the cursor that starts the text or follows
/// whitespace, with no slash, whitespace or quote after it.
pub fn find_active_slash_command(text: &str, cursor: usize) -> Option<SlashCommandRange> {
    let cursor = clamp_boundary(text, cursor);
    let before = &text[..cursor];
    for (slash, _) in before.rmatch_indices('/') {
        if slash > 0 && !prev_char(text, slash).is_some_and(char::is_whitespace) {
            continue;
        }
        let query = &before[slash + 1..];
        if query.chars().any(|c| c == '/' || c.is_whitespace() || c == '"' || c == '\'') {
            continue;
        }
        return Some(SlashCommandRange { start: slash, end: cursor, query: query.to_owned(), at_start: slash == 0 });
    }
    None
}

/// findActiveFileMention: the nearest `@` before the cursor that is not part of an email
/// address, with no whitespace, quote or angle bracket after it.
pub fn find_active_file_mention(text: &str, cursor: usize) -> Option<FileMentionRange> {
    let cursor = clamp_boundary(text, cursor);
    let before = &text[..cursor];
    for (at, _) in before.rmatch_indices('@') {
        if at > 0 && prev_char(before, at).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+') {
            continue;
        }
        let query = &before[at + 1..];
        if query.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>')) {
            continue;
        }
        return Some(FileMentionRange { start: at, end: cursor, query: query.to_owned() });
    }
    None
}

/// applySlashCommandReplacement: a trailing space only when the command ends the text.
pub fn apply_slash_command_replacement(text: &str, command: &SlashCommandRange, name: &str) -> String {
    let replaced = format!("{}/{}{}", &text[..command.start], name, &text[command.end..]);
    if command.end == text.len() { format!("{replaced} ") } else { replaced }
}

/// applyFileMentionReplacement: the mention becomes the quoted relative path.
pub fn apply_file_mention_replacement(text: &str, mention: &FileMentionRange, path: &str) -> String {
    format!("{}\"{}\"{}", &text[..mention.start], path.replace('"', "\\\""), &text[mention.end..])
}

pub struct ClientCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description_key: &'static str,
}

/// client-slash-commands CLIENT_SLASH_COMMANDS.
pub const CLIENT_SLASH_COMMANDS: &[ClientCommand] = &[
    ClientCommand { name: "exit", aliases: &["quit", "q"], description_key: "composer.clientCommands.archiveAgent" },
    ClientCommand { name: "clear", aliases: &["new"], description_key: "composer.clientCommands.freshDraft" },
];

/// resolveClientSlashCommand: the canonical name of a whole-text client command.
pub fn resolve_client_slash_command(text: &str, has_attachments: bool) -> Option<&'static str> {
    if has_attachments {
        return None;
    }
    let trimmed = text.trim();
    let name = trimmed.strip_prefix('/')?;
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return None;
    }
    CLIENT_SLASH_COMMANDS
        .iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
        .map(|command| command.name)
}

const TIER_EXACT: i64 = 0;
const TIER_WHOLE_WORD: i64 = 1;
const TIER_PREFIX: i64 = 2;
const TIER_WORD_START: i64 = 3;
const TIER_SUBSTRING: i64 = 4;
const TIER_SUBSEQUENCE: i64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchScore {
    pub tier: i64,
    pub offset: i64,
    pub spread: i64,
}

fn boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(c) => !(c.is_ascii_lowercase() || c.is_ascii_digit()),
    }
}

fn score_match(query: &str, text: &str) -> Option<MatchScore> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    if q.is_empty() || t == q {
        return Some(MatchScore { tier: TIER_EXACT, offset: 0, spread: 0 });
    }
    let mut best: Option<MatchScore> = None;
    if q.len() <= t.len() {
        for found in 0..=(t.len() - q.len()) {
            if t[found..found + q.len()] != q[..] {
                continue;
            }
            let starts = found == 0 || boundary(Some(t[found - 1]));
            let ends = found + q.len() >= t.len() || boundary(t.get(found + q.len()).copied());
            let tier = if starts && ends {
                TIER_WHOLE_WORD
            } else if found == 0 {
                TIER_PREFIX
            } else if starts {
                TIER_WORD_START
            } else {
                TIER_SUBSTRING
            };
            let offset = found as i64;
            if best.is_none_or(|b| tier < b.tier || (tier == b.tier && offset < b.offset)) {
                best = Some(MatchScore { tier, offset, spread: 0 });
            }
        }
    }
    if best.is_some() {
        return best;
    }
    let (mut qi, mut first, mut last) = (0usize, -1i64, -1i64);
    let mut ti = 0usize;
    while ti < t.len() && qi < q.len() {
        if t[ti].is_whitespace() {
            qi = 0;
            first = -1;
            last = -1;
            ti += 1;
            continue;
        }
        if t[ti] == q[qi] {
            if first == -1 {
                first = ti as i64;
            }
            last = ti as i64;
            qi += 1;
        }
        ti += 1;
    }
    if qi != q.len() || first == -1 {
        return None;
    }
    Some(MatchScore { tier: TIER_SUBSEQUENCE, offset: first, spread: last - first + 1 })
}

pub fn compare_match_scores(a: &MatchScore, b: &MatchScore) -> std::cmp::Ordering {
    a.tier.cmp(&b.tier).then(a.offset.cmp(&b.offset)).then(a.spread.cmp(&b.spread))
}

/// scoreTextFields: every query token must match one field; scores add up.
pub fn score_text_fields(query: &str, fields: &[String]) -> Option<MatchScore> {
    let lowered = query.trim().to_lowercase();
    let (mut tier, mut offset, mut spread) = (0, 0, 0);
    for token in lowered.split_whitespace() {
        let mut best: Option<MatchScore> = None;
        for field in fields {
            if let Some(score) = score_match(token, field)
                && best.is_none_or(|b| compare_match_scores(&score, &b).is_lt())
            {
                best = Some(score);
            }
        }
        let best = best?;
        tier += best.tier;
        offset += best.offset;
        spread += if best.tier == TIER_SUBSEQUENCE { best.spread } else { token.chars().count() as i64 };
    }
    Some(MatchScore { tier, offset, spread })
}

/// A command candidate: (name, aliases, argumentHint, description, client command).
type CommandEntry = (String, Vec<String>, String, String, bool);

#[derive(Clone, Debug, PartialEq)]
pub struct AutocompleteOption {
    pub id: String,
    pub label: String,
    /// "command", "file" or "directory".
    pub kind: String,
    pub detail: Option<String>,
    /// For client commands this is a localization key.
    pub description: Option<String>,
    pub client_command: bool,
    pub entry_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutocompleteMode {
    Command,
    File,
}

/// What a key press did to the popover.
#[derive(Clone, Debug, PartialEq)]
pub enum AutocompleteKey {
    Ignored,
    Moved,
    Select(AutocompleteOption),
    Escape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKey {
    Up,
    Down,
    Tab,
    Enter,
    Escape,
}

/// hooks/use-agent-autocomplete.ts + use-autocomplete.ts state. File lookups are issued by
/// the owner: [`ComposerAutocomplete::update`] returns the query that needs a debounced fetch.
#[derive(Default)]
pub struct ComposerAutocomplete {
    text: String,
    cursor: usize,
    pub slash: Option<SlashCommandRange>,
    pub mention: Option<FileMentionRange>,
    pub provider_commands: Vec<Value>,
    pub commands_loading: bool,
    pub commands_error: Option<String>,
    pub is_draft: bool,
    /// Why commands cannot be listed yet (draft without a model), a localization key.
    pub draft_empty_key: Option<&'static str>,
    pub can_load_files: bool,
    file_entries: Vec<Value>,
    file_pending: bool,
    file_error: Option<String>,
    loaded_file_query: Option<String>,
    /// The file query whose debounce is running.
    pub pending_file_query: Option<String>,
    pub file_generation: u64,
    pub selected_index: Option<usize>,
    previous_query: String,
    pub options: Vec<AutocompleteOption>,
}

impl ComposerAutocomplete {
    pub fn new() -> Self {
        Self { can_load_files: true, ..Default::default() }
    }

    pub fn mode(&self) -> Option<AutocompleteMode> {
        if self.mention.is_some() {
            Some(AutocompleteMode::File)
        } else if self.slash.is_some() {
            Some(AutocompleteMode::Command)
        } else {
            None
        }
    }

    pub fn visible(&self) -> bool {
        match self.mode() {
            Some(AutocompleteMode::Command) => !self.commands_loading || self.draft_empty_key.is_some(),
            Some(AutocompleteMode::File) => self.can_load_files,
            None => false,
        }
    }

    pub fn is_loading(&self) -> bool {
        match self.mode() {
            Some(AutocompleteMode::Command) => self.commands_loading && self.options.is_empty(),
            Some(AutocompleteMode::File) => self.file_pending && self.options.is_empty(),
            None => false,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self.mode() {
            Some(AutocompleteMode::Command) => self.commands_error.as_deref(),
            Some(AutocompleteMode::File) => self.file_error.as_deref(),
            None => None,
        }
    }

    pub fn loading_key(&self) -> &'static str {
        if self.mode() == Some(AutocompleteMode::File) {
            "agentAutocomplete.searchingWorkspace"
        } else {
            "agentAutocomplete.loadingCommands"
        }
    }

    pub fn empty_key(&self) -> &'static str {
        if self.mode() == Some(AutocompleteMode::File) {
            "agentAutocomplete.noFiles"
        } else {
            self.draft_empty_key.unwrap_or("agentAutocomplete.noCommands")
        }
    }

    fn query(&self) -> String {
        match (&self.mention, &self.slash) {
            (Some(mention), _) => mention.query.clone(),
            (None, Some(slash)) => slash.query.clone(),
            _ => String::new(),
        }
    }

    /// The text or cursor changed. Returns `Some(query)` when a file lookup must be
    /// (re)scheduled after the 180ms debounce; `None` cancels any pending debounce.
    pub fn update(&mut self, text: &str, cursor: usize) -> Option<String> {
        self.text = text.to_owned();
        self.cursor = clamp_boundary(text, cursor);
        self.slash = find_active_slash_command(&self.text, self.cursor);
        self.mention = find_active_file_mention(&self.text, self.cursor);
        let mut schedule = None;
        if let Some(mention) = &self.mention
            && self.loaded_file_query.as_deref() != Some(mention.query.as_str())
        {
            if self.loaded_file_query.is_none() {
                self.file_pending = true;
            }
            schedule = Some(mention.query.clone());
        }
        self.pending_file_query = schedule.clone();
        self.rebuild();
        schedule
    }

    pub fn set_commands(&mut self, commands: Vec<Value>, loading: bool, error: Option<String>) {
        self.provider_commands = commands;
        self.commands_loading = loading;
        self.commands_error = error;
        self.rebuild();
    }

    /// Start a debounced file fetch; returns its generation for the response guard.
    pub fn begin_file_fetch(&mut self) -> u64 {
        self.file_generation += 1;
        self.file_generation
    }

    /// directory_suggestions_response for `query` (generation-guarded by the caller).
    pub fn files_loaded(&mut self, generation: u64, query: &str, result: Result<Vec<Value>, String>) {
        if generation != self.file_generation {
            return;
        }
        match result {
            Ok(entries) => {
                self.file_entries = entries
                    .into_iter()
                    .filter(|e| matches!(s(get(e, "kind")), "file" | "directory"))
                    .collect();
                self.file_error = None;
            }
            Err(error) => self.file_error = Some(error),
        }
        self.loaded_file_query = Some(query.to_owned());
        self.file_pending = false;
        self.rebuild();
    }

    fn rebuild(&mut self) {
        self.options = self.build_options();
        let query = self.query();
        let changed = self.previous_query != query;
        self.previous_query = query;
        if !self.visible() || self.options.is_empty() {
            self.selected_index = None;
        } else if changed || self.selected_index.is_none_or(|i| i >= self.options.len()) {
            self.selected_index = Some(self.options.len() - 1);
        }
    }

    fn build_options(&self) -> Vec<AutocompleteOption> {
        if !self.visible() {
            return Vec::new();
        }
        if self.mode() == Some(AutocompleteMode::File) {
            return self
                .file_entries
                .iter()
                .rev()
                .map(|entry| AutocompleteOption {
                    id: format!("{}:{}", s(get(entry, "kind")), s(get(entry, "path"))),
                    label: owned(get(entry, "path")),
                    kind: owned(get(entry, "kind")),
                    detail: None,
                    description: None,
                    client_command: false,
                    entry_path: Some(owned(get(entry, "path"))),
                })
                .collect();
        }
        let Some(slash) = &self.slash else {
            return Vec::new();
        };
        // (name, aliases, argumentHint, description, client)
        let mut entries: Vec<CommandEntry> = Vec::new();
        if slash.at_start {
            let mut claimed: Vec<String> = Vec::new();
            for command in CLIENT_SLASH_COMMANDS {
                if !self.is_draft {
                    entries.push((
                        command.name.into(),
                        command.aliases.iter().map(|a| a.to_string()).collect(),
                        String::new(),
                        command.description_key.into(),
                        true,
                    ));
                }
                claimed.push(command.name.into());
                claimed.extend(command.aliases.iter().map(|a| a.to_string()));
            }
            for command in &self.provider_commands {
                let name = owned(get(command, "name"));
                if !claimed.contains(&name) {
                    entries.push(provider_entry(command));
                }
            }
        } else {
            for command in &self.provider_commands {
                if s(get(command, "kind")) == "skill" {
                    entries.push(provider_entry(command));
                }
            }
        }
        let query = slash.query.trim().to_lowercase();
        if !query.is_empty() {
            let mut scored: Vec<(MatchScore, CommandEntry)> = entries
                .into_iter()
                .filter_map(|entry| {
                    let mut fields = vec![entry.0.clone()];
                    fields.extend(entry.1.iter().cloned());
                    score_text_fields(&query, &fields).map(|score| (score, entry))
                })
                .collect();
            scored.sort_by(|a, b| compare_match_scores(&a.0, &b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
            entries = scored.into_iter().map(|(_, entry)| entry).collect();
        }
        entries
            .into_iter()
            .rev()
            .map(|(name, _, hint, description, client)| AutocompleteOption {
                id: name.clone(),
                label: format!("/{name}"),
                kind: "command".into(),
                detail: if hint.is_empty() { None } else { Some(hint) },
                description: Some(description),
                client_command: client,
                entry_path: None,
            })
            .collect()
    }

    /// use-autocomplete.ts onKeyPress. `repeat` marks auto-repeat key events (navigation only).
    pub fn handle_key(&mut self, key: NavKey, repeat: bool) -> AutocompleteKey {
        if !self.visible() || self.options.is_empty() {
            return AutocompleteKey::Ignored;
        }
        let count = self.options.len();
        match key {
            NavKey::Up | NavKey::Down => {
                let down = key == NavKey::Down;
                self.selected_index = Some(match self.selected_index {
                    None => if down { 0 } else { count - 1 },
                    Some(index) if down => (index % count + 1) % count,
                    Some(index) => (index % count + count - 1) % count,
                });
                AutocompleteKey::Moved
            }
            NavKey::Tab | NavKey::Enter if !repeat => {
                let index = self.selected_index.filter(|i| *i < count).unwrap_or(count - 1);
                AutocompleteKey::Select(self.options[index].clone())
            }
            NavKey::Escape
                if !repeat
                    && self.mode() == Some(AutocompleteMode::Command)
                    && self.slash.as_ref().is_some_and(|s| s.at_start) =>
            {
                AutocompleteKey::Escape
            }
            _ => AutocompleteKey::Ignored,
        }
    }

    /// The text after applying `option`; None when nothing applies.
    pub fn applied(&self, option: &AutocompleteOption) -> Option<String> {
        if option.kind == "command" {
            return Some(match &self.slash {
                None => format!("/{} ", option.id),
                Some(range) => apply_slash_command_replacement(&self.text, range, &option.id),
            });
        }
        let range = self.mention.as_ref()?;
        let path = option.entry_path.as_ref()?;
        Some(apply_file_mention_replacement(&self.text, range, path))
    }
}

fn provider_entry(command: &Value) -> CommandEntry {
    (
        owned(get(command, "name")),
        list_ref(get(command, "aliases")).iter().map(display).collect(),
        owned(get(command, "argumentHint")),
        owned(get(command, "description")),
        false,
    )
}

/// The popover strips the ⚡ emoji and variation selector from labels and descriptions.
pub fn clean_label(value: &str) -> String {
    value.replace(['\u{26A1}', '\u{FE0F}'], "").trim().to_owned()
}
