//! Desktop input-method compositions as full-editor snapshots.
//!
//! Windows (IMM32) reports a composition as its current string and, when text is
//! committed, a result string. Editors already apply full-editor snapshots
//! (`TextImeEvent`, the Android model): they show the composing text inline and group it
//! in their undo history. `ImeComposer` mirrors the focused editor as it was last
//! published and turns each composition step into such a snapshot, so the text being
//! composed appears in the editor at once instead of in the input method's own window.

use crate::{area::Area, event::TextImeEvent};

/// One composition message, already read from the input method. Public only because the
/// public `Win32Event` carries it; the module itself is private.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompositionUpdate {
    /// Text the input method committed (`GCS_RESULTSTR`).
    pub result: Option<String>,
    /// The new composition (`GCS_COMPSTR`); empty when it was cleared.
    pub composition: Option<String>,
    /// Caret inside `composition` in UTF-16 units (`GCS_CURSORPOS`); its end when absent.
    pub cursor: Option<usize>,
    /// The input method ended the composition (`WM_IME_ENDCOMPOSITION`).
    pub ended: bool,
}

/// What the platform must do after the focused editor republished itself.
#[derive(Debug, Default)]
pub(crate) struct Interruption {
    /// Discard the input method's composition: the editor moved on and keeps the text.
    pub cancel: bool,
    /// Tells the still-focused editor that its composition ended where it stands.
    pub notify: Option<TextImeEvent>,
}

/// Mirror of the focused editor. `A` identifies its drawn area (`Area`); its default
/// value means "no area".
#[derive(Default)]
pub(crate) struct ImeComposer<A = Area> {
    area: A,
    editor: Option<u64>,
    text: String,
    /// Published (anchor, cursor) in UTF-8 bytes.
    selection: (usize, usize),
    /// UTF-8 byte range of the composition inside `text`.
    composing: Option<(usize, usize)>,
    revision: u64,
}

impl<A: Copy + PartialEq + Default> ImeComposer<A> {
    pub fn area(&self) -> A {
        self.area
    }

    pub fn has_editor(&self) -> bool {
        self.editor.is_some()
    }

    /// Mirrors the focused editor as drawn. A snapshot from `compose` leaves the editor
    /// exactly as mirrored, so any other difference while composing (typing elsewhere, a
    /// click, filtered input, another editor) ends the composition where it stands.
    pub fn publish(&mut self, area: A, editor: u64, text: &str, anchor: usize, cursor: usize) -> Interruption {
        let mut interruption = Interruption::default();
        let selection = (char_floor(text, anchor), char_floor(text, cursor));
        let same = self.editor == Some(editor);
        self.area = area;
        if same && self.text == text && self.selection == selection {
            return interruption;
        }
        self.editor = Some(editor);
        self.text.clear();
        self.text.push_str(text);
        self.selection = selection;
        if self.composing.take().is_some() {
            interruption.cancel = true;
            if same {
                interruption.notify = Some(self.snapshot());
            }
        }
        interruption
    }

    /// The editor at `area` lost focus. Returns whether a composition must be discarded;
    /// the editor keeps the text composed so far.
    pub fn clear(&mut self, area: A) -> bool {
        if area == A::default() || self.area != area {
            return false;
        }
        self.area = A::default();
        self.editor = None;
        self.text.clear();
        self.selection = (0, 0);
        self.composing.take().is_some()
    }

    /// Applies one composition message to the mirror and returns the snapshots to deliver,
    /// in order.
    pub fn compose(&mut self, update: CompositionUpdate) -> Vec<TextImeEvent> {
        let mut events = Vec::new();
        if self.editor.is_none() {
            return events;
        }
        let (mut start, mut end) = self.composing.unwrap_or_else(|| {
            let (a, b) = self.selection;
            (a.min(b), a.max(b))
        });
        if let Some(result) = update.result {
            self.text.replace_range(start..end, &result);
            start += result.len();
            end = start;
            self.composing = None;
            self.selection = (start, start);
            events.push(self.snapshot());
        }
        match update.composition {
            Some(composition) if !composition.is_empty() => {
                self.text.replace_range(start..end, &composition);
                let caret = start + update.cursor.map_or(composition.len(), |units| utf16_to_utf8(&composition, units));
                self.composing = Some((start, start + composition.len()));
                self.selection = (caret, caret);
                events.push(self.snapshot());
            }
            Some(_) => {
                if self.composing.take().is_some() {
                    self.text.replace_range(start..end, "");
                    self.selection = (start, start);
                    events.push(self.snapshot());
                }
            }
            None => {}
        }
        // Ending without a result discards what was still being composed.
        if update.ended {
            if let Some((start, end)) = self.composing.take() {
                self.text.replace_range(start..end, "");
                self.selection = (start, start);
                events.push(self.snapshot());
            }
        }
        events
    }

    fn snapshot(&mut self) -> TextImeEvent {
        self.revision += 1;
        let text = &self.text;
        let units = |byte: usize| text[..byte].encode_utf16().count();
        TextImeEvent {
            session: 0,
            revision: self.revision,
            text: text.clone(),
            selection_start: units(self.selection.0),
            selection_end: units(self.selection.1),
            composing: self.composing.map(|(start, end)| (units(start), units(end))),
        }
    }
}

