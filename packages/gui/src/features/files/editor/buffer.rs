//! The source editor's text model (Flutter `SourceController` + the `TextEditingValue` it edits):
//! a `\n`-separated buffer with a line index, a selection in byte offsets, grapheme/word
//! navigation, grouped undo/redo and literal find/replace. All offsets are UTF-8 byte offsets on
//! char boundaries.

use std::ops::Range;

use makepad_widgets::event::TextImeEvent;
use makepad_widgets::text_input::{TextInputKeyCommand, TextInputMotion, text_input_word_boundary};
use unicode_segmentation::UnicodeSegmentation;

use super::geometry::Geometry;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(offset: usize) -> Selection {
        Selection {
            anchor: offset,
            head: offset,
        }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditKind {
    Insert,
    Backspace,
    Delete,
    Ime,
    Other,
}

#[derive(Clone, Debug)]
struct Edit {
    start: usize,
    removed: String,
    inserted: String,
}

#[derive(Clone, Debug)]
struct Group {
    kind: EditKind,
    edits: Vec<Edit>,
    before: Selection,
    after: Selection,
}

#[derive(Default)]
pub struct TextBuffer {
    text: String,
    line_starts: Vec<usize>,
    pub(super) geometry: Geometry,
    find: FindCache,
    pub selection: Selection,
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Typing into the last group extends it until this is cleared (caret moved, other edit).
    open_group: bool,
    /// Bumps on every text change.
    pub revision: u64,
    ime_composing: bool,
}

impl TextBuffer {
    pub fn new(text: String) -> TextBuffer {
        let mut buffer = TextBuffer {
            text,
            line_starts: Vec::new(),
            ..TextBuffer::default()
        };
        buffer.index();
        buffer
    }

    fn index(&mut self) {
        self.line_starts.clear();
        self.line_starts.push(0);
        self.line_starts.extend(newline_starts(&self.text, 0));
        self.geometry = Geometry::default();
    }

    pub(super) fn ensure_geometry(&mut self, cols: usize, font_size: f64) -> bool {
        self.geometry
            .update(&self.text, &self.line_starts, cols, font_size)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts[line.min(self.line_starts.len() - 1)]
    }

    /// The end of `line`, before its `\n`.
    pub fn line_end(&self, line: usize) -> usize {
        match self.line_starts.get(line + 1) {
            Some(next) => next - 1,
            None => self.text.len(),
        }
    }

    pub fn line(&self, line: usize) -> &str {
        &self.text[self.line_start(line)..self.line_end(line)]
    }

    /// Zero-based line of a byte offset.
    pub fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset.min(self.text.len())) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    /// One-based (line, column) with the column in characters (Flutter `locationAt`).
    pub fn location(&self, offset: usize) -> (usize, usize) {
        let offset = floor_boundary(&self.text, offset.min(self.text.len()));
        let line = self.line_of(offset);
        let column = self.text[self.line_start(line)..offset].chars().count();
        (line + 1, column + 1)
    }

    /// Byte offset of a one-based (line, column), clamped into the line (Flutter `offsetAt`).
    pub fn offset_at(&self, line: usize, column: usize) -> usize {
        let line = (line.max(1) - 1).min(self.line_count() - 1);
        let start = self.line_start(line);
        let end = self.line_end(line);
        match self.text[start..end].char_indices().nth(column.max(1) - 1) {
            Some((index, _)) => start + index,
            None => end,
        }
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection.start()..self.selection.end()]
    }

    /// Replaces the whole text (a reload or a remote change); selection is clamped and history
    /// dropped, as a controller value replacement did.
    pub fn set_text(&mut self, text: String) {
        if text == self.text {
            return;
        }
        self.text = text;
        self.index();
        let clamp = |offset: usize, text: &str| floor_boundary(text, offset.min(text.len()));
        self.selection = Selection {
            anchor: clamp(self.selection.anchor, &self.text),
            head: clamp(self.selection.head, &self.text),
        };
        self.undo.clear();
        self.redo.clear();
        self.open_group = false;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn set_selection(&mut self, selection: Selection) {
        let len = self.text.len();
        self.selection = Selection {
            anchor: floor_boundary(&self.text, selection.anchor.min(len)),
            head: floor_boundary(&self.text, selection.head.min(len)),
        };
        self.open_group = false;
    }

    /// Move one semantic endpoint without swapping the fixed end when handles cross.
    /// Layout hit tests use source byte offsets; clamp through the same UTF-8 boundary
    /// path as keyboard selection without editing text or disturbing IME composition.
    pub fn move_selection_handle(
        &mut self,
        endpoint: makepad_widgets::TextHandleEndpoint,
        offset: usize,
    ) {
        use makepad_widgets::TextHandleEndpoint;
        let selection = match endpoint {
            TextHandleEndpoint::Caret => Selection::caret(offset),
            TextHandleEndpoint::Anchor => Selection {
                anchor: offset,
                head: self.selection.head,
            },
            TextHandleEndpoint::Cursor => Selection {
                anchor: self.selection.anchor,
                head: offset,
            },
        };
        let composing_group = self.ime_composing && self.open_group;
        self.set_selection(selection);
        if composing_group {
            self.open_group = true;
            if let Some(group) = self.undo.last_mut() {
                group.after = self.selection;
            }
        }
    }

    /// Moves the head, keeping the anchor when extending.
    pub fn move_head(&mut self, head: usize, extend: bool) {
        let head = floor_boundary(&self.text, head.min(self.text.len()));
        self.selection = if extend {
            Selection {
                anchor: self.selection.anchor,
                head,
            }
        } else {
            Selection::caret(head)
        };
        self.open_group = false;
    }

    fn replace_text(&mut self, start: usize, end: usize, inserted: &str) {
        let first = self.line_of(start);
        let last = if start == end {
            first
        } else {
            self.line_of(end)
        };
        // A start at `end` belongs to a newline consumed by the edit. Starts strictly
        // after `end` survive, and only their absolute byte offsets change.
        let following = &mut self.line_starts[last + 1..];
        if inserted.len() != end - start {
            for offset in following {
                *offset = *offset - (end - start) + inserted.len();
            }
        }
        let old_starts = first + 1..last + 1;
        let mut new_starts = newline_starts(inserted, start).peekable();
        let mut added = 0;
        if !old_starts.is_empty() || new_starts.peek().is_some() {
            // `splice` consumes the inserted starts without scanning any surviving text.
            self.line_starts
                .splice(old_starts, new_starts.inspect(|_| added += 1));
        }
        self.geometry.invalidate(first..last + 1, added + 1);
        self.text.replace_range(start..end, inserted);
    }

    fn apply(&mut self, start: usize, end: usize, inserted: &str) -> Edit {
        let removed = self.text[start..end].to_owned();
        self.replace_text(start, end, inserted);
        self.revision = self.revision.wrapping_add(1);
        Edit {
            start,
            removed,
            inserted: inserted.to_owned(),
        }
    }

    /// Replaces `start..end` with `text`, recording undo; the caret lands after the insertion.
    pub fn replace(&mut self, start: usize, end: usize, text: &str, kind: EditKind) {
        let start = floor_boundary(&self.text, start.min(self.text.len()));
        let end = floor_boundary(&self.text, end.min(self.text.len())).max(start);
        if start == end && text.is_empty() {
            return;
        }
        let before = self.selection;
        let edit = self.apply(start, end, text);
        let after = Selection::caret(start + text.len());
        self.selection = after;
        self.redo.clear();
        let extend = self.open_group
            && kind != EditKind::Other
            && self
                .undo
                .last()
                .is_some_and(|group| group.kind == kind && group.after == before);
        if extend {
            let group = self.undo.last_mut().unwrap();
            group.edits.push(edit);
            group.after = after;
        } else {
            self.undo.push(Group {
                kind,
                edits: vec![edit],
                before,
                after,
            });
            if self.undo.len() > 500 {
                self.undo.remove(0);
            }
        }
        self.open_group = kind != EditKind::Other;
    }

    /// Inserts at the selection (typing, paste, IME commit).
    pub fn insert(&mut self, text: &str, kind: EditKind) {
        let (start, end) = (self.selection.start(), self.selection.end());
        self.replace(start, end, text, kind);
    }

    /// Apply the Android editor snapshot without resetting native undo history.
    pub fn apply_ime(&mut self, event: &TextImeEvent, single_line: bool) {
        let changed = self.text != event.text;
        let composition = self.ime_composing || event.composing.is_some();
        if changed {
            if !self.ime_composing && composition {
                self.open_group = false;
            }
            let (start, end, replacement) = event.replacement(&self.text);
            let replacement = if single_line && replacement.contains(['\n', '\r']) {
                std::borrow::Cow::Owned(replacement.replace(['\n', '\r'], " "))
            } else if replacement.contains('\r') {
                std::borrow::Cow::Owned(replacement.replace("\r\n", "\n").replace('\r', "\n"))
            } else {
                std::borrow::Cow::Borrowed(replacement)
            };
            let kind = if composition {
                EditKind::Ime
            } else if start == end && replacement.chars().count() == 1 {
                EditKind::Insert
            } else if replacement.is_empty() && self.selection.is_empty() {
                if end == self.selection.head {
                    EditKind::Backspace
                } else {
                    EditKind::Delete
                }
            } else {
                EditKind::Other
            };
            self.replace(start, end, &replacement, kind);
        }
        let (anchor, head) = event.selection_utf8();
        self.selection = Selection {
            anchor: floor_boundary(&self.text, anchor.min(self.text.len())),
            head: floor_boundary(&self.text, head.min(self.text.len())),
        };
        if changed && let Some(group) = self.undo.last_mut() {
            group.after = self.selection;
        }
        self.ime_composing = event.composing.is_some();
        if !self.ime_composing && (composition || !changed) {
            self.open_group = false;
        }
    }

    pub fn is_composing(&self) -> bool {
        self.ime_composing
    }

    /// Apply source-indexed commands; the view owns vertical visual-row movement.
    pub fn handle_edit_key(&mut self, command: TextInputKeyCommand, extend: bool, editable: bool) {
        use TextInputMotion::*;
        if self.ime_composing {
            return;
        }
        let (motion, delete) = match command {
            TextInputKeyCommand::Move(motion) => (motion, false),
            TextInputKeyCommand::Delete(motion) if editable => (motion, true),
            _ => return,
        };
        let head = self.selection.head;
        let target = match motion {
            Left if !delete && !extend && !self.selection.is_empty() => self.selection.start(),
            Right if !delete && !extend && !self.selection.is_empty() => self.selection.end(),
            Left => self.prev(head),
            Right => self.next(head),
            WordLeft | WordRight | NextWord => text_input_word_boundary(&self.text, head, motion),
            LineStart => self.line_start(self.line_of(head)),
            LineEnd => self.line_end(self.line_of(head)),
            DocumentStart => 0,
            DocumentEnd => self.len(),
            Up | Down => return,
        };
        if !delete {
            self.move_head(target, extend);
            return;
        }
        let (start, end) = if self.selection.is_empty() {
            (head.min(target), head.max(target))
        } else {
            (self.selection.start(), self.selection.end())
        };
        let kind = match motion {
            Left => EditKind::Backspace,
            Right => EditKind::Delete,
            _ => EditKind::Other,
        };
        self.replace(start, end, "", kind);
    }

    pub fn undo(&mut self) -> bool {
        let Some(group) = self.undo.pop() else {
            return false;
        };
        for edit in group.edits.iter().rev() {
            let end = edit.start + edit.inserted.len();
            self.replace_text(edit.start, end, &edit.removed);
        }
        self.revision = self.revision.wrapping_add(1);
        self.selection = group.before;
        self.redo.push(group);
        self.open_group = false;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(group) = self.redo.pop() else {
            return false;
        };
        for edit in &group.edits {
            let end = edit.start + edit.removed.len();
            self.replace_text(edit.start, end, &edit.inserted);
        }
        self.revision = self.revision.wrapping_add(1);
        self.selection = group.after;
        self.undo.push(group);
        self.open_group = false;
        true
    }

    pub fn close_group(&mut self) {
        self.open_group = false;
    }

    pub fn prev(&self, offset: usize) -> usize {
        prev_grapheme(&self.text, offset)
    }

    pub fn next(&self, offset: usize) -> usize {
        next_grapheme(&self.text, offset)
    }

    /// The word around `offset` (double click).
    pub fn word_at(&self, offset: usize) -> (usize, usize) {
        let line = self.line_of(offset);
        let (start, end) = (self.line_start(line), self.line_end(line));
        for (index, word) in self.text[start..end].split_word_bound_indices() {
            let (a, b) = (start + index, start + index + word.len());
            if offset >= a && offset < b {
                return (a, b);
            }
        }
        (offset, offset)
    }

    /// The whole line around `offset`, including its `\n` (triple click).
    pub fn line_range_at(&self, offset: usize) -> (usize, usize) {
        let line = self.line_of(offset);
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        (self.line_start(line), end)
    }

    /// Byte ranges of literal, non-overlapping matches (Flutter `_matches`).
    pub fn matches(&mut self, needle: &str, match_case: bool) -> &[Range<usize>] {
        self.find
            .matches(&self.text, self.revision, needle, match_case)
    }

    pub fn replace_match(&mut self, needle: &str, match_case: bool, replacement: &str) -> bool {
        let selection = self.selection;
        if selection.is_empty() {
            return false;
        }
        let selected = self.selected_text();
        let same = if match_case {
            selected == needle
        } else {
            selected.to_lowercase() == needle.to_lowercase()
        };
        if !same {
            return false;
        }
        self.replace(
            selection.start(),
            selection.end(),
            replacement,
            EditKind::Other,
        );
        true
    }

    pub fn replace_all(&mut self, needle: &str, match_case: bool, replacement: &str) -> bool {
        let matches = self
            .find
            .matches(&self.text, self.revision, needle, match_case);
        if matches.is_empty() {
            return false;
        }
        let mut out = String::with_capacity(self.text.len());
        let mut last = 0;
        for matched in matches {
            out.push_str(&self.text[last..matched.start]);
            out.push_str(replacement);
            last = matched.end;
        }
        out.push_str(&self.text[last..]);
        let head = floor_boundary(&out, self.selection.head);
        self.replace(0, self.len(), &out, EditKind::Other);
        self.set_selection(Selection::caret(head));
        true
    }
}

