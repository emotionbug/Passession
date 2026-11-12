//! Port of `stream/text_reveal.dart` (agent-stream/text-reveal.ts +
//! use-revealed-text.ts): streamed text is painted at a pace derived from the
//! backlog, so 60ms coalesced deltas do not show up as jagged jumps.

use unicode_segmentation::GraphemeCursor;

/// Backlog is drained over this horizon.
pub const TEXT_REVEAL_HORIZON_MS: f64 = 150.0;
/// What the first frame after growth counts as (text-reveal.ts
/// `nextTextRevealFrame` without a previous frame): one 60Hz interval, so
/// arriving text starts to show on the frame that first sees it.
pub const TEXT_REVEAL_FRAME_INTERVAL_S: f64 = 1.0 / 60.0;
const MAX_ELAPSED_MS: f64 = 250.0;

/// Characters to reveal on this frame: proportional to the backlog, at least one.
pub fn compute_reveal_step(backlog: usize, elapsed_ms: f64) -> usize {
    if backlog == 0 {
        return 0;
    }
    let elapsed = elapsed_ms.clamp(0.0, MAX_ELAPSED_MS);
    if elapsed <= 0.0 {
        return 0;
    }
    if elapsed >= TEXT_REVEAL_HORIZON_MS {
        return backlog;
    }
    let step = (backlog as f64 * elapsed / TEXT_REVEAL_HORIZON_MS).ceil() as usize;
    step.clamp(1, backlog)
}

/// Pull a cut back to the start of the extended grapheme cluster containing `index`.
pub fn clamp_to_safe_reveal_boundary(text: &str, index: usize) -> usize {
    if index == 0 {
        return 0;
    }
    if index >= text.len() {
        return text.len();
    }
    // Start at the requested cut, not at byte zero. GraphemeCursor requests
    // only the context needed for this boundary (including RI/ZWJ sequences).
    let mut cut = index;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut cursor = GraphemeCursor::new(cut, text.len(), true);
    if cursor.is_boundary(text, 0).expect("complete grapheme context") {
        cut
    } else {
        cursor.prev_boundary(text, 0).expect("complete grapheme context").unwrap_or(0)
    }
}

fn trailing_regional_indicators(text: &str) -> usize {
    text.chars().rev().take_while(|c| ('\u{1F1E6}'..='\u{1F1FF}').contains(c)).count()
}

/// Holds back a trailing fragment that is certainly waiting on more input.
pub fn trim_incomplete_trailing_cluster(text: &str) -> &str {
    if let Some(last) = text.chars().last() {
        if last == '\u{200D}' {
            return &text[..text.len() - last.len_utf8()];
        }
        if trailing_regional_indicators(text) % 2 == 1 {
            return &text[..text.len() - last.len_utf8()];
        }
    }
    text
}

/// One paced text (use-revealed-text.ts). First sight shows the text whole;
/// only growth while streaming is paced; a completed phase releases everything.
#[derive(Clone, Debug, Default)]
pub struct Reveal {
    target_len: usize,
    revealed: usize,
    last_step: Option<f64>,
}

impl Reveal {
    pub fn new(text: &str) -> Self {
        Self { target_len: text.len(), revealed: text.len(), last_step: None }
    }

    /// Syncs with the latest text; returns whether pacing needs another frame.
    pub fn sync(&mut self, text: &str, streaming: bool, paced: bool) -> bool {
        if text.len() != self.target_len {
            self.target_len = text.len();
            // A shorter string means this slot shows a different message.
            if self.revealed > self.target_len {
                self.revealed = self.target_len;
            }
        }
        if !streaming || !paced {
            self.revealed = self.target_len;
            self.last_step = None;
            return false;
        }
        self.revealed < self.target_len
    }

    /// Advances one paced step at `now` (seconds); returns whether it changed.
    pub fn step(&mut self, now: f64) -> bool {
        let Some(previous) = self.last_step.replace(now) else {
            return false;
        };
        let step = compute_reveal_step(self.target_len.saturating_sub(self.revealed), (now - previous) * 1000.0);
        if step == 0 {
            return false;
        }
        self.revealed = (self.revealed + step).min(self.target_len);
        true
    }

