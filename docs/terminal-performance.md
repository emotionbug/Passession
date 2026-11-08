# Terminal and log performance

The Rust daemon owns every PTY. The native client shows interactive terminal tabs and read-only script/setup logs over the same terminal stream; closing or hiding a view does not remove process, PTY, replay or stream responsibilities. Lifecycle rules live in [terminal activity](terminal-activity.md).

Daemon ownership is `packages/daemon/src/terminals.rs` and `terminals/`, including runtime, streams, screen state, scripts and setup. The client renders interactive terminals in `packages/gui/src/features/terminal/` and execution logs in `packages/gui/src/features/logs/` (`session.rs` stream state, `view.rs` visible rows, `panel.rs` follow-output, find and view lifecycle).

## Terminal emulation

Daemon screens and client views interpret VT output with Ghostty (`libghostty-vt`) through the shared `paseo-terminal` crate (`packages/terminal`), which exposes rows, cursor, modes and key/paste encoding as plain Rust data. Building it needs Zig; see [development](development.md#toolchains).

Ghostty objects are `!Send`. The daemon keeps each terminal's screen on its own thread (`paseo_terminal::ThreadBound`) and calls into it from the PTY reader, snapshot/restore and capture paths; the client keeps its emulators on the UI thread. Only the daemon screen answers terminal queries (device attributes, cursor position, colors, mode reports) back into the PTY, so client emulators never reply.

Ghostty bounds scrollback in bytes and prunes whole pages. `paseo_terminal::Options::scrollback` is a row count: the emulator budgets bytes from it and presents exactly the newest rows, so daemon snapshots (1000 rows), log and terminal scrollback preferences keep row semantics. Development profiles build Ghostty `ReleaseSafe`: its `Debug` mode scans page-list integrity on every mutation and could not keep up with a PTY printing a few thousand lines.

An interactive terminal sends input and its claimed size. A log observer sends neither and drops emulator replies; its local viewport size is not a PTY resize request. Restore authoritative dimensions and remap the observation slot before consuming binary frames after reconnect.

## Boundaries to measure

Measure process spawn/start, first output, keystroke-to-echo latency, sustained output, replay after reconnect, backpressure, memory under large scrollback, completion/error publication and cancellation. Separate server PTY cost, daemon screen-thread work, core delivery, client VT parsing and Makepad layout/draw work. An isolated parser benchmark does not prove end-to-end latency.

Slow or hidden clients must not create unbounded buffers or stall unrelated sessions. Subscriptions have explicit owners and cleanup. Output remains observable across view changes; closing a log view must not silently terminate a running service. Conversely, stopping a script must release its owned process tree and PTY.

Use a disposable workspace and daemon home. Preserve stdout/stderr, exit status and timing evidence. Test failure, worker restart and client reconnect, not only a short successful command. Do not run the retired xterm/browser/React terminal benchmarks as native acceptance checks.

No hardware/platform measurements are inferred from CI definitions. See [testing](testing.md) for actual recorded native execution.
