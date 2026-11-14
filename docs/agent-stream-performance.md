# Agent stream performance

Provider output flows through the Rust daemon's agent owner and transport into `paseo-gui-core` and the Makepad timeline. Network/disk work belongs off the GUI thread; feature controllers retain paged history rather than putting transcripts into the shared directory/preferences state.

Ownership:

- `packages/daemon/src/agent/timeline_stream.rs`, `timeline.rs`, `timeline_storage.rs`, and adjacent agent modules: canonical history, event ordering and publication.
- `packages/daemon/src/transport`: connection lifecycle and output/backpressure.
- `packages/gui-core/src/connection.rs` and `runtime/`: wire delivery, correlated requests, owned observations and directory synchronization.
- `packages/gui/src/features/timeline/{session,store,fold}.rs`: paged history and live reconciliation; `panel.rs`, `chat_row.rs`, `markdown/` and `reveal.rs`: virtualized presentation and paced reveal.

## Correctness before smoothing

Live events provide immediacy; authoritative history and sequence/cursor reconciliation recover gaps. Never drop events or invent text to make a stream appear smoother. Client disconnect, hidden views and slow consumers must not lose durable provider output. Cancelling a view subscription is not cancelling a provider turn.

Bound memory and update work. Avoid rereading/rebuilding the complete retained history for every delta, eagerly materializing offscreen content, or sending per-token filesystem writes without the owner's ordering guarantees. Measure provider arrival, daemon processing, network delivery, core queue work and Makepad layout/draw separately.

Parent and provider-child timeline commits update only the affected projected row and cursor
metadata in SQLite, not the complete retained history. Owners retain connections and prepared
statements. Child descriptors have a separate parent index.
Child storage work runs on blocking workers, without holding an agent data guard across I/O.
Publication still follows durable commit; client text-reveal timing must not change that order.

Pi/OMP stdout dispatch resolves RPC responses and routes host-tool calls/cancellation before
transcript delivery. Ordinary frames go through a single FIFO delivery pump; consumer backlog
spills through the admission spool instead of blocking the stdout reader on a bounded channel.
This isolates a demonstrated propagation mechanism, not proof of the first lock owner in a
historical incident. Private spool I/O can still fail or stall independently of the consumer.
Admission and replay share a 4096-frame/16 MiB accounting budget that includes
inline JSON values, array/string reserved capacity and a conservative live map-entry
allowance. This is an estimate, not an allocator or RSS ceiling: opaque retained map
capacity, allocator overhead, spool buffers and outer stage-vector spare capacity
are excluded. An incoming or deserialized single frame may exceed the budget;
delivery preserves the complete frame rather than truncating or dropping it.

OMP requests `set_event_filter` with `events: null` and `messageUpdates: "delta"`
on initial open and runtime restart. Only an echoed `"delta"` enables that
capability; older OMP without the acknowledgement retains full snapshots. Invalid
acknowledgements and transport failures remain errors. Pi is unchanged.

OMP reconstruction tracks RPC message IDs and content-block indices independently,
preserving Unicode and text/thinking order. Block ends and the complete terminal
message recover missing suffixes and replace unpublished fragments without
duplicating emitted text. The neutral stream is append-only: a terminal body
contradicting already published bytes produces an explicit error, not a fabricated
merge; native history remains the authority for recovering that final body.
Trailing thinking whitespace stays unpublished until it becomes internal text or
an explicit end/terminal body confirms it. An end event without a body closes the
block but does not assert empty content; an explicit empty body is authoritative.
This changes provider payload handling, not batching, settling or polling policy.

Physical client frame admission has a one-second stall deadline as well as its byte budget;
a saturated small-frame queue disconnects that source without failing the agent commit.
Read-only agent snapshots fall back to committed storage with
`snapshotAvailability: "last_persisted"` when live state is unavailable. Usage discovery and fetch
run independently per source, streaming successful reports without waiting for another source.
Completion reports `complete`, `unavailableAgentIds`, and `unavailableSourceIds`; these do not
fabricate provider availability or discard previously displayed reports.
The supervisor receives an authenticated stopping frame and enforces a ten-second worker
teardown deadline for both normal stop and restart.

The core hydrates project, workspace and agent directories independently, exposing per-list progress
and errors while retaining previous rows. Live directory updates remain ordered behind the relevant
baselines, including workspace changes that also affect projects. Request/ACK waiting is distinct
from the agent's actual activity; stale/offline snapshots do not animate as current running state.