    pub fn settled(&self) -> bool {
        self.revealed >= self.target_len
    }

    pub fn visible<'a>(&self, text: &'a str, streaming: bool) -> &'a str {
        if self.revealed >= text.len() {
            return if streaming { trim_incomplete_trailing_cluster(text) } else { text };
        }
        &text[..clamp_to_safe_reveal_boundary(text, self.revealed)]
    }

    /// Starts the pacing clock (the first frame after growth).
    pub fn arm(&mut self, now: f64) {
        if self.last_step.is_none() {
            self.last_step = Some(now - TEXT_REVEAL_FRAME_INTERVAL_S);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn reveal_cuts_preserve_extended_clusters_at_every_byte() {
        for text in [
            "a\u{301}b",
            "👩🏽‍💻 family 👨‍👩‍👧‍👦",
            "🇦🇧🇨🇩🇪",
            "한글",
            "क्\u{200d}ष",
            "a\r\nb",
        ] {
            let boundaries: Vec<_> = text.grapheme_indices(true).map(|(offset, _)| offset).collect();
            for cut in 0..text.len() {
                let expected = boundaries.iter().copied().take_while(|offset| *offset <= cut).last().unwrap_or(0);
                assert_eq!(clamp_to_safe_reveal_boundary(text, cut), expected, "{text:?} at {cut}");
            }
            assert_eq!(clamp_to_safe_reveal_boundary(text, text.len()), text.len());
        }
    }

    #[test]
    fn reveal_boundary_uses_current_text_after_appended_cluster_extension() {
        let prefix = "ordinary text ".repeat(4096);
        let first = format!("{prefix}👩");
        let mut reveal = Reveal::new(&first);
        let extended = format!("{first}🏽‍💻");
        assert!(reveal.sync(&extended, true, true));
        assert_eq!(reveal.visible(&extended, true), prefix);
        assert!(!reveal.sync(&extended, false, true));
        assert_eq!(reveal.visible(&extended, false), extended);
    }

    /// One frame of rows.rs' pacing; returns whether text is still held back.
    fn frame(reveal: &mut Reveal, text: &str, now: f64) -> bool {
        if reveal.sync(text, true, true) {
            reveal.arm(now);
            reveal.step(now);
        }
        !reveal.settled()
    }

    #[test]
    fn backlog_drains_geometrically_and_starts_on_its_first_frame() {
        let grown = "a".repeat(10_001);
        let mut reveal = Reveal::new("a");
        let mut now = 100.0;
        assert!(frame(&mut reveal, &grown, now));
        // A ninth of the backlog (one 60Hz interval of the 150ms horizon).
        assert_eq!(reveal.revealed, 1 + 10_000_usize.div_ceil(9));
        let mut frames = 1;
        loop {
            now += 1.0 / 60.0;
            frames += 1;
            if !frame(&mut reveal, &grown, now) {
                break;
            }
            assert!(frames < 70, "10 kB must not trail the stream for more than about 1.1s");
        }
        assert_eq!(reveal.visible(&grown, true), grown);
    }

    #[test]
    fn a_stalled_frame_releases_the_whole_backlog() {
        let grown = "a".repeat(50_000);
        let mut reveal = Reveal::new("a");
        assert!(frame(&mut reveal, &grown, 10.0));
        // No frame for longer than the horizon (hidden tab, busy UI thread).
        assert!(!frame(&mut reveal, &grown, 10.0 + 2.0 * TEXT_REVEAL_HORIZON_MS / 1000.0));
    }

    #[test]
    fn completion_releases_what_pacing_still_held_back() {
        let grown = "a".repeat(50_000);
        let mut reveal = Reveal::new("a");
        assert!(frame(&mut reveal, &grown, 10.0));
        assert!(!reveal.sync(&grown, false, true));
        assert!(reveal.settled());
        assert_eq!(reveal.visible(&grown, false), grown);
    }
}
