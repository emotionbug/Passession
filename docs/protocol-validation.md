# Protocol Validation

The native `paseo-protocol` crate owns shared wire schemas, JSON validation, binary framing and encrypted-record codecs. Rust consumers compile the crate with Cargo; there is no executable-validator generation or package-install step. The GUI uses the `paseo-client` transport through `paseo-gui-core`; the core also owns response correlation, response-shape checks and domain rejection handling in `src/protocol.rs`. Schema acceptance alone does not establish that a feature handles delivery, cancellation or reconnect correctly.

## Runtime Path

The public boundary is [`packages/protocol/src/wire.rs`](../packages/protocol/src/wire.rs). Session schemas are compiled lazily into a discriminator-indexed catalog with the native `jsonschema` library. Envelope, terminal snapshot and daemon configuration validators use their maintained schemas. Compilation is offline; validation does not retrieve remote schemas.

- `validate_session(&value, Direction)` checks a session message and cross-field semantics.
- `SessionMessage::parse(value, Direction)` validates and retains a lossless JSON object. Unknown extension fields are not stripped or normalized away.
- `parse_envelope(bytes, Direction)` checks the wire size, parses JSON, validates the envelope and nested semantic constraints, then produces an `Envelope`.
- `encode_envelope(&envelope, Direction)` validates the output and enforces the wire size before returning encoded bytes.
- `validate_terminal_snapshot` and `validate_config` validate those separate documents.
- `message_types(Direction)` exposes supported discriminator names; iterator order is not a stable catalog order.

Direction is relative to the daemon: **Inbound is client → daemon; Outbound is daemon → client**. Do not validate a response against the request catalog. `SessionMessage::new` permits constructing extension messages; it is not full validation. Ordinary serde deserialization checks representation but is not a substitute for `parse` at an untrusted boundary.

The direct WebSocket JSON ceiling is `MAX_WEBSOCKET_MESSAGE_BYTES` (100 MiB). Relay/encrypted frames have independent limits in `crypto`. Binary messages use `binary_frames`, not the JSON validator. Encryption authenticates transport peers/records; daemon password or local-credential authorization is a separate handshake concern.

## Schema Ownership

Maintained, checked-in schemas are:

- [`session-inbound.json`](../packages/protocol/schema/session-inbound.json)
- [`session-outbound.json`](../packages/protocol/schema/session-outbound.json)
- [`ws-inbound.json`](../packages/protocol/schema/ws-inbound.json)
- [`ws-outbound.json`](../packages/protocol/schema/ws-outbound.json)
- [`terminal-state.json`](../packages/protocol/schema/terminal-state.json)
- [`paseo.config.v1.json`](../packages/protocol/src/paseo.config.v1.json)

`wire` embeds these documents with `include_str!`; update the affected session and envelope definitions together when changing a message. They are runtime authority, not temporary generated build products. Preserve existing discriminator strings, record IDs, optionality, extension fields and binary compatibility. A new message also needs real daemon/client behavior, not just a schema entry.

## Example

Use Rust 1.93 or newer and edition 2024. In an application beside the checkout:

```toml
[dependencies]
paseo-protocol = { path = "../paseo/packages/protocol" }
serde_json = "1"
```

This path dependency does not assume registry publication. The following synchronous program validates and round-trips a request without connecting to a daemon:

```rust
use paseo_protocol::wire::{Direction, SessionMessage, encode_envelope, parse_envelope};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let message = SessionMessage::parse(json!({
        "type": "fetch_agent_request",
        "requestId": "example-request",
        "agentId": "saved-agent-id"
    }), Direction::Inbound)?;
    let encoded = encode_envelope(&message.into_envelope(), Direction::Inbound)?;
    let decoded = parse_envelope(&encoded, Direction::Inbound)?;
    println!("{decoded:?}");
    Ok(())
}
```

## Errors and Normalization

`ProtocolError` distinguishes malformed JSON, wire limits, missing/invalid fields, unknown message types and schema failures. Schema errors carry an instance path; do not rely on the text reproducing another validator's diagnostic format. Structural validation does not repair input, inject defaults, coerce types or normalize provider models. JSON Schema `default` annotations do not mutate messages.

Compatibility transformations belong in explicit consumer logic. For example, `providers::{compact_provider_snapshot,expand_provider_snapshot}` handles provider catalogs, while `timeline` and `helpers` expose specific presentation/identity helpers. Never hide malformed supported data behind a generic fallback. Preserve unknown extension data when forwarding messages.

## Regression Tests

The native regression suite lives in [`packages/protocol/tests/boundaries.rs`](../packages/protocol/tests/boundaries.rs) and module tests, including retained wire/crypto fixtures. Integration verification should cover:

- every supported discriminator in both directions and unknown-type rejection;
- malformed envelopes, required fields, nested unions and cross-field correlation;
- preservation of provider/plugin extension fields without default insertion;
- compact provider snapshot round-trips and invalid references;
- binary boundaries, truncated input, byte order and size ceilings;
- encrypted record interoperability, replay/sequence handling and security-mode requirements;
- existing daemon/client wire fixtures and native client request/response paths.

Suggested integration command: `cargo test -p paseo-protocol`. Run it after the source cutover is integrated, together with the relevant daemon/client tests. This document does not claim those checks or the example have been executed.

## Schema Purity

Keep schemas structural and transport-neutral. Put cross-field invariants in an explicit semantic validator when structural JSON Schema cannot express the intended contract clearly. Keep normalization in separately named consumer functions. Validation must not load a daemon, mutate configuration, contact providers, or authenticate a remote service.
