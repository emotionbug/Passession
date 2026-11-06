# Markdown rendering

Markdown is parsed with `pulldown-cmark` and painted by Rust/Makepad code. Chat owns `packages/gui/src/features/timeline/markdown/{model,view,html}.rs` and `chat_row.rs`; file preview owns `features/files/markdown.rs`. Syntax highlighting is shared through `paseo-highlight`, with feature-owned workers and layout.

Keep source text separate from its rendering. Streaming a message must not rewrite durable history or lose the association between a displayed block and its source message. Markdown file preview and source editing use the same host-side file identity, not a second local copy with independent save behavior.

Links must retain their context: relative file links resolve against the document/session location on the selected host, while external URLs open outside the application. Equal path strings on different hosts are not the same file. Do not introduce a webview to render links or execute embedded HTML/scripts.

Use native theme/text styles and preserve copy/selection, code formatting, lists, tables and long-content scrolling. Chat keeps stable row identity and source-indexed selection while its `PortalList` virtualizes rows. File preview shares the editing session's remote content; switching presentation must not create a second save owner. Keep error states visible for failed file/image acquisition. Do not fabricate a successful preview for unsupported content.

## Verification

Exercise chat and file preview with streaming text, code, lists, tables, links, selection/copy and a large document. Verify edit/save/read-back and failure recovery separately from preview. Keyboard, pointer and touch selection have different native behavior and must be tested on the affected platform.

Record Rust rendering evidence separately from the historical Flutter screenshots in [testing](testing.md). Neither Linux rendering nor a parser test proves Android/iOS selection, device image acquisition or every Markdown edge case. See [native text selection](native-text-selection.md).
