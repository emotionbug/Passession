# SDK API reference

[中文](reference.zh-CN.md) · [Dependency and Tokio setup](quickstart.md)

The Rust public roots are `paseo_client`, `paseo_protocol`, and `paseo_highlight`. See [client source](../../packages/client/src/lib.rs), [operation mappings](../../packages/client/src/operations.rs), and [subscription types](../../packages/client/src/subscriptions.rs). JSON operation arguments and response payloads are `serde_json::Value`, preserving provider/plugin extension fields. There are no implicit agent/workspace facade objects.

## Connection and authentication

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

`DaemonTarget::Instance { home: PathBuf }` discovers a local daemon. `DaemonTarget::Endpoint { host: String }` accepts:

| Target | Example / meaning |
| --- | --- |
| Direct WebSocket | `wss://daemon.example/ws` or loopback `ws://127.0.0.1:6767/ws` |
| Host/port | `127.0.0.1:6767`, `tcp://127.0.0.1:6767` |
| Unix socket | `unix:///absolute/path/to/daemon.sock` |
| Windows pipe | `pipe://` followed by an actual named-pipe path |
| SSH | `ssh://user@host:22?daemonPort=6767` |
| Relay | Daemon-generated `relay://` URL or pairing URL carrying `#connect=` / `#offer=` |
| Registered host | `hst_…` or `srv_…`, resolved through the local managing daemon |

Use daemon-generated pairing material unchanged; do not invent a public key. Relay URLs carry the daemon key pin. For encrypted direct connections set `ConnectOptions.server_public_key`. `minimum_security: paseo_client::crypto::SecurityMode::Protected` requires protected encrypted records; it does not enable TLS or replace a key pin/password. `wss` authenticates the TLS endpoint. Never send a daemon password over untrusted plaintext `ws`.

`ConnectOptions::new(target)` defaults to a 15-second connection timeout, `client_type: "cli"`, and `SecurityMode::Legacy`. Optional `password` overrides URL authentication; otherwise `PASEO_PASSWORD` can supply it. `local_credential` is an explicit local credential. Automatic local credentials are used only for the exact endpoint published by the selected local daemon, never arbitrary remote targets. `home` chooses identity/discovery storage for an explicit endpoint; `client_id` overrides the normally persisted SDK identity. Keep passwords, credential files, pairing URLs and keys out of logs.

SSH uses the local `ssh` executable, noninteractive batch authentication and `-W` forwarding to remote loopback. Configure keys/agent and trusted host keys beforehand. It does not start a remote daemon. The `password` option is daemon authentication, not an SSH login password. SSH configuration and host verification remain the SSH client's responsibility.

## Client lifecycle

| Public API | Result / behavior |
| --- | --- |
| `DaemonClient::connect(options, cancel).await` | Connected `Result<DaemonClient>`; performs handshake/authentication. |
| `server_info()` | Current JSON server capabilities/metadata snapshot. |
| `require_feature(key, message)` | Reject unsupported capabilities explicitly. |
| `subscribe()`, `subscribe_binary()` | Local bounded receivers; do not create server observations. |
| `send(message).await`, `send_binary(bytes).await` | Send session JSON or a binary frame without RPC result handling. |
| `request(message, response_type, timeout).await` | Correlated RPC; generates a request ID if absent. |
| `rpc(request_type, response_type, fields, options).await` | Correlated JSON operation with explicit timeout/cancellation. |
| `flush().await` | Wait for outbound transport flush. |
| `cancellation()` | Connection lifetime cancellation token. |
| `close().await` | Cancel and join owned tasks; no agent/workspace archival. |

`RequestOptions { timeout, cancel }` defaults to 30 seconds and a fresh cancellation token. A zero request timeout disables its deadline. `request_cancellable` accepts a token directly. A submitted mutation may still complete after timeout, cancellation or disconnect. Never assume dropping a future rolls it back.

## Agents and workspaces

`create_agent(fields, options)` / `create_workspace(fields, options)` require keyed creation lifecycle support and preserve/allocate `idempotencyKey`. They return payloads containing resources rather than handles. Keep explicit keys for recoverable workflows. Ordinary named operation methods take one JSON `fields` argument and use default request options. `update_agent(agent_id, patch)` is an exception with a separate ID argument.

