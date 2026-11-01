# paseo-client

Native asynchronous Rust SDK for an existing Paseo daemon. It provides direct/local/relay/SSH connections, authentication, correlated JSON RPCs, agent/workspace operations, owned observations, reconnection, pagination, and bounded binary file/terminal transport.

Use Rust 1.93 or newer and edition 2024. In an application beside a checkout:

```toml
[package]
name = "paseo-integration"
version = "0.1.0"
edition = "2024"
rust-version = "1.93"

[dependencies]
paseo-client = { path = "../paseo/packages/client" }
paseo-protocol = { path = "../paseo/packages/protocol" }
anyhow = "1"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "time"] }
tokio-util = "0.7"
```

Adjust paths to your checkout. These dependencies do not imply registry publication. Run the application under Tokio; the [complete quickstart](../../public-docs/sdk/quickstart.md) includes an async entry point, provider/model discovery prerequisites and cleanup on error.

```rust
use anyhow::Result;
use paseo_client::{ConnectOptions, DaemonClient, DaemonTarget};
use tokio_util::sync::CancellationToken;

async fn connect_remote() -> Result<DaemonClient> {
    let host = std::env::var("PASEO_HOST")?;
    let mut options = ConnectOptions::new(DaemonTarget::Endpoint { host });
    options.password = std::env::var("PASEO_PASSWORD").ok();
    DaemonClient::connect(options, CancellationToken::new()).await
}
```

`PASEO_HOST` may be a direct WebSocket endpoint, supported socket/pipe target, SSH target, daemon-generated relay/pairing URL, or registered host ID. `DaemonTarget::select(None, None, false)` instead selects from environment/local discovery. Authentication and encryption are separate: use TLS or a trusted encrypted transport for remote credentials, and preserve the daemon's key pin for relay/encrypted direct access. SSH needs a configured local SSH client and trusted host keys; it never starts the remote daemon.

## API conventions

- Named operations accept `serde_json::Value` with wire field names and return response payloads. Agent creation uses `config.provider`, separate `config.model`, `config.cwd`, `initialPrompt` and `config.providerOptions`.
- `create_agent`/`create_workspace` also take `RequestOptions`; save returned resource IDs. There are no cached local agent/workspace handles.
- Use `Arc<DaemonClient>` for `observe_agents`, `observe_workspaces`, `observe_events`, and other owned observations. Consume snapshots/events and release subscriptions explicitly.
- `ReconnectingClient` restores owned observations but never blindly replays mutations. Reconcile missing history after restored snapshots. Recover uncertain creation only with its original idempotency key.
- Requests can fail after a mutation was submitted. `ClientError` preserves code/message/details; other failures remain ordinary `anyhow::Error`. Timeout/cancellation does not roll back remote work.
- Call `close().await` even on errors. Closing never archives agents or workspaces.

See the [English SDK guides](../../public-docs/sdk/index.md), [中文指南](../../public-docs/sdk/index.zh-CN.md), [API reference](../../public-docs/sdk/reference.md), and [example index](examples/README.md). New capabilities can require a newer daemon; unsupported owned subscriptions are not silently emulated.
