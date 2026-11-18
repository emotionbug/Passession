# Development

## Toolchains

- Rust and Cargo for the daemon/CLI, native client (`packages/gui*`, Makepad 1.0.0), client/protocol/plugin/highlighting SDKs, relay and `xtask` tooling. `Cargo.toml` specifies the minimum Rust version.
- Zig **0.15.2** on every target. `paseo-terminal` links Ghostty's VT library, which the vendored `libghostty-vt-sys` build script compiles with `zig build` (`packages/terminal/vendor/VENDORED.md`). Put `zig` on `PATH` or set `ZIG` to the executable; `xtask` build commands stop before compiling when it is missing or outside Ghostty's accepted range (0.15.x, patch 2 or newer). A cold build `git clone`s Ghostty at the pinned commit into Cargo's `OUT_DIR` and lets Zig fetch Ghostty's package dependencies, so it needs network access. Offline, set `GHOSTTY_SOURCE_DIR` to a checkout of that commit and `GHOSTTY_ZIG_SYSTEM_DIR` to a pre-populated Zig package directory (passed to `zig build --system`).
- Go **1.27.4** for native GUI bundles' embedded libtailscale engine; daemon-only builds do not require Go. Put it on `PATH` or set `GO` to its executable. `packages/tailnet/native/go.mod`, `go.sum` and `UPSTREAM.json` pin the toolchain, dependency graph and maintained upstream source; builds reject another Go version and never rewrite the module graph.
- Node, npm, Flutter/Dart and a TypeScript compiler are not Paseo build prerequisites. Separately installed agent CLIs retain their own runtime requirements.
- Linux desktop: `pkg-config` and the X11, Xcursor, GLX/EGL, ALSA and PulseAudio development packages Makepad links (Ubuntu: `libx11-dev libxcursor-dev libglx-dev libgl-dev libegl-dev libasound2-dev libpulse-dev`), and `patchelf`: `build-gui` rewrites the RPATHs of the bundled daemon runtime. Native file dialogs use the XDG portal over D-Bus and link no Wayland library. Packaging adds `dpkg-dev`, `xdg-utils` and the runtime-loaded `libxss1 libsm6 libice6 libfontconfig1`.
- macOS: Xcode command-line tools (`clang`, `codesign`, `iconutil`). iOS also needs full Xcode (`actool`, `devicectl`, `simctl`) and `rustup target add aarch64-apple-ios` (plus `aarch64-apple-ios-sim` for simulators). Simulator builds need an Apple-silicon Mac: Ghostty ships no x86_64 simulator library.
- Windows: Visual Studio C++ tools (MSVC linker, `rc.exe`, CRT redistributable); NSIS for installers.
- Android: JDK 17+, the Android SDK (`ANDROID_HOME`), NDK 28.2.13676358 and `rustup target add aarch64-linux-android x86_64-linux-android` (64-bit only; see [Android](android.md#build-and-launch)). `ANDROID_NDK_HOME` overrides SDK discovery and must point to that exact NDK; installing it with SDK Manager does not replace an inherited runner path. `xtask` downloads a pinned Gradle 9.9.0 into `$XDG_CACHE_HOME/paseo-packaging`; there is no Gradle wrapper in the repository.

Fetch Rust dependencies with `cargo fetch --locked`. Desktop builds require their target host; `xtask` chooses the GUI target itself and rejects `CARGO_BUILD_TARGET`.

Default CI runs Rust workspace tests (including GUI crates) and isolated native Tailscale control/DERP regressions on one Ubuntu x64 runner; it does not build release artifacts or repeat tests across platforms. Packaging and deployment workflows run only on explicit manual dispatch.

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

`cargo xtask build-daemon` and `cargo xtask dev app` default to `dev-fast`: optimization level 1, incremental compilation, 256 codegen units and no LTO. Use `build-daemon --profile debug` or `dev app --profile debug` for unoptimized debugging. `build-gui` still defaults to `release`; `--profile dev-fast` selects the iterative profile explicitly. Release keeps thin LTO with one codegen unit per crate, except four for the daemon and GUI crates. None of these profiles grants publication identity: `PASEO_NATIVE_RELEASE=1` remains a separate validated choice; see [release](release.md).

`paseo daemon status` reports daemon liveness without running external provider/model discovery.
Its `providers` list is a cached observation (`providersCached: true`); `available` is `null`
until availability is known, with catalog `status` and `fetchedAt` retaining context.
The CLI's 1500ms status RPC deadline is unchanged. Use the existing
`list_available_providers_request`, `provider_diagnostic_request` or catalog refresh for
fresh provider diagnostics instead of treating basic status as a fresh provider probe.

## Build cache and retention

Commands launched by `xtask` share Cargo outputs across worktrees of the same Git common directory. They use `$XDG_CACHE_HOME/paseo-build/v1/<repository-hash>/generation-<epoch>/target` (or `~/.cache` without `XDG_CACHE_HOME`), rotating generations every 30 days. A source archive without Git uses its canonical source directory as its cache identity. Cargo still fingerprints compiler, target, profile, features and source paths; switching worktrees does not promise reuse of workspace crates whose absolute resource paths changed.

An explicit `CARGO_TARGET_DIR` wins and is never adopted or pruned by housekeeping. It must be outside the internal managed cache tree; leave it unset to use that tree. The initial `cargo xtask` bootstrap and direct `cargo build` retain Cargo's own target-directory configuration; the shared directory applies to the product builds launched inside `xtask`. CI sets an explicit checkout target to retain its existing Rust-cache integration.

Mutable output stays source-bound: `.dev/build/native/<profile>` contains the staged daemon executable, skills and provenance; `.dev/build/gui` contains GUI bundles and mobile/Gradle staging. Compilation and copying out of a shared target hold a cache lease. Running a staged GUI releases that cache lease but pins its checkout bundle, so another worktree can build without replacing the running client's resources. Development launchers also pin native staging. Do not point a direct Cargo invocation at the managed internal directory; use `CARGO_TARGET_DIR` for operator-owned caches.

Optional accelerators require existing tools and never install them:

- `PASEO_SCCACHE=1` selects `sccache` from `PATH` unless `RUSTC_WRAPPER` is already set. It sets child `CARGO_INCREMENTAL=0` unless explicitly supplied: sccache does not cache incremental Rust compilations. Choose this mode for compiler-cache reuse rather than local incremental rebuilds; its external cache is not managed here.
- `PASEO_FAST_LINKER=1` selects `clang` with `mold`, or `ld.lld` when mold is absent, for native Linux GNU targets only. Explicit linker/target environment variables, `RUSTFLAGS`, or `CARGO_ENCODED_RUSTFLAGS` take precedence. The target-scoped flags do not replace Android NDK or Apple linkers. Missing requested tools fail with a prerequisite error.

`cargo xtask housekeeping` previews removal of old, marked cache generations, oldest first, toward a **20 GiB soft budget**. `--max-bytes N` changes that budget; `--apply` performs the listed removals. Current generations, active leases, and anything used within seven days are retained even above budget. No automatic deletion occurs during builds, and legacy checkout targets, explicit targets, downloads, artifacts, and external compiler caches are outside this command's ownership.

Add `--staging` to include this checkout's marked `.dev/build` tree only when unused for seven days and no build/development lease is held. It never removes `.dev/paseo-home`, `.dev/gui-state`, or `artifacts/`. Review the dry run first; recreating deleted generations requires compilation. CI upload retention is documented in [release](release.md).

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
cargo xtask build-gui android             # arm64 APK
cargo xtask build-gui android --arch universal --format aab
cargo xtask build-gui ios
cargo xtask package-desktop
```

Only run the desktop target matching the host OS. iOS requires macOS. `cargo xtask build-gui ios` without `PASEO_IOS_PROVISIONING_PROFILE`/`PASEO_IOS_SIGNING_IDENTITY` emits an explicitly unsigned device app archive, not an installable IPA. Android output without a release key is marked debug-signed. Build descriptors and client artifacts live under `artifacts/native/`; staged bundles, Android work directories and Gradle state live under `.dev/build/gui/`. See [release](release.md) before producing distribution artifacts.

Every build passes the client's identity at compile time (`PASEO_VERSION`, `PASEO_RELEASE_CHANNEL`, `PASEO_UPDATE_REPOSITORY`, `PASEO_BUILD_NUMBER`, `PASEO_BUILD_SOURCE_SHA`). Staged desktop builds (`build-gui`, `dev app`) also set `MAKEPAD_PACKAGE_DIR=makepad`: Makepad then opens resources at `makepad/<crate>/resources/…` relative to the working directory on Linux and Windows (the client switches to its executable directory at startup) and relative to `Contents/Resources` on macOS (`MAKEPAD=apple_bundle`). Android and iOS always use `makepad/` inside the APK assets or app bundle.

On Windows, the GUI uses the Windows subsystem and launches the bundled `daemon ensure` command with `CREATE_NO_WINDOW`; redirected standard streams alone do not prevent a console window. Managed executable probes and background restart launchers use the same flag, while foreground CLI launches keep their console. The daemon runs independently after the desktop exits.

The release's Windows smoke job checks that the daemon CLI has no console window, sees EOF on stdin, and can write captured stdout and stderr.

Makepad platform, draw, widgets, vector and shader compiler are patched through `packages/gui-platform/vendor/` and root `[patch.crates-io]`. Each crate retains its upstream license and `VENDORED.md` provenance. Platform carries Android API 24 compatibility, native lifecycle/input fixes, the packaged live-watcher guard and the `windows` 0.61 port; draw carries Unicode line breaking and runtime font families; widgets carries scrollbar and caret behavior and decodes images with `image`. Draw and vector use the standard `rustybuzz` 0.20, `ttf-parser` 0.25 and `resvg` 0.45 the client already resolves, rather than Makepad's forks or a second resvg stack. Keep changes in the existing copy, not a second framework fork. The source watcher is disabled when `MAKEPAD_PACKAGE_DIR` is set: staged builds have no live reload; plain `cargo run` keeps it.

`zbus-secret-service-keyring-store` is patched the same way so the Linux Secret Service store uses the shared Tokio-only zbus without its async-io executor; see its `VENDORED.md`.

Additional narrow patches share existing dependencies: `packages/terminal/vendor/portable-pty` uses the workspace's nix/winreg versions, `packages/gui/vendor/tiny-skia` uses png 0.18, and `packages/daemon/vendor/node-semver` omits miette diagnostics without replacing its Node-compatible range parser. Each retains its license, upstream manifest and `VENDORED.md` provenance; remove a patch when upstream supports the same dependency graph.

Packages copy the `resources/` directory of every Makepad-aware crate in the `paseo-gui` dependency closure, minus the reviewed `excluded` files in `packaging/resource-notices.json`. A font without a digest-bound, revision-pinned license entry fails the build; so does a Cargo dependency without license text (add a revision-pinned entry to `packaging/cargo-notice-supplements.json`, never an SPDX template) and a natively built library whose pinned source changed without a reviewed `packaging/native-notices.json` entry (see [release](release.md#distribution-requirements)).

Embedded Tailscale builds use `<target>/paseo-tailnet/` for verified module downloads, Go cache and target-specific libraries. `xtask` packages the DLL/dylib/so beside the desktop executable, the Android library in each ABI's JNI directory, and an iOS static archive. Go dependency licenses and the Go runtime license join the package notices. `PASEO_TAILSCALE_STATIC_DIR` is set by the iOS builder; it is not a runtime search path. A raw `cargo run` needs the matching native library beside its executable to enable Tailscale; use `build-gui` for a complete bundle.

Run native engine regressions from `packages/tailnet/native` with `GOTOOLCHAIN=local GOMAXPROCS=2 go test -mod=readonly -p=2 . ./tsnet`. They use isolated local control/DERP servers, not a real account. Linux runtime key storage requires an unlocked Secret Service and a session bus; missing secure storage is an explicit feature error, never a plaintext fallback.

### Fonts

The theme's five text families (`THEME_FONT_REGULAR`, `_BOLD`, `_ITALIC`, `_BOLD_ITALIC`, `_CODE` in `packages/gui/src/theme_makepad.rs`) retain their bundled fonts. Startup and appearance/preference changes resolve the UI/mono preferences on a worker through `paseo_gui_platform::appearance::system_fonts`:

- Proportional roles prefer the OS UI face and an eager Hangul face, then the bundled chain. The eager lookup is a startup optimization, not a list of supported scripts.
- Code keeps the requested mono face or Jetendard first, preserving existing Latin and Hangul metrics; bundled emoji remains available.
- When either UI or background layout encounters an uncovered glyph, a shared queue requests an installed font for that Unicode character and style. Discovery runs off the UI thread and appends validated faces without replacing glyphs already covered by the family. No script whitelist or new bundled language pack is required.
- Native discovery preserves collection indices and variable-font axes. The renderer requires an eligible sfnt outline face; system font availability is not a promise of complete bidirectional paragraph layout.

Positive and negative discoveries belong to the current appearance/preference epoch. Refreshing it permits newly installed or changed fonts to resolve, including a different file under the same family name. Pending discovery and per-family Unicode deduplication are bounded; overflow retries without permanently excluding characters, and unavailable characters eventually stop requesting work.

Completed discovery invalidates synchronous and asynchronous layouts, typography-dependent caches and wrapped diff row heights. System faces get ascender/descender adjustments to fit the primary face's line height. The first frame draws immediately with the bundled fonts; when discovery completes, text is laid out again with the system fonts. `system fonts: …` and `system font fallback: …` log the chosen sources. Accessibility text scale still owns size changes.

## Source ownership

- Rust daemon: `packages/daemon/src`; CLI commands in `cli/`, providers in `providers/`, installation and process ownership in `daemon_control/`.
- Native client: `packages/gui` (Makepad shell, theme, common widgets, `src/features/*`), `packages/gui-core` (host connections, directory synchronization, persisted client state), `packages/gui-platform` (desktop activation, updater, notifications, pickers, audio, Android/iOS sources).
- Embedded Tailscale: `packages/tailnet` (Rust lifecycle, portable Go/C engine, maintained tsnet source and encrypted state), OS credential providers in `packages/gui-platform`, transport and host policy in `packages/client` and `packages/gui-core`.
- Terminal emulation: `packages/terminal` (`paseo-terminal`, Ghostty VT for daemon screens, terminal tabs and logs) and its vendored `libghostty-vt-sys` build-script patch under `vendor/`.
- Native build, packaging and release: `xtask/src`; packaging resources, the Android Gradle project and vendored Makepad Android Java: `packaging/`.
- Rust SDKs: `packages/client`, `packages/protocol`, `packages/plugin`, `packages/highlight`; relay: `packages/relay`.

Use Rust instrumentation for daemon and client work. Browser/CDP and React profilers do not profile the native client. A development mock provider can test controlled UI states but is not evidence of real provider interoperability.

Platform completions share Makepad's action batch with unsolicited events.
This delivery is not a redraw signal: only the instance with a pending request
for the completion's token may consume the reply and invalidate its UI. Match and
remove that token before converting the payload, scheduling work, or redrawing.

## Scripts, logs, and diagnostics

Workspace setup and services remain daemon-side shell/PTY operations. The client exposes start/stop and **read-only logs**; interactive shells are separate Terminal tabs ([terminal activity](terminal-activity.md)).

Inspect the selected home's `daemon.log` and native CLI status. Keep diagnostic captures free of credentials and unrelated user state. A script or provider process is owned by the host daemon, never the mobile client.

Set `PASEO_FRAME_TRACE=1` in the native client's environment to write a one-second
frame summary to stderr, including when the window is idle. `event_ms` excludes
Makepad `Draw` dispatch (`draw_ms`); on X11, `render_ms` separately measures native
rendering and presentation. Timer totals use the caller that started each timer.
Layer entries are keyed by the `Layer::begin` call site and report repaint/replay
counts. Redraw and next-frame entries report requests, which may coalesce into
fewer actual draws or events. This is wall-clock timing, not a CPU profile.
Without the opt-in, no reporter starts and the hooks do not sample clocks or
collect data.

### Indicator redraws

Draw time-dependent GUI indicators through `common::clock`. Register a
painted area at its next visible content boundary (`register_until`) or
use `register_every` for finite visual steps; never register while measuring.
The clock owns one timeout. Status rings tick at 8 Hz; transient loading
motion can tick at 30 Hz. Repeating motion is capped at 4 Hz when unfocused
and stops when the app is backgrounded or the indicator is detached, clipped
out, or fully covered. Record guaranteed opaque cover rectangles with
`occlude` in the surface's retained draw list;
translucent scrims and rounded corners are not covers.

Use the painter's retained spinner for rotating arcs: it changes only
the shader instance and does not invalidate the widget or its layout.
When an indicator must change text or geometry, retain it separately
from expensive layout (e.g. a timeline row's Markdown); expire copy
feedback once, not on every frame. Use `NextFrame` only for actual
interaction or bounded layout/motion such as an opening sheet.

## Checks

See [testing](testing.md) for Rust, GUI, SDK, and real-runtime checks. Run the appropriate checks only when authorized by the assignment. A CI job definition, compilation, or a unit test is not an actual platform smoke test.