| Area | Methods |
| --- | --- |
| Agent queries | `fetch_agent`, `fetch_agents`, `fetch_all_agents`, `fetch_agent_history`, `fetch_agent_timeline`, `list_commands` |
| Turns | `send_agent_message`, `wait_for_finish(agent_id, options)`, `cancel_agent`, `rewind_agent` |
| Configuration | `update_agent`, `set_agent_mode`, `set_agent_model`, `set_agent_feature`, `set_agent_thinking_option`, `apply_agent_config` |
| Lifecycle | `resume_agent`, `import_agent`, `refresh_agent`, `archive_agent`, `detach_agent`, `delete_agent` |
| Subagents | `list_provider_subagents`, `fetch_provider_subagent_timeline`, `build_agent_fork_context` |
| Projects | `open_project`, `add_project`, `list_projects`, `rename_project`, `set_project_icon`, `remove_project` |
| Workspaces | `fetch_workspaces`, `archive_workspace`, `set_workspace_title`, `set_workspace_pinned`, `inspect_workspace_recovery`, `restore_workspace` |
| Setup/scripts | `fetch_workspace_setup_status`, `run_workspace_setup`, `list_workspace_scripts`, `start_workspace_script_with_status`, `stop_workspace_script` |
| Labels | `list_workspace_labels`, `set_workspace_label`, `update_workspace_label`, `delete_workspace_label` |

Use [Agents](agents.md), [Workspaces](workspaces.md), and the [complete request schema](../../packages/protocol/schema/session-inbound.json) for argument shapes. `CursorPages::{agents,workspaces,history}` and fallible `CursorPages::timeline` provide `next(&client, options).await -> Result<Option<Value>>`; repeated cursors are errors. They do not collect all pages unless you do so.

## Events and recovery

Owned observations require `Arc<DaemonClient>`: `observe_agents`, `observe_workspaces`, `observe_events`, `observe_timeline`, `observe_terminals`, `observe_terminal`. An `OwnedSubscription` exposes `snapshot`, `subscription_id`, `recv().await` and `release().await`. Current owned observation methods require `ownedSubscriptions`; they do not emulate old shared-filter subscriptions. Event bodies remain full session messages.

`ReconnectingClient` restores explicitly owned observations, not arbitrary mutations. `connection(&cancel)` returns the current `Arc<DaemonClient>`. `RestoringSubscription` reports `SubscriptionEvent::{Snapshot,Update}`; restored snapshots invalidate old baselines and require history reconciliation. `recover_creation(kind, key, fields, options)` explicitly reconciles an uncertain creation with its original key. See [Events](events.md) and [Recipes](recipes.md).

## Terminals and files

`create_terminal`, `list_terminals`, `rename_terminal`, `capture_terminal`, `kill_terminal` manage daemon PTYs. Include workspace identity and actual directory. `observe_terminal(id, restore, options)` returns `TerminalStream`; `recv` yields `TerminalEvent::{Frame,Exit}`, `input` sends bytes, and `resize` takes `paseo_protocol::binary_frames::TerminalResize`. Release `stream.subscription` explicitly. These APIs do not add an interactive terminal UI to Flutter.

`read_file(cwd, path, max_bytes, options)` returns `DownloadedFile { metadata, bytes }` with a byte ceiling. `upload_file(file_name, mime, modified_at, bytes, options)` uses bounded binary frames. `list_directory`, `write_file`, `create_file_entry`, `rename_file_entry`, `duplicate_file_entry`, `delete_file_entry` expose file operations; check their exact schema before destructive calls. Upload cancellation does not promise rollback or automatic retry.

## Providers, configuration, and connected hosts

Provider methods: `list_available_providers`, `get_providers_snapshot`, `refresh_providers_snapshot`, `list_provider_models`, `list_provider_modes`, `list_provider_features`, `get_provider_diagnostic`, `list_provider_usage`, `list_usage_reports`. See [Providers](providers.md) for compact snapshots and draft configuration.

Configuration methods are `get_daemon_config`, `patch_daemon_config`, `read_project_config`, `write_project_config`; `get_daemon_status` and feature-gated `daemon_status(timeout)` read daemon status. Do not expose secrets in diagnostic output. Management methods such as `restart`, `shutdown`, `update_daemon` and `reload_daemon_config` affect the selected running daemon and require explicit operator intent.

Connected hosts use `list_hosts`, `add_host`, `update_host`, `remove_host`, `open_host_route`, `open_host_link`. Select a registered ID as `DaemonTarget::Endpoint` to use its managed route. Both managing and destination daemons must support the requested host capabilities. Routing is not implicit permission to manage an unrelated daemon. Low-level route metadata/frame codecs are in `paseo_protocol::{routing,binary_frames}`.

