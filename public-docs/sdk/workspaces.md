# Workspaces with the SDK

[中文](workspaces.zh-CN.md) · [Setup](quickstart.md)

Workspaces provide durable ownership for agents, script/setup PTYs and files. The SDK can control retained daemon terminals; the native app presents read-only task logs, not an embedded interactive terminal. All paths below belong to the daemon host.

## Open a directory

`client.open_project(json!({"cwd": directory})).await?` opens a project directory and reuses the active workspace for that exact directory. Inspect its response payload rather than assuming a local workspace handle. This is different from `create_workspace`, which requests a fresh workspace.

## Create a fresh workspace

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};

async fn create_work(client: &DaemonClient, cwd: &str, model: &str) -> Result<Value> {
    client.create_workspace(json!({
        "source": {"kind": "directory", "path": cwd},
        "title": "Checkout review",
        "background": true,
        "agent": {
            "config": {"provider": "codex", "model": model, "cwd": cwd},
            "initialPrompt": "Map the checkout flow before changing anything."
        }
    }), RequestOptions::default()).await
}
```

The returned payload contains `workspace`, optional `agent`, and setup/creation metadata. Persist their IDs. For an isolated checkout replace `source` with `{"kind":"worktree","cwd":cwd,"action":"branch-off","refName":"main","branchName":"fix/checkout-42"}`. Directory and worktree sources may include `projectId`; otherwise the daemon resolves the project. Worktree creation determines the actual directory: use the returned workspace's directory for subsequent operations, not the source repository path.

`background` belongs on the workspace. Omission defaults to false, or inherits the caller's workspace visibility when `callerAgentId` is supplied. Explicit true/false overrides inheritance. It is creation-only; adding an agent to an existing workspace does not change visibility. Nested `agent.background` is rejected. Implicit workspace creation through `create_agent` can use top-level `background` only when not specifying an existing `workspaceId`.

Check `client.require_feature("backgroundWorkspaces", "Update the host for background workspaces")?` before requesting background creation or inclusive discovery. Do not silently downgrade to visible work.

## Start an agent in a workspace

Call `create_agent` with `workspaceId` and `config: {provider, model, cwd}` using the workspace's actual directory. Without `workspaceId`, agent creation creates a fresh workspace/project as needed. Unlike a local handle, the raw JSON API does not fill placement fields for you.

## Start a terminal in a workspace

```rust
use anyhow::{Context, Result};
use paseo_client::DaemonClient;
use serde_json::json;

async fn run_log(client: &DaemonClient, workspace_id: &str, cwd: &str) -> Result<()> {
    let created = client.create_terminal(json!({
        "workspaceId": workspace_id, "cwd": cwd, "name": "Repository status",
        "command": "git", "args": ["status", "--short"],
        "size": {"rows": 24, "cols": 100}
    })).await?;
    let id = created["terminal"]["id"].as_str().context("missing terminal ID")?;
    let capture = client.capture_terminal(json!({"terminalId": id, "stripAnsi": true})).await;
    let cleanup = client.kill_terminal(json!({"terminalId": id})).await;
    println!("{}", capture?);
    cleanup?;
    Ok(())
}
```

Capture is a point-in-time snapshot, not a wait for process exit. To wait for exit/output use `Arc<DaemonClient>::observe_terminal`, receive `TerminalEvent`, and release its subscription; see [reference](reference.md#terminals-and-files). Always pass `workspaceId` to distinguish workspaces sharing a directory. `list_terminals` accepts the same placement context. Input is available through `TerminalStream::input`; command arguments are separate from the executable. Never interpolate untrusted text into shell commands.

## List workspaces

```rust
use anyhow::Result;
use paseo_client::{CursorPages, DaemonClient, RequestOptions};
use serde_json::json;

async fn list_work(client: &DaemonClient) -> Result<()> {
    let mut pages = CursorPages::workspaces(json!({
        "filter": {"query": "storefront", "includeBackground": true},
        "page": {"limit": 50}
    }));
    while let Some(page) = pages.next(client, RequestOptions::default()).await? {
        println!("{}", page["entries"]);
    }
    Ok(())
}
```

Background workspaces are hidden unless `filter.includeBackground` is true. `fetch_workspaces` returns one payload; `CursorPages::workspaces` walks all pages and returns each page object. Cursors are opaque.

## Refresh and archive a handle

There is no Rust handle to refresh. Fetch current workspace data with `fetch_workspaces`, or hold an [owned workspace observation](events.md). Archive explicitly with `archive_workspace(json!({"workspaceId": id})).await?`. Workspace archive is distinct from agent archive. Inspect recovery with `inspect_workspace_recovery` and use `restore_workspace` only when your integration intends to restore it. Archive only resources you own.
