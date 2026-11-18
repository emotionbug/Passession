# Terminals and script activity

Every terminal is a **Rust daemon-owned PTY**: interactive shells opened as Terminal tabs and configured project scripts. Clients observe them; only an interactive terminal accepts client input and size requests.

`packages/daemon/src/terminals` owns process startup, output, screen/replay state, hooks and scripts. Internal directory-owner records retain process scope, but worktree setup/teardown execution is retired. The native process layer verifies process identity and owns termination; a client must not send signals to an arbitrary PID.

## Interactive terminals

New Session → Launch → **Terminal** replaces the composer with **Open terminal**, without a startup-command field. It sends `create_terminal_request` for the selected host and directory, retaining an internal `workspaceId` owner, and opens a `terminal` tab (`packages/gui/src/features/terminal`). Desktop and mobile share this surface; mobile drives the host shell, never a local PTY. Typed text, encoded keys and pastes use the ordered terminal input channel. Preserve encoded bytes: X10 mouse coordinates are not necessarily UTF-8. Use the mouse format negotiated by the terminal program. The focused tab claims PTY size; other clients cannot resize it (`Terminal::resize` in `terminals/runtime.rs`).

Live interactive terminals appear under their directory after agent sessions, including terminal-only directories on compact layouts. The sidebar subscribes with internal owner scope (`subscribe_terminals_request {cwd, workspaceId}`); script/service terminals named in the owner's `scripts[]` are excluded. Rows retain first-seen order because the daemon list is unordered.

Android input methods edit a hidden editor (`ImeBridge` in `features/terminal/input.rs`). Windows input methods compose into the same hidden editor: the platform turns each IMM32 composition step into the same snapshots (`ime_composer.rs` in the vendored makepad-platform). Each snapshot's committed text is diffed against the previous one; composing text is sent only when committed. Until then the view draws it at the cursor as an underlined preedit overlay (`composing_text`/`preedit_cells`), so a Hangul syllable is visible while it is being composed; the PTY never sees uncommitted text. Never rewrite that editor inside an edit callback: the platform drops an echo when the IME already sent a newer revision, and every later diff would repeat text. Resets (after Enter, an emptied editor or a long line) are published on draw as a new input session, which fences edits of the old editor.

An interactive terminal leaves the daemon's list when its shell exits. Use **Terminate terminal** from the terminal header or the sidebar row's menu to stop the process and remove the terminal. Closing a live desktop terminal tab also asks for confirmation and terminates it; an exited terminal's tab closes without asking. Once the daemon confirms termination, all views of that terminal close and its row disappears without waiting for a list refresh. On failure, the client reports the error and keeps the terminal available. Compact Back, hiding a tab, and disconnecting only end observation.

## Scripts

Activity and output have different meanings. Receiving bytes does not establish successful completion; silence does not establish that a command stopped. Preserve the script's running/completed/failed state and actual exit outcome, and present errors in the log surface. Agent hook activity must remain associated with the correct host/session rather than inferred from whichever tab is visible.

Configured scripts run as the PTY's actual shell command (`-c` on Unix, `/d /s /c` on Windows), rather than as text typed into a long-lived interactive shell. Child exit is the lifecycle authority; an early shell-integration escape sequence cannot mark a still-running process complete. Interactive terminal creation keeps its existing shell behavior.

An exited or stopped script retains bounded capture for later log attachment but leaves the live terminal list. Restart retires the previous terminal; explicit owner cleanup and daemon shutdown release retained state. None of these operations removes the execution directory.

Script output is read-only: it never writes input or resizes the PTY. The log view applies Follow after current content/viewport bounds are known. Touch panning pauses Follow; re-enabling it reveals the tail even after the process exits.

Reconnect or closing a log view changes observation, not daemon ownership. Explicit script stop releases owned process resources. Avoid duplicate subscriptions and unbounded replay buffers when multiple clients observe one script.

Native CLI chat still exists as a separate terminal program. For emulation, measurement and verification, see [terminal performance](terminal-performance.md) and [testing](testing.md).
