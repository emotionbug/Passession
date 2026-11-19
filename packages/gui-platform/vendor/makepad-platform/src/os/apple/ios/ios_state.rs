use crate::event::TextImeEvent;
use std::sync::atomic::{AtomicU8, Ordering};

const ACTIVE: u8 = 1;
const QUEUED: u8 = 2;

/// Background work still sets Makepad's signal flags. Only the foreground pays
/// for a main-queue wake, and resuming always takes a pass over those flags.
#[derive(Default)]
pub(crate) struct WakeGate(AtomicU8);

impl WakeGate {
    pub const fn new() -> Self { Self(AtomicU8::new(0)) }

    pub fn set_active(&self, active: bool) {
        if active {
            self.0.fetch_or(ACTIVE, Ordering::AcqRel);
        } else {
            self.0.fetch_and(!ACTIVE, Ordering::AcqRel);
        }
    }

    pub fn request(&self) -> bool {
        self.0.compare_exchange(ACTIVE, ACTIVE | QUEUED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Clear the outstanding wake before draining, so concurrent posts can
    /// schedule another pass even if they miss the current channel drain.
    pub fn take(&self) -> bool {
        self.0.fetch_and(!QUEUED, Ordering::AcqRel) == (ACTIVE | QUEUED)
    }
}

pub(super) fn utf16_selection(text: &str, anchor: usize, cursor: usize) -> (usize, usize) {
    fn boundary(text: &str, mut byte: usize) -> usize {
        byte = byte.min(text.len());
        while !text.is_char_boundary(byte) { byte -= 1; }
        byte
    }
    let start = boundary(text, anchor.min(cursor));
    let end = boundary(text, anchor.max(cursor));
    let start_units = text[..start].encode_utf16().count();
    (start_units, start_units + text[start..end].encode_utf16().count())
}

/// Last mirror admitted from this native responder. UIKit performs range edits
/// on its UITextInput document; snapshots carry the result to the Rust editor.
pub(super) struct TextSession {
    pub id: u64,
    revision: u64,
    text: String,
    pub selection: (usize, usize),
    pub composing: Option<(usize, usize)>,
}

impl TextSession {
    pub fn new(id: u64, text: &str, selection: (usize, usize)) -> Self {
        Self { id, revision: 0, text: text.into(), selection, composing: None }
    }

    /// An unchanged Rust echo preserves the native mark. A changed text or
    /// selection is a new edit and ends the mark without reverting its text.
    pub fn publish(&mut self, text: &str, selection: (usize, usize)) -> Option<(bool, bool)> {
        let text_changed = self.text != text;
        if !text_changed && self.selection == selection { return None; }
        if text_changed {
            self.text.clear();
            self.text.push_str(text);
        }
        self.selection = selection;
        Some((text_changed, self.composing.take().is_some()))
    }

    pub fn observe(&mut self, text: String, selection: (usize, usize), composing: Option<(usize, usize)>) -> Option<TextImeEvent> {
        if self.text == text && self.selection == selection && self.composing == composing { return None; }
        self.text = text;
        self.selection = selection;
        self.composing = composing;
        Some(self.snapshot())
    }

    pub fn snapshot(&mut self) -> TextImeEvent {
        self.revision += 1;
        TextImeEvent {
            session: self.id,
            revision: self.revision,
            text: self.text.clone(),
            selection_start: self.selection.0,
            selection_end: self.selection.1,
            composing: self.composing,
        }
    }
}

pub(super) fn accepts_edit(active_session: u64, last_revision: u64, session: u64, revision: u64) -> bool {
    active_session == session && revision > last_revision
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_coalesce_and_can_be_requested_again_during_drain() {
        let wake = WakeGate::new();
        wake.set_active(true);
        assert!(wake.request());
        assert!(!wake.request());
        assert!(wake.take());
        assert!(wake.request());
        assert!(wake.take());
        assert!(!wake.take());
    }

    #[test]
    fn suspend_drops_a_dispatched_wake_but_resume_can_pick_up_work() {
        let wake = WakeGate::new();
        assert!(!wake.request());
        wake.set_active(true);
        assert!(wake.request());
        wake.set_active(false);
        assert!(!wake.take());
        assert!(!wake.request());
        wake.set_active(true);
        assert!(wake.request());
        wake.set_active(false);
        wake.set_active(true);
        assert!(!wake.request()); // the old block has not yet run
        assert!(wake.take());
    }

    #[test]
    fn reversed_selection_and_surrogate_pair_keep_character_boundaries() {
        assert_eq!(utf16_selection("A😀한z", 8, 1), (1, 4));
        assert_eq!(utf16_selection("A😀한z", 3, 99), (1, 5));
    }

    #[test]
    fn snapshots_replace_middle_text_and_finish_composition_without_reinserting() {
        let mut editor = String::from("A😀old!");
        let mut session = TextSession::new(9, &editor, (3, 6));
        for (text, selection, mark) in [
            ("A😀ㅎ!", (4, 4), Some((3, 4))),
            ("A😀한!", (4, 4), Some((3, 4))),
            ("A😀한!", (4, 4), None),
            ("A😀!", (3, 3), None),
        ] {
            let edit = session.observe(text.into(), selection, mark).unwrap();
            let (start, end, fragment) = edit.replacement(&editor);
            editor.replace_range(start..end, fragment);
            assert_eq!(editor, text);
            assert_eq!(edit.composing, mark);
        }
        assert_eq!(session.revision, 4);
        assert!(session.observe(editor, (3, 3), None).is_none());
    }

    #[test]
    fn rust_echo_preserves_a_mark_but_moving_selection_finishes_it() {
        let mut session = TextSession::new(1, "A😀", (3, 3));
        session.observe("A😀か".into(), (4, 4), Some((3, 4))).unwrap();
        assert_eq!(session.publish("A😀か", (4, 4)), None);
        assert_eq!(session.composing, Some((3, 4)));
        assert_eq!(session.publish("A😀か", (1, 1)), Some((false, true)));
        let edit = session.snapshot();
        assert_eq!((edit.text.as_str(), edit.composing), ("A😀か", None));
        assert_eq!(edit.selection_utf8(), (1, 1));
    }

    #[test]
    fn old_responder_and_already_delivered_revisions_cannot_edit() {
        assert!(!accepts_edit(2, 0, 1, 99));
        assert!(!accepts_edit(2, 3, 2, 3));
        assert!(accepts_edit(2, 3, 2, 4));
    }
}
