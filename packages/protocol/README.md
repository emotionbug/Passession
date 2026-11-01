# paseo-protocol

Native Rust wire contracts, JSON validation, provider metadata, routing/timeline helpers, binary codecs and transport crypto shared by Paseo components. Rust 1.93, edition 2024. Use the source checkout; this README does not assert that the crate has been published.

```toml
[dependencies]
paseo-protocol = { path = "../paseo/packages/protocol" }
serde_json = "1"
```

Adjust the path relative to your application's manifest. The crate does not require a running daemon to validate or encode messages.

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

## Public modules

- `wire`: `Direction`, `SessionMessage`, `Envelope`, hello/authentication types, validation and envelope encoding. Inbound means to the daemon; outbound means from it.
- `binary_frames`: borrowed frame decoders and encoders for terminal streams, file transfer and host routes.
- `crypto`: pinned-peer encrypted transport channels and record codecs; application authorization remains separate.
- `providers`: built-in metadata and lossless compact snapshot expansion/compression, not live provider discovery.
- `routing`: endpoint and connection-offer codecs.
- `timeline`, `helpers`: explicit identity, display and compatibility operations.

The checked-in `schema/*.json` documents are embedded and compiled lazily/offline by native validation. No build-time schema generation is needed. `SessionMessage::parse` validates; `new` and ordinary serde deserialization do not perform full boundary validation. Unknown extensions remain intact; normalization is explicit.

See [protocol validation](../../docs/protocol-validation.md) for schema ownership, error semantics and regression guidance, and the [SDK reference](../../public-docs/sdk/reference.md) for consumer integration. Public symbols can evolve during the native cutover; pin a repository revision when integrating and check negotiated daemon capabilities rather than assuming every new operation exists on an older host.
