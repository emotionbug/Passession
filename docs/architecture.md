# Architecture

Paseo is a native client/server system. **Rust owns the daemon, the CLI and the Linux, macOS, Windows, Android and iOS clients** (Makepad 1.0.0 widgets). Desktop can run a local daemon. Mobile clients connect to a desktop/server daemon and do not ship or start one.

```text
Rust desktop client ── native desktop-host IPC ── Rust lifecycle/supervisor
       │                                            │
Rust mobile client ── WebSocket / encrypted relay ── Rust daemon
Rust CLI / external Rust SDK ─────────────────────┤
                                                  ├─ provider processes
                                                  ├─ persistent state / files / Git
                                                  ├─ terminal and script PTYs
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
| Projects and directory ownership | `projects.rs`, `projects/`, `directory_sync/` | Existing-directory identity, retained ownership registries and reconciliation |
| Files and Git               | `files/`, `git/`                                                                       | Remote picker, reads/writes, observation, Git facts and lazy diff bodies                  |
| Terminals and scripts       | `terminals.rs`, `terminals/` | PTY lifecycle, interactive shells, service scripts, screen state and replayable output |
| Automation and integrations | `automation/`, `mcp/`, `mcp_client/`, `hosts/`, `hub/`                                 | Schedules, MCP client/server, connected-host and Hub behavior                             |
| Desktop lifecycle           | `desktop_host.rs`, `desktop_host/`, `daemon_control/`                                  | Structured IPC, process identity, ownership, supervisor and worker lifecycle              |
| Client runtime              | `packages/gui-core/src`                                                                | Host connections, directory synchronization, persisted client state, request/event boundary |
| Embedded tailnet             | `packages/tailnet`, `packages/client/src/transport`, `packages/gui-core/src/runtime` | App-owned libtailscale lifecycle, authenticated loopback transport, host routing and cancellation |
| Client UI                   | `packages/gui/src` (`shell/`, `common/`, `features/*`)                                 | Makepad shell, navigation, tabs/windows, feature views and controllers                    |
| Platform integration        | `packages/gui-platform/src`, `packages/gui-platform/{android,ios}`                     | Desktop host, updater, tray, notifications, pickers, audio, mobile bridges                |
| Terminal emulation          | `packages/terminal`                                                                    | Ghostty VT engine shared by daemon screens, terminal tabs and logs                        |
| Packaging                   | `xtask/src`, `packaging/`                                                              | Native runtime assets, installers, provenance, updates, attributions                      |

Paths in the daemon rows are relative to `packages/daemon/src` unless otherwise shown. The client (`paseo-client`), shared protocol (`paseo-protocol`), relay (`paseo-relay`), plugin SDK (`paseo-plugin`), highlighting (`paseo-highlight`) and terminal emulation (`paseo-terminal`, wrapping Ghostty's `libghostty-vt`) are Rust workspace crates. `paseo-xtask` owns development and release commands. The unused website source is removed; Markdown documentation remains in the repository.

## Protocol and persistence

The existing JSON request/response and event contracts, binary stream framing, request IDs, session IDs, provider-native session identity, and configuration formats are preserved. `packages/protocol` supplies Rust wire types, validation and shared cryptography for the daemon and, through `paseo-client`, for the native client. Compatibility work must still exercise the wire boundary against older daemons and clients, not just compile one side.

The default daemon home is `~/.paseo`. `PASEO_HOME` and CLI `--home` select another instance. State remains in that home: `config.json`, agent/session records and history, project/workspace records, schedules, plugin configuration, credentials and runtime bookkeeping. A UI cutover is not permission to delete, reset, or relocate a user's state. Checkout development defaults to `.dev/paseo-home` and `127.0.0.1:6768`, separate from the ordinary `6767` endpoint.

A client disconnect does not end a provider turn or archive a session. Daemon-owned operations must survive socket replacement. Request failures, persistence failures, and unsupported capabilities are visible; the client must not acknowledge a durable mutation before it succeeds.

Embedded Tailscale is a client transport, not an OS VPN or a replacement daemon. One app-owned node is shared across windows; `gui-platform` supplies its OS-backed encryption key, `paseo-tailnet` owns encrypted state and the pinned native engine, and `gui-core` owns host admission and lifecycle. Explicitly selected tailnet hosts cannot fall back to saved Normal/relay alternatives. Normal hosts and unrelated public HTTP remain usable at the same time. Host HTTP uses the same network boundary; daemon credentials are restricted to the admitted origin and never follow externally initiated redirect/icon chains.

The node outlives focus changes: regaining focus keeps a responsive node and starts one only when none is running, and the 5 s status poll drops tailnet routes only after three consecutive transient failures (`Starting` during a network rebind, a stalled native call) or at once for a terminal state (sign-in, approval, stopped). A slow native close is waited out, and the next start waits for any drain still holding the state lease, so it never fails as "in use" or creates a second Go node. The lease is a non-blocking `flock` on Unix and `File::try_lock` elsewhere: Rust 1.93's `File::try_lock` and `File::lock` return `Unsupported` on Android, which would fail every start there. Only contention is reported as "in use"; any other lock failure is its own error. Pause disconnects the node until Resume without changing the saved setting, so a restarted app is not paused; Stop is Settings' disconnect and persists Embedded Tailscale off. While it is enabled, the app hands the platform a localized background presence with Pause/Resume and Stop: the Android foreground-service notification and the desktop tray menu. iOS has no background presence; its controls are in Settings. Entering the background begins one UIKit background task, ended on reactivation or by its expiration handler, so the node and host links survive the system-granted grace (about 30 s) and a quick app switch returns to a running node. iOS suspends the app after that grace; on return, the foreground `ensure` starts a node if none is running.

## Client state and navigation

`packages/gui-core` owns client protocol behavior and shared application state behind `GuiCore`/`CoreHandle`; network and disk IO never run on the GUI thread. `packages/gui/src/shell` composes the native shell and routes; feature widgets under `packages/gui/src/features` own their paged data.

The primary navigation tree is **host → directory → session**. Keys include host identity: equal paths on two machines must not merge. Empty directories remain visible. Search temporarily reveals ancestors without overwriting saved collapse state. Sessions open directly as tabs without an intermediate workspace or nested session tab bar. Internal `workspaceId` records remain durable directory owners for protocol, history and runtime state; they are not exposed as a product creation or selection concept.

File operations use the selected host. New Session and Add Project browse host-side directories through the daemon, never client-device paths. New work reuses an existing directory, including a user-managed Git worktree; Paseo does not create, restore or remove worktrees. Markdown and source editing are native views. Git Log reads commit history, while Commit reviews and submits selected changes; patch bodies load on demand.

## Rendering

Each window composes retained layers, like Android `RenderNode`s or iOS `CALayer`s. The window's own draw list holds only the background capture and the composition. The chrome (header, banner, tab strips, dividers and empty states), the sidebar, every pane root and the page each paint into their own `common::layer::Layer`. A layer that is not invalidated replays its retained draw calls, shell hit regions and overlay children instead of running its widgets again.

Feature code keeps the ordinary Makepad contract: when a widget's own data changes, it redraws its own area (`self.redraw(cx)`, `area.redraw(cx)`). That repaints only the draw list holding the area; nested draw lists repaint their ancestors. Widgets whose parts change at different rates keep them in separate retained draw lists: the chat panel retains its transcript, composer, chrome, rail, modal and toasts independently, so typing, hover or a toast does not re-run transcript rows. Unchanged rows can reuse their work under the [Markdown row-retention contract](markdown-renderer.md#streaming-work). The shell repaints a layer by itself when:

- its rect changes;
- its inputs change (focus, app activity, width class, safe-area insets, a different page root);
- it is shown again after being hidden for a frame (hidden layers are detached and their overlays pruned);
- hover or press feedback changes, which repaints the layer that recorded the affected hit id (transient overlay hits included; an unknown hit repaints nothing);
- a shell action names its region (chrome, sidebar, page, sessions, overlays or one pane root); there is no "repaint the whole shell" call, and layout changes recompose the chrome while retained layers compare their own geometry;
- a core `State` publication changes what the layer presents (`common::state_delta`).

The core publishes `CoreState`: small sections and host records as JSON, plus shared `prefs`, `layouts` and per-host `DirectorySnapshot`s whose rows are `Arc` values changed copy-on-write. A publication copies pointers for everything that did not change, and `revisions` names what did. The app classifies each publication once per batch from those revisions:

- Preferences, storage, session, the host set, host record content and directory content repaint the layers that present them.
- `prefs` keys and Tailnet status repaint only the panels that present them (Settings, Git). Composer drafts and saved window layouts repaint nothing: their writers already own their presentation.
- Activity alone (`updatedAt`/`lastActivityAt`, host latency and probes) repaints only recency presentations: sidebar rows and the History/sessions views, once per second, and only while they are shown. A chat never repaints for its own activity timestamps.

A pane that presents another agent's recency must invalidate itself.

Results that arrive in the background must repaint only the owner that drew the waiting content, such as code highlighting, images, Mermaid diagrams and decoded attachments. Record that owner's draw list or area when drawing, or compare a revision in the owner's event handling. A stale or invisible result repaints nothing. `Cx::redraw_all` repaints every layer of every window. It is reserved for theme, font and locale changes, and the GUI crate's `clippy.toml` rejects any other use.

## Desktop ownership

The client talks to the Rust `desktop-host` process through structured IPC (`paseo-gui-platform`). Desktop lifecycle tracks the daemon's process-start identity and whether this application owns its supervisor. A pre-existing daemon is not treated as a child to kill on quit. Keep-running policy and owned-stop behavior are distinct from hiding or closing a window. A supervisor restart must not create a second competing daemon.

All desktop windows live in one process and share one set of host connections; each window or pane keeps its own route and selection. A second launch forwards its arguments to the running primary process and exits. Closing a window or tab neither archives sessions nor stops a pre-existing daemon; owned-daemon cleanup runs only at application quit, after every document close guard has resolved.

Platform tray/menu/window behavior is native integration, not an Electron renderer feature. Runtime verification is platform-specific; Linux lifecycle evidence does not establish Windows or macOS execution.

While Embedded Tailscale is enabled, the desktop tray menu (ksni on Linux, tray-icon on macOS/Windows) starts with its status and Pause/Resume and Stop controls; they reach the app as `PlatformEvent::TailnetControl`. A presence set before the tray exists is applied when the tray appears, and a Linux session without a StatusNotifier host simply has no tray. Hiding windows to the tray keeps the process, and with it the node, running. A notification click or agent link raises the app and routes into the first window; while that window is dormant it is reopened with the route instead of routing invisibly. On Linux the notification's `ActivationToken` becomes the window's `_NET_STARTUP_ID` and activation timestamp so focus-stealing prevention grants the raise; on Windows a refused `SetForegroundWindow` briefly attaches to the foreground thread's input.

The app hands the tray its localized Open and Quit labels (`set_tray_labels`) at startup and on each language change. Until then they are English, and labels sent before the tray exists apply when it appears.

## Terminals and excluded surfaces

Daemon PTYs back interactive terminals and project scripts. The client opens interactive shells as Terminal tabs on every platform and presents script output as read-only logs; see [terminal activity](terminal-activity.md). Native CLI chat remains available outside the application.

The client has no embedded browser, browser automation, web target, Chromium capture surface, or executable React UI-plugin host. Links open externally. Backend plugin RPCs/providers/lifecycle/usage contributions remain an optional extension boundary, described in [plugins](plugins.md); a server extension does not imply support for its former React client entry.

## Runtime assets and release boundary

Desktop packaging carries the native executable, skills, native runtime assets and desktop editor assets. It does not carry a Node executable, JavaScript plugin compiler or speech runtime. Speech recognition, voice conversation and TTS are excluded; generic effect-sound playback remains.

Cargo and bundled-asset notices are generated by the Rust tooling in `xtask/src/notices.rs`. Preserve them with the packaged artifacts. See [release requirements](release.md#distribution-requirements).
