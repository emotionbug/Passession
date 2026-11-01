# Paseo Sessions

Paseo Sessions is a native environment for running and reviewing coding-agent work. The client is Flutter on **Linux, macOS, Windows, Android, and iOS**. The daemon and `paseo` CLI are Rust. Desktop clients can manage a local daemon; Android and iOS connect to a daemon on another machine.

This checkout is version **0.13.1**. See [release requirements](docs/release.md) for native artifacts, signing and distribution.

## Work across hosts

The sidebar is **host → directory → session**. Every host has its own expandable row, including a single configured host. Directories remain separate, including empty directories; sessions are ordered by recent activity. Host and directory rows collapse independently, directory collapse is persisted, and identical paths on different hosts remain distinct.

Search matches hosts, directory names and paths, session titles, and providers. Matching ancestors expand without replacing saved collapse preferences. A directory's plus action creates a session in that checkout on that host. Add Project includes a host-side folder browser: a remote host's path is never selected with a local OS file dialog.

Workspace grouping controls are not part of this sidebar. Workspaces still exist for orchestration, worktrees, scripts, and Command Center navigation. Existing session IDs, history, files, configuration, and daemon storage remain in place. Closing a chat tab does not archive the session.

## Native application scope

- Agent chat, permissions, provider/model selection, profiles, session import, and history.
- Markdown rendering, file browsing and source editing, and summary-first Git diffs with patch bodies loaded on demand.
- Workspace setup and service scripts run in daemon-owned PTYs. The app shows **read-only logs**, not an interactive terminal emulator.
- Schedules, MCP tools and servers, connected hosts, and remote directory selection.
- Native desktop integration and local-daemon lifecycle, with remote-only mobile clients.
- Generic effect-sound playback; no speech recognition, voice conversation or TTS.

There is **no web client, Electron or Chromium runtime, embedded browser, browser automation, or executable React UI-plugin compatibility**. External links open outside the app. The unused website has been removed; documentation remains in this repository. Paseo's runtime, SDKs and build tools do not require Node or TypeScript. Agent CLIs are external programs and may have their own runtime requirements.

Built-in provider and usage integrations execute in Rust. Backend plugins use native executables and the Rust `paseo-plugin` SDK; existing JavaScript/TypeScript plugins must be ported to that contract. See [plugins](docs/plugins.md).

## Build and run from this checkout

Install Rust/Cargo, the Flutter SDK and the native toolchain for your target. See [development](docs/development.md) for prerequisites and isolated development state.

```sh
cargo fetch --locked
cargo xtask build-daemon
cargo xtask dev app
```

For a standalone development daemon and a native client:

```sh
cargo xtask dev daemon
# In another terminal:
cargo xtask dev app
```

Both development commands target the checkout's `.dev/paseo-home` and port `6768`; keep the installed daemon's home and port separate. Build and release commands run through the Rust `paseo-xtask` crate.

Build a client on a suitable host:

```sh
cargo xtask build-flutter linux
cargo xtask build-flutter macos
cargo xtask build-flutter windows
cargo xtask build-flutter android --arch arm64
cargo xtask build-flutter ios
```

Desktop builds require their target OS; iOS requires macOS/Xcode. Android/iOS builds contain no daemon. Artifacts and build descriptors are written under `artifacts/flutter/`. Android uses **Paseo Sessions**, package `sh.paseo.sessions`; unsigned/debug-signed output is not a store-ready release. See [Android](docs/android.md) and [release](docs/release.md) for signing and packaging.

For a headless host, the native build produces `target/debug/paseo` (`paseo.exe` on Windows), or the corresponding directory under `CARGO_TARGET_DIR`. Keep its generated runtime assets alongside it:

```sh
./target/debug/paseo onboard
./target/debug/paseo daemon start
./target/debug/paseo daemon status
```

`paseo onboard` is explicit setup. Bare `paseo` opens CLI chat; it is not an installer or onboarding command. The normal runtime home is `~/.paseo`, overridable with `PASEO_HOME` or `--home`; `config.json`, agent history, project/workspace metadata, and connection credentials keep their existing persistence contracts. Use `--host` for a remote daemon and `PASEO_PASSWORD` when required. SSH transport connects to an existing daemon; it does not install one.

The [Docker guide](docs/docker.md) builds a native daemon-only container. It does not serve an app at the HTTP root.

## CLI and SDK

Choose **Use for CLI** on an agent profile in host settings to set the default provider, model, mode, and thinking level. Explicit CLI options take priority. Conflicting provider/model arguments fail before connecting.

```sh
paseo
paseo --resume <session-id>
paseo run --provider codex "review this checkout"
paseo ls
paseo attach <session-id>
paseo send <session-id> "also add tests"
paseo import --provider pi <native-session-id>
paseo run --host workstation.local:6767 --cwd /workspace --provider codex "review the diff"
```

The Rust `paseo-client` SDK lives in `packages/client`. Shared wire types and relay cryptography live in `packages/protocol` (`paseo-protocol`); the Rust daemon and Dart client retain the same wire contract. The former TypeScript SDK is replaced rather than wrapped.

## Development map

| Area                                                          | Source                                 |
| ------------------------------------------------------------- | -------------------------------------- |
| Daemon, CLI, providers, storage, supervisor, desktop-host IPC | `packages/daemon`                      |
| Five-platform native client                                   | `packages/flutter_app`                 |
| Desktop packaging and runtime assets                          | `packages/flutter_desktop_host`        |
| Native audio integration                                      | `packages/flutter_audio`               |
| Rust client SDK and shared protocol                           | `packages/client`, `packages/protocol` |
| Native plugin SDK                                             | `packages/plugin`                      |
| Rust relay and syntax highlighting                            | `packages/relay`, `packages/highlight` |
| Rust build and release tooling                                | `xtask`                                |

Read [architecture](docs/architecture.md), [development](docs/development.md), [testing](docs/testing.md), and [release](docs/release.md) before changing runtime boundaries.

Linux has actual native daemon/client and desktop-host lifecycle execution coverage. Default CI runs tests in one Linux environment; manually available cross-platform build definitions are not evidence that Windows, macOS, Android, or iOS have been executed in this migration. The current evidence and outstanding checks are recorded in [testing](docs/testing.md).

## License and distribution

Paseo source is Apache-2.0; dependency licenses remain their own. Packaging generates Cargo and bundled-asset attributions. Preserve those notices with the distributed artifacts; they are not a compliance certification or corresponding-source offer. See [release](docs/release.md#distribution-requirements).
