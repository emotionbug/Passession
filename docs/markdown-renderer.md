# Markdown rendering

Markdown is parsed with `pulldown-cmark` and painted by Rust/Makepad code. Chat owns `packages/gui/src/features/timeline/markdown/{model,worker,view,html}.rs` and `chat_row.rs`; file preview owns `features/files/markdown.rs`. Syntax highlighting is shared through `paseo-highlight`, with feature-owned workers and layout.

Keep source text separate from its rendering. Streaming a message must not rewrite durable history or lose the association between a displayed block and its source message. Markdown file preview and source editing use the same host-side file identity, not a second local copy with independent save behavior.

Links must retain their context: relative file links resolve against the document/session location on the selected host, while external URLs open outside the application. Equal path strings on different hosts are not the same file. Do not introduce a webview to render links or execute embedded HTML/scripts.

Use native theme/text styles and preserve copy/selection, code formatting, lists, tables and long-content scrolling. Chat keeps stable row identity and source-indexed selection while its `PortalList` virtualizes rows. File preview shares the editing session's remote content; switching presentation must not create a second save owner. Keep error states visible for failed file/image acquisition. Do not fabricate a successful preview for unsupported content.

## Streaming work

Assistant messages retain grapheme-safe paced reveal rather than waiting to
accumulate a minimum batch. Small live documents (at most 16 KiB of captured
source) normalize, parse and flatten inline using the same parser as the worker.
The final update of an inline-published document uses that path too. Preparation
admits at most 16 KiB of inline layout text per pass, with a soft 4 ms deadline
checked between layout calls. Multiple ready paragraphs can therefore publish
in the current frame instead of requiring a worker round trip for every reveal.
This is an admission limit, not a hard frame-time guarantee: one font
initialization or shaping operation can exceed the deadline.

Larger documents and work exceeding the inline layout allowance retain the
staged whole-document pipeline, including an open paragraph with no safe
Markdown split:

1. `MarkdownWorker` retains the immutable timeline entry and copies at most
   64 KiB per preparation pass. Two parser workers share a 64-request queue.
   Normalization, Markdown parsing, semantic flattening, find ranges and
   grapheme-boundary walks for larger documents run there, not during UI paint.
2. One captured revision finishes even while newer tokens arrive. The latest
   demand replaces intermediate demands; it cannot continuously cancel the
   revision being prepared. Canonical timeline events are never coalesced here.
3. `MarkdownPresentation` retains the published document while preparing its
   successor. Background font-span mapping advances at most 256 segments in one
   paragraph per preparation pass. `AsyncLayout` shapes and wraps the complete
   immutable text with a worker-local font loader/shaper and shared immutable
   font bytes. Inline and worker layout use identical options. Arbitrary
   byte/word splitting is not used to approximate Unicode shaping.
4. UI hydration admits at most 4096 glyphs and 128 rows per preparation pass.
   Source, styles, find results, links, copy ranges and all paragraph/code/table
   layouts publish together. Width, typography, appearance and find occurrence
   are part of presentation identity; a recycled row cannot inherit an older
   owner's result. Pointer-keyed paragraph indexes retain their immutable owner.

The layout service has one CPU worker per font service, at most 64 waiting jobs,
and latest-waiting replacement per owner. Full queues retain unadmitted demand
and retry without blocking. Final UI layout disposal is also deferred, including
when the last external selection run releases it. The app owns the initial
signal and 1/60-second timer continuation, draining 4096 glyphs/128 rows per
batch even when no row/window is drawing. Signals cannot bypass that cadence.
Font-face initialization and allocation are not claimed to have a strict
wall-clock bound.

Chat fences and read/write source previews preserve their shared highlighting
worker and existing 100,000-byte syntax-color limit. Tool previews retain their
plain-text pending behavior. An assistant candidate waits for available syntax
colors or the existing unsupported/over-limit plain-text result before publication;
the prior complete assistant snapshot remains selectable meanwhile.

`PortalList`'s opt-in measurement pass determines exact heights and final row
positions before painting. Only then is the viewport supplied to `ChatKit` and
`Canvas`. Never substitute preliminary upward/sticky-row positions or stale
geometry for this contract. Row and glyph indexes cull final-position paint,
including ordinary giant unwrapped code lines, before glyph rasterization.
Selection keeps the full authoritative layout. Visible decorations use clipped
indexed ranges; caret, word and selected-prefix lookups reuse source indexes.
`Canvas` retains layout runs and separators rather than concatenating every
paragraph into a new source string on every paint. Explicit selection copy
materializes only the requested source range; code-copy hits share a `Substr`.

The assistant pipeline no longer uses the previous desktop presentation cap.
Full source reaches the parser and layout path; no new truncation was added.
Synchronous Markdown entry points remain for other feature surfaces. Work still
scales with document content on the workers and with rendered block/decorative
structure on the UI; pathological overlapping glyphs cannot be safely discarded
merely to claim a constant frame time.

## Verification

Exercise chat and file preview with streaming text, code, lists, tables, links, selection/copy and a large document. Verify edit/save/read-back and failure recovery separately from preview. Keyboard, pointer and touch selection have different native behavior and must be tested on the affected platform.

Record Rust rendering evidence separately from the historical Flutter screenshots in [testing](testing.md). Neither Linux rendering nor a parser test proves Android/iOS selection, device image acquisition or every Markdown edge case. See [native text selection](native-text-selection.md).

The staged pipeline, Unicode/layout boundary cases, queue saturation, retained
source selection and final-position geometry have regression test source.
Those additions were not executed during the static-only implementation request;
they are not runtime or performance evidence.

An isolated smoke using the production parser, worker and reveal code exercised
incremental short Hangul/ZWJ updates, final publication, a 198,003-byte worker
handoff retaining the prior snapshot, and owner replacement. It does not prove
native frame cadence, layout latency or Android interaction; no GUI build or
device scenario was run for the inline-reveal change.
