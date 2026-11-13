# Development

## Toolchains

- Rust and Cargo for everything: the daemon/CLI, the native client (`packages/gui*`, Makepad 1.0.0), client/protocol/plugin/highlighting SDKs, relay and `xtask` tooling. `Cargo.toml` specifies the minimum Rust version.
- Zig **0.15.2** on every target. `paseo-terminal` links Ghostty's VT library, which the vendored `libghostty-vt-sys` build script compiles with `zig build` (`packages/terminal/vendor/VENDORED.md`). Put `zig` on `PATH` or set `ZIG` to the executable; `xtask` build commands stop before compiling when it is missing or outside Ghostty's accepted range (0.15.x, patch 2 or newer). A cold build `git clone`s Ghostty at the pinned commit into Cargo's `OUT_DIR` and lets Zig fetch Ghostty's package dependencies, so it needs network access. Offline, set `GHOSTTY_SOURCE_DIR` to a checkout of that commit and `GHOSTTY_ZIG_SYSTEM_DIR` to a pre-populated Zig package directory (passed to `zig build --system`).
- Node, npm, Flutter/Dart and a TypeScript compiler are not Paseo build prerequisites. Separately installed agent CLIs retain their own runtime requirements.
- Linux desktop: `pkg-config` and the X11, Xcursor, GLX/EGL, ALSA and PulseAudio development packages Makepad links, plus Wayland development files for native file dialogs (Ubuntu: `libx11-dev libxcursor-dev libglx-dev libgl-dev libegl-dev libasound2-dev libpulse-dev libwayland-dev`), and `patchelf`: `build-gui` rewrites the RPATHs of the bundled daemon runtime. Packaging adds `dpkg-dev`, `xdg-utils` and the runtime-loaded `libxss1 libsm6 libice6 libfontconfig1`.
- macOS: Xcode command-line tools (`clang`, `codesign`, `iconutil`). iOS also needs full Xcode (`actool`, `devicectl`, `simctl`) and `rustup target add aarch64-apple-ios` (plus `aarch64-apple-ios-sim` for simulators). Simulator builds need an Apple-silicon Mac: Ghostty ships no x86_64 simulator library.
- Windows: Visual Studio C++ tools (MSVC linker, `rc.exe`, CRT redistributable); NSIS for installers.
- Android: JDK 17+, the Android SDK (`ANDROID_HOME`), NDK 28.2.13676358 and `rustup target add aarch64-linux-android x86_64-linux-android` (64-bit only; see [Android](android.md#build-and-launch)). `ANDROID_NDK_HOME` overrides SDK discovery and must point to that exact NDK; installing it with SDK Manager does not replace an inherited runner path. `xtask` downloads a pinned Gradle 9.9.0 into `$XDG_CACHE_HOME/paseo-packaging`; there is no Gradle wrapper in the repository.

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

`cargo xtask build-daemon` and `cargo xtask dev app` default to `dev-fast`: optimization level 1, incremental compilation, 256 codegen units and no LTO. Use `build-daemon --profile debug` or `dev app --profile debug` for unoptimized debugging. `build-gui` still defaults to `release`; `--profile dev-fast` selects the iterative profile explicitly. Release keeps thin LTO and one codegen unit. None of these profiles grants publication identity: `PASEO_NATIVE_RELEASE=1` remains a separate validated choice; see [release](release.md).

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

Makepad platform, draw and widgets are patched through `packages/gui-platform/vendor/` and root `[patch.crates-io]`. Each crate retains its upstream license and `VENDORED.md` provenance. Platform carries Android API 24 compatibility, native lifecycle/input fixes and the packaged live-watcher guard; draw carries Unicode line breaking and runtime font families; widgets carries scrollbar and caret behavior. Keep changes in the existing copy, not a second framework fork. The source watcher is disabled when `MAKEPAD_PACKAGE_DIR` is set: staged builds have no live reload; plain `cargo run` keeps it.

Packages copy the `resources/` directory of every Makepad-aware crate in the `paseo-gui` dependency closure, minus the reviewed `excluded` files in `packaging/resource-notices.json`. A font without a digest-bound, revision-pinned license entry fails the build; so does a Cargo dependency without license text (add a revision-pinned entry to `packaging/cargo-notice-supplements.json`, never an SPDX template) and a natively built library whose pinned source changed without a reviewed `packaging/native-notices.json` entry (see [release](release.md#distribution-requirements)).

### Fonts

The theme's five text families (`THEME_FONT_REGULAR`, `_BOLD`, `_ITALIC`, `_BOLD_ITALIC`, `_CODE` in `packages/gui/src/theme_makepad.rs`) are bundled fallback chains: IBM Plex, Noto Sans KR, Noto Color Emoji and Jetendard; code is Jetendard. At startup and whenever `uiFontFamily`, `monoFontFamily` or the OS appearance changes, `packages/gui/src/fonts.rs` resolves fonts on a worker thread through `paseo_gui_platform::appearance::system_fonts` and redefines those families in place (`Fonts::redefine_font_family`):

- Proportional roles: `[OS UI face, OS Hangul face, …bundled chain]`. An empty interface-font preference means the OS UI font; a CSS-like list (`'Pretendard', system-ui, sans-serif`) is tried in order, with `system-ui`/`sans-serif`/`-apple-system` naming the OS UI font.
- Code: `[user mono face, Jetendard]`; an empty code-font preference keeps Jetendard, whose Hangul spans exactly two terminal cells.
- Icon fonts (Lucide, Font Awesome) and emoji always stay bundled.

Font discovery is refreshed on appearance changes, not cached for the process lifetime: the same requested family can resolve to different files after the user changes desktop settings or installs a font. The GUI deduplicates unchanged requests between refreshes.

System faces get ascender/descender fudges so their line height equals the bundled face they precede, so authored `line_spacing` values look the same. Variable system fonts use the axis values the platform reports (`wght`, …). The first frame waits up to 300 ms for the worker; a slower result is applied on a later signal with one relayout. The log line `system fonts: ui=… mono=… faces=[…]` lists the files chosen. Only the OS accessibility text scale changes sizes; `uiBaseFontSize` keeps its 14/15 px defaults.

## Source ownership

- Rust daemon: `packages/daemon/src`; CLI commands in `cli/`, providers in `providers/`, process ownership in `daemon_control/`, desktop IPC in `desktop_host/`.
- Native client: `packages/gui` (Makepad shell, theme, common widgets, `src/features/*`), `packages/gui-core` (host connections, directory synchronization, persisted client state), `packages/gui-platform` (desktop host, updater, tray, notifications, pickers, audio, Android/iOS sources).
- Terminal emulation: `packages/terminal` (`paseo-terminal`, Ghostty VT for daemon screens, terminal tabs and logs) and its vendored `libghostty-vt-sys` build-script patch under `vendor/`.
- Native build, packaging and release: `xtask/src`; packaging resources, the Android Gradle project and vendored Makepad Android Java: `packaging/`.
- Rust SDKs: `packages/client`, `packages/protocol`, `packages/plugin`, `packages/highlight`; relay: `packages/relay`.

Use Rust instrumentation for daemon and client work. Browser/CDP and React profilers do not profile the native client. A development mock provider can test controlled UI states but is not evidence of real provider interoperability.

## Scripts, logs, and diagnostics

Workspace setup and services remain daemon-side shell/PTY operations. The client exposes start/stop and **read-only logs**; interactive shells are separate Terminal tabs ([terminal activity](terminal-activity.md)).

Inspect the selected home's `daemon.log` and native CLI status. Keep diagnostic captures free of credentials and unrelated user state. A script or provider process is owned by the host daemon, never the mobile client.

## Checks

See [testing](testing.md) for Rust, GUI, SDK, and real-runtime checks. Run the appropriate checks only when authorized by the assignment. A CI job definition, compilation, or a unit test is not an actual platform smoke test.
