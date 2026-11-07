# Markdown rendering

Markdown is parsed with `pulldown-cmark` and painted by Rust/Makepad code. Chat owns `packages/gui/src/features/timeline/markdown/{model,view,html}.rs` and `chat_row.rs`; file preview owns `features/files/markdown.rs`. Syntax highlighting is shared through `paseo-highlight`, with feature-owned workers and layout.

Keep source text separate from its rendering. Streaming a message must not rewrite durable history or lose the association between a displayed block and its source message. Markdown file preview and source editing use the same host-side file identity, not a second local copy with independent save behavior.

Links must retain their context: relative file links resolve against the document/session location on the selected host, while external URLs open outside the application. Equal path strings on different hosts are not the same file. Do not introduce a webview to render links or execute embedded HTML/scripts.

Use native theme/text styles and preserve copy/selection, code formatting, lists, tables and long-content scrolling. Chat keeps stable row identity and source-indexed selection while its `PortalList` virtualizes rows. File preview shares the editing session's remote content; switching presentation must not create a second save owner. Keep error states visible for failed file/image acquisition. Do not fabricate a successful preview for unsupported content.

## Streaming work

Keep immutable parsed-block ownership when reusing paragraph layouts. Width,
typography, theme and paragraph style invalidate geometry; hover and selection
do not. A pointer alone is not a cache identity after a parsed tail is replaced.
Retain only the current live tail rather than a history of growing snapshots.

Chat fences and read/write source previews share one highlighting worker: pending source
is selectable plain text, and only a result matching the current source,
language and theme can color it. Parser work is single-flight with a bounded
latest-per-block queue. Never move Makepad font shaping onto that worker.

Do not cull chat glyphs using the row's initial draw position. `PortalList`
shifts upward-drawn rows and sticky-bottom ranges after painting; an apparently
offscreen run can end up visible. Keep complete selection geometry and exact
heights unless the list supplies final-position-aware visibility.

Stable paragraphs reuse preparation and layout, but an edited open Markdown
block still needs normalization, parsing and layout of that block. These caches
do not make arbitrary single growing paragraphs constant-time. The existing
desktop render cap and syntax-highlight size limit are unchanged; neither is
an additional streaming optimization.

## Verification

Exercise chat and file preview with streaming text, code, lists, tables, links, selection/copy and a large document. Verify edit/save/read-back and failure recovery separately from preview. Keyboard, pointer and touch selection have different native behavior and must be tested on the affected platform.

Record Rust rendering evidence separately from the historical Flutter screenshots in [testing](testing.md). Neither Linux rendering nor a parser test proves Android/iOS selection, device image acquisition or every Markdown edge case. See [native text selection](native-text-selection.md).
