# Testing

Test observable behavior at the boundary that owns it. Rust unit tests, Dart/widget tests, wire integration tests, actual provider runs, native app interaction, packaging, and signed distribution each prove different things. None is a substitute for all the others.

## Commands and ownership

From the repository root:

```sh
cargo check --locked -p <changed-crate> --all-targets
cargo test --locked -p <changed-crate> --lib <test-name-filter>
cargo xtask flutter test test/<file>_test.dart
cargo xtask flutter analyze
```

Use bounded package/test filters locally; do not run the complete workspace or Flutter suite on the development host. CI owns `cargo test --workspace --locked`, the full Flutter suite and platform matrices.

Rust tests are colocated with the daemon, SDKs, relay and `xtask` modules, with protocol integration tests under each crate's `tests/`. Flutter tests live in `packages/flutter_app/test`; actual app integration runs must use the Flutter client, not a browser rendering of the old UI. Native plugin checks must exercise executable IPC. Packaging/tooling tests validate artifact contracts, not the actual behavior of every target OS.

Do not run all heavy suites concurrently. Select a bounded relevant check during an explicitly authorized verification pass. Preserve errors and failing output; do not suppress a real service failure or silently relabel a mock as end-to-end.

## Required native smoke boundaries

Use a disposable `PASEO_HOME`, checkout, endpoint, and model cache where applicable. Never stop, reset, install over, or reuse the user's installed daemon/application for a migration smoke.

- Launch the real Rust daemon and native Flutter client; connect, authenticate, and restore persisted host state.
- Exercise host → directory → session navigation, independent collapse, empty directories, filtering, and same-path separation across hosts.
- Browse a remote host's folders; create a session in the selected directory; rename and reopen it.
- Stream provider output, answer permissions, reconnect, restore history, and verify failures remain visible.
- Render Markdown, edit/read back source, and open a large summary-first diff without eagerly loading every patch.
- Start/stop workspace setup and service scripts; inspect read-only logs. Verify the backend PTY survives client presentation changes.
- Exercise schedules, MCP, connected-host forwarding, profiles, and native CLI operations at their real protocol boundaries.
- Exercise desktop start/idempotence, worker restart, supervisor ownership, owned shutdown, keep-running policy, and preservation of an unowned daemon. A direct Rust unit test is not a Dart-to-desktop-host IPC test.
- Exercise native tray/menu behavior on the actual desktop, and bundled Neovim editing as a real process. Compilation alone does not prove either.

The client deliberately excludes interactive terminal UI, embedded browser/automation, web, and executable React UI plugins. Do not keep obsolete Electron/Metro/browser smoke commands as the acceptance path for the native product.

## Retained Linux desktop regression

`packages/flutter_app/integration_test/desktop_lifecycle.dart` exercises the real native primary user window, additional engines, managed daemon and editor. `tools/native-desktop-smoke/run.py` and `bus.py` provide isolated Linux desktop/session services and collect results. Prerequisites are Linux, Xvfb, `dbus-run-session`, Python 3 with `python3-dbus` and `python3-gi`, Gdk 3 introspection, the Flutter Linux toolchain, and a complete built native runtime directory. Do not replace that runtime with a bare Cargo executable missing its assets.

From `packages/flutter_app`, with consistent `ANDROID_HOME`/`ANDROID_SDK_ROOT` if Android tooling is also configured:

```sh
flutter build linux --debug --target=integration_test/desktop_lifecycle.dart
python3 ../../tools/native-desktop-smoke/run.py --runtime /absolute/path/to/native-runtime-directory
```

The runner uses private `HOME`/`PASEO_HOME` and the default renderer, without a Node product runtime. It waits for actual widgets before reporting case state. Check single-engine startup, cold-start project routing, primary close/Quit cancellation, hidden-primary reuse from another window, and secondary teardown without losing the primary. Its three lifecycle cases cover tray hide/activate/menu Quit, last-window close with no tray panel (a real watcher without a host), and GNOME logout while the user window is hidden. Assert managed-daemon shutdown in every case. Xvfb does not establish hardware-GPU performance; software-renderer overrides are diagnostic evidence, not a substitute for default-renderer coverage.

Screenshot-ready phases wait for a rasterized frame, not just a completed widget build. A compact tool-detail capture checks a long, label-only Wait command without executing it. Screenshot phase acknowledgements keep the user window alive until capture completes, preventing a race with Quit. The visual assertion compares the same window immediately before hiding and after restoration: matching pixels establish that hide/restore preserved its rendered contents, not conformity to a style golden.

### Vendored Linux multiwindow ownership