The app admits at most 64 queued core events in one dispatch, taking a finite queue snapshot
under one lock. A nonempty batch arms a 1/60-second continuation timer; incoming signals cannot
bypass that timer. This yields between batches even while a producer keeps refilling the queue,
including when no window is drawing. It does not drop canonical events or impose a byte/time
budget on an individual event.

Timeline sessions apply matching events in order, then publish one projection after the batch.
Fold text and sequence ranges append in place while privately owned, using copy-on-write when
an older screen snapshot still references the row. Hidden controllers can reconcile without
painting; `PortalList` limits mounted rows and row UI state remains outside recycled items.
Reveal, streaming updates and composer growth must preserve the reader's anchor, selection and
IME state. Cancelling presentation work is not cancelling the provider turn.

Small live assistant documents prepare the paced reveal inline, avoiding a
mandatory parser/layout worker round trip per visible update. The source and
aggregate inline-layout byte limits plus a soft between-layout deadline are
defined in [Markdown rendering](markdown-renderer.md#streaming-work). The
deadline does not preempt an individual layout call.

Larger assistant documents capture immutable source in 64-KiB preparation
slices. Whole-document normalization, parsing, semantic flattening, find
matching and grapheme-boundary walks run in a bounded two-worker parser pool.
One captured revision proceeds to publication while newer demand coalesces
behind it; a continuously growing open paragraph cannot repeatedly cancel all
progress. This presentation queue does not discard or reorder canonical events.

`MarkdownPresentation` prepares a complete successor while retaining the previous
coherent source/layout snapshot. Beyond the inline allowance, semantic font-span
conversion is sliced; native `AsyncLayout` shapes and wraps on an independent
worker-local loader/shaper with immutable font definitions. Background layout
hydration and final glyph destruction are sliced. Each preparation pass
advances every unprepared paragraph it reaches within a soft 4 ms deadline, so
the time to publish follows the document's work rather than its paragraph
count; the reveal rule and that pass are defined in
[Markdown rendering](markdown-renderer.md#streaming-work).
The app drains retired layouts on an initial signal and then a 1/60-second timer
while work remains, independent of drawing and row/window lifetime; additional
signals cannot bypass the timer cadence.
The layout queue admits at most 64 waiting jobs, replaces latest demand per owner,
and retries full-queue admission without blocking. Full source geometry, links,
styles, find ranges and copy offsets are published as one revision. No new text
cap or arbitrary Markdown/shaping split is used; the old assistant desktop
presentation cap is no longer on this path.

Exact measure-first `PortalList` drawing resolves upward rows, bottom alignment
and reader-anchor geometry before assigning a viewport. Indexed visible rows and
glyph ranges then restrict paint/rasterization, including ordinary unwrapped code
lines. Selection retains the full layout; `Canvas` records source-owning runs and
separators instead of rebuilding all plain text each paint. Indexed cursor, word
and prefix-hash operations avoid rescanning the selected prefix every frame.

Collapsed reasoning still avoids building its hidden body. Fences and source
previews preserve the shared syntax worker, bounded latest-snapshot queue and
existing syntax-color size limit; central timeline signal handling polls results.
Tool previews may display selectable plain text while waiting. Assistant layout
publication waits for matching colors or the existing plain-text fallback so
new source is never paired with previous glyph geometry.

These are source-level work-placement and admission guarantees, not measured
frame-time claims. Whole-source parsing/layout still costs worker CPU and memory.
Font initialization, allocations, many block/decorative objects and pathological
overlapping glyphs are not hard real-time bounded. Other synchronous Markdown
surfaces are not implicitly migrated by the assistant worker path. The current
request prohibited builds, tests, runtime scenarios and benchmarks; newly added
regression cases remain unexecuted.

## Evidence

Use repeatable input for an isolated performance comparison and a real provider for interoperability. Report first visible output, sustained cadence, long-gap behavior, CPU/memory and retained-history size, not one average that hides stalls. A controlled development provider is useful for timing but must not be called a real-provider test.

Retired web/React benchmark paths and earlier Expo measurements are not native baselines. Capture actual native traces/screens and retain the run conditions. Script/setup PTY output is a separate pipeline described in [terminal-performance](terminal-performance.md). Current executed evidence is in [testing](testing.md).
