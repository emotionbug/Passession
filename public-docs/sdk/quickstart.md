# SDK quickstart

[中文](quickstart.zh-CN.md) · [SDK overview](index.md)

Use Rust 1.93 or newer. Create an application beside your Paseo checkout and use these path dependencies; adjust paths to your checkout. These instructions do not assume a published crate or release.

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

## Connect

Start or select your own development daemon using the [repository instructions](../../README.md). Connecting never starts a daemon. `DaemonTarget::select(None, None, false)` reads `PASEO_HOST` or `PASEO_HOME`, otherwise uses the default local home. Setting both environment variables is ambiguous; select one explicitly. Local discovery reads the daemon's published endpoint and uses its matching local credential. For a remote endpoint, set `PASEO_HOST` and supply the daemon password when required.

## Create an agent

Save the following as `src/main.rs`. Install/authenticate the external provider on the daemon host, discover a Codex model using [Providers](providers.md), and set `PASEO_MODEL` to that model's ID. Working directories are paths on the daemon host; replace `current_dir()` for remote use.

```rust
use anyhow::{Context, Result};
use paseo_client::{ConnectOptions, DaemonClient, DaemonTarget, RequestOptions};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let target = DaemonTarget::select(None, None, false)?;
    let mut options = ConnectOptions::new(target);
    options.password = std::env::var("PASEO_PASSWORD").ok();
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;

    let result: Result<()> = async {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let model = std::env::var("PASEO_MODEL")
            .context("set PASEO_MODEL to a model ID discovered for codex")?;
        let created = client.create_agent(json!({
            "config": {"provider": "codex", "model": model, "cwd": cwd},
            "initialPrompt": "Review the current diff and identify its greatest risk."
        }), RequestOptions::default()).await?;
        let id = created["agent"]["id"].as_str().context("missing agent ID")?;
        let outcome = client.wait_for_finish(id, RequestOptions {
            timeout: Duration::from_secs(300),
            ..RequestOptions::default()
        }).await?;
        println!("{}: {}", outcome["status"], outcome["lastMessage"]);
        Ok(())
    }.await;
    client.close().await;
    result
}
```

## Wait for the reply

Creation uses wire-shaped JSON: `config.provider` is the provider ID, `config.model` is the separate model ID, `config.cwd` is the directory, and `initialPrompt` starts the first turn. The return value is the response payload, not an agent handle. Keep `agent.id` for future calls.

`wait_for_finish` returns a payload with `status`, `lastMessage`, `final`, and `error`, or an error for transport/RPC failures. Agent/provider turn failures remain outcomes in the returned payload: inspect `status` and `error` even when the Rust result is `Ok`. A request deadline or cancellation stops waiting, not the agent; see [error handling](reference.md#errors-and-cleanup).

## Disconnect

Always call `client.close().await` after your work, including on error as above. Dropping a client cancels its transport, but explicit close waits for tasks to finish. Neither action archives an agent. Explicitly archive temporary resources you own.

Run your application with `cargo run`; this may create real provider work and incur usage. Examples are source examples, not a claim that a live daemon/provider run has been verified.

## Next

- [Agents](agents.md): follow-ups, labels, parentage, structured output.
- [Workspaces](workspaces.md): placement, background work and terminals.
- [Events](events.md): owned subscriptions and history repair.
- [Reference](reference.md): authentication, relay, SSH and protocol APIs.
