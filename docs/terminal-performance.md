# Script and setup log performance

The native application has **read-only script/setup logs**, not an interactive terminal emulator. The Rust daemon retains PTYs for workspace setup and service scripts. Removing a terminal UI does not remove process, PTY, replay or stream responsibilities.

Daemon ownership is `packages/daemon/src/terminals.rs` and `terminals/`, including runtime, streams, screen state, scripts and setup. The Rust client observes execution in `packages/gui/src/features/logs/session.rs` and paints visible rows in `view.rs`; `panel.rs` owns follow-output, find and view lifecycle.

## Boundaries to measure

Measure process spawn/start, first output, sustained output, replay after reconnect, backpressure, memory under large logs, completion/error publication and cancellation. Separate server PTY cost, core delivery, ANSI processing and Makepad layout/draw work. An isolated parser benchmark does not prove end-to-end log latency.

Slow or hidden clients must not create unbounded buffers or stall unrelated sessions. Subscriptions have explicit owners and cleanup. Script output remains observable across view changes; closing a log view must not silently terminate a running service. Conversely, stopping a script must release its owned process tree and PTY.

The headless `alacritty_terminal` parser interprets cursor movement, erasure, alternate screens and SGR styling before rows are painted. This dependency does not enable an interactive terminal: the observer sends neither input nor resize, and drops emulator reply, clipboard and bell events. Restore authoritative dimensions and remap the observation slot before consuming binary frames after reconnect; a local viewport size is not a PTY resize request.

Use a disposable workspace and daemon home. Preserve stdout/stderr, exit status and timing evidence. Test failure, worker restart and client reconnect, not only a short successful command. Do not run the retired xterm/browser/React terminal benchmarks as native acceptance checks.

No hardware/platform measurements are inferred from CI definitions. See [testing](testing.md) for actual recorded native execution and [terminal activity](terminal-activity.md) for the retained lifecycle scope.