The operations module also exposes checkout/forge, schedules, plugins, usage and browser-host registration RPCs. These preserve the wire contract; they are not separate local runtime implementations. `rpc` supports an explicit request/response pair when a named convenience method is unnecessary.

<a id="errors-and-cleanup"></a>
## Errors and cleanup

```rust
use anyhow::Result;
use paseo_client::{ClientError, DaemonClient};
use serde_json::json;

async fn inspect_errors(client: &DaemonClient, id: &str) -> Result<()> {
    match client.fetch_agent(json!({"agentId": id})).await {
        Ok(payload) => println!("{}", payload),
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<ClientError>() {
                eprintln!("daemon code: {}", failure.code);
                // Inspect failure.details deliberately; it may contain private data.
            }
            return Err(error);
        }
    }
    Ok(())
}
```

Methods return `anyhow::Result`; not every error is `ClientError`. That type provides `code`, `message`, and optional JSON `details`. Known transport/request codes include `DAEMON_UNREACHABLE`, `DAEMON_CONNECTION_LOST`, `DAEMON_REQUEST_TIMEOUT`, `DAEMON_REQUEST_CANCELLED`, and `DAEMON_RPC_ERROR`; target selection also reports ambiguity/invalid/local-only errors. Daemon rejection payloads can contain more specific codes.

RPC payloads with a string `error`, `accepted: false`, or `success: false` are generally surfaced as errors, with the original payload in `ClientError.details`. `wait_for_finish_response` is deliberately different: its string `error` describes the agent's turn outcome and remains in the returned payload. Always inspect outcome `status` and `error` even when the Rust result is `Ok`; `permission`/`timeout` is not completion. Actual transport/RPC failures still return `Err`. Cleanup must release subscriptions and close the client on failures. Archive only resources you own; closing transport never substitutes for archival.

## Protocol APIs

`paseo_protocol::wire::{Direction,SessionMessage,Envelope,parse_envelope,encode_envelope,validate_session}` validates native JSON schemas. `Inbound` means **to the daemon**, `Outbound` means **from the daemon**. `SessionMessage::parse` validates without stripping unknown extension fields; `new` and ordinary deserialization are not full schema validation. Protocol crypto handles transport encryption, not application password authorization. See [protocol validation](../../docs/protocol-validation.md) for authoritative schema ownership and examples.

## Syntax highlighting

Add `paseo-highlight = { path = "../paseo/packages/highlight" }` to the quickstart application's dependencies. Native grammars require a platform C compiler in addition to Rust/Cargo. A standalone synchronous example:

```rust
use paseo_highlight::{ColorScheme, SyntaxThemeId, highlight_code, resolve_syntax_colors};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = "fn main() { println!(\"hello\"); }";
    let colors = resolve_syntax_colors(SyntaxThemeId::Github, ColorScheme::Dark);
    for line in highlight_code(source, "example.rs")? {
        for token in line {
            let color = token.style.map(|role| colors.color(role));
            println!("{:?}: {:?}", color, token.text);
        }
    }
    Ok(())
}
```

`highlight_code`/`highlight_line` borrow token text from the input. Reuse `Highlighter::new()` and its methods for repeated work. `get_language_for_file`, `is_language_supported`, `get_supported_extensions`, and `get_parser_for_file` expose language lookup; unknown suffixes are plain text, supported-parser failures are errors. `StyledRange` uses UTF-8 byte offsets; `byte_to_utf16` and `utf16_to_byte` adapt other consumers. Highlight complete documents when cross-line syntax matters.

`highlight_code_in_steps(code, filename, HighlightStepsOptions)` launches a native worker. Its `HighlightSteps` supports blocking iteration or nonblocking `poll_next`. `HighlightStep::Preview` is provisional; final `Lines { first_line, lines }` replaces those positions, and `Finished` marks completion. This is bounded incremental delivery, not an edit-delta parser. Dropping the handle cancels delivery, though an active native parse may finish.

`SyntaxThemeId` covers Github, Catppuccin, Dracula, TokyoNight, One, Nord, Gruvbox and Solarized. `resolve_syntax_colors` selects light/dark palettes (Dracula uses one palette); `create_highlight_style` returns native style rules for the 20 `HighlightStyle` roles. Rendering is your responsibility; no browser editor or framework runtime is required.
