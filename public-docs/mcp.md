---
title: MCP reference
description: Reference for the Paseo tools agents use to manage agents, workspaces, scripts, terminals, and schedules.
nav: MCP reference
order: 34
category: Orchestration
---

# MCP reference

[Enable Paseo tools](/docs/orchestration#get-started) to give agents this catalog. Ask for an outcome in natural language, or use the tool interfaces below.

## Configuration

| Setting                       | Default | Purpose                                            |
| ----------------------------- | ------- | -------------------------------------------------- |
| `daemon.mcp.enabled`          | `true`  | Run the MCP server.                                |
| `daemon.mcp.injectIntoAgents` | `false` | Give agents launched by Paseo access to its tools. |

Depending on the provider, Paseo delivers tools through its native tool interface or MCP. The capabilities are the same. Start a new agent or reload an existing one after changing injection settings.

## Limit Paseo tools by provider

Use provider policies when different agent profiles should receive different Paseo tools. Enable
tool injection globally, then add `paseoTools` to the exact provider IDs you launch:

```json
{
  "$schema": "https://paseo.sh/schemas/paseo.config.v1.json",
  "version": 1,
  "daemon": {
    "mcp": {
      "enabled": true,
      "injectIntoAgents": true
    }
  },
  "agents": {
    "providers": {
      "codex-lead": {
        "extends": "codex",
        "label": "Codex Lead"
      },
      "codex-worker": {
        "extends": "codex",
        "label": "Codex Worker",
        "paseoTools": {
          "disabledTools": ["create_agent", "send_agent_prompt", "kill_agent"]
        }
      },
      "codex-isolated": {
        "extends": "codex",
        "label": "Codex Isolated",
        "paseoTools": {
          "enabled": false
        }
      }
    }
  }
}
```

Run `paseo reload` after editing `~/.paseo/config.json`, then start a new agent or reload an
existing one. A running session keeps the catalog it received at launch.

Omitting `paseoTools` enables the complete catalog. Set `enabled` to `false` to remove the catalog,
or list exact tool IDs in `disabledTools` to remove selected tools. Custom profiles do not inherit
this policy from `extends`; configure each custom provider ID separately.

The native runtime has no embedded browser automation tools or browser host.

This setting limits the catalog presented to an agent. It is not a security boundary for an agent
that can access the host through a shell.

## Mental model

Workspaces decide where work happens; agent parentage decides who owns the work.

- An agent that calls `create_agent` without a `workspaceId` gets a subagent in its own workspace.
- Passing a `workspaceId` places that subagent in another workspace without detaching it from its parent.
- A top-level MCP caller without a workspace gets a new local workspace.
- Create a workspace first when you need worktree isolation, a specific branch, or a pull request checkout.

MCP does not expose an agent-detach tool. Detaching is a manual user action in the app or CLI.

## Tools

### Agents

| Tool                 | Function                                                                                                                                                       |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create_agent`       | Create an agent, optionally placing it in an existing workspace with `workspaceId`. Use `initialPrompt` to start work; returns without waiting for completion. |
| `send_agent_prompt`  | Send a prompt using `agentId` and `prompt`; returns without waiting for completion.                                                                            |
| `get_agent_status`   | Return compact lifecycle, error and permission status; use `detail` to recover full snapshot fields.                                                           |
| `list_agents`        | List recent agents as compact metadata; `includeBackground: true` includes background work.                                                                    |
| `cancel_agent`       | Abort an agent's current run but keep the agent alive.                                                                                                         |
| `archive_agent`      | Soft-delete an agent and remove it from the active list.                                                                                                       |
| `kill_agent`         | Close an agent's runtime without deleting its record or history. The agent can be resumed.                                                                    |
| `update_agent`       | Update an agent name, labels, or runtime settings such as mode/model/thinking/features.                                                                        |
| `get_agent_activity` | Return a bounded recent timeline page with compact status and references for exact retained item details.                                                      |
| `set_agent_mode`     | Switch an agent's session mode.                                                                                                                                |

Paseo agents receive finish, error, and permission notifications by default; set `notifyOnFinish: false` to disable them. External MCP clients use `get_agent_status` and `get_agent_activity` to check results.

#### Reading activity and details

`list_agents` keeps identity, lifecycle, location, labels and attention metadata.
Null fields, short IDs, detailed model/thinking settings and redundant timestamps
are omitted. A non-null `archivedAt` still identifies archived agents. Use
`get_agent_status` detail to recover the complete snapshot when those fields matter.

`get_agent_activity` defaults to eight canonical rows; `limit` accepts 1–20.
Pages run backward, with entries ordered oldest to newest within each page.
Following `nextCursor` never expands a page to include an entire overlapping tool
lifecycle. `view: "failures"` filters each bounded page: an empty `entries` array
does not mean there are no older failures. Continue until `nextCursor` is null.
Coverage describes retained canonical history, not every byte a provider ever emitted.

Activity reads do not load or resume a provider, including closed agents. If no
retained timeline exists, the request fails explicitly instead of preparing one.
Remote hosts must advertise `readOnlyAgentTimeline`; older hosts require an update
for this model-facing observation path.

Reasoning text is omitted from the default page; long fields have explicit previews.
For the exact retained item JSON, call `get_agent_activity` with the page's
`agentId`/`serverId` and
`detail: {epoch: page.coverage.epoch, seqStart: entry.seqStart, seqEnd: entry.seqEnd}`.
The page carries the shared identity and epoch once instead of repeating a full
request on every row. Follow `nextRead` unchanged, concatenate its `chunk` strings,
then decode JSON. Chunks contain at most 4096 UTF-8 bytes; offsets are bytes, not
character positions. Unrelated tail growth does not invalidate older pages or item
details. Changed requested rows, epoch changes and retention loss fail explicitly
rather than mixing incompatible history.

`get_agent_status` shows selected status/error fields, a permission count and at most
three permission previews. Its `detail` reads JSON pointers into the original
snapshot, such as `/pendingPermissions/3` or `/lastError`; an empty path reads the
whole snapshot. Start at offset zero, then use `nextRead` unchanged. A returned
`revision` pins the selected value, so unrelated status changes do not invalidate
a permission/error read. A changed selected value requires a new read; whole-snapshot
reads therefore require a stable snapshot. Truncated previews include their path
and revision. An activity page's `agentStatus.detail` belongs to `get_agent_status`.

Recovery references retain `agentId` and, for remote calls, `serverId`. These compact
defaults affect model-facing tools only; UI/SDK timeline and snapshot contracts
retain their existing contents. “Exact” recovery means the stored canonical item or
the transport's authorized snapshot, not fields discarded by earlier normalization.

#### Batching read-only checks with OMP Eval

Use Eval to combine independent reads and print the fields needed for the current
question instead of returning every intermediate response to the model. This is
explicit task-specific selection, not a server-side lossy summary or an automatic
compaction policy. Keep errors, permission counts and exact-detail references;
follow truncated previews when their omitted content is needed.

For example, check the lifecycle and errors of up to three recent agents in one
Eval call. OMP's Eval tool bridge returns structured tool data in `details`:

```python
import asyncio

listed = (await tool.list_agents({"limit": 3}))["details"]
requests = [
    {"agentId": agent["id"], "serverId": listed["serverId"]}
    for agent in listed["agents"]
]
responses = await asyncio.gather(
    *(tool.get_agent_status(request) for request in requests)
)
fields = (
    "agentId", "serverId", "status", "lastError", "errorText",
    "requiresAttention", "attentionReason", "pendingPermissions", "detail",
)
print([
    {key: value for key, value in response["details"].items() if key in fields}
    for response in responses
])
```

Give the Eval call a descriptive `title`. The timeline uses that request title;
opening the row shows the request (language, code and options) alongside available
output and errors. Running calls already expose their request. A missing or blank
title leaves the row labelled **Eval**, without using code or output as its title.


### Workspaces

| Tool                | Function                                                                                                                                                                                                                                                                                                        |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create_workspace`  | Create a local or worktree-isolated workspace. Worktrees can branch off, check out a branch, or a PR. `background: true` hides it from default lists and the sidebar while preserving history and access by ID. Omission inherits the calling agent’s workspace setting, or defaults to false without a caller. |
| `list_workspaces`   | List active workspaces and their directories and isolation. Use `includeBackground: true` to include background workspaces.                                                                                                                                                                                     |
| `rename_workspace`  | Change the user-visible name of the current or specified workspace.                                                                                                                                                                                                                                             |
| `archive_workspace` | Archive a workspace and the sessions it owns.                                                                                                                                                                                                                                                                   |

For worktree isolation, `create_workspace` accepts the same useful choices as the app: branch off from a base, check out an existing branch, or check out a pull request. The worktree remains an implementation detail of the workspace lifecycle.

### Workspace scripts

These tools manage scripts configured in a workspace's `paseo.json`. Each requires an explicit `workspaceId`; start and stop also require the configured `scriptName`.

| Tool                     | Function                                                                                |
| ------------------------ | --------------------------------------------------------------------------------------- |
| `list_workspace_scripts` | List configured scripts with lifecycle, terminal, port, proxy URL, and health metadata. |
| `start_workspace_script` | Start a configured script through Paseo's managed launcher.                             |
| `stop_workspace_script`  | Stop a running script through its supervised terminal.                                  |

See [Git worktrees](/docs/worktrees#scripts-and-services) for `paseo.json` configuration.

### Terminals

These are daemon-side PTY APIs. The native client opens interactive shells as **Terminal** tabs and shows script/setup output as read-only logs.

| Tool                 | Function                                                                     |
| -------------------- | ---------------------------------------------------------------------------- |
| `list_terminals`     | List terminal sessions for one working directory or all working directories. |
| `create_terminal`    | Create a terminal session for a working directory.                           |
| `kill_terminal`      | Kill a terminal session.                                                     |
| `capture_terminal`   | Capture plain-text output from a terminal session.                           |
| `send_terminal_keys` | Send text or special key tokens to a terminal session.                       |

### Schedules and heartbeats

Both use the same cron engine, but they have deliberately different interfaces.

| Tool                | Function                                                                     |
| ------------------- | ---------------------------------------------------------------------------- |
| `create_schedule`   | Create a cron schedule that starts a new agent for each run.                 |
| `list_schedules`    | List new-agent schedules managed by the daemon.                              |
| `inspect_schedule`  | Inspect a schedule and its run history.                                      |
| `pause_schedule`    | Pause an active schedule.                                                    |
| `resume_schedule`   | Resume a paused schedule.                                                    |
| `update_schedule`   | Change a schedule's cron, prompt, agent settings, limits, or other settings. |
| `schedule_logs`     | Return recent runs and output for a schedule.                                |
| `run_schedule_once` | Start one new-agent schedule run without changing its cron.                  |
| `delete_schedule`   | Delete a new-agent schedule permanently.                                     |
| `create_heartbeat`  | Send a recurring cron-backed prompt into the current agent.                  |
| `delete_heartbeat`  | Delete one of the current agent's heartbeats.                                |

MCP heartbeats are ephemeral: create or delete them. To change one, delete it and create a replacement. Pause, resume, update, inspect, logs, and run-once apply to new-agent schedules only.

New-agent run workspaces are archived when each run ends by default. `create_schedule` and `update_schedule` do not expose **Archive on finish** (`archiveOnFinish`); [change the option in the app](/docs/schedules#keep-run-workspaces) to keep subsequent run workspaces active.

### Agent profiles

| Tool            | Function                                                                                                                           |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `list_profiles` | Return the host's saved agent profiles, including their notes and launch settings. Returns an empty list when none are configured. |

Before delegating, read each profile's `notes` and choose the profile the user named or the one that fits the task. See [Agent profiles](/docs/agent-profiles) for setup and example notes.

`create_agent` has no profile parameter and requires a `provider/model` pair. If the profile has no model, call `list_models` for its provider and choose an available model for the task before launching. Apply the chosen profile's values to the launch request:

| Profile field                   | `create_agent` field                                                        |
| ------------------------------- | --------------------------------------------------------------------------- |
| `provider` and optional `model` | `provider` as `provider/model`, using the saved or discovered model ID      |
| `modeId`                        | `settings.modeId`                                                           |
| `thinkingOptionId`              | `settings.thinkingOptionId`                                                 |
| `featureValues`                 | `settings.features`                                                         |
| `notes`                         | Selection guidance for the orchestrator; supply the task in `initialPrompt` |

Omit absent optional settings. If no profile fits, use provider discovery to choose available settings.

### Providers

| Tool               | Function                                                          |
| ------------------ | ----------------------------------------------------------------- |
| `list_providers`   | List configured agent providers, availability, and modes.         |
| `list_models`      | List models for an agent provider.                                |
| `inspect_provider` | Inspect compact provider capabilities and draft feature settings. |

### Permissions

| Tool                       | Function                                          |
| -------------------------- | ------------------------------------------------- |
| `list_pending_permissions` | Return pending permission requests across agents. |
| `respond_to_permission`    | Approve or deny a pending permission request.     |
