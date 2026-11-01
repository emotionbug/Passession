# Development

## Toolchains

- Rust and Cargo for the daemon/CLI, client/protocol/plugin/highlighting SDKs, relay and `xtask` tooling. `Cargo.toml` specifies the minimum Rust version.
- Flutter/Dart for `packages/flutter_app`; set `FLUTTER_BIN` or `FLUTTER_ROOT`, or put Flutter on `PATH`. Packaging also searches `$XDG_CACHE_HOME/paseo-flutter-sdk` (default `~/.cache/paseo-flutter-sdk`).
- Node, npm and a TypeScript compiler are not Paseo build prerequisites. Separately installed agent CLIs retain their own runtime requirements.
- Linux desktop: Flutter's Linux toolchain and native GTK/build dependencies. macOS/iOS: macOS, Xcode and the Flutter Apple toolchain. Windows: Flutter's Windows/Visual Studio C++ toolchain. Android: JDK and Android SDK as required by the checked-in Flutter Android project.

Fetch Rust dependencies with `cargo fetch --locked`. Flutter wrappers run `flutter pub get --enforce-lockfile`. Desktop builds require their target host; native packaging rejects `CARGO_BUILD_TARGET` cross-compilation.

Default CI runs Rust and Flutter tests on one Ubuntu x64 runner; it does not build release artifacts or repeat tests across platforms. Packaging and deployment workflows run only on explicit manual dispatch. Requested Linux/Windows ARM64 builds use the reusable SDK release pinned by `.github/flutter-sdk.json`, not a source bootstrap in each app build. The dedicated SDK workflow must publish both platforms before those builds run; see [SDK prerequisites and upgrades](release.md#reusable-linux-and-windows-arm64-flutter-sdks). This uses official Flutter source and native Dart/engine artifacts, with no engine fork. Normal Linux x64 CI does not need the ARM64 SDK release.

## Development commands

Run commands from the repository root:

```sh
cargo xtask build-daemon                # Cargo binary plus native runtime assets
cargo xtask dev daemon                  # run the isolated Rust daemon
cargo xtask dev daemon --watch          # rebuild/restart that dev process on source changes
cargo xtask dev app                     # build/run the native desktop Flutter client
cargo xtask dev app android --device <device-id>
cargo xtask dev app ios --device <device-id>
cargo xtask dev cli ls                  # native CLI against the checkout dev home
```

`cargo xtask dev daemon` starts the development daemon. There is no website build, Metro server, Expo prebuild, Electron renderer, Chromium debugging port, or web client build.

For direct Rust iteration, `cargo build --locked -p paseo-daemon --bin paseo` compiles the executable. Use `cargo xtask build-daemon` when the run also needs staged skills, native runtime assets or packaging provenance. Cargo-only compilation is not a complete distributable runtime.

`cargo xtask build-daemon --release` produces an optimized local native runtime, not a published release identity. Default Flutter release builds use optimized Rust; debug development builds use the debug profile. `PASEO_NATIVE_RELEASE=1` is a separate, validated publication-provenance choice; see [release](release.md).

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

The Flutter `--run` wrapper fixes its development home to this checkout's `.dev/paseo-home` and defaults its listen endpoint to `127.0.0.1:6768`. Do not assume a custom daemon `PASEO_HOME` automatically changes the app wrapper's managed home. To use another host instance, add it as an explicit client connection.

`PASEO_NODE_ENV=development` is a retained configuration name used for development-only provider behavior; it does not switch the daemon implementation to Node.

A mobile emulator's `localhost` is not the development machine. On an Android AVD use `10.0.2.2:6768` and a reachable daemon listen address, or an explicitly configured ADB reverse tunnel. Physical devices need a reachable LAN/VPN/relay endpoint and the matching password. See [Android](android.md).

## Native builds

```sh
cargo xtask build-flutter linux
cargo xtask build-flutter macos
cargo xtask build-flutter windows
cargo xtask build-flutter android
cargo xtask build-flutter android --arch arm64
cargo xtask build-flutter ios
cargo xtask package-desktop
```

Only run the desktop target matching the host OS. iOS requires macOS. `cargo xtask build-flutter ios` without export/provisioning configuration emits an explicitly unsigned device app archive, not an installable IPA. Android output without a release key is marked debug-signed. Build descriptors and client artifacts live under `artifacts/flutter/`; see [release](release.md) before producing distribution artifacts.

Linux and Windows app CMake configurations disable optional JNI discovery. JNI arrives transitively through Android `path_provider`/`shared_preferences` implementations; native desktop implementations do not use it. This prevents an installed development JDK from silently adding `libdartjni` and a JVM runtime dependency to desktop bundles. Android JNI configuration and dependencies are unchanged.

## Source ownership

- Rust: `packages/daemon/src`; CLI commands in `cli/`, providers in `providers/`, process ownership in `daemon_control/`, desktop IPC in `desktop_host/`.
- Dart: `packages/flutter_app/lib`; protocol and state in `core/`, features under `features/`, native desktop/mobile integration under `platform/`.
- Native build, packaging and release: `xtask/src`; platform project integration: `packages/flutter_desktop_host`.
- Rust SDKs: `packages/client`, `packages/protocol`, `packages/plugin`, `packages/highlight`; relay: `packages/relay`.
- Maintained audio dependency: `packages/flutter_audio/third_party/flutter_soloud` retains upstream 5.2.2 runtime/build sources and licenses. Its MSVC-only `<malloc.h>` include fixes ARM64 `_alloca` declarations without disabling audio or changing architecture. Archive identity, retained files and the patch are recorded in `UPSTREAM.json`; normal development and CI use the same path dependency.

Use Flutter tooling for frame/layout profiling and Rust instrumentation for daemon work. Browser/CDP and React profilers do not profile the native client. A development mock provider can test controlled UI states but is not evidence of real provider interoperability.

## Scripts, logs, and diagnostics

Workspace setup and services remain daemon-side shell/PTY operations. The Flutter app exposes start/stop and **read-only logs**. Do not remove PTY backend support because the interactive terminal UI is excluded.

Inspect the selected home's `daemon.log` and native CLI status. Keep diagnostic captures free of credentials and unrelated user state. A script or provider process is owned by the host daemon, never the mobile client.

## Checks

See [testing](testing.md) for Rust, Flutter, SDK, and real-runtime checks. Run the appropriate checks only when authorized by the assignment. A CI job definition, compilation, or a mocked widget test is not an actual platform smoke test.
