# Native text selection

Selection belongs to the Rust content owner. Chat uses source-indexed text runs in `packages/gui/src/features/timeline/chat_row.rs` and `markdown/`; files use `features/files/markdown.rs` and `editor/{buffer,view,field}.rs`; execution logs use `features/logs/view.rs`. See [Markdown rendering](markdown-renderer.md).

Selection must remain usable while scrolling, changing pane width and opening/closing supporting views. A selection gesture must not submit a prompt, activate a link unintentionally, or switch sessions. Preserve source text exactly for copying, including code whitespace and literal file paths.

Keep selection offsets in source text, not in a previous frame's glyph rectangles. Chat paints selection after the current row layout so resizing follows reflow; the source editor keeps buffer selection separate from cached visible-row geometry. Preserve code whitespace and Unicode boundaries when mapping hits to source and copying. Scrolling a code block or detail body must not also scroll the outer timeline.

Test pointer and touch selection, cross-block ranges within a message, code copy, links, text scaling, keyboard visibility and resizing on the actual platform. Chat selection is row-owned; do not imply one selection spans arbitrary virtualized messages. Linux screenshots do not establish Android/iOS selection behavior or mobile source editing; record those boundaries separately. Embedded browser/DOM selection and executable React renderers are outside the native product scope.