The project vendors `desktop_multi_window` **0.3.1+paseo.1**, retaining its license and upstream macOS/Windows implementations. The Linux patch gives each Flutter engine a private `GdkDisplay`/`EGLDisplay`, scopes GL bindings during creation, unbinds EGL before GTK's before-paint phase, and keeps the display alive until engine teardown finishes. Flutter 3.47 terminates an engine's EGL display during teardown; sharing that display across live engines invalidated another window's rendering resources. A captured failure reached GTK's next-frame `gdk_gl_context_make_current` and libGLX `BadAccess` after deferred EGL teardown. The regression must cover closing and restoring actual windows, not merely engine construction or a mocked channel response.

## Recorded 0.13.1 Rust cutover smoke

The local Linux cutover pass intentionally omitted broad tests, lint, benchmarks and app reinstallation at the user's request. Observed boundaries:

- `cargo build --locked --offline --workspace --all-targets -j 2` completed, including native SDK, plugin example and regression-test compilation. Tests were not executed. The daemon retained one pre-existing unused-variable warning.
- A disposable native daemon started with only `git` exposed on its private `PATH`, with no Node executable. The native CLI connected through the extracted Rust client SDK and returned the empty agent list with exit code 0 (`ev_e8eca15be4e1f8edf07970e71c0d95e2317cbfd405d8acf7fba5c0884ffc9118`).
- A native highlighting consumer recognized a Rust keyword while preserving Korean source text (`ev_4740030ce0921dbdf7ea70b18b55c5696e22c645b94cc7921fe65f3091805ea0`).
- Two real WebSocket peers exchanged opaque binary frames in both directions through the native relay (`ev_1d239eea94c3178796bd3617a3c24bdfbdaa50db9c0dc7f96beac0063a3eead0`).
- With plugins explicitly enabled in that disposable daemon, a Rust SDK consumer installed the native workspace-inspector executable and invoked `file.inspect` through the real plugin host. The result preserved Korean preview text and reported 21 bytes, two lines and three words, without Node on the private `PATH` (`ev_066acc9e74494cd06c9a37ecdff15ca09c979e6cf613e7e3dae66745214aa7a9`).
- The native release planner emitted all eight targets without publishing (`ev_de840adeffb7b887037083628babdc9cf853d298f4bf29ef7dcbf1564f92a753`). This smoke found and corrected the TOML document parser before the successful run.
- The native `paseo-xtask build-daemon` entrypoint completed runtime-asset, provenance and third-party-notice packaging. Its strict inventory check caught a duplicate attribution entry; the inventory was corrected without weakening validation.
- The grouped native tooling check passed 54 `paseo-xtask` tests and built its executable, including SDK pin/archive/native-ABI boundaries and packaged JSON permissions. Native ARM64 SDK assembly and installation still require the dedicated workflow; these local tests do not establish that result.
- A private runtime-staging smoke compiled a real executable → plugin → engine dependency chain with an absolute build-tree RPATH. The production bundler relocated it; after deleting the original library tree, the staged executable still returned the engine's value. The staged native CLI reported `paseo 0.13.1`, JSON metadata was readable by installed-product users, and vendored libraries were not copied or attributed as system packages (`ev_2837501f2440eb7141bbf8462d23e2e42eb6f9261a81f9ae12e2f19665da8d66`).
- A private Git index checked out the two frozen OpenCode ABI bundles and V8 DateParser source with `core.autocrlf=true`; their bytes and digests remained identical to the committed LF sources (`ev_a953405b1d10ffb98180b54f5694c94541410873c7399f0059086687bf4f0fd5`).
- A later local workspace test run reached 596 passing daemon tests but failed `port_probe_reservations_rollback_refresh_and_release` because its selected service port `10240` was unavailable. That run is not a full-workspace pass. The preceding GitHub CI run `41291036373` passed all 597 daemon library tests on Linux x64 and ARM64, including staged-event FIFO ordering and failure/close recovery; its highlighting and platform-attribution jobs still required the subsequent repairs.
- A separate native SDK consumer verified JSX/TSX tag names versus comparison operators and repeated Unicode Vue string literals with exact UTF-8 ranges (`ev_a7e554d7daa68d7ed844558e0657c2209dfc1294acb82b4fad212aa18fbd7dae`).
- A private native relay process accepted real WebSocket clients, delivered control/data replacement code `1008`, preserved bidirectional routing, echoed peer close frames before clean EOF, and delivered dependent-client close code `1012` (`ev_4e47ab16fcf20212335750b882ca30e3f0b623dc0f0bd76377e1c6ba97895600`). No production service was restarted.
- The final local `cargo test --locked --offline --workspace --all-targets -j 2` passed **781 tests** across 20 suites; one subprocess-only fixture remained intentionally ignored. This includes all 597 daemon tests and 12 relay socket scenarios. The port fixture runs in its own process to avoid concurrent PTY forks inheriting its probe sockets; the production allocator is unchanged. A malformed example-test manifest was corrected to serialize its executable name as a JSON string. A separate intermediate compile exhausted local storage; only incremental caches of crates introduced by this migration were removed before the successful unified run, preserving source, backups and built binaries.

