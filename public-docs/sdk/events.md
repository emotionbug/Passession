# SDK events

[中文](events.zh-CN.md) · [Setup](quickstart.md)

Connecting or calling `subscribe()` only attaches a transport listener; it does not request daemon-side observation. Wrap a connected client in `Arc::new(client)` and use owned observation methods to obtain a server snapshot and subscription ID. Independent observations have independent server IDs on capable daemons.

## Follow one agent's status

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::json;
use std::sync::Arc;

async fn watch(client: &Arc<DaemonClient>) -> Result<()> {
    let mut observation = client.observe_agents(json!({
        "filter": {"includeArchived": false}, "page": {"limit": 50}
    }), RequestOptions::default()).await?;
    println!("initial: {}", observation.snapshot);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                event = observation.recv() => println!("update: {}", event?),
                signal = tokio::signal::ctrl_c() => { signal?; break; }
            }
        }
        Ok(())
    }.await;
    let released = observation.release().await;
    result?;
    released
}
```

The initial payload is `observation.snapshot`; subsequent `recv()` values are whole session messages with `type` and usually `payload`. Filter entries/events for your agent ID. Do not confuse the RPC payload shape with an event envelope. Bounded receivers can lag; an error is not an empty update. Refresh your snapshot and reconcile missing history before continuing. Explicit `release` waits for server cleanup; dropping an observation only attempts asynchronous cleanup when a runtime remains available.

## Follow timeline events

`client.observe_timeline(&[agent_id.to_owned()], RequestOptions::default()).await?` returns an owned observation. Consume `agent_stream` and timeline-related messages according to their `type` and payload. A stream delta is not necessarily a complete message. Preserve item/turn identity, apply replacements rather than appending duplicate items, and refetch when a replacement invalidates your cache. Helpers live in `paseo_protocol::timeline`; the client does not maintain a presentation cache for you.

## Fetch timeline history

```rust
use anyhow::Result;
use paseo_client::{CursorPages, DaemonClient, RequestOptions};

async fn history(client: &DaemonClient, agent_id: &str) -> Result<()> {
    let mut pages = CursorPages::timeline(agent_id, "before", 100)?;
    while let Some(page) = pages.next(client, RequestOptions::default()).await? {
        println!("{}", page);
    }
    Ok(())
}
```

`CursorPages::timeline` supports `before` and `after`; page order and opaque start/end cursors are authoritative. For a single page call `fetch_agent_timeline` with `agentId`, `direction`, `limit` and optional `cursor`/`projection`. `fetch_agent_history` and `CursorPages::history(fields)` address the separate history endpoint; they are not interchangeable with timeline pages. History is explicit, not automatically replayed by subscriptions.

## Follow workspace updates

Use `observe_workspaces(json!({"filter": {"includeBackground": true}}), options)` with an `Arc<DaemonClient>`. Release the returned observation when your view closes. Ordinary `fetch_workspaces` calls do not create observations unless you explicitly request one.

## Follow provider catalog changes

Subscribe with `observe_events(json!({"events": ["providers_snapshot_update", "agent_permission_request", "agent_permission_resolved"]}), options)`. Then fetch the provider snapshot for your working directory and consume updates. Event names are protocol values, not Rust callbacks. Permission IDs come from daemon events, not generated request IDs:

```rust
use anyhow::Result;
use paseo_client::DaemonClient;
use serde_json::json;

async fn deny(client: &DaemonClient, agent_id: &str, permission_request_id: &str) -> Result<()> {
    client.respond_to_permission(json!({
        "agentId": agent_id, "requestId": permission_request_id,
        "response": {"behavior": "deny", "message": "Not approved by the operator"}
    })).await
}
```

Only send an allow response after the operator or your explicit application policy authorizes that exact request. Receiving a permission request does not authorize it.

## Reconnection and older daemons

`DaemonClient` does not reconnect automatically. `ReconnectingClient::connect` manages reconnects; `connection(&cancel).await?` obtains the current `Arc<DaemonClient>`. Its `observe(request, response, fields, options)` returns `RestoringSubscription`; `recv().await` yields `Option<Result<SubscriptionEvent>>`. `SubscriptionEvent::Snapshot { payload, restored: true }` replaces the baseline after reconnect or lag. Repair history gaps yourself. `SubscriptionEvent::Update` is a live event. Release restoring subscriptions before `ReconnectingClient::close(&mut self)`.

Reconnect never blindly replays mutations. Owned observation methods check the daemon's `ownedSubscriptions` capability and require server-issued `subscriptionId` responses. Unsupported hosts receive an explicit update-host error; there is no hidden legacy shared-filter fallback. Raw RPC and backward-readable wire messages remain available, but they do not make an unadvertised observation feature available. Upgrade the host to use owned observations.
