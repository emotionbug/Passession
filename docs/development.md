# Development

## Toolchains

- Rust and Cargo for everything: the daemon/CLI, the native client (`packages/gui*`, Makepad 1.0.0), client/protocol/plugin/highlighting SDKs, relay and `xtask` tooling. `Cargo.toml` specifies the minimum Rust version.
- Node, npm, Flutter/Dart and a TypeScript compiler are not Paseo build prerequisites. Separately installed agent CLIs retain their own runtime requirements.
- Linux desktop: `pkg-config` and the X11, Xcursor, GLX/EGL, ALSA and PulseAudio development packages Makepad links, plus Wayland development files for native file dialogs (Ubuntu: `libx11-dev libxcursor-dev libglx-dev libgl-dev libegl-dev libasound2-dev libpulse-dev libwayland-dev`), and `patchelf`: `build-gui` rewrites the RPATHs of the bundled daemon runtime. Packaging adds `dpkg-dev`, `xdg-utils` and the runtime-loaded `libxss1 libsm6 libice6`.
- macOS: Xcode command-line tools (`clang`, `codesign`, `iconutil`). iOS also needs full Xcode (`actool`, `devicectl`, `simctl`) and `rustup target add aarch64-apple-ios` (plus `aarch64-apple-ios-sim` for Apple-silicon simulators).
- Windows: Visual Studio C++ tools (MSVC linker, `rc.exe`, CRT redistributable); NSIS for installers.
- Android: JDK 17+, the Android SDK (`ANDROID_HOME`), NDK 28.2.13676358 and `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android`. `xtask` downloads a pinned Gradle 9.3.1 into `$XDG_CACHE_HOME/paseo-packaging`; there is no Gradle wrapper in the repository.

Fetch Rust dependencies with `cargo fetch --locked`. Desktop builds require their target host; `xtask` chooses the GUI target itself and rejects `CARGO_BUILD_TARGET`.

Default CI runs the Rust workspace tests, GUI crates included, on one Ubuntu x64 runner; it does not build release artifacts or repeat tests across platforms. Packaging and deployment workflows run only on explicit manual dispatch.

## Development commands

Run commands from the repository root:

```sh
cargo xtask build-daemon                # Cargo binary plus native runtime assets
cargo xtask dev daemon                  # run the isolated Rust daemon
cargo xtask dev daemon --watch          # rebuild/restart that dev process on source changes
cargo xtask dev app                     # build/run the native desktop client
cargo xtask dev app android --device <device-id>
cargo xtask dev app ios --device <device-id-or-simulator-udid>
cargo xtask dev cli ls                  # native CLI against the checkout dev home
```

`cargo xtask dev daemon` starts the development daemon. There is no website build, Metro server, Expo prebuild, Electron renderer, Chromium debugging port, or web client build.

For direct Rust iteration, `cargo build --locked -p paseo-daemon --bin paseo` compiles the executable. Use `cargo xtask build-daemon` when the run also needs staged skills, native runtime assets or packaging provenance. Cargo-only compilation is not a complete distributable runtime.

`cargo run -p paseo-gui --bin paseo-sessions` runs the client from the source tree: Makepad then loads fonts and icons from each crate's `resources/` directory through absolute build paths. `cargo xtask dev app` instead runs the staged bundle exactly as it is packaged (resources under `makepad/`, bundled daemon runtime), so packaging mistakes show up in development.

`cargo xtask build-daemon --release` produces an optimized local native runtime, not a published release identity. Default GUI release builds use optimized Rust; debug development builds use the debug profile. `PASEO_NATIVE_RELEASE=1` is a separate, validated publication-provenance choice; see [release](release.md).

`paseo daemon status` reports daemon liveness without running external provider/model discovery.
Its `providers` list is a cached observation (`providersCached: true`); `available` is `null`
until availability is known, with catalog `status` and `fetchedAt` retaining context.
The CLI's 1500ms status RPC deadline is unchanged. Use the existing
`list_available_providers_request`, `provider_diagnostic_request` or catalog refresh for
fresh provider diagnostics instead of treating basic status as a fresh provider probe.

## Isolated state and endpoints

The ordinary native runtime defaults to `~/.paseo` and port `6767`. Development launchers use `<checkout>/.dev/paseo-home` and `127.0.0.1:6768`; they must not disturb an installed daemon or application.

The daemon development wrapper honors:

- `PASEO_HOME`: explicit isolated home.
- `PASEO_DEV_ROOT`: root for its default `.dev/paseo-home`.
- `PASEO_LISTEN`: listening endpoint.
- `PASEO_SERVICE_DAEMON_PORT`: service port, defaulting the listen address to `0.0.0.0:<port>` when supplied.
- `PASEO_DEV_SEED_HOME`: copy durable agent/project JSON and configuration into an empty development home, not process IDs, sockets, or logs.
- `PASEO_DEV_RESET_HOME=1`: destructive reset of that development destination before seeding; use only with an explicitly disposable target.

