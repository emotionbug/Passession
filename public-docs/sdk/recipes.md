# SDK recipes

[中文](recipes.zh-CN.md) · [Setup](quickstart.md)

These async functions use a connected client and the quickstart dependencies. They perform real provider work. Pass a discovered model ID and a directory on the daemon host. The caller closes the connection after the function returns, including failures.

## Turn an issue into visible work

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::json;

async fn start_issue(client: &DaemonClient, cwd: &str, model: &str,
                     issue_id: &str, title: &str, description: &str) -> Result<(String, String)> {
    let opened = client.open_project(json!({"cwd": cwd})).await?;
    let workspace = &opened["workspace"];
    let workspace_id = workspace["id"].as_str().context("missing workspace ID")?;
    let directory = workspace["workspaceDirectory"].as_str().context("missing directory")?;
    let created = client.create_agent(json!({
        "workspaceId": workspace_id,
        "config": {"provider": "codex", "model": model, "cwd": directory, "title": title},
        "labels": {"my-tracker/issue-id": issue_id},
        "initialPrompt": format!("Implement issue {issue_id}: {title}\n\n{description}")
    }), RequestOptions::default()).await?;
    let agent_id = created["agent"]["id"].as_str().context("missing agent ID")?;
    Ok((workspace_id.to_owned(), agent_id.to_owned()))
}
```

Persist both IDs in your integration. On later webhooks, fetch and reuse the existing resources instead of blindly calling creation again. Issue text is untrusted task content, not authorization for unrelated host operations. The opened workspace may predate your integration; do not archive it merely because this function used it.

## Run parallel reviewers

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};
use std::time::Duration;

async fn reviewers(client: &DaemonClient, cwd: &str, model: &str) -> Result<()> {
    let mut owned = Vec::<String>::new();
    let work: Result<()> = async {
        for prompt in ["Review correctness and edge cases.", "Review security and input handling."] {
            let created = client.create_agent(json!({
                "config": {"provider": "codex", "model": model, "cwd": cwd},
                "initialPrompt": prompt
            }), RequestOptions::default()).await?;
            owned.push(created["agent"]["id"].as_str().context("missing agent ID")?.to_owned());
        }
        let options = RequestOptions { timeout: Duration::from_secs(300), ..RequestOptions::default() };
        let (first, second) = tokio::join!(
            client.wait_for_finish(&owned[0], options.clone()),
            client.wait_for_finish(&owned[1], options)
        );
        for outcome in [first, second] {
            let outcome: Value = outcome?;
            println!("{}", outcome);
            anyhow::ensure!(outcome["status"] == "idle", "review did not complete: {outcome}");
        }
        Ok(())
    }.await;
    let mut cleanup_error = None;
    for id in owned {
        if let Err(error) = client.archive_agent(json!({"agentId": id})).await {
            eprintln!("archive failed for {id}: {error}");
            if cleanup_error.is_none() { cleanup_error = Some(error); }
        }
    }
    work?;
    if let Some(error) = cleanup_error { return Err(error); }
    Ok(())
}
```

Both agents begin work at creation, and the waits run concurrently. Each known created ID is retained before the next creation, so partial failure still cleans up earlier agents. If creation times out after submission, its result may exist without a returned ID: preserve a caller-supplied `idempotencyKey` and recover it before deciding whether cleanup is complete. Never substitute a new key as a retry.

## Keep a resident role across process restarts

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::json;

async fn planner(client: &DaemonClient, cwd: &str, model: &str) -> Result<String> {
    let found = client.fetch_all_agents(json!({
        "filter": {"includeArchived": false, "includeBackground": true,
                   "labels": {"my-app/role": "planner"}},
        "page": {"limit": 100}
    })).await?;
    anyhow::ensure!(found.len() <= 1, "multiple planners; resolve ownership explicitly");
    if let Some(agent) = found.first() {
        return Ok(agent["id"].as_str().context("missing agent ID")?.to_owned());
    }
    let created = client.create_agent(json!({
        "config": {"provider": "codex", "model": model, "cwd": cwd, "title": "Planner"},
        "labels": {"my-app/role": "planner"}
    }), RequestOptions::default()).await?;
    Ok(created["agent"]["id"].as_str().context("missing agent ID")?.to_owned())
}
```

Use the returned ID with `send_agent_message` and `wait_for_finish`. Namespace labels and serialize this find-or-create operation in your integration; labels are not uniqueness locks. If multiple processes may create the same role, coordinate a durable creation key/lock rather than assuming the list/create sequence is atomic. Filter by the intended host/project ownership as appropriate, and do not select the first unrelated matching agent.

## Clean up temporary agents

The reviewers example attempts every archive even after work or another cleanup fails. Closing the connection is separate from resource cleanup. Keep IDs durably when cleanup must survive process failure. Archive only your own temporary agents; resident roles and preexisting workspaces should remain intact.

For uncertain creation, `ReconnectingClient::recover_creation(kind, key, original_fields, options)` inspects the keyed lifecycle and only resubmits when its snapshot is missing. Keep the original fields and key. Recovery can return either a creation lifecycle snapshot or the creation RPC payload; inspect the shape rather than assuming both have identical paths. Other mutations are not automatically retried; inspect current state before any deliberate retry.
