# Script and setup activity

Workspace setup and service scripts remain **Rust daemon-owned PTY operations**. Their native client surface is read-only output with lifecycle controls, not an interactive terminal emulator. Do not remove this backend while removing terminal UI code.

`packages/daemon/src/terminals` owns process startup, output, screen/replay state, hooks, setup and scripts. Workspace lifecycle under `projects/` determines when setup and teardown operations belong to a workspace. The native process layer verifies process identity and owns termination; a client must not send signals to an arbitrary PID.

Activity and output have different meanings. Receiving bytes does not establish successful completion; silence does not establish that a command stopped. Preserve the script's running/completed/failed state and actual exit outcome, and present errors in the log surface. Agent hook activity must remain associated with the correct host/session rather than inferred from whichever tab is visible.

Configured scripts run as the PTY's actual shell command (`-c` on Unix, `/d /s /c` on Windows), rather than as text typed into a long-lived interactive shell. Child exit is the lifecycle authority; an early shell-integration escape sequence cannot mark a still-running process complete. Ordinary interactive terminal creation keeps its existing shell behavior.

An exited or explicitly stopped configured script retains its bounded capture for later log attachment, but is no longer listed as a live terminal. Restart retires the previous terminal; workspace cleanup and daemon shutdown release retained state. A setup result with no commands is a loaded empty result, not a perpetual loading indicator.

The read-only log view applies Follow after current content/viewport bounds are known. Touch panning pauses Follow; re-enabling it immediately reveals the tail even if the process has already exited and emits no more bytes.

Client reconnect or closing a log view changes observation, not daemon ownership. Explicit script stop and workspace teardown must release owned resources according to their lifecycle. Avoid duplicate subscriptions and unbounded replay buffers when multiple clients observe the same script.

Interactive shell input, xterm rendering and embedded browser terminals are outside the native product scope. Native CLI chat still exists as a separate terminal program. For measurement and verification, see [script/log performance](terminal-performance.md) and [testing](testing.md).
