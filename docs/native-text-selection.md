# Native text selection

Selection belongs to the Rust content owner. Chat uses source-indexed text runs in `packages/gui/src/features/timeline/chat_row.rs` and `markdown/`; files use `features/files/markdown.rs` and `editor/{buffer,view,field}.rs`; execution logs use `features/logs/view.rs`. See [Markdown rendering](markdown-renderer.md).

Selection must remain usable while scrolling, changing pane width and opening/closing supporting views. A selection gesture must not submit a prompt, activate a link unintentionally, or switch sessions. Preserve source text exactly for copying, including code whitespace and literal file paths.

Keep selection offsets in source text, not in a previous frame's glyph rectangles. Chat paints selection after the current row layout so resizing follows reflow; the source editor keeps buffer selection separate from cached visible-row geometry. Preserve code whitespace and Unicode boundaries when mapping hits to source and copying. Scrolling a code block or detail body must not also scroll the outer timeline.

Android uses the system floating text-action toolbar (`ActionMode.TYPE_FLOATING`), not automatic copy on release. Editable fields expose Paste when the clipboard contains text, Select All, and Copy/Cut only when their capabilities permit them; read-only content exposes Copy and Select All. Password fields do not expose their contents through Copy/Cut. The Rust owner performs each selected action through its existing editing/undo model. JNI clipboard and editor strings preserve UTF-16, including Hangul and supplementary characters.

The shared selection registry fences native actions by owner and request token. A pointer down outside the owner clears the previous selection and toolbar before dispatching that same pointer event normally; it must still activate the clicked control. Focus loss, owner disposal, source replacement and app deactivation also invalidate stale actions. A held text drag retains selection ownership instead of starting parent scrolling.

Toolbar anchors come from the painted caret/selection area and are resolved after parent layout, alignment and clipping finish. An early draw-time zero-size clip must not destroy a valid request. Reflow, scrolling and keyboard resizing update the visible anchor; offscreen selections do not leave a floating toolbar at stale coordinates.

Touch-down in an editable field only starts a pending gesture. Focus, caret
placement and keyboard presentation require an uncancelled tap released inside
the same field. A pan, leaving the field, cancellation or owner/model/geometry
change invalidates it even if the pointer later returns. Long press owns text
selection separately; mouse and explicit programmatic focus remain immediate.
This contract applies to common TextInput and the manually routed file fields.

The hold threshold opens the Android menu before finger-up. Stationary move
events must not collapse the selected word merely because keyboard appearance
moves its field. Closing or declining a native toolbar does not blur the input.
Manually drawn file panels publish their current focus area before children
publish action requests; parent alignment/clipping still resolves afterward.

Glyph atlas overflow must not invalidate existing painted coordinates or omit
the next character. Text uses stable texture pages, with page-specific draw
batches and a complete hit area across pages. Overlay backgrounds, text and
icons share a distinct draw-call group; long runs do not accumulate depth past
later layers.

Test pointer and touch selection, cross-block ranges within a message, code copy, links, text scaling, keyboard visibility and resizing on the actual platform. Chat selection is row-owned; do not imply one selection spans arbitrary virtualized messages. Linux screenshots do not establish Android/iOS selection behavior or mobile source editing; record those boundaries separately. Embedded browser/DOM selection and executable React renderers are outside the native product scope.