Runtime smoke observations remain narrow; passing native tests does not establish cross-platform, performance or release certification. The local toolchain lacked the Wasm target and binding generator; edge-target compilation belongs to its configured CI lane.

## Recorded 0.13.0 native-client migration evidence

The following is **historical local Linux evidence for the 0.13.0 native-client migration**, not verification of the subsequent Rust SDK/toolchain cutover, a release certification or a statement about another OS:

| Boundary                                | Observed result                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust daemon checks                      | 633 unit tests and one doctest passed (634 total)                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Retained JavaScript workspaces          | `npm run test:node` passed 1,994 tests, with four skipped; repository Node script checks passed 34 tests; final retained-workspace typecheck passed, including worker, examples and SDKs                                                                                                                                                                                                                                                                                                     |
| Native Flutter + Rust UI smoke          | Ten-flow smoke passed; four real native screenshots reviewed: host tree, Markdown, 432 KB diff, compact sidebar                                                                                                                                                                                                                                                                                                                                                                              |
| Provider and extension interoperability | Real Rust adapters/daemon passed Claude, ACP and MCP scenarios against controlled provider-protocol boundary fixtures; these were not paid live Claude/ACP LLM executions. Optional external-Node backend-extension lifecycle was exercised separately                                                                                                                                                                                                                                       |
| Desktop lifecycle IPC                   | Actual Dart `DesktopDaemonService` → Rust desktop-host passed managed start, idempotence, worker restart, supervisor ownership, owned stop, keep-running, and unowned-daemon preservation                                                                                                                                                                                                                                                                                                    |
| Native runtime assets and CLI           | Normal-entry Linux release build via `node scripts/build-flutter.mjs linux` passed with a bundled native runtime and no publication identity. Packaged executable hash matched provenance; `PATH=/nonexistent` version execution returned `paseo 0.13.0`. Reserved/Unicode CLI deep-link IDs and spaced project paths passed a separate launcher-boundary check                                                                                                                              |
| Normal-entry Linux release UI           | Actual release `paseo_sessions`, with no test Dart entry or runtime override, passed in private Xvfb/D-Bus and private homes using `/usr/bin:/bin` with Node absent. The supervisor executable resolved to bundled `runtime/paseo`; daemon status was reachable, desktop-managed and v0.13.0. Reviewed production screenshot showed the full Sessions shell and This computer Live without an error banner. Native tray Quit Paseo exited the app with status 0 and stopped its owned daemon |
| Flutter static/unit checks              | Final Flutter analyze reported no issues; the complete Flutter test run passed 15 tests                                                                                                                                                                                                                                                                                                                                                                                                      |
| Android compilation and payload         | Final Linux-hosted ARM64 release-mode wrapper build passed with Flutter 3.47.5, compile SDK 37.0 and AGP 9.1.1. ZIP enumeration verified only `arm64-v8a` native libraries, including `libapp.so` and `libflutter.so`; `apksigner verify` passed with Android Debug signing. The corrected APK is 37,410,467 bytes (37.4 MB decimal). No installation, device execution or publication is claimed                                                                                            |
| Native editor                           | Actual packaged Neovim delete, undo, Korean insertion and `:w` passed                                                                                                                                                                                                                                                                                                                                                                                                                        |
| Native desktop lifecycle                | Latest retained debug integration build and portable default-GPU regression passed all three cases after the GLES `eglBindAPI` correction, with actual `PaseoShell` readiness and no renderer overrides. Earlier phase-acknowledged captures matched the same window's pixels before hide and after restoration. Earlier software-renderer and Skia-GPU runs also passed                                                                                                                     |

Recorded local proof locations include:

