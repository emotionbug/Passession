# Protocol Compatibility

The app and the daemon are separate products that ship separately. A user updates the app from an app store or a desktop auto-update; they update the daemon when they feel like it. Every combination happens in the wild: new app against an old daemon, old app against a new daemon, and both sides months apart.

In development both sides are always the same version, which is why this is the constraint contributors miss most often.

Two contracts follow from it.

## The protocol contract: always compatible

A schema change must not break parsing in either direction. An old app still parses messages from a new daemon. A new daemon still parses messages from an old app.

`hello` carries an optional credential. The daemon accepts a client protocol version from its minimum through newer client versions, selects the lower of client and daemon maximum, and reports it as optional `server_info.protocolVersion`. A `hello.rejected` frame precedes an auth close only when the client sent `hello.auth` or advertised `hello_rejection`; older clients receive the existing WebSocket close code and reason. This avoids sending them a new top-level message their validator does not recognize.

- New fields remain optional for older peers. Any presentation default is explicit consumer policy, not a validator mutation.
- Never flip optional to required, remove a field, or narrow a type. `string` to `enum` and nullable to non-null are both narrowing.
- A field you stop sending stays accepted. You stop writing it, you don't stop reading it.
- Wire schemas are structural JSON Schema documents embedded by `paseo-protocol`. Validation never injects defaults, coerces types or transforms data; normalization is an explicitly named consumer pass. See [protocol validation](protocol-validation.md).
- Preserve literal message discriminators and lossless extension fields. A Rust enum or schema edit must still accept unknown optional data from a newer peer.

Two questions to ask before you commit a schema change:

1. Does a six-month-old app still parse this message?
2. Does a six-month-old daemon still send something this app accepts?

If you can't answer both with yes, the change isn't done.

Shared wire types and validation live in the Rust `packages/protocol` crate. New RPC names follow [rpc-namespacing.md](rpc-namespacing.md).

## The feature contract: per feature, gated once

Features don't have to work across versions. A new feature usually needs a new daemon capability, and old daemons don't have it.

The app checks for the capability and either runs the feature or tells the user to update the host.

- **No fallback paths.** Don't build a degraded version of the feature for old daemons. Don't fan out across legacy RPCs to simulate a capability that isn't there. The user updates or doesn't get the feature.
- **No defensive branches spread through the feature.** Detection happens in one place, and everything downstream reads a clean shape.
- Capability flags live in `features` on the `server_info` message; keep shared Rust protocol definitions, core negotiation and feature consumers aligned.

Existing functionality keeps working across versions because of the protocol contract. Gating a new feature never substitutes for that.

Model-facing history reads require `server_info.features.readOnlyAgentTimeline`.
They send the optional boolean `observationOnly: true` on
`fetch_agent_timeline_request`, reading retained memory/checkpoint pages without
loading or preparing a provider. Missing retained history fails explicitly.
Remote model tools gate this capability once; they do not fall back to a
provider-starting read. Existing UI/SDK requests omit the flag and retain their
history preparation behavior. Older clients still parse the unchanged response.

## Client capability ownership

Each client boundary advertises only behavior it implements. The GUI core's connection pump uses
the Rust SDK transport but owns its hello capability set and observation policy; SDK/CLI callers
can have different consumers. Keep advertised capabilities aligned with those implementations,
not merely with accepted schemas. The native product does not advertise embedded browser
automation. A schema accepting a message does not establish support for its delivery semantics.

On capable daemons, connecting creates no timeline or event demand. Client subscriptions own their network membership,
release it on unsubscribe, and restore it after reconnect. Raw message observers inspect traffic
without requesting streams. Application caches and which agents are visible remain caller-owned.

## Owned observations

`owned_subscriptions` and `server_info.features.ownedSubscriptions` negotiate the source-owned
contract described in [architecture](architecture.md#protocol-and-persistence). The client selects delivery
behavior once at its connection boundary. App workflows use the same observation interface on both.

With an older daemon, the client uses the existing connection and legacy RPCs. Directory subscriptions
remain shared and last-query-wins. Local handle IDs identify listeners; they do not promise independent
server filters. Timeline and event membership retain their existing shared behavior. Releasing a handle
detaches its listener and uses the old unsubscribe operation where one exists. Broadcast-only hosts keep
broadcasting; readiness there means local attachment, not a daemon acknowledgement. No extra sockets
or multiplexing emulation are introduced.

Preserve established workflows when changing delivery internals. Independent filters and quiet
connections require a capable daemon; opening the app, reading history and using terminals do not.
Pre-registry workspace grouping and legacy event normalization belong inside the client boundary.

Old clients keep their existing wire shapes and slot behavior at the daemon's source boundary.
The adapter keys legacy slots by physical socket, so an old connection cannot replace a modern
sibling's observation even when both use the same logical client ID. Optional wire IDs stay accepted
for parsing compatibility; modern requests cannot select their subscription ID.

Workspace setup details negotiate `workspace_setup_observation` with
`server_info.features.workspaceSetupObservation`. Capable clients cannot request details through
the legacy session event category. The setup panel owns a workspace-scoped observation while
active and visible; directory metadata signals failures without requesting output. Older clients
retain their legacy event delivery. A new app makes no setup event demand on older hosts and
requires a host update to view details. Subscription-capable older hosts honor that absence of
demand; broadcast-era hosts require an update before wire silence is guaranteed.

## Every shim is tagged and dated

A shim that exists for old-app or old-daemon support carries a comment naming it, the version it arrived in, and when it can go:

```rust
// COMPAT(workspaceFileEditing): added in v0.2.0, remove after 2027-01-18 once daemon floor >= v0.2.0.
```

`rg "COMPAT\("` is the full cleanup backlog, so:

- One tag per shim, at the site that has to be deleted.
- Give it a name, a version, and a removal condition or date. Six months out is the usual default.
- Never bury compatibility in an untagged `??` fallback or an optional-chain tunnel. Untagged back-compat never gets removed, because nobody can find it.

When a tag's condition is met, delete the shim and the tag in the same change.

## QA

Tests don't fully cover compatibility. If you touched `packages/protocol`, say in the pull request why an older app still parses your message and why an older daemon still satisfies your app. See [qa.md](qa.md).