fn newline_starts(text: &str, start: usize) -> impl Iterator<Item = usize> + '_ {
    text.bytes()
        .enumerate()
        .filter(|&(_, byte)| byte == b'\n')
        .map(move |(index, _)| start + index + 1)
}

#[derive(Default)]
struct FindCache {
    needle: String,
    revision: u64,
    match_case: bool,
    matches: Vec<Range<usize>>,
    #[cfg(test)]
    scans: usize,
}

impl FindCache {
    fn matches(
        &mut self,
        text: &str,
        revision: u64,
        needle: &str,
        match_case: bool,
    ) -> &[Range<usize>] {
        if self.revision != revision || self.match_case != match_case || self.needle != needle {
            self.revision = revision;
            self.match_case = match_case;
            self.needle.clear();
            self.needle.push_str(needle);
            self.matches = find_matches(text, needle, match_case);
            #[cfg(test)]
            {
                self.scans += 1;
            }
        }
        &self.matches
    }
}

/// Match the complete lowercase expansion of each source character, without letting a
/// match start or end inside an expansion. Record byte ranges in the original text.
fn find_matches(text: &str, needle: &str, match_case: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    if match_case {
        return text
            .match_indices(needle)
            .map(|(index, matched)| index..index + matched.len())
            .collect();
    }
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let (first, rest) = needle.split_first().unwrap();
    let mut matches = Vec::new();
    let mut candidates = text.char_indices();
    'candidate: while let Some((start, ch)) = candidates.next() {
        let mut lower = ch.to_lowercase();
        if lower.next() != Some(*first) {
            continue;
        }
        let mut tail = candidates.clone();
        let mut expected = rest.iter().copied();
        loop {
            for lower in lower {
                if expected.next() != Some(lower) {
                    continue 'candidate;
                }
            }
            if expected.len() == 0 {
                matches.push(start..tail.offset());
                candidates = tail;
                break;
            }
            let Some((_, next)) = tail.next() else {
                // No later start can have more lowercase characters left.
                break 'candidate;
            };
            lower = next.to_lowercase();
        }
    }
    matches
}