- `/tmp/paseo-native-smoke-proof-RQuBYN`: ten-flow native client/daemon smoke.
- `/tmp/paseo-dart-lifecycle-proof-EGYKDJ`: final recorded Dart-to-desktop-host lifecycle run.
- `/tmp/paseo-native-package-proof-i1Dilu/verification.json`: relocated executable hash and native version execution without Node on `PATH`. Reserved/Unicode deep-link IDs and spaced project paths were verified separately in the recorded `desktop_cli.py` stdout (job `bg108`), not by this JSON file.
- `packages/flutter_app/build/linux/x64/release/bundle`: normal-entry local Linux release build, with packaged runtime SHA-256 `49461ebb63a8a5cebed6756e78664f36e4c557100c689e523281920382bdc256` matching provenance, native version execution with `PATH=/nonexistent`, and release identity false. These hash/version observations are build evidence; UI execution is recorded separately below.
- `/tmp/paseo-production-bundle-proof-ifgtgr7c`: normal-entry release UI, bundled-runtime executable identity, reachable desktop-managed daemon, reviewed `production.png`, native tray Quit and owned-daemon shutdown. The run used private Xvfb/D-Bus, `HOME` and `PASEO_HOME`, no `PASEO_DESKTOP_RUNTIME` override, and `/usr/bin:/bin` with Node confirmed absent. Final artifact verification JSON is retained alongside the screenshot.
- `/tmp/paseo-native-extension-smoke-JRISnz/attempt-1793297454710918880/evidence`: optional extension lifecycle, with a native-only default process tree.
- `/tmp/paseo-native-desktop-proof-cvkj2q7m/tray/events.jsonl`: actual Neovim editing.
- `/tmp/paseo-native-desktop-proof-gp71uas4`: default-GPU portable regression, three of three lifecycle cases passed; `bus.jsonl` records `restored-pixels-match-before-hide` from phase-acknowledged captures of the same window.
- `/tmp/paseo-native-desktop-proof-yrr4uyir`: latest retained integration build and all three default-GPU lifecycle scenarios passed after the GLES `eglBindAPI` correction.
- `/tmp/paseo-native-desktop-proof-3hw3cdv6/results.json`: all three software-renderer lifecycle cases, including real no-panel behavior.
- `artifacts/flutter/Paseo-Sessions-Flutter-0.13.0-android-arm64-debug-signed.apk`: verified ARM64-only native payload, SHA-256 `11a5c8ddca90e1dfa46e57474086dcf81099a10b2995a1864dc068df2f3d47b9`; verified Android Debug certificate SHA-256 `fac61745dc0903786fb9ede62a962b399f7348f0bb6f899b8332667591033b9c`.

These are temporary local investigation artifacts, not published downloadable release evidence. Each proves the recorded run, not unexecuted later edits.

Residual upstream GTK 3 signal-disconnect cleanup warnings and Flutter implicit-view/messenger teardown warnings were observed. They were not suppressed; none of the three recorded lifecycle flows crashed. Passing these flows does not establish warning-free teardown.

The empty-`PATH` check covers the native CLI's version command only. A full GTK UI still needs system helpers: removing every command from `PATH` prevented the system glycin PNG loader from finding `bwrap`. For a Node-absent UI check, preserve required system paths and verify that Node is absent there; do not treat the deliberately broken system-helper environment as an application packaging failure.

At the time of this record there was **no actual Windows, macOS, Android-device, iOS or Wayland execution**. Android ARM64 compilation passed, but no production signing, installation or mobile runtime test is claimed. Provider fixtures do not establish paid live LLM execution. No release was published, and the user's existing app, daemon and OMP were not installed over or restarted. Do not convert an unexecuted check into a passing claim.

## CI definitions versus execution

`.github/workflows/ci.yml` defines native-daemon Linux, Windows and macOS lanes, each on x64 and arm64, plus Flutter lanes. Native-daemon jobs run `cargo xtask build-daemon` and `cargo test --workspace --all-targets --locked`. Quality checks use Cargo formatting, scoped xtask tests and workspace Clippy; the Rust Cloudflare relay has a separate Wasm build/check lane. Retired TypeScript server/integration/CLI jobs are not the product runtime acceptance path. `.github/ci-paths.yml` and CI Recovery route the native jobs. The historical evidence above predates the full Rust tooling/SDK/plugin cutover and is not verification of this checkout.

Linux/Windows ARM64 Flutter jobs consume the reserved SDK release defined by `.github/flutter-sdk.json`; they never assemble the SDK as part of an app check. Pin changes route Flutter checks, and shared setup/SDK workflow changes route normal CI checks. The manually dispatched **Flutter ARM64 SDK** workflow assembles and validates both native SDKs separately; its seven-day intermediate artifacts and durable non-latest prerelease assets are SDK evidence, not app runtime evidence. See [release prerequisites](release.md#reusable-linux-and-windows-arm64-flutter-sdks) before interpreting a missing SDK asset as an application regression.

A matrix entry proves intended routing only. Report its actual run ID, platform, revision, result and logs before calling that platform verified. The same rule applies to release/build workflows: an installer filename or generated manifest does not establish successful execution, signing, publication, or legal distribution readiness.

## Failure and security expectations

Every fallible action exposes pending, success, and error state. A failed save must retain the draft; a failed provider turn must retain the error; a failed directory listing must allow retry without selecting a different host. Tests should assert these transitions, not just absence of crashes.

Use bounded waits tied to observable conditions rather than arbitrary sleeps. Retain process identity when testing shutdown so an unrelated process cannot be signalled. Redact credentials and tokens from shared logs. A missing real credential or unavailable service is an explicit test limitation, not permission to substitute a fake result.
