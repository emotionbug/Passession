# Architecture

Paseo is a native client/server system. **Rust owns the daemon, the CLI and the Linux, macOS, Windows, Android and iOS clients** (Makepad 1.0.0 widgets). Desktop can start an independent local daemon. Mobile clients connect to a desktop/server daemon and do not ship or start one.

```text
Rust desktop client ── optional `daemon ensure` ── Rust lifecycle/supervisor
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
| Daemon lifecycle            | `daemon_control/`                                                                      | Installation, launch owner, supervisor and worker lifecycle                               |
| Client runtime              | `packages/gui-core/src`                                                                | Host connections, directory synchronization, persisted client state, request/event boundary |
| Embedded tailnet             | `packages/tailnet`, `packages/client/src/transport`, `packages/gui-core/src/runtime` | App-owned libtailscale lifecycle, authenticated loopback transport, host routing and cancellation |
| Client UI                   | `packages/gui/src` (`shell/`, `common/`, `features/*`)                                 | Makepad shell, navigation, tabs/windows, feature views and controllers                    |
| Platform integration        | `packages/gui-platform/src`, `packages/gui-platform/{android,ios}`                     | Desktop activation, updater, notifications, pickers, audio, mobile bridges                |
| Terminal emulation          | `packages/terminal`                                                                    | Ghostty VT engine shared by daemon screens, terminal tabs and logs                        |
| Packaging                   | `xtask/src`, `packaging/`                                                              | Native runtime assets, installers, provenance, updates, attributions                      |

Paths in the daemon rows are relative to `packages/daemon/src` unless otherwise shown. The client (`paseo-client`), shared protocol (`paseo-protocol`), relay (`paseo-relay`), plugin SDK (`paseo-plugin`), highlighting (`paseo-highlight`) and terminal emulation (`paseo-terminal`, wrapping Ghostty's `libghostty-vt`) are Rust workspace crates. `paseo-xtask` owns development and release commands. The unused website source is removed; Markdown documentation remains in the repository.

## Protocol and persistence

The existing JSON request/response and event contracts, binary stream framing, request IDs, session IDs, provider-native session identity, and configuration formats are preserved. `packages/protocol` supplies Rust wire types, validation and shared cryptography for the daemon and, through `paseo-client`, for the native client. Compatibility work must still exercise the wire boundary against older daemons and clients, not just compile one side.

The default daemon home is `~/.paseo`. `PASEO_HOME` and CLI `--home` select another instance. State remains in that home: `config.json`, agent/session records and history, project/workspace records, schedules, plugin configuration, credentials and runtime bookkeeping. A UI cutover is not permission to delete, reset, or relocate a user's state. Checkout development defaults to `.dev/paseo-home` and `127.0.0.1:6768`, separate from the ordinary `6767` endpoint.

A client disconnect does not end a provider turn or archive a session. Daemon-owned operations must survive socket replacement. Request failures, persistence failures, and unsupported capabilities are visible; the client must not acknowledge a durable mutation before it succeeds.

Embedded Tailscale is a client transport, not an OS VPN or a replacement daemon. One app-owned node is shared across windows; `gui-platform` supplies its OS-backed encryption key, `paseo-tailnet` owns encrypted state and the pinned native engine, and `gui-core` owns host admission and lifecycle. Explicitly selected tailnet hosts cannot fall back to saved Normal/relay alternatives. Normal hosts and unrelated public HTTP remain usable at the same time. Host HTTP uses the same network boundary; daemon credentials are restricted to the admitted origin and never follow externally initiated redirect/icon chains.

The node outlives focus changes: regaining focus keeps a responsive node and starts one only when none is running, and the 5 s status poll drops tailnet routes only after three consecutive transient failures (`Starting` during a network rebind, a stalled native call) or at once for a terminal state (sign-in, approval, stopped). A slow native close is waited out, and the next start waits for any drain still holding the state lease, so it never fails as "in use" or creates a second Go node. The lease is a non-blocking `flock` on Unix and `File::try_lock` elsewhere: Rust 1.93's `File::try_lock` and `File::lock` return `Unsupported` on Android, which would fail every start there. Only contention is reported as "in use"; any other lock failure is its own error. Pause disconnects the node until Resume without changing the saved setting, so a restarted app is not paused; Stop is Settings' disconnect and persists Embedded Tailscale off. Android offers these controls in its foreground-service notification. Desktop and iOS expose them in Settings; quitting the desktop app ends its embedded node, not the local daemon. On iOS, entering the background begins one UIKit background task, ended on reactivation or expiration, so the node and host links survive the system-granted grace (about 30 s). iOS suspends the app after that grace; on return the foreground `ensure` starts a node if none is running.

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

Window overlays composite in rank order: pane overlays below shell dialogs and toasts, then the window cursor. Within a rank, a retained overlay always paints after its codeflow ancestors, so repainting a sheet cannot cover its still-retained children. Hiding a layer also detaches the overlays below it.

The core publishes `CoreState`: small sections and host records as JSON, plus shared `prefs`, `layouts` and per-host `DirectorySnapshot`s whose rows are `Arc` values changed copy-on-write. A publication copies pointers for everything that did not change, and `revisions` names what did. The app classifies each publication once per batch from those revisions:

- Preferences, storage, session, the host set, host record content and directory content repaint the layers that present them.
- `prefs` keys and Tailnet status repaint only the panels that present them (Settings, Git). Composer drafts and saved window layouts repaint nothing: their writers already own their presentation.
- Activity alone (`updatedAt`/`lastActivityAt`, host latency and probes) repaints only recency presentations: sidebar rows and the History/sessions views, once per second, and only while they are shown. A chat never repaints for its own activity timestamps.

A pane that presents another agent's recency must invalidate itself.

Results that arrive in the background must repaint only the owner that drew the waiting content, such as code highlighting, images, Mermaid diagrams and decoded attachments. Record that owner's draw list or area when drawing, or compare a revision in the owner's event handling. A stale or invisible result repaints nothing. `Cx::redraw_all` repaints every layer of every window. It is reserved for theme, font and locale changes, and the GUI crate's `clippy.toml` rejects any other use.

## Desktop lifecycle

At launch the desktop client may run the bundled CLI's `daemon ensure` command for its local daemon home. The CLI installs a durable runtime and starts it independently (a systemd user service when available on Linux, detached otherwise). The desktop observes existing local daemons without taking ownership and never stops them.

The ready daemon owns a tray icon in supported graphical sessions. Its worker launches a separate `__tray` child, so the daemon does not run the GUI event loop. The child closes its icon when the worker's lifetime pipe ends; a confirmed stop from the tray requests graceful shutdown of that worker. On Linux the daemon waits for a session D-Bus StatusNotifierWatcher and retries if it appears later. On macOS it requires an Aqua session; on Windows it requires an interactive window station. Headless deployments can disable it with `daemon.tray: false` or `daemon run --no-tray`.

All desktop windows live in one process and share one set of host connections; each window or pane keeps its own route and selection. A second launch forwards arguments to the primary process and exits. Closing the last window runs guarded quit, including unsaved documents, and exits the process without stopping the daemon or its tray. Embedded Tailscale controls are in Settings. Daemon updates use the launch owner; app, Tailscale and update-helper updates use the local desktop updater without a daemon handoff.

A notification click or agent link raises the app and routes into the first window. While that window is dormant, it is reopened with the route rather than routing invisibly. On Linux the notification's `ActivationToken` becomes the window's `_NET_STARTUP_ID` and activation timestamp, so focus-stealing prevention can grant the raise. On Windows a refused `SetForegroundWindow` briefly attaches to the foreground thread's input. Runtime verification is platform-specific; Linux lifecycle evidence does not establish Windows or macOS execution.

## Browser client

Host Overview → Host Configuration → **Enable web UI** sets `daemon.web.enabled` (off by default). The daemon applies changes without restarting. `PASEO_WEB_ENABLED` overrides the persisted setting for the lifetime of the process.

The daemon serves the Makepad WebAssembly client at `/` on its existing HTTP listener. `cargo xtask build-web` builds the release bundle in `.dev/build/web` and records its version, source commit and digest in `.dev/build/web-build.json`; daemon runtime staging copies the bundle to `web/` beside the executable. Release runtimes (the desktop apps' bundled daemon and the standalone daemon packages) accept only a bundle built from their own release source; the Docker image ships none. Set `PASEO_WEB_DIR` to an absolute bundle path for development. A daemon runtime without those assets still works; its web entry point returns 404 even if enabled.

The static bundle is public on allowed hosts. All browser control goes through the same-origin `/ws` connection with the existing `hello` authentication. For password-protected access, use TLS or a trusted network. See the [security policy](../SECURITY.md#browser-ui).

The browser build is the same Makepad client compiled for `wasm32-wasip1` with WebGL rendering. The page has one thread: the client core, Markdown parsing, highlighting, text layout and image decoding run as queued work on the page thread instead of worker threads. Browser APIs (WebSocket, local storage, file picker, downloads, clipboard) are reached through `packages/browser`. Client state and host settings, including a saved password, live in the page's local storage, as the native app keeps them in its state file. Features that need the local machine are not shown: dropping files by path, local HTML previews, project-icon picking, local daemon management, app updates, OS notifications and embedded Tailscale. Attachments come from the browser's file picker, and exports download through the browser.

Composer images live in memory for the page session; local storage is too small for image bytes. The daemon echoes a sent prompt without its images, so after a reload, messages sent from that page show their text only, as they do on another device. The native app keeps sent images in its blob directory.

## Agent forms in web views

OMP's `ask` tool sends one HTML form. The daemon shows it as a question card (`metadata.ompAskForm`) that carries the form's HTML (`input.html`) and each question's field `name`. An `<img src>` naming a file (relative to the agent's working directory, absolute, or `file:`) is inlined as a `data:` URI when the card is built, because the client may be on another machine. The daemon accepts either the card's answers or `formData` (`{values: {name: [value…]}, other: {name: text}}`) from a client that showed the HTML, maps both onto the form's fields, and answers OMP 19.1's `form` dialog or replays the per-field dialogs of earlier OMP.

Where an OS web view is available, the card shows that HTML (`paseo_gui_platform::webview`, one backend per platform behind the same `WebView` API and the shared `bridge.js`). The view is clipped by a container sized to the visible part of the card, with the page positioned inside it:

- Linux: WebKitGTK 4.1, loaded at run time so the app starts without it, in a GTK window reparented into a child of the Makepad X11 window; GLib runs from the row's 16 ms timer.
- Windows: WebView2 (Evergreen Runtime) in a child window of Makepad's window; the loader is linked statically.
- macOS and iOS: WKWebView in a clipping subview of Makepad's view; page JavaScript is off and the bridge runs in a client content world. AppKit delivers keys and wheel turns aimed at the web view to WebKit instead of Makepad. On iOS a vertical pan over the page scrolls the transcript.
- Android: the system WebView over Makepad's surface, driven through JNI on the UI thread; JavaScript stays on for the bridge and the CSP blocks the page's own scripts.
- Web UI: a sandboxed iframe without `allow-scripts` over the canvas; the bridge runs in the app's page against the iframe's document.

Elsewhere, and where the engine is missing, the card shows the questions as before. The card's Submit asks the page for its values, and Enter in a one-line field does the same. A native view paints above everything the app draws, so it hides while a menu, dialog or sheet covers the window, while a popover overlaps it, and while its row is scrolled out or another page is shown. A wheel turn the page does not use scrolls the transcript.

## Terminals and excluded surfaces

Daemon PTYs back interactive terminals and project scripts. The client opens interactive shells as Terminal tabs on every platform and presents script output as read-only logs; see [terminal activity](terminal-activity.md). Native CLI chat remains available outside the application.

The client has no embedded browser, browser automation, Chromium capture surface, or executable React UI-plugin host. Links open externally. Backend plugin RPCs/providers/lifecycle/usage contributions remain an optional extension boundary, described in [plugins](plugins.md); a server extension does not imply support for its former React client entry.

## Runtime assets and release boundary

Desktop packaging carries the native executable, skills, native runtime assets and desktop editor assets. It does not carry a Node executable, JavaScript plugin compiler or speech runtime. Speech recognition, voice conversation and TTS are excluded; generic effect-sound playback remains.

Cargo and bundled-asset notices are generated by the Rust tooling in `xtask/src/notices.rs`. Preserve them with the packaged artifacts. See [release requirements](release.md#distribution-requirements).
