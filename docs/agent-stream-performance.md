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

Reveal boundary lookup starts at the current Unicode grapheme boundary rather than rescanning
the complete prefix. Visible assistant text is borrowed; collapsed reasoning does not build its
hidden detail body. Markdown preparation/layout caches reuse unchanged paragraphs. Code syntax
highlighting runs on one native worker with a bounded latest-snapshot queue; pending text remains
plain and selectable, and results apply only to the matching content, language and theme.
Completion is polled by the central timeline signal handler, not by an arbitrary mounted row.

These changes bound admission and remove repeated work; they are not a hard frame-time guarantee.
A single changing Markdown paragraph still requires parsing and exact layout on the UI path,
and virtualization cannot avoid the cost of one very large row. The existing desktop assistant
presentation limit is unchanged; no additional truncation or canonical-history loss is used as
a streaming optimization. Absolute-position culling before `PortalList`'s final row alignment
is unsafe and is not enabled.

## Evidence

Use repeatable input for an isolated performance comparison and a real provider for interoperability. Report first visible output, sustained cadence, long-gap behavior, CPU/memory and retained-history size, not one average that hides stalls. A controlled development provider is useful for timing but must not be called a real-provider test.

Retired web/React benchmark paths and earlier Expo measurements are not native baselines. Capture actual native traces/screens and retain the run conditions. Script/setup PTY output is a separate pipeline described in [terminal-performance](terminal-performance.md). Current executed evidence is in [testing](testing.md).
