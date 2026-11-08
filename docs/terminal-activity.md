# Terminals and script activity

Every terminal is a **Rust daemon-owned PTY**: interactive shells opened as Terminal tabs, and workspace setup and service scripts. Clients observe them; only an interactive terminal accepts input and size requests from a client.

`packages/daemon/src/terminals` owns process startup, output, screen/replay state, hooks, setup and scripts. Workspace lifecycle under `projects/` determines when setup and teardown operations belong to a workspace. The native process layer verifies process identity and owns termination; a client must not send signals to an arbitrary PID.

## Interactive terminals

New session → Launch → **Terminal** replaces the prompt composer with a single **Open terminal** button; there is no startup command field. The button creates a shell with `create_terminal_request` on the selected host, workspace and directory and opens it as a `terminal` tab (`packages/gui/src/features/terminal`). Desktop and Android/iOS share this surface; on mobile the tab drives the remote host's shell, never a local PTY. Typed text, encoded keys and pastes go to the PTY as `terminal_input`. The focused tab claims the PTY size; a client that does not own the size cannot resize the PTY (`Terminal::resize` in `terminals/runtime.rs`).

Live interactive terminals are listed in the sidebar under their directory, after its agent sessions, so a terminal-only workspace stays reachable on compact layouts. The sidebar subscribes per shown workspace (`subscribe_terminals_request {cwd, workspaceId}`) because the daemon matches a terminal only to its deepest owning workspace root; script and service terminals named in the workspace's `scripts[]` are left out. Rows keep first-seen order since the daemon's list is unordered.

Android input methods edit a hidden editor (`ImeBridge` in `features/terminal/input.rs`). Each snapshot's committed text is diffed against the previous one; composing text is sent only when committed. Until then the view draws it at the cursor as an underlined preedit overlay (`composing_text`/`preedit_cells`), so a Hangul syllable is visible while it is being composed; the PTY never sees uncommitted text. Never rewrite that editor inside an edit callback: the platform drops an echo when the IME already sent a newer revision, and every later diff would repeat text. Resets (after Enter, an emptied editor or a long line) are published on draw as a new input session, which fences edits of the old editor.

An interactive terminal leaves the daemon's list when its shell exits. Closing a live terminal tab asks for confirmation and kills the daemon terminal; an exited terminal's tab closes without asking. Disconnecting or hiding the tab only ends observation.

## Scripts and setup

Activity and output have different meanings. Receiving bytes does not establish successful completion; silence does not establish that a command stopped. Preserve the script's running/completed/failed state and actual exit outcome, and present errors in the log surface. Agent hook activity must remain associated with the correct host/session rather than inferred from whichever tab is visible.

Configured scripts run as the PTY's actual shell command (`-c` on Unix, `/d /s /c` on Windows), rather than as text typed into a long-lived interactive shell. Child exit is the lifecycle authority; an early shell-integration escape sequence cannot mark a still-running process complete. Interactive terminal creation keeps its existing shell behavior.

An exited or explicitly stopped configured script retains its bounded capture for later log attachment, but is no longer listed as a live terminal. Restart retires the previous terminal; workspace cleanup and daemon shutdown release retained state. A setup result with no commands is a loaded empty result, not a perpetual loading indicator.

Script and setup output is a read-only log: it never writes input or resizes the PTY. The log view applies Follow after current content/viewport bounds are known. Touch panning pauses Follow; re-enabling it immediately reveals the tail even if the process has already exited and emits no more bytes.

Client reconnect or closing a log view changes observation, not daemon ownership. Explicit script stop and workspace teardown must release owned resources according to their lifecycle. Avoid duplicate subscriptions and unbounded replay buffers when multiple clients observe the same script.

Native CLI chat still exists as a separate terminal program. For emulation, measurement and verification, see [terminal performance](terminal-performance.md) and [testing](testing.md).
