# Agents with the SDK

[中文](agents.zh-CN.md) · [Setup](quickstart.md)

Examples are async functions to call with a connected `DaemonClient` using the quickstart dependencies. JSON field names are the wire names, not Rust struct fields. The SDK does not create local agent handles or cache agent properties for you.

## Run an initial prompt

Call `create_agent(fields, RequestOptions)` with `config: {provider, model, cwd}` and `initialPrompt`. Omit the prompt to create an idle session. The response payload contains `agent`; persist its `id` and `workspaceId`. `config.title`, top-level `labels`, `env`, `attachments`, `images`, and `outputSchema` are supported wire fields.

## Keep a session alive for follow-ups

```rust
use anyhow::{Context, Result};
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};
use std::time::Duration;

async fn review(client: &DaemonClient, cwd: &str, model: &str) -> Result<Value> {
    let created = client.create_agent(json!({
        "config": {"provider": "codex", "model": model, "cwd": cwd,
                   "title": "Checkout reviewer"},
        "labels": {"my-app-role": "reviewer"}
    }), RequestOptions::default()).await?;
    let id = created["agent"]["id"].as_str().context("missing agent ID")?;
    client.send_agent_message(json!({
        "agentId": id, "text": "Review the current diff."
    })).await?;
    client.wait_for_finish(id, RequestOptions {
        timeout: Duration::from_secs(300), ..RequestOptions::default()
    }).await
}
```

`send_agent_message` waits for delivery acknowledgement, not turn completion. `wait_for_finish` then waits for the current turn. Inspect `status` (`idle`, `permission`, `error`, or `timeout`), `lastMessage`, and `error`: agent/provider turn failures remain in the outcome payload, while transport/RPC failures return `Err`. A timeout never cancels the agent. Use `cancel_agent(json!({"agentId": id}))` explicitly to stop it. Set `activeTurnBehavior` to `steer` or `interrupt` on a message when sending during an active turn; do not assume concurrent sends create independent turns.

## Find agents by label

```rust
use anyhow::Result;
use paseo_client::DaemonClient;
use serde_json::json;

async fn inspect(client: &DaemonClient, id: &str) -> Result<()> {
    let state = client.fetch_agent(json!({"agentId": id})).await?;
    println!("{}", state);
    let matches = client.fetch_all_agents(json!({
        "filter": {"labels": {"my-app-role": "reviewer"}, "includeArchived": false},
        "page": {"limit": 50}
    })).await?;
    println!("{} matching agents", matches.len());
    let commands = client.list_commands(json!({"agentId": id})).await?;
    println!("{}", commands["commands"]);
    Ok(())
}
```

`fetch_all_agents` walks cursor pages. Use `CursorPages::agents` instead for bounded-memory processing. Labels are application metadata; namespace them when sharing a daemon. Include background work with `filter.includeBackground: true`.

## Continue an agent by ID

Call `fetch_agent` before sending to an old ID if you need fresh existence/state information; a missing resource is not an empty local handle. Then call `send_agent_message` and `wait_for_finish` with that ID. `resume_agent` and `import_agent` are separate provider-session operations; fetching an ID does not implicitly invoke them.

## Create a subagent

Pass `callerAgentId` and `workspaceId` to `create_agent`; `config.cwd` must match that workspace's actual directory. Read the parent's current snapshot rather than guessing placement. Archiving a parent cascade-archives its children. Use `detach_agent(json!({"agentId": child_id}))` before archiving the parent if the child must remain independent.

## Run work in a background workspace

Use `create_workspace` with top-level `background: true`, `source`, and optional `agent: {config, initialPrompt}`; see [workspace creation](workspaces.md#create-a-fresh-workspace). Do not put background visibility inside the nested agent. Agents retain normal lifecycle and history. Default lists hide background workspaces; include them explicitly. An exact agent ID remains usable.

## Request structured output

```rust
use anyhow::Result;
use paseo_client::{DaemonClient, RequestOptions};
use serde_json::{Value, json};

async fn assess(client: &DaemonClient, cwd: &str, model: &str) -> Result<Value> {
    client.create_agent(json!({
        "config": {"provider": "codex", "model": model, "cwd": cwd},
        "outputSchema": {
            "type": "object",
            "properties": {"summary": {"type": "string"},
                           "risk": {"type": "string", "enum": ["low", "medium", "high"]}},
            "required": ["summary", "risk"], "additionalProperties": false
        },
        "initialPrompt": "Assess the release risk of the current diff."
    }), RequestOptions::default()).await
}
```

After waiting successfully for that agent, parse `lastMessage` with `serde_json::from_str::<Value>`. Check status, presence, schema and application constraints before trusting the result. A provider that cannot fulfill structured output must not be treated as having validated it for your application.

## Read the current state of an agent

`fetch_agent` returns a fresh snapshot. Agent fields include `status`, `capabilities`, `availableModes`, `pendingPermissions`, `activeTurn`, `lastUsage`, `lastError`, `features`, `runtimeInfo`, `archivedAt`, `workspaceId` and `cwd`. Fields can be absent or null. Rust JSON indexing returns null for missing fields; use `get`/`as_*` and report malformed required fields instead of silently substituting defaults. For ongoing updates use an [owned observation](events.md).

## List the commands a session loaded

`list_commands` reads the running provider session, including built-in commands/skills not on disk. `kind` is optional; missing does not mean `command`. An unsupported provider can return an error, exposed as `ClientError` with the response payload in `details`.

## Archive or detach

`archive_agent(json!({"agentId": id}))` soft-deletes the agent and closes its runtime. `detach_agent` removes parentage without archiving. `delete_agent` is a distinct destructive operation. Closing the SDK connection archives nothing. Clean up only resources your integration owns, including failure paths; see [recipes](recipes.md).
