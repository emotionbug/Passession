# Architecture

Paseo is a native client/server system. **Rust owns the daemon and CLI; Flutter owns the Linux, macOS, Windows, Android, and iOS clients.** Desktop can run a local daemon. Mobile clients connect to a desktop/server daemon and do not ship or start one.

```text
Flutter desktop ── native desktop-host IPC ── Rust lifecycle/supervisor
       │                                         │
Flutter mobile ── WebSocket / encrypted relay ── Rust daemon
Rust CLI / external Rust SDK ─────────────────────┤
                                                  ├─ provider processes
                                                  ├─ persistent state / files / Git
                                                  ├─ script and setup PTYs
                                                  ├─ MCP / schedules / connected hosts
                                                  └─ optional native plugin executables
```

Paseo's runtime, SDKs and build tools require neither Node, Electron, nor Chromium. Optional backend plugins run as native executables with bounded JSONL IPC. External provider programs retain their own installation and runtime requirements.

## Ownership

| Component                   | Source                                                                                 | Responsibility                                                                            |
| --------------------------- | -------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| Native executable           | `packages/daemon/src/main.rs`, `cli.rs`, `cli/`                                        | CLI dispatch, daemon commands, client operations                                          |
| Transport                   | `packages/daemon/src/transport/`, `packages/client/src`, `packages/protocol/src/crypto` | HTTP/WebSocket, authentication, relay encryption, local/remote SDK and CLI connections |
| State and configuration     | `state.rs`, `config.rs`, `config/`                                                     | Config resolution, durable state, mutation transactions                                   |
| Agents                      | `agent.rs`, `agent/`, `providers/`                                                     | Provider processes, permissions, lifecycle, snapshots, timeline persistence and streaming |
| Projects and workspaces     | `projects.rs`, `projects/`, `directory_sync/`                                          | Checkout/worktree identity, acquisition, registries, directory reconciliation             |
| Files and Git               | `files/`, `git/`                                                                       | Remote picker, reads/writes, observation, Git facts and lazy diff bodies                  |
| Scripts                     | `terminals.rs`, `terminals/`                                                           | PTY lifecycle, setup, service scripts, replayable output                                  |
| Automation and integrations | `automation/`, `mcp/`, `mcp_client/`, `hosts/`, `hub/`                                 | Schedules, MCP client/server, connected-host and Hub behavior                             |
| Desktop lifecycle           | `desktop_host.rs`, `desktop_host/`, `daemon_control/`                                  | Structured IPC, process identity, ownership, supervisor and worker lifecycle              |
| Client                      | `packages/flutter_app/lib`                                                             | Dart transport/store, navigation, native views and platform integration                   |
| Packaging                   | `xtask/src`, `packages/flutter_desktop_host`                                            | Native runtime assets, installers, provenance, updates, attributions                      |
| Audio bridge                | `packages/flutter_audio`                                                               | Native audio integration                                                                  |

Paths in the daemon rows are relative to `packages/daemon/src` unless otherwise shown. The client (`paseo-client`), shared protocol (`paseo-protocol`), relay (`paseo-relay`), plugin SDK (`paseo-plugin`) and highlighting (`paseo-highlight`) are Rust workspace crates. `paseo-xtask` owns development and release commands. The unused website source is removed; Markdown documentation remains in the repository.

## Protocol and persistence

The existing JSON request/response and event contracts, binary stream framing, request IDs, session IDs, provider-native session identity, and configuration formats are preserved. `packages/protocol` supplies Rust wire types, validation and shared cryptography; Dart implements the same wire contract. Compatibility work must update both sides and exercise the wire boundary, not just compile one side.

The default daemon home is `~/.paseo`. `PASEO_HOME` and CLI `--home` select another instance. State remains in that home: `config.json`, agent/session records and history, project/workspace records, schedules, plugin configuration, credentials and runtime bookkeeping. A UI cutover is not permission to delete, reset, or relocate a user's state. Checkout development defaults to `.dev/paseo-home` and `127.0.0.1:6768`, separate from the ordinary `6767` endpoint.

A client disconnect does not end a provider turn or archive a session. Daemon-owned operations must survive socket replacement. Request failures, persistence failures, and unsupported capabilities are visible; the client must not acknowledge a durable mutation before it succeeds.

## Client state and navigation

`packages/flutter_app/lib/core/client.dart` owns client protocol behavior; `core/app_store.dart` owns shared application state. `ui/shell.dart` and `features/navigation` compose the native shell.

The primary navigation tree is **host → directory → session**, not workspace grouping. Keys include host identity: equal paths on two machines must not merge. Empty directories remain visible. Search temporarily reveals ancestors without overwriting saved collapse state. Workspaces remain daemon entities and are available through Command Center and workspace operations.

File operations use the selected host. Add Project browses host-side folders through the daemon rather than choosing a path on the client device. Markdown and source editing are native views. Diffs request a summary/file tree before loading individual patch bodies; opening a large diff must not eagerly fetch/render every patch.

## Desktop ownership

Flutter talks to the Rust `desktop-host` process through structured IPC. Desktop lifecycle tracks the daemon's process-start identity and whether this application owns its supervisor. A pre-existing daemon is not treated as a child to kill on quit. Keep-running policy and owned-stop behavior are distinct from hiding or closing a window. A supervisor restart must not create a second competing daemon.

The first visible desktop window also owns process coordination; only additional windows create additional Flutter engines. Closing the first window while another remains open retains its hidden engine for daemon/tray ownership and reuses that window on the next New Window request. Destroy the primary engine only at application shutdown, after all document guards and owned-daemon cleanup have completed.

Platform tray/menu/window behavior is native integration, not an Electron renderer feature. Runtime verification is platform-specific; Linux lifecycle evidence does not establish Windows or macOS execution.

## Retained and excluded surfaces

Script and workspace setup PTYs are retained. Their Flutter presentation is **read-only output**: script start/stop and log observation are not an interactive terminal emulator. Native CLI chat remains available outside the application.

The client has no embedded browser, browser automation, web target, Chromium capture surface, or executable React UI-plugin host. Links open externally. Backend plugin RPCs/providers/lifecycle/usage contributions remain an optional extension boundary, described in [plugins](plugins.md); a server extension does not imply support for its former React client entry.

## Runtime assets and release boundary

Desktop packaging carries the native executable, skills, native runtime assets and desktop editor assets. It does not carry a Node executable, JavaScript plugin compiler or speech runtime. Speech recognition, voice conversation and TTS are excluded; generic effect-sound playback remains.

Cargo and bundled-asset notices are generated by the Rust tooling in `xtask/src/notices.rs`. Preserve them with the packaged artifacts. See [release requirements](release.md#distribution-requirements).
