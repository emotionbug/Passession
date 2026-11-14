# Timeline sync

Agent chat delivery has two paths:

1. **Live stream** — `agent_stream` WebSocket messages for immediacy. These may be delta-shaped lifecycle updates.
2. **Authoritative history** — `fetch_agent_timeline_request` for correctness. This always returns full projected timeline items, never lifecycle deltas.

The daemon retains projected items in memory. Each source event advances the stream sequence,
then replaces the previous tool state or merges into the current text item. Intermediate payloads
are never retained for history or catch-up. Provider history supplies the native transcript,
but a successful replay call alone does not certify completeness. Providers distinguish complete
snapshots from drained caches, bounded context or damaged/missing source records.
Cached rows and startup notifications do not certify that replay has completed. A resumed agent's
first authoritative history response waits for replay; live events received during recovery follow
it. Clients that opt into saved reads get the checkpoint first, labelled as such (see
[Saved history and execution readiness](#saved-history-and-execution-readiness)).

A refresh must uniquely match every existing canonical row before replacing the projection,
preserving client/provider message IDs and turn IDs. Ambiguous or missing matches retain the
existing checkpoint. Agent snapshots expose `historySync.status` as `complete`, `partial`, or
`retained`; an unverified replay can seed an empty projection without claiming completeness.
Only an explicit rewind backed by a complete source may remove existing rows; a complete native
replay of a finished child has the same authority over that child. Matching displayed content never
settles a user-send receipt.

Non-authoritative refreshes also require each matched row's non-empty `item.text` to remain a
prefix of the incoming text. Matching identities alone cannot authorize shortening or rewriting
live text; a stale snapshot retains the canonical projection.

Per-agent projection checkpoints live in `agent-timelines/{id}.sqlite3`. A retained SQLite
connection commits the changed projected row and sequence metadata together using WAL and
`synchronous=FULL` before publishing the event. Failed commits restore the in-memory projection;
full provider replay replaces the checkpoint in one transaction. The checkpoint never replaces
provider history as the transcript authority. `{id}.json` checkpoints from earlier releases are not
read.

Saved-page selection uses a SQLite expression index for sequence metadata, avoiding
source-row JSON extraction across the whole history on each indexed page. Selected
rows are still decoded and validated. The checkpoint remains version 1: SQLite
maintains the index even when an older writer changes `row_json`. Writable open
creates and validates the index transactionally; failure leaves no partial migration.
Read-only opens never migrate a checkpoint. An unmigrated database retains the
original JSON-extraction read path until a writable open creates its index.

The invariants are:

> A continuously subscribed client applies every committed row in order. Opening or resuming an
> agent establishes the daemon's current tail in one bounded request, with older history reachable
> through backward pagination.

Tool output is bounded before it enters either delivery path. Canonical shell tool output is sliced
to 64 KiB, and the same bounded item is used for runtime timeline rows and live stream events.
Provider history hydration applies the same rule so reopening an agent cannot restore an oversized
tool payload.

## OMP authoritative message repair

An OMP terminal message can contradict text already published live. Repair is
limited to that exact marked message in the current agent cell, with its turn
checked when supplied; a provider-supplied identifier never authorizes changes to
another agent. Reasoning and assistant items retain `metadata.ompStreamMessageId`
so adjacent native messages cannot lose their ownership through projection.

The private `timeline_reconcile` provider event must be consumed by the agent
owner, never forwarded as an unknown public event. A safe repair replaces the
marked message's complete authoritative content atomically, preserves unrelated
rows, and publishes the existing `timeline_replacement` epoch invalidation after
persistence. Clients recover through normal history fetches; no new public wire
event or duplicate final append is needed.

Missing or ambiguous ownership, including interleaving that cannot be placed
uniquely, must retain the previous canonical data. Report an ordinary error with
structured recovery guidance and bounded non-text diagnostics, without ending the
event owner. Do not infer placement from matching text or treat a failed repair as
successful reconciliation. Native history remains the transcript authority.

## Saved history and execution readiness

Reading a conversation and running it are separate. A cold provider start plus full history replay
takes seconds (several for large Codex threads), and a provider can fail outright; neither may delay
or hide what the daemon already saved.

Clients advertising the `saved_history_reads` capability get `fetch_agent_timeline_request` answered
without waiting for the provider. The response carries `historySource`: `live` when a ready,
history-synchronized runtime served it, `saved` when it came from the checkpoint or a runtime still
replaying. A `saved` page has ordinary epochs and cursors but certifies nothing about currency.
Connected-host routes forward the origin client's hello capabilities, so a routed app session gets
saved reads from the destination daemon. The CLI, MCP fleet tools and SDK do not advertise the
capability: they return transcript entries without readiness metadata to callers (models, scripts)
that treat them as current, so they keep the blocking authoritative read.

A saved read starts one owned background preparation per agent: provider start and history replay.
Concurrent reads join it. Agent snapshots carry `preparation: {status: "preparing"}` or
`{status: "failed", error}` while it runs or after it failed, and `agent_update` announces the start
and the outcome. The field is an observation: it is never persisted and never authorizes anything.
Sends, permissions, mode changes and every other mutation still wait for a live owner and never act
on a saved snapshot. The next saved read retries a failed preparation; a live owner established
any other way clears the failure. Daemon shutdown cancels outstanding preparation.

An agent without a checkpoint has nothing saved to show, so that read waits for preparation and
returns its error rather than an empty conversation. An unreadable checkpoint is an error response
and starts nothing. Timeline search and the prompt index read the same projection without starting a
provider.

After a `saved` page, the client refreshes `after` its end cursor once per readiness transition.
Replay normally changes the epoch, so that refresh resets to the new tail through the stale-cursor
path. Once a saved page lands the chat stays readable: no overlay or full-screen error replaces it.
Send, Enter, send-now, queue drain and agent controls require a live snapshot (no `preparation`, not
`last_persisted`) observed since the latest preparation started; a later `last_persisted`
observation does not revoke that. While `preparation` is present the client shows the provider
error with Retry inline (Retry is an ordinary resume fetch) and treats the snapshot's pending
permissions and running turn as absent. A `last_persisted` snapshot neither grants nor revokes
permissions or turns.

## Presence is not delivery

Client heartbeat reports presence:

- device type
- app visibility
- focused agent
- last activity time

Heartbeat is used for notification routing. It must not be used as a correctness gate for `agent_stream` delivery. A stale mobile focus heartbeat may affect whether the user gets notified; it must not make timeline rows disappear from the live stream.

## Gap recovery is paged but complete

Large unbounded timeline responses can exceed relay frame limits, so catch-up uses bounded pages. Bounded does not mean partial.

Page limits are projected-item targets. A tool call lifecycle is one projected item even if it spans many source sequence numbers, and assistant/reasoning chunks are merged before counting. The response carries `seqStart`, `seqEnd`, `sourceSeqRanges`, and `collapsed` so clients can advance sequence cursors without rendering delta rows.

When live delivery detects a sequence gap, the app fetches `direction: "after"`. If the daemon
responds with `hasNewer: true`, the app immediately fetches the next page from `endCursor`. Gap
recovery is complete only when `hasNewer: false`.

Initialization timeouts guard lack of catch-up progress, not the full multi-page sync. A successful page that queues the next `after` page refreshes the watchdog.

Opening or resuming an agent fetches one bounded latest tail page. Older history remains
user-driven by scrolling upward.

A failed catch-up or subscription reconcile retries on its own, doubling from 1s to a 30s ceiling.
A fixed 1s retry turned a persistent daemon-side refusal — a Codex thread that already has an active
writer, say — into a request and a log line every second on an idle app. The delay resets on success,
reconnect, delivery-mode change, and visibility change, so recovery is still immediate once the
condition clears. Republishing the same visibility set is a no-op and never bypasses backoff.

Background retries are silent. The retry the user presses in the sync-error callout is a fallible
user action and owns its pending state: `retrying` is a status the sync model publishes, not a view-local
boolean, so the button reports in-flight and the callout returns to `error` when the attempt fails.

Reaching the history-start threshold loads one older page. Before replacing the visible model, the
view samples the current measured content row and pixel offset, never the history/loading sentinel.
Stable row identity, with projected-sequence fallback for merged groups, preserves that anchor and
any drag or fling movement since the last draw.

Pagination is evaluated after the replacement has been remapped and laid out. Cursor progress alone
does not trigger another page: the user must leave and return to the threshold. Short or compacted
content may continue automatically only while the viewport remains underfilled and the reply makes
backward cursor progress in the same epoch. Errors, unchanged cursors and epoch replacement cannot
create an automatic loading loop.

## Durable item anchors

Provider message IDs are not guaranteed for every displayed item. Paseo-generated system errors are one example. Rendered item indices are not durable either because pagination and projection can merge source rows.

Actions that address a point in chat history, such as Fork, use the daemon timeline `epoch` plus the projected item's `seqEnd`. The app carries that position on the rendered assistant item for both live and fetched history. When adjacent projected chunks merge, the merged item retains the newer chunk's position.

The daemon validates the epoch and locates the projected item at the selected position. A fork
includes projected items through that checkpoint. If an item spans the checkpoint and changed
afterward, the daemon refuses that fork with an actionable error: discarded historical payloads
cannot be reconstructed from sequence metadata. Forking the current context remains available.

## Resume behavior

Opening, reconnecting, and returning from app background establish the current timeline through
bounded catch-up. Switching between continuously subscribed open chats needs no fetch.
Focus alone does not mutate timeline state; the response is compared with the local
authoritative range first.

Cached history remains readable during recovery. The chat shows Reconnecting to host while the host is
offline, then Updating messages until authoritative catch-up completes. Socket connectivity alone cannot
certify that the displayed conversation is current. The timeline owner publishes freshness; the
view renders it without a toast timer or a separate resume workflow.

The draft-create handoff has the same lifetime: the viewed-timeline owner releases it when the sync
stops owing that chat a catch-up, not when the first authoritative page lands. Releasing it at the
first page leaves a chat this client just created looking hydrated but still catching up, which
shows Updating messages for a conversation that cannot be out of date. Closing the tab ends the
obligation too, so a reopened chat starts from authoritative state instead of a stale optimistic
one; disconnect and backgrounding keep it.

Foregrounding probes a nominally connected session immediately. A healthy response preserves the
socket; a failed three-second probe starts reconnecting without waiting for the background heartbeat
or retry backoff. This cannot keep a mobile socket alive after the operating system suspends it.

- The same epoch and `window.maxSeq` is an exact display no-op. The app advances synchronization
  bookkeeping without replacing timeline arrays, preserving an upward-scrolled viewport.
- When the page overlaps or is adjacent to the local end cursor, only projected items newer than
  that cursor are applied. Already-covered rows are not replayed.
- A true middle gap, epoch change, or rewind atomically replaces stale canonical history with the
  latest tail. The replacement reconciles positioned live rows beyond its coverage and unresolved
  local submissions; it never retains two discontiguous canonical ranges.

The installed tail carries `hasOlder`, so history skipped by a replacement remains reachable through
ordinary backward pagination. A backward page is accepted only when it is adjacent to the current
history start; a response requested from a pre-replacement range is stale and is discarded.

A plan approval keeps the original proposal's tool-call identity through resolution and provider
history replay. The pending approval UI can hide that tool from presentation, but the client model
must retain its position. Creating a new history card on rejection places it after the prompt that
rejected it; changing steer-event ordering would also put new assistant output before that prompt.

The daemon stages live events during start/steer admission and history hydration. This includes
output from an already active turn: accepted steering must precede output received while its
acknowledgement was pending. The 4,096-event and approximately 16 MiB thresholds bound each
in-memory staging chunk, not the total accepted output. Larger queues spill to a private,
temporary file and replay disk batches before the memory tail. A single oversized provider frame
is spilled immediately; these thresholds estimate retained payload, not total allocator usage.
The spool is not a durable event log; provider history remains authoritative after restart.

The Pi/OMP stdout reader uses that spool independently of the admission stage. One delivery pump
preserves ordinary-event FIFO, while correlated responses and host-tool call/cancel controls
are handled at the reader rather than waiting for transcript publication. This does not move
ordinary output ahead of an accepted steering prompt.

Out-of-band commands are recognized synchronously without side effects. Their canonical prompt
is published before execution starts in the background, without holding the admission stage or
mutation lane until command completion. Command errors are streamed to that same cell.
Spool or event-persistence failure ends the event owner, clears pending ownership and barriers,
and permits close/reload; a late acknowledgement cannot restore the failed owner to running.

## Provider child history

Child transcripts use the same projected-page reconciliation as the main conversation. The client
retains rendered items and sequence cursors, never a second cache of source events. Pagination
uses the projected display anchor; live updates advance the source cursor without moving tools.

Persistence uses `agent-subagents/{parentId}/index.sqlite3` for descriptors and one
`{sha256(childId)}.sqlite3` timeline per child. Row/cursor changes use the same WAL/FULL
commit-before-publication contract as parent timelines. Blocking storage work runs outside
the async executor; ordinary events do not clone/rewrite sibling histories.

User deletion (`agent.provider_subagents.delete.request`) removes a finished child's catalog row
and timeline file and inserts its id into the catalog's `tombstones` table in the same
transaction (catalogs written before this gain the table on open). Replayed history (parent
resume, daemon restart, child-body reads) and every live event for a tombstoned child are dropped
without publication; only a live `running` upsert lifts the tombstone, as a new run.

Parent cards that summarize child work keep their recorded log across resume without reading any
child: `resume_session` options carry `subagentCardLogs` (call ID → stored card log). A replayed card
replaces the stored row by call ID, so the provider must emit the seeded log in it and append live
child output after it once.

The descriptor list and child pages never start the parent. Parent replay and live events announce
children with descriptors carrying a provider-owned `source` locator, which stays inside the daemon.
Opening a child reads that child alone: a child with stored rows answers from storage, and its native
body is fetched through the parent's provider (`AgentProvider::stream_subagent_history`) in the
background. Only a complete replay that replaced the stored rows (or a provider without a separate
body) is reused, and only for a finished child; a running child, an unverified replay, or a replay
that could not account for the stored rows is read again on the next open. A
child with no stored rows waits for that fetch, and a failed fetch
is an error response. With stored rows, the page and the child's descriptor (list responses and
`agent.provider_subagents.update` upserts, published when the refresh starts and settles) carry
`nativeRefresh: {status: "refreshing"}` or `{status: "failed", error}`; the rows stay visible and
the next `timeline.get` for that child, including the pane's Retry, retries. The client shows the
state only for the open child. The fetch replays only rows for the opened child; rows for other
children are discarded. A complete replay of a child already recorded as finished is authoritative
and replaces its stored rows. Anything else is a guarded replacement: a running child can have live
rows its native history does not contain yet, however well-formed that history is.

Codex includes unfinished turns when reading a cold child with no attached live stream, but leaves
them to that stream when it exists; either unfinished snapshot is unverified.

A provider whose live session publishes rows for that child cannot hand the replay back through the
hook: rows published between its snapshot and the daemon's apply would be overwritten or
duplicated, and comparing text or timestamps cannot recover the order. It runs a per-child
two-phase replay instead (`SubagentHistoryRequest::replay`):

1. Under its own live-event ordering it reads the snapshot and publishes `replay.marker(...)` on
   the parent's live sink, then returns `SubagentHistory::Published`. Rows published before the
   marker are covered by it; nothing can fall between.
2. It holds that child's later live updates and awaits `replay.outcome()`. The parent's event
   consumer applies the marker in order and reports `Applied` or `Retained`.
3. `Applied`: the provider rebases its live position onto the snapshot. `Retained` (or `None`, when
   the parent closed first): it keeps its previous position, which still matches the stored rows.
   Held updates are then computed against that position and published after the marker.

Only that child's rows wait, and only until its own marker is applied. The provider must keep
serving the parent session meanwhile (await the outcome in a separate task, never in the session's
event loop): a marker behind a held event stage (the parent's own history replay or a turn
admission) applies only once that stage releases, and releasing it can need the same session.
For the same reason, and so a parent session cannot start between a provider's live/cold decision
and the application of a cold (`Replayed`) result, child reads share a per-parent gate that starting
or replaying the parent takes exclusively: a child read waits for a parent already starting, and a
parent start waits for child reads in flight. Child reads never start the parent. Providers register
a live session before it can publish any child row, so "no live session" means no live rows. When a
read ends without its marker being applied (error, cancellation, shutdown), the outcome resolves
`None` and a marker arriving later is ignored, so stored rows and the provider's position never
diverge.

A provider without a live session for the parent returns rows through the hook (`Replayed`); no live
rows exist then. Providers without a separately
readable child body answer from rows collected from the parent's stream. The read holds no MCP
grant: issuing one for the parent's agent ID would revoke the live session's grant. Existing
children absent from an incomplete replay remain available. A changed child epoch triggers the
existing projected-timeline resynchronization path; an open child refetches its tail once per newly
announced epoch, and inactive children never fetch.

Clients advertising `projected_subagent_timeline` receive child streams and projected fetches.
Updated clients require `features.projectedSubagentTimeline` for child history; older hosts show
an update-host notice for that pane.
Older clients retain child names/status and receive an upgrade message when opening a child
conversation. This gate affects only the child transcript; it does not gate the connection,
main conversation, or current-context forks. A legacy `canonical` root request still receives
projected items.

## Client replica lifetime

The session projection remains host-scoped for as long as the host is registered. The viewed-timeline
owner wraps cached preparation, network catch-up, accepted timeline application, and persistence
behind one interface. The client supplies transport and projection operations without selecting a cache
path or issuing a separate persistence notification.

Active-agent list reconciliation does not own transcript lifetime. An archived agent's fresh
history response can arrive before the active-list response that omits it. Clearing history there
would discard accepted rows while the viewed owner still considers them synchronized. Entity
deletion clears the transcript; list membership changes do not.

Removing the host from the registry is the destructive boundary: it stops the runtime and clears the
session and host-scoped setup state together.

The timeline owner asks durable replica storage for an agent when that agent becomes visible. An
accepted row paints immediately before subscription acknowledgement or timeline fetch. The stored
range describes those exact items: `startSeq` drives older pagination and `endSeq` drives forward
catch-up. The owner requests `after endSeq`, and requests `before startSeq` when the user loads older
history. Code outside the owner does not distinguish cached and network timelines.

The first resume request is bounded. If it reports more newer history, fetch one latest bounded tail
instead of replaying every missed page. Live gap recovery still pages forward until current.

If the canonical window exceeds the cache item limit, contains a discontiguous retained range, has a
live head, or includes presentation data the cache cannot encode losslessly, persistence drops the
range and keeps a display-only tail. Never slice items while retaining the pre-slice range; that
falsely certifies discarded source rows. A display-only row paints without granting synchronization
authority, so the owner uses the ordinary bounded `tail` bootstrap.

Live rows received between cache paint and catch-up stay in the separate live head and reconcile with
the authoritative range through the existing forward-page path. The cache does not persist sync
generation or unreconciled local submissions.

Every daemon-derived live item carries its timeline epoch and sequence position. Bootstrap
replacement keeps only positioned rows newer than the page it installs, while unresolved local
user presentations identify themselves by having client identity without a timeline position. This
prevents a page from duplicating rows it already covers without coupling display continuity to the
shorter-lived submission registry. Unreconciled local presentations are not persisted in the durable
replica cache.

## Selective and legacy delivery

The app chooses one delivery policy from `server_info.features.selectiveAgentTimeline`:

- Selective daemons receive the chats this session has opened, plus any visible agent pane. A chat
  enters that set the first time the user opens it and leaves when its tab closes or the session
  ends, independently of mounted or retained native views: switching workspaces, evicting a retained
  view, app backgrounding, and reconnect all preserve the demand. There is no recent-agent limit.
  Restored workspace layout is a release signal, never a source. It carries a tab for every chat the
  user has ever opened on the host, and a catch-up fetch resumes its agent on the daemon, so
  subscribing from layout spawns a provider session per historical tab at every launch and stamps
  every one of those workspaces as just used (see
  [agent lifecycle](agent-lifecycle.md#workspace-activity)). Visible chats get the first catch-up
  attempt; the rest follow when those attempts settle, including failures, so a failed visible chat
  does not starve background recovery. Split panes catch up together. Hidden chats update the
  replica while the native presentation keeps rendering. Revealing a chat reads the current store
  and preserves its local UI state.
- Legacy daemons keep globally streaming agent timelines. Visibility still triggers the existing
  authoritative catch-up, but the app does not issue selective-subscription RPCs.

This is a client synchronization policy; downstream presentation must not branch independently
on daemon version. The former `viewed-timeline-sync.ts` is not a current implementation owner.

## Projected pages reconcile with live presentation

A projected page is canonical state, not a sequence of live deltas. One projected item can overlap
rows already received live—for example, a tool call retained at its original display position while
its completion advances `seqEnd`, followed by a merged assistant message. The app uses
`sourceSeqRanges` to replace overlapping assistant and reasoning projections before applying the
remaining page through the existing stream reducer. It must not append full projected text to a
live prefix.

Composer sends, queued send-now, and `ChatDelivery`'s queue drain preserve the same immutable
message ID and behavior through `agentMessageFields`. `SentUserMessages` retains attachment
presentation when the daemon echoes a text-only canonical user row.

A submitted prompt keeps its stable local presentation identity, text, images and attachments.
`ChatDrafts` stores pending delivery separately from the editor generation. An authoritative
receipt or successful legacy RPC can settle that exact send; a late error must not restore its
text over a newer draft. Canonical row reconciliation changes presentation, not delivery evidence.

Host send admission, native provider acceptance, model consumption and canonical presentation
are different observations. A canonical user row can record a rejected attempt and is not a
delivery receipt. Timeline rendering or directory activity cannot settle an uncertain send.

Hosts advertising `agentMessageReceipts` persist exact `(agentId, messageId)` receipts before
dispatch. Clients query `agent.message_receipt.get.request`, explicitly subscribe to
`agent.message_receipt.update`, and apply revisions monotonically. `submitted` with
`correlation.acknowledgementPending: true` remains unconfirmed until a correlated native ACK;
`accepted` does not prove that a model has consumed a steering instruction.

Owner loss before the durable dispatch boundary is `not_delivered`; after that boundary it is
`outcome_unknown`, unless native evidence proves acceptance or rejection. A missing receipt
also means unknown, not safe-to-resend. No new-ID automatic retry is allowed.
`agent.message_receipt.dismiss.request` creates a sticky exact-ID tombstone. Dismissing does not
claim that already-dispatched work was stopped; a late ACK can improve delivery evidence but
cannot clear dismissal or resurrect a client pending draft. Only proven non-delivery permits
same-ID retry. Failed content is retained separately from newer editor generations.

`canonicalSubmittedPrompts` still guarantees canonical presentation with the client's message
ID, including daemon-handled commands. For older hosts without receipt support, an explicit
successful RPC response settles the local send; a missing visible history row is never evidence
of non-delivery. Local-only dismissal is labelled as such and does not claim remote cancellation.

Turn activity has one client-side replica. Lifecycle events can attach `turnId`, and agent snapshots can
expose `activeTurn: { turnId, startedAt } | null`. An identified terminal cannot close a different
identified turn; an unnamed legacy terminal can close the current turn. Snapshots, stream events,
cancellation requests, and every visible surface update the same per-agent record. The snapshot covers
both user-started foreground turns and autonomous provider turns;
foreground control ownership remains a separate daemon concern. Cancellation request identity is stored
with that record rather than in a widget, so an old request cannot clear a newer one. Submissions
remain separate from the active provider turn and settle by exact-request delivery evidence.

Canonical turns and visible responses are different boundaries. Agent-originated prompts and lifecycle
notifications project into tool rows, so they do not create a human-message boundary. Legacy system
prompts, including schedule injections, remain hidden. One visible response can span several canonical
turns; layout and copy group that response together while lifecycle, timing, tool sequences, and exact
fork positions retain the canonical `turnId` boundaries.

Agent-message XML belongs to the daemon's `agent/agent-messages` boundary. Prompt delivery encodes
provenance; acceptance, provider echoes, import, and replay share its projection. The delivery ID is
also the projected call ID, so a provider echo cannot create a second row. Sender IDs are opaque and
do not imply local ownership or authorization. Capture the sender title at delivery: archived or
removed senders must remain understandable without a live directory lookup.

The optional `agentMessage` payload carries event, sender, and delivered text. Shared client
presentation (`protocol/paseo-tool-presentation`) owns its heading and expanded details, next to the
labels for every Paseo MCP tool. The daemon
must not construct UI labels or prepend sender labels to the semantic message body. The existing
`tool_call` / `plain_text` envelope is a compatibility adapter: older clients still parse the row and
expand its body. New clients use the typed payload, never infer the event from a tool name or parse
XML. Stream reduction and disk-cache serialization preserve the payload without rendering it.

The compatibility boundary for older daemons is snapshot normalization: running/idle status becomes an
anonymous active turn or idle state once, and downstream code consumes the same activity shape. The app
does not combine anonymous lifecycle events, timestamps, timeline rows, and resume coverage to infer a
second running state. Disconnect preserves the last replicated turn until cache or network hydration
advances it; replica removal remains the destructive close boundary. Elapsed time comes only from turn
liveness, never from submission records or whichever timeline rows happen to be mounted.

For human prompts, the daemon records one canonical submitted user row at acceptance. Its wire `messageId` is the
submission's `clientMessageId`, so the row is born with its final identity and remains immutable on
the wire. A correlated provider echo records the provider's native identity internally without
dispatching another timeline event. Rewind resolves the wire identity to that provider identity at
the provider boundary. Daemon-handled prompts follow the same identity rule.

Content matching is limited to the dated compatibility path for daemon timelines created before
that field existed. Canonical ingestion may match only an explicit unreconciled local candidate;
the draft-create handoff is the one boundary that also permits the legacy canonical twin to have
arrived first. Generic reducers and consumers do not reimplement message identity matching.

Ordinary bootstrap, same-epoch reset, and catch-up replacement preserve unmatched locally submitted
rows because a provider may never echo them. A known epoch change or rewind replaces history and
drops acknowledged local rows omitted by the new canonical epoch; every transaction not yet
acknowledged by the provider, and no other local row, crosses that destructive boundary. A cold
reset without an existing epoch is destructive because the client has no continuity anchor.

Tail rows are positioned history, so an unmatched local presentation is appended after the
canonical replacement rather than ordered by timestamps from different machines. The head is a
live overlay: cursorless items stay there during continuity replacement until canonical positions
arrive, while a destructive replacement retains only active submission transactions.

Canonical replacement owns both timeline lanes. A matching local row keeps its presentation ID and
payload while taking the canonical row's ordered position. If a live assistant head is the
canonical assistant prefix, it stays in the head lane. No row may be returned in both lanes.

## Relevant code

- Rust canonical timeline, persistence and forwarding: `packages/daemon/src/agent/timeline.rs`, `timeline_storage.rs`, `timeline_stream.rs`, and `transport/`
- Rust saved reads, background preparation and lazy child bodies: `packages/daemon/src/agent/history_read.rs`
- Rust client transport, response rules and observations: `packages/gui-core/src/connection.rs`, `protocol.rs` and `runtime/`
- Viewed-session history reconciliation and retained replicas: `packages/gui/src/features/timeline/{session,store,fold}.rs`
- Virtualized rows, Markdown, reading hold and paced reveal: `packages/gui/src/features/timeline/{panel,chat_row,reveal}.rs` and `markdown/`
- External Rust SDK: `packages/client`
