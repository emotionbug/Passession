---
title: Build a provider plugin
description: Connect a real agent through the native Rust provider protocol.
nav: Provider plugins
order: 46
category: Plugins
---

# Build a provider plugin

Start with the [quickstart](index.md). This guide targets the unpublished **0.13.1 source checkout**. Providers run in native executables on the daemon host; Flutter renders protocol data, never plugin React components. Existing first-party native providers remain built-in. Kiro remains disabled; developing a plugin does not enable it.

The complete runnable example is [workspace-inspector](../../plugin-examples/workspace-inspector/src/lib.rs), crate `paseo-workspace-inspector`. It implements permission-gated local UTF-8 file statistics, not an AI/model service. The [quickstart commands](index.md#run-the-repository-example) build, validate and install it. It deliberately does not advertise persistence, steering, configuration changes, images or output schemas. Use its real registration/connection/session flow as a starting point, not a claim that it supports every optional protocol feature.

## Register a direct provider

Implement the public [provider traits](../../packages/plugin/src/provider.rs):

- `Provider::metadata(&self) -> Metadata`: use `Metadata::new(id, label)` and set applicable description, `icon_path`, command and optional capability flags.
- `Provider::connect(&self, request: Value, events: Events, context: Context) -> Result<Arc<dyn Connection>>`: use `#[paseo_plugin::async_trait]` on the implementation. Negotiate a version and capability subset from `request.versions`/`request.capabilities`.
- `Connection::version() -> u64` and `capabilities() -> Vec<String>` report the negotiated result. The runtime rejects values not offered by the host.
- `Connection::send(input: Value) -> Result<()>` accepts input. Results and session changes are emitted through `events.emit(event).await`, not returned from `send`.
- `Connection::close() -> Result<()>` stops owned sessions, producer tasks and child processes and resolves pending permissions. `Events::cancellation()` signals connection closure and interrupts blocked publishers.

Register with `plugin.provider(implementation)?` before `plugin.run().await`. Optional `Provider::status` and `catalog_cache_key` must agree with `Metadata::has_status` and `has_catalog_cache_key`. Keep metadata deterministic so validation and readiness describe the same contributions. JSON payloads retain host wire shapes; validate them rather than relying on Rust `Value` to enforce a schema.

## Launch the provider CLI

An external agent CLI is a separate product. Use Rust process APIs such as `tokio::process::Command` with discrete arguments, working directory and the supplied `request.launch` command, argument prefix and environment. Pass the resolved environment explicitly; do not accidentally inherit unrelated credentials or assume launch overrides are the complete environment. Do not wrap your Paseo plugin in an interpreter script: `server.executable` must remain a native binary.

For a direct `Provider`/`Connection` implementation, own platform-specific launch handling, process-tree cleanup and bounded waits; the old JavaScript `spawnProcess`/`execCommand`/`terminateProcess` functions are not Rust APIs. Close stdin for graceful shutdown, then terminate owned processes when necessary; do not kill unrelated processes. For an ACP agent, use the [native ACP adapter](#adapt-an-acp-agent), which owns its transport and session lifecycle. Disclose required external software and obtain user approval before downloading/installing it.

## Return models, modes, and thinking options

A `catalog` input includes `requestId`. Emit a `catalog` event with the same ID and a `catalog` object. Models/modes contain stable IDs and labels; optional thinking options and defaults must use the current host protocol. Return empty categories when unsupported, not fictional choices. The example's real response is:

```json
{"type":"catalog","requestId":"request-1","catalog":{"models":[{"id":"local-file","label":"Local text files"}],"modes":[],"defaultModel":"local-file"}}
```

Catalog choices exist before a session. Session-specific configuration arrives later through `session.config`. For full shapes, use the [host provider bridge](../../packages/daemon/src/plugins/provider.rs) and the [SDK provider-options guide](../sdk/provider-options.md).

## Open a session

`session.open` carries `sessionId`, `requestId`, launch configuration, optional persistence and history mode. `config` includes working directory, model/mode/thinking, environment, system prompt, MCP servers, tool policy and opaque provider options. Validate provider-private options; keep user-facing settings separate. Read persisted plugin settings through `Settings::read`, and subscribe if you cache them.

Create/resume the actual underlying session, then emit `session.opened` with the matching IDs, effective `cwd`, supported capabilities and restoration policy. Publish effective `session.config` where applicable, then `session.ready`. Report committed values after normalization, not simply a copy of the request. Host tools arrive through `config.mcpServers`, not a second callback tool API.

## Complete the first prompt

All prompts use `session.prompt`, including messages and structured commands. Each `prompt.clientMessageId` needs exactly one matching `session.prompt_result`:

1. Publish a standard `user_message` timeline snapshot with that `clientMessageId`, so the host can reconcile optimistic text.
2. Publish `session.prompt_result` with `result: {"type":"turn","turnId":"..."}` for a new turn, or the protocol's non-turn completion/failure result when appropriate.
3. Emit `session.turn` with `state: "started"`.
4. Emit complete `timeline.item` snapshots with stable item IDs as actual output changes.
5. Emit exactly one terminal `session.turn`: `completed`, `failed` or `canceled`.

`send()` returning successfully acknowledges acceptance only. Do not report completion before the agent finishes or swallow a failed operation. Keep request IDs, client-message IDs, session IDs and turn IDs distinct.

## Permissions and cancellation

Emit `session.permission` with the session ID and a request containing a stable permission ID, descriptive name/detail, kind and the actual allowed actions. For example, the inspector offers `allow`/`deny` for a specific workspace-relative file. Handle the matching incoming `session.permission` response by validating the permission ID and chosen behavior before executing the read. Emit `session.permission_resolved` and the resulting turn outcome; a denied action must not run.

Permissions are structured operations, not universally shell strings. Preserve question/approval choices from the underlying agent; never manufacture approval or silently select a permissive default. Session cancellation must stop active work and settle pending permissions. Closing/reloading a plugin while approval is outstanding must not leave a detached action running.

## Add session-specific composer controls

Advertise supported configuration behavior and emit `session.config` with standard toggle/select descriptors when the actual agent supports them. User changes arrive as `session.configure`; apply them, emit committed configuration, then `request.completed`. Do not acknowledge a change you did not apply. Controls may change with the selected model.

`providerOptions` is opaque JSON passed at session creation, not an automatically rendered settings panel. A provider-specific token budget belongs in actual provider options or a supported configuration descriptor, not an invented prompt field. Standard configuration data is not an executable Flutter extension.

## Add persistence and replay

Advertise `session.persistence` only when the underlying session can actually resume. Without `session.open.persistence`, open a new session; with it, restore that session. Return opaque persistence via `session.opened` or `session.persistence`; Paseo stores it without interpreting it.

For `history: "replay"`, publish prior complete timeline items before `session.ready`; `history: "skip"` avoids replay. Refresh closes and reopens with current configuration/persistence, so re-read credentials, environment and MCP servers on open. A normal restorable session uses `restoration: "core"`; provider-owned children use `restoration: "parent"` and `parentSessionId`, and must be recreated while restoring their parent. Do not advertise persistence for a transient in-memory map.

## Add commands and steering

Message input contains structured text/images/attachments; command input contains name and arguments. Advertise only supported content and capabilities. `session.commands` publishes actual commands. `delivery: "steer"` asks to add input to an active turn; if unsupported, omit `prompt.steer` and reject unsupported inputs rather than pretending they were steered. `delivery: "auto"` may start a turn or complete a command without one. Preserve the prompt-result correlation in both paths.

## Publish timeline items

Use complete `timeline.item` snapshots and reuse IDs for updated streamed text, tools or todos. Standard types cover user/assistant messages, reasoning, tools, todos, errors, notifications and compaction. Native clients render these data shapes without loading plugin code. Historical custom plugin payloads and React renderers are not a presentation API; essential output must remain understandable through standard items.

```json
{"type":"timeline.item","sessionId":"session-1","item":{"type":"assistant_message","id":"inspection-1","text":"README.md: 42 lines"}}
```

This illustrates a wire shape, not evidence that a file has actually been inspected.

## Adapt an ACP agent

Use [`paseo_plugin::acp`](../../packages/plugin/src/acp/mod.rs), the native replacement for the former `runAcpProvider` functionality. It implements `Provider`/`Connection` for you: register it before `plugin.run().await`. Existing first-party ACP integration remains native as well; plugins do not import daemon-private modules or retain a JavaScript worker.

```rust
use paseo_plugin::{Plugin, Result, acp::{AcpOptions, AcpProvider}};

fn register_acp(plugin: &mut Plugin, command: Vec<String>) -> Result<()> {
    let provider = AcpProvider::command("my-acp", "My ACP agent", command)?
        .options(AcpOptions {
            wait_for_initial_commands: true,
            ..AcpOptions::default()
        });
    plugin.provider(provider)?;
    Ok(())
}
```

Pass the actual installed agent's executable and discrete arguments as `command`; this function does not install software or supply an agent. The plugin's own manifest still points to its native binary. Register deterministic metadata during `--validate` too; probing/connecting the agent happens through the provider connection, not registration.

The command adapter honors a host-supplied launch descriptor's executable, argv and complete environment; otherwise it uses the registered command and inherited environment. Session `config.env` values overlay that environment. Stdout is reserved for ACP JSON-RPC; stderr is drained into plugin logs. On Unix, shutdown sends `SIGTERM` to the owned direct child PID, waits up to one second, then uses `SIGKILL` and reaps it if needed; this is not process-group termination. On Windows, shutdown invokes `taskkill.exe /PID <owned PID> /T /F` to target the child and its process tree, with a five-second helper deadline, then reaps the direct child. Helper failures are logged and fall back to forcibly terminating/reaping the direct child; descendant cleanup is not guaranteed when the helper fails.

For a custom byte transport, `AcpProvider::connector(id, label, factory)?` accepts an asynchronous factory taking a cancellation token and returning a fresh owned `AcpStream`. Construct it with `AcpStream::new(reader, writer)` where both halves implement Tokio `AsyncRead`/`AsyncWrite` respectively, plus `Unpin + Send + 'static`. The stream exposes its `cancellation` token and optional `task: Option<tokio::task::JoinHandle<()>>`; attach a connector task only when the stream owns its lifetime, and make it observe cancellation. Closing cancels the stream and joins the owned task, with bounded shutdown and task abortion on timeout. Do not block a Tokio runtime thread or assume task abortion cleans up independently spawned processes.

The factory is called separately for capability probing, catalog discovery and sessions; do not share an active session stream or return borrowed resources. A custom connector owns any transport-specific process/network cleanup that the command adapter would otherwise provide.

The adapter negotiates provider protocol 1. It supports text/command prompts, configuration and permissions; image input, session listing and persistence are enabled only when the ACP agent advertises the corresponding capabilities and the host offers them. It rejects steering and structured-output schemas rather than pretending to support them. `AcpOptions::default()` uses a 10-second startup timeout, global catalog discovery (`workspace_only_catalog: false`), and no initial-command wait. When enabled, `wait_for_initial_commands` waits up to `initial_commands_timeout` (default 1 second). Prompt duration itself is not capped by the startup timeout.

Interrupt sends ACP `session/cancel` and resolves outstanding permission requests as cancelled; the agent must settle its active prompt. The 2-second default `cancellation_timeout` bounds the wait when a new prompt replaces an active one, not every standalone interrupt. If that previous turn does not settle, the adapter closes its transport instead of starting overlapping work.

The ACP client advertises `clientCapabilities: {}` and handles only `session/request_permission` from the agent. It does not expose client filesystem or terminal services. Permissions cover the agent's reported requests, not every OS operation: the agent and plugin remain trusted native processes with the daemon user's access.

### Vendor transformers

Attach ordered extensions with `.transformer(implementation)` and implement `AcpTransformer` using `#[paseo_plugin::async_trait]`:

- `discover(catalog, AcpContext) -> Result<Value>` transforms the discovered catalog.
- `configure(&ConfigChange, AcpContext) -> Result<bool>` handles model/mode/thinking or `{id, value}` setting changes. Return `true` only after applying the change; `false` leaves it for another transformer or the adapter's normal mapping.
- `notification(method, params, session_id) -> Result<Vec<VendorUpdate>>` handles vendor notifications. Updates can carry commands, configuration, timeline items or notices.
- `tool_call(snapshot, session_id) -> Result<Value>` transforms a complete tool snapshot (`id`, optional `name`, `title`, optional `kind`, `status`, `input`, `output`, `locations`).
- `AcpContext` provides the adapter's `session_id` and `config`; `ConfigAccess::read().await` reads current options and `set(id, value).await` applies an option.

Notification/tool transforms are synchronous so wire updates precede the following JSON-RPC response; perform asynchronous configuration through the async hooks instead. Validate vendor payloads and leave unrelated/malformed data unchanged. Preserve ordinary tool/edit semantics rather than replacing the event stream with a custom renderer. The historical `provider-acp-transformer` directory is not an installable native plugin; its JavaScript API needs an explicit port to these hooks.

## Test and publish

On an isolated test daemon, exercise the actual external agent or local-file example: registration and catalog, session creation, real prompt, permissions/denial, cancellation, optional steering/persistence/replay, and cleanup while work is active. Verify standard data renders on the native clients/platforms you claim to support. Compilation, validation metadata and `running` status alone do not prove this behavior.

Follow [distribution](publishing.md) for Git, Cargo and checksum-pinned release sources. No tests, installation or publication are implied by this guide. Document any unimplemented optional capability explicitly and do not advertise it.