pub fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn prev_grapheme(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if start == offset {
        return offset - 1;
    }
    text[start..offset]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| start + i)
        .unwrap_or(start)
}

pub fn next_grapheme(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    if text.as_bytes()[offset] == b'\n' {
        return offset + 1;
    }
    let end = text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len());
    text[offset..end]
        .graphemes(true)
        .next()
        .map(|g| offset + g.len())
        .unwrap_or(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_handles_cross_without_moving_the_opposite_endpoint_or_text() {
        use makepad_widgets::TextHandleEndpoint::{Anchor, Caret, Cursor};
        let mut buffer = TextBuffer::new("한😀글".into());
        buffer.set_selection(Selection { anchor: 0, head: 3 });
        let revision = buffer.revision;
        buffer.move_selection_handle(Anchor, 3);
        assert_eq!(buffer.selection, Selection::caret(3));
        buffer.move_selection_handle(Anchor, 8);
        assert_eq!(buffer.selection, Selection { anchor: 7, head: 3 });
        assert_eq!(buffer.selected_text(), "😀");
        buffer.move_selection_handle(Cursor, usize::MAX);
        assert_eq!(
            buffer.selection,
            Selection {
                anchor: 7,
                head: 10
            }
        );
        buffer.move_selection_handle(Caret, 5);
        assert_eq!(buffer.selection, Selection::caret(3));
        assert_eq!(buffer.revision, revision);
        assert_eq!(buffer.text(), "한😀글");
        assert!(!buffer.undo());
    }

    #[test]
    fn selection_handles_preserve_hangul_composition_undo_group() {
        let mut buffer = TextBuffer::new("A😀".into());
        buffer.set_selection(Selection::caret(buffer.len()));
        buffer.apply_ime(
            &TextImeEvent {
                session: 1,
                revision: 1,
                text: "A😀ㅎ".into(),
                selection_start: 4,
                selection_end: 4,
                composing: Some((3, 4)),
            },
            false,
        );
        buffer.move_selection_handle(makepad_widgets::TextHandleEndpoint::Caret, 1);
        assert!(buffer.ime_composing);
        buffer.apply_ime(
            &TextImeEvent {
                session: 1,
                revision: 2,
                text: "A😀한".into(),
                selection_start: 4,
                selection_end: 4,
                composing: None,
            },
            false,
        );
        assert_eq!(buffer.text(), "A😀한");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "A😀");
        assert_eq!(buffer.selection, Selection::caret(5));
        assert!(!buffer.undo());
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "A😀한");
    }

    #[test]
    fn android_ime_composition_is_one_undo_group_and_commit_is_not_duplicated() {
        let mut buffer = TextBuffer::new("A😀Z\nunchanged\n".into());
        buffer.set_selection(Selection::caret(5));
        for (text, composing) in [
            ("A😀ㅎZ\nunchanged\n", true),
            ("A😀하Z\nunchanged\n", true),
            ("A😀한Z\nunchanged\n", true),
            ("A😀한Z\nunchanged\n", false),
        ] {
            buffer.apply_ime(
                &TextImeEvent {
                    session: 1,
                    revision: 1,
                    text: text.into(),
                    selection_start: 4,
                    selection_end: 4,
                    composing: composing.then_some((3, 4)),
                },
                false,
            );
            assert_full_line_index(&buffer);
        }
        assert_eq!(buffer.text(), "A😀한Z\nunchanged\n");
        assert_eq!(buffer.selection, Selection::caret(8));
        buffer.undo();
        assert_eq!(buffer.text(), "A😀Z\nunchanged\n");
        assert_eq!(buffer.selection, Selection::caret(5));
        assert_full_line_index(&buffer);
        buffer.redo();
        assert_eq!(buffer.text(), "A😀한Z\nunchanged\n");
        assert_full_line_index(&buffer);
    }

    #[test]
    fn android_ime_deletion_selection_and_ascii_paste() {
        let mut buffer = TextBuffer::new("한국어".into());
        for (text, start, end, composing) in [
            ("한국", 2, 2, true),
            ("한", 1, 1, false),
            ("한", 0, 1, false),
            ("ASCII paste 😀", 14, 14, false),
        ] {
            buffer.apply_ime(
                &TextImeEvent {
                    session: 1,
                    revision: 1,
                    text: text.into(),
                    selection_start: start,
                    selection_end: end,
                    composing: composing.then_some((0, end)),
                },
                false,
            );
            assert_eq!(buffer.text(), text);
        }
        buffer.undo();
        assert_eq!(buffer.text(), "한");
        assert_eq!(buffer.selection, Selection { anchor: 0, head: 3 });
    }

    #[test]
    fn locations_and_offsets_round_trip() {
        let buffer = TextBuffer::new("ab\n한글x\n".into());
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.location(0), (1, 1));
        let offset = buffer.offset_at(2, 3);
        assert_eq!(&buffer.text()[offset..offset + 1], "x");
        assert_eq!(buffer.location(offset), (2, 3));
        assert_eq!(buffer.offset_at(2, 99), buffer.line_end(1));
        assert_eq!(buffer.offset_at(9, 1), buffer.line_start(2));
    }

    #[test]
    fn typing_groups_and_undo_redo() {
        let mut buffer = TextBuffer::new(String::new());
        for c in ["a", "b", "c"] {
            buffer.insert(c, EditKind::Insert);
        }
        buffer.handle_edit_key(
            TextInputKeyCommand::Delete(TextInputMotion::Left),
            false,
            true,
        );
        assert_eq!(buffer.text(), "ab");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "abc");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "");
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "abc");
    }

    fn assert_full_line_index(buffer: &TextBuffer) {
        let starts: Vec<_> = std::iter::once(0)
            .chain(
                buffer
                    .text()
                    .match_indices('\n')
                    .map(|(index, _)| index + 1),
            )
            .collect();
        assert_eq!(buffer.line_starts, starts);
        for (line, text) in buffer.text().split('\n').enumerate() {
            assert_eq!(buffer.line(line), text);
        }
    }

    #[test]
    fn incremental_line_index_matches_rebuild_through_edits_undo_and_redo() {
        let mut buffer = TextBuffer::new("한글\n😀 row\n끝\n".into());
        for (start, end, inserted) in [
            (0, 0, "\nα\n"),
            (0, "\nα\n".len(), ""),
            ("한글".len(), "한글\n".len(), "-"),
            ("한글-😀".len(), "한글-😀 row\n".len(), "\n\n"),
            (0, usize::MAX, "one\n\n끝"),
            (0, usize::MAX, ""),
        ] {
            let before = buffer.text().to_owned();
            buffer.replace(start, end, inserted, EditKind::Other);
            let after = buffer.text().to_owned();
            assert_full_line_index(&buffer);
            assert!(buffer.undo());
            assert_eq!(buffer.text(), before);
            assert_full_line_index(&buffer);
            assert!(buffer.redo());
            assert_eq!(buffer.text(), after);
            assert_full_line_index(&buffer);
        }
        buffer.set_text("one\n두\n".into());
        assert_full_line_index(&buffer);
    }

    // Reference: the previous eager char/offset-array search, including rejection
    // of matches that would finish partway through a source character's lowercase.
    fn legacy_matches(text: &str, needle: &str, match_case: bool) -> Vec<Range<usize>> {
        if needle.is_empty() {
            return Vec::new();
        }
        if match_case {
            return text
                .match_indices(needle)
                .map(|(start, _)| start..start + needle.len())
                .collect();
        }
        let needle: Vec<_> = needle.chars().flat_map(char::to_lowercase).collect();
        let chars: Vec<_> = text.char_indices().collect();
        let mut result = Vec::new();
        let mut i = 0;
        'candidate: while i < chars.len() {
            let mut j = 0;
            let mut k = i;
            while j < needle.len() {
                let Some(&(_, ch)) = chars.get(k) else {
                    break 'candidate;
                };
                for lower in ch.to_lowercase() {
                    if needle.get(j) != Some(&lower) {
                        i += 1;
                        continue 'candidate;
                    }
                    j += 1;
                }
                k += 1;
            }
            result.push(chars[i].0..chars.get(k).map_or(text.len(), |&(offset, _)| offset));
            i = k;
        }
        result
    }

    #[test]
    fn direct_find_matches_legacy_unicode_and_overlap_rules() {
        for text in [
            "",
            "Foo foo FOO\nFoo",
            "aaaaaAbAaBa",
            "한글한글 한글 a한글",
            "İ i\u{307} I ı Iİİi\u{307}",
            "ẞ ß ss SS Σ ς ΟΣ Οσ",
            "ÉCOLE école K K 😺",
        ] {
            for query in [
                "",
                "foo",
                "Foo\n",
                "aa",
                "aba",
                "abaaa",
                "a한",
                "한글",
                "한글",
                "i",
                "İ",
                "i\u{307}",
                "\u{307}",
                "İi",
                "ss",
                "ß",
                "ẞ",
                "σ",
                "ς",
                "ΟΣ",
                "éc",
                "k",
                "😺",
            ] {
                for match_case in [false, true] {
                    assert_eq!(
                        find_matches(text, query, match_case),
                        legacy_matches(text, query, match_case),
                        "{text:?}, {query:?}, {match_case}",
                    );
                }
            }
        }
    }

    #[test]
    fn find_cache_reuses_matches_and_replace_all_preserves_utf8_caret() {
        let mut buffer = TextBuffer::new("İ İ i".into());
        assert_eq!(buffer.matches("i\u{307}", false), &[0..2, 3..5]);
        assert_eq!(buffer.matches("i\u{307}", false).len(), 2);
        assert_eq!(buffer.find.scans, 1);

        buffer.set_selection(Selection::caret(2));
        assert!(buffer.replace_all("i\u{307}", false, "한"));
        assert_eq!(buffer.find.scans, 1);
        assert_eq!(buffer.text(), "한 한 i");
        assert_eq!(buffer.selection, Selection::caret(0));
        assert!(buffer.matches("i\u{307}", false).is_empty());
        assert_eq!(buffer.find.scans, 2);

        assert!(buffer.undo());
        assert_eq!(buffer.selection, Selection::caret(2));
        assert_eq!(buffer.matches("i\u{307}", false), &[0..2, 3..5]);
        assert!(buffer.matches("i\u{307}", true).is_empty());
        assert_eq!(buffer.matches("İ", true), &[0..2, 3..5]);
        assert_eq!(buffer.find.scans, 5);
    }

    #[test]
    fn native_edit_keys_preserve_source_selection_history_and_ime() {
        use makepad_widgets::{KeyCode, KeyModifiers};
        for apple in [false, true] {
            let word = KeyModifiers {
                alt: apple,
                control: !apple,
                ..Default::default()
            };
            let key = |buffer: &mut TextBuffer, code, modifiers: KeyModifiers, editable| {
                if let Some(command) = TextInputKeyCommand::resolve(code, modifiers, apple) {
                    buffer.handle_edit_key(command, modifiers.shift, editable);
                }
            };
            let mut buffer = TextBuffer::new("한글 👩‍💻\nlast".into());
            buffer.move_head("한글 👩‍💻".len(), false);
            key(
                &mut buffer,
                KeyCode::ArrowLeft,
                KeyModifiers {
                    shift: true,
                    ..word
                },
                true,
            );
            assert_eq!(buffer.selected_text(), "👩‍💻");
            key(&mut buffer, KeyCode::Backspace, word, false);
            assert_eq!(buffer.text(), "한글 👩‍💻\nlast");
            key(&mut buffer, KeyCode::Backspace, word, true);
            assert_eq!(buffer.text(), "한글 \nlast");
            assert!(buffer.undo());
            assert_eq!(buffer.selected_text(), "👩‍💻");
            key(
                &mut buffer,
                KeyCode::ArrowRight,
                KeyModifiers::default(),
                true,
            );
            assert_eq!(buffer.selection, Selection::caret("한글 👩‍💻".len()));
            key(
                &mut buffer,
                KeyCode::ArrowLeft,
                KeyModifiers {
                    control: true,
                    alt: true,
                    ..Default::default()
                },
                true,
            );
            assert_eq!(buffer.selection, Selection::caret("한글 👩‍💻".len()));
            buffer.ime_composing = true;
            key(&mut buffer, KeyCode::Backspace, word, true);
            assert_eq!(buffer.text(), "한글 👩‍💻\nlast");
            buffer.ime_composing = false;
            key(
                &mut buffer,
                if apple {
                    KeyCode::ArrowLeft
                } else {
                    KeyCode::Home
                },
                KeyModifiers {
                    logo: apple,
                    ..Default::default()
                },
                true,
            );
            assert_eq!(buffer.selection, Selection::caret(0));
            key(&mut buffer, KeyCode::ArrowRight, word, true);
            assert_eq!(
                buffer.selection.head,
                if apple {
                    "한글".len()
                } else {
                    "한글 ".len()
                }
            );
        }
    }
}