fn char_floor(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn utf16_to_utf8(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (index, ch) in text.char_indices() {
        if seen >= units {
            return index;
        }
        seen += ch.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composing(text: &str) -> CompositionUpdate {
        CompositionUpdate { composition: Some(text.into()), ..Default::default() }
    }

    /// Applies snapshots the way editors do (`replacement` on their own text) and checks
    /// each one reproduces the snapshot's text.
    fn apply(editor: &mut String, events: Vec<TextImeEvent>) -> Vec<TextImeEvent> {
        for event in &events {
            let (start, end, replacement) = event.replacement(editor);
            editor.replace_range(start..end, replacement);
            assert_eq!(editor, &event.text);
        }
        events
    }

    /// Editor 7 drawn at area 1; areas are plain numbers here, 0 being "no area".
    fn composer(text: &str, anchor: usize, cursor: usize) -> ImeComposer<u32> {
        let mut composer = ImeComposer::default();
        assert!(!composer.publish(1, 7, text, anchor, cursor).cancel);
        composer
    }

    #[test]
    fn hangul_appears_while_composing_and_commits_into_the_next_syllable() {
        let mut ime = composer("ab", 2, 2);
        let mut editor = "ab".to_owned();
        for (step, text) in [("ㅎ", "abㅎ"), ("하", "ab하"), ("한", "ab한")] {
            let events = apply(&mut editor, ime.compose(composing(step)));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].text, text);
            assert_eq!(events[0].composing, Some((2, 3)));
            assert_eq!((events[0].selection_start, events[0].selection_end), (3, 3));
        }
        // 한 + ㄱ: the IME commits 한 and starts ㄱ in one message.
        let events = apply(&mut editor, ime.compose(CompositionUpdate {
            result: Some("한".into()),
            composition: Some("ㄱ".into()),
            ..Default::default()
        }));
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].text.as_str(), events[0].composing), ("ab한", None));
        assert_eq!((events[1].text.as_str(), events[1].composing), ("ab한ㄱ", Some((3, 4))));
        for step in ["그", "글"] {
            apply(&mut editor, ime.compose(composing(step)));
        }
        let events = apply(&mut editor, ime.compose(CompositionUpdate {
            result: Some("글".into()),
            ended: true,
            ..Default::default()
        }));
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].text.as_str(), events[0].composing), ("ab한글", None));
        assert_eq!(editor, "ab한글");
    }

    #[test]
    fn clearing_or_cancelling_a_composition_removes_it() {
        let mut ime = composer("ab", 2, 2);
        let mut editor = "ab".to_owned();
        apply(&mut editor, ime.compose(composing("ㅎ")));
        let events = apply(&mut editor, ime.compose(composing("")));
        assert_eq!((events[0].text.as_str(), events[0].composing), ("ab", None));
        assert!(ime.compose(CompositionUpdate { ended: true, ..Default::default() }).is_empty());

        apply(&mut editor, ime.compose(composing("か")));
        let events = apply(&mut editor, ime.compose(CompositionUpdate { ended: true, ..Default::default() }));
        assert_eq!((events[0].text.as_str(), events[0].composing), ("ab", None));
        assert_eq!(editor, "ab");
    }

    #[test]
    fn composition_replaces_a_reversed_selection_in_utf16_units() {
        let mut ime = composer("x😀yz", 6, 5);
        let mut editor = "x😀yz".to_owned();
        let events = apply(&mut editor, ime.compose(composing("한")));
        assert_eq!(events[0].text, "x😀한z");
        assert_eq!(events[0].composing, Some((3, 4)));
        assert_eq!((events[0].selection_start, events[0].selection_end), (4, 4));
        let events = apply(&mut editor, ime.compose(CompositionUpdate {
            composition: Some("かな😀".into()),
            cursor: Some(1),
            ..Default::default()
        }));
        assert_eq!(events[0].composing, Some((3, 7)));
        assert_eq!((events[0].selection_start, events[0].selection_end), (4, 4));
    }

    #[test]
    fn editor_changes_end_the_composition_where_it_stands() {
        let mut ime = composer("ab", 2, 2);
        ime.compose(composing("ㅎ"));
        // Republishing the snapshot just applied (caret after ㅎ, UTF-8 byte 5) is not an
        // interruption.
        assert!(!ime.publish(1, 7, "abㅎ", 5, 5).cancel);
        // A click moves the caret: the editor keeps the syllable and leaves composition.
        let interruption = ime.publish(1, 7, "abㅎ", 0, 0);
        assert!(interruption.cancel);
        let notify = interruption.notify.unwrap();
        assert_eq!((notify.text.as_str(), notify.composing, notify.selection_start), ("abㅎ", None, 0));
        // The next composition starts at the new caret.
        assert_eq!(ime.compose(composing("ㄱ"))[0].text, "ㄱabㅎ");

        // Focus moves to another editor: nothing to tell the old one.
        let interruption = ime.publish(2, 8, "other", 5, 5);
        assert!(interruption.cancel && interruption.notify.is_none());
    }

    #[test]
    fn blurring_the_editor_discards_only_an_active_composition() {
        let mut ime = composer("ab", 2, 2);
        assert_eq!(ime.area(), 1);
        assert!(!ime.clear(0), "no area never matches the published editor");
        assert!(!ime.clear(2), "another area is not the published editor");
        ime.compose(composing("ㅎ"));
        assert!(ime.clear(1));
        assert!(!ime.has_editor());
        assert!(ime.compose(composing("ㅎ")).is_empty(), "nothing is composed without an editor");
        let mut ime = composer("ab", 2, 2);
        assert!(!ime.clear(1), "no composition to discard");
    }
}
