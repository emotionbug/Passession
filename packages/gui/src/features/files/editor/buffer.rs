//! The source editor's text model (Flutter `SourceController` + the `TextEditingValue` it edits):
//! a `\n`-separated buffer with a line index, a selection in byte offsets, grapheme/word
//! navigation, grouped undo/redo and literal find/replace. All offsets are UTF-8 byte offsets on
//! char boundaries.

use makepad_widgets::event::TextImeEvent;
use makepad_widgets::text_input::{TextInputKeyCommand, TextInputMotion, text_input_word_boundary};
use unicode_segmentation::UnicodeSegmentation;

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
        for (index, byte) in self.text.bytes().enumerate() {
            if byte == b'\n' {
                self.line_starts.push(index + 1);
            }
        }
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

    fn apply(&mut self, start: usize, end: usize, inserted: &str) -> Edit {
        let removed = self.text[start..end].to_owned();
        self.text.replace_range(start..end, inserted);
        self.index();
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
            self.text.replace_range(edit.start..end, &edit.removed);
        }
        self.index();
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
            self.text.replace_range(edit.start..end, &edit.inserted);
        }
        self.index();
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

    /// Byte offsets of literal, non-overlapping matches (Flutter `_matches`).
    pub fn matches(&self, needle: &str, match_case: bool) -> Vec<usize> {
        find_matches(&self.text, needle, match_case)
    }
}

/// Literal non-overlapping matches; case-insensitive matching compares lowercased chars and
/// reports offsets in the original text.
pub fn find_matches(text: &str, needle: &str, match_case: bool) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    if match_case {
        return text.match_indices(needle).map(|(index, _)| index).collect();
    }
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut result = Vec::new();
    let mut i = 0;
    'outer: while i < chars.len() {
        let mut j = 0;
        let mut k = i;
        while j < needle.len() {
            let Some(&(_, c)) = chars.get(k) else {
                break 'outer;
            };
            let mut lower = c.to_lowercase();
            let mut matched = true;
            for lc in lower.by_ref() {
                if needle.get(j) != Some(&lc) {
                    matched = false;
                    break;
                }
                j += 1;
            }
            if !matched {
                i += 1;
                continue 'outer;
            }
            k += 1;
        }
        result.push(chars[i].0);
        i = k.max(i + 1);
    }
    result
}

/// The byte length of the match starting at `start` for `needle` (case-insensitive matches may
/// differ in bytes from the needle).
pub fn match_len(text: &str, start: usize, needle: &str, match_case: bool) -> usize {
    if match_case {
        return needle.len();
    }
    let count = needle.chars().flat_map(char::to_lowercase).count();
    let mut lowered = 0;
    for (index, c) in text[start..].char_indices() {
        if lowered >= count {
            return index;
        }
        lowered += c.to_lowercase().count();
    }
    text.len() - start
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
        let mut buffer = TextBuffer::new("A😀Z".into());
        buffer.set_selection(Selection::caret(5));
        for (text, composing) in [
            ("A😀ㅎZ", true),
            ("A😀하Z", true),
            ("A😀한Z", true),
            ("A😀한Z", false),
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
        }
        assert_eq!(buffer.text(), "A😀한Z");
        assert_eq!(buffer.selection, Selection::caret(8));
        buffer.undo();
        assert_eq!(buffer.text(), "A😀Z");
        assert_eq!(buffer.selection, Selection::caret(5));
        buffer.redo();
        assert_eq!(buffer.text(), "A😀한Z");
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

    #[test]
    fn case_insensitive_matches_use_original_offsets() {
        assert_eq!(find_matches("Foo foo FOO", "foo", false), vec![0, 4, 8]);
        assert_eq!(find_matches("Foo foo", "foo", true), vec![4]);
        assert_eq!(find_matches("aaaa", "aa", true), vec![0, 2]);
        assert_eq!(match_len("ÉCOLE", 0, "éc", false), "ÉC".len());
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