The desktop `--run` launcher fixes its development home to this checkout's `.dev/paseo-home`, defaults its listen endpoint to `127.0.0.1:6768`, and points the client's own state at `.dev/gui-state` (`PASEO_GUI_STATE_DIR`), so a development client never reads or rewrites the installed app's hosts, drafts or layout. Do not assume a custom daemon `PASEO_HOME` automatically changes the app launcher's managed home. To use another host instance, add it as an explicit client connection.

`PASEO_NODE_ENV=development` is a retained configuration name used for development-only provider behavior; it does not switch the daemon implementation to Node.

A mobile emulator's `localhost` is not the development machine. On an Android AVD use `10.0.2.2:6768` and a reachable daemon listen address, or an explicitly configured ADB reverse tunnel. Physical devices need a reachable LAN/VPN/relay endpoint and the matching password. See [Android](android.md).

## Native builds

```sh
cargo xtask build-gui linux
cargo xtask build-gui macos
cargo xtask build-gui windows
cargo xtask build-gui android
cargo xtask build-gui android --arch arm64
cargo xtask build-gui ios
cargo xtask package-desktop
```

Only run the desktop target matching the host OS. iOS requires macOS. `cargo xtask build-gui ios` without `PASEO_IOS_PROVISIONING_PROFILE`/`PASEO_IOS_SIGNING_IDENTITY` emits an explicitly unsigned device app archive, not an installable IPA. Android output without a release key is marked debug-signed. Build descriptors and client artifacts live under `artifacts/native/`; staged bundles, Android work directories and Gradle state live under `target/paseo-gui/`. See [release](release.md) before producing distribution artifacts.

Every build passes the client's identity at compile time (`PASEO_VERSION`, `PASEO_RELEASE_CHANNEL`, `PASEO_UPDATE_REPOSITORY`, `PASEO_BUILD_NUMBER`, `PASEO_BUILD_SOURCE_SHA`). Staged desktop builds (`build-gui`, `dev app`) also set `MAKEPAD_PACKAGE_DIR=makepad`: Makepad then opens resources at `makepad/<crate>/resources/…` relative to the working directory on Linux and Windows (the client switches to its executable directory at startup) and relative to `Contents/Resources` on macOS (`MAKEPAD=apple_bundle`). Android and iOS always use `makepad/` inside the APK assets or app bundle.

Makepad platform, draw and widgets are patched through `packages/gui-platform/vendor/` and root `[patch.crates-io]`. Each crate retains its upstream license and `VENDORED.md` provenance. Platform carries Android API 24 compatibility, native lifecycle/input fixes and the packaged live-watcher guard; draw carries Unicode line breaking; widgets carries scrollbar and caret behavior. Keep changes in the existing copy, not a second framework fork. The source watcher is disabled when `MAKEPAD_PACKAGE_DIR` is set: staged builds have no live reload; plain `cargo run` keeps it.

Packages copy the `resources/` directory of every Makepad-aware crate in the `paseo-gui` dependency closure, minus the reviewed `excluded` files in `packaging/resource-notices.json`. A font without a digest-bound, revision-pinned license entry fails the build; so does a Cargo dependency without license text (add a revision-pinned entry to `packaging/cargo-notice-supplements.json`, never an SPDX template).

## Source ownership

- Rust daemon: `packages/daemon/src`; CLI commands in `cli/`, providers in `providers/`, process ownership in `daemon_control/`, desktop IPC in `desktop_host/`.
- Native client: `packages/gui` (Makepad shell, theme, common widgets, `src/features/*`), `packages/gui-core` (host connections, directory synchronization, persisted client state), `packages/gui-platform` (desktop host, updater, tray, notifications, pickers, audio, Android/iOS sources).
- Native build, packaging and release: `xtask/src`; packaging resources, the Android Gradle project and vendored Makepad Android Java: `packaging/`.
- Rust SDKs: `packages/client`, `packages/protocol`, `packages/plugin`, `packages/highlight`; relay: `packages/relay`.

Use Rust instrumentation for daemon and client work. Browser/CDP and React profilers do not profile the native client. A development mock provider can test controlled UI states but is not evidence of real provider interoperability.

## Scripts, logs, and diagnostics

Workspace setup and services remain daemon-side shell/PTY operations. The client exposes start/stop and **read-only logs**. Do not remove PTY backend support because the interactive terminal UI is excluded.

Inspect the selected home's `daemon.log` and native CLI status. Keep diagnostic captures free of credentials and unrelated user state. A script or provider process is owned by the host daemon, never the mobile client.

## Checks

See [testing](testing.md) for Rust, GUI, SDK, and real-runtime checks. Run the appropriate checks only when authorized by the assignment. A CI job definition, compilation, or a unit test is not an actual platform smoke test.
