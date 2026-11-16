# Release

The checkout version is **0.13.34**. Normal CI runs tests on one Ubuntu x64 runner. Building, packaging and publishing artifacts require an explicit manual workflow dispatch; commit and tag pushes do not start those operations.

## Distribution requirements

Native packages exclude speech recognition, voice conversation and TTS, including Sherpa, the speech ONNX runtime and espeak-ng. Generic effect-sound playback remains.

Rust tooling in `xtask/src/notices.rs` generates Cargo and bundled-asset attributions: the daemon closure in `runtime/THIRD-PARTY-NOTICES.txt`, and the client's `paseo-gui` closure for the packaged target plus every packaged font in the application's `THIRD-PARTY-NOTICES.txt` (desktop bundle, APK assets, iOS bundle). Both also attribute the natively built Ghostty library described below. Preserve those notices with the exact packaged artifacts and review the licenses of remaining bundled dependencies and platform runtimes. Generated attributions are not a compliance certification or a corresponding-source offer.

The revision-pinned objc2 supplements preserve the authors' complete upstream `LICENSE.md`, including its Apple SDK caveat. That file is a licensing declaration linking standard terms, not a fabricated replacement license. Frozen OpenCode ABI bytes and the V8 notice use explicit LF checkout attributes so Windows cannot silently change their validated content.

The makepad 1.0.0 crates are published without license files; `packaging/cargo-notice-supplements.json` carries the MIT text from the exact makepad commit each crate was published from. Supplements apply to registry sources only: a vendored, patched path crate such as `packages/gui-platform/vendor/makepad-platform` must contain its own license file. A Cargo dependency without license text fails packaging by name. Remove a supplement once its exact name and version leave `Cargo.lock`.

A crate published without `.cargo_vcs_info.json` (the arborium highlighting crates, `cesu8`, `fxhash`) cannot prove its source revision, so its supplement binds `checksum`, the registry SHA-256 `Cargo.lock` records for the downloaded `.crate`, and pins the license URL with `upstreamRevision`. Each arborium grammar crate carries arborium's own license plus the license of the upstream tree-sitter grammar at the commit its `arborium.yaml` names.

The daemon and the client statically link Ghostty's VT library, which `libghostty-vt-sys` compiles with Zig from a pinned Ghostty commit instead of shipping it in a Cargo package. `packaging/native-notices.json` lists the works inside that archive (Ghostty, the simdutf and Highway SIMD libraries, uucode and its Unicode data, Zig's compiler-rt and standard library) with license text pinned at upstream revisions, and binds the list to the crate version and the `GHOSTTY_COMMIT` its `build.rs` pins. Bumping either fails notice generation until the list is re-reviewed against Ghostty's `src/build/GhosttyZig.zig` and `SharedDeps.zig`. `libghostty-vt` 0.2.2 is published without a license file and uses a supplement; the vendored `libghostty-vt-sys` path crate carries its own `LICENSE`.

Fonts are third-party works with their own licenses. `packaging/resource-notices.json` binds packaged font digests to revision-pinned licenses; changed or unknown fonts fail packaging. Jetendard (OFL-1.1) supplies code glyphs, including two-cell Hangul, without redistributing Makepad's GPL-licensed Liberation Mono. Bundled fallback fonts and their notices remain available offline. Native system-font discovery supplements them by glyph coverage, including scripts outside CJK; code layout keeps its cell grid when a fallback face is used. Font discovery is not permission to redistribute installed fonts. Excluded resources and every `crate://…/resources/…` reference are checked during packaging.

## Release contents

| Target                        | Native artifact scope                                                                         |
| ----------------------------- | --------------------------------------------------------------------------------------------- |
| Linux, macOS, Windows desktop | Native Rust client, Rust daemon/CLI, native desktop integration and runtime assets            |
| Android                       | Native Rust remote client arm64 APK only; no daemon                                           |
| iOS                           | Native Rust remote client; no daemon; unsigned app archive unless a provisioning profile is supplied |
| Standalone daemon             | Rust executable with libraries, skills, notices and provenance                                |
| Container                     | Native daemon and runtime assets; no browser UI or Node executable in the default image       |
| Rust libraries                | Client, protocol, plugin and highlighting crates; source/package availability is separate from binary releases |

No Electron, Chromium, web client or embedded automation is shipped. Daemon PTYs back interactive Terminal tabs and read-only script/setup logs. Backend plugins use native executables; no Node worker or JavaScript compiler is packaged.

## Local build versus distribution identity

Use Rust/Cargo and each target's native toolchain. Build desktop artifacts on the matching OS; build iOS on macOS with Xcode. Build and release commands run through `cargo xtask`, without Node, npm or Flutter.

```sh
cargo fetch --locked
cargo xtask build-daemon                # dev-fast binary and runtime assets
cargo xtask build-daemon --release      # optimized local build, not publication
cargo xtask build-gui linux             # matching host only
cargo xtask package-desktop
cargo xtask build-gui android --arch arm64 --format apk
cargo xtask build-gui android --arch universal --format aab
cargo xtask build-gui ios
```

The default GUI release build uses optimized Rust; `--profile debug` and `--profile dev-fast` select development builds. Optimization is independent of publication: `--release` alone must not turn a local binary into an updater-enabled official build.

Published provenance is explicitly selected by `PASEO_NATIVE_RELEASE=1` together with the optimized build and validated `PASEO_UPDATE_REPOSITORY`, `PASEO_RELEASE_CHANNEL`, and `PASEO_BUILD_SOURCE_SHA`. The source SHA must identify the checked-out source. Standalone `cargo xtask package-daemon` requires that release identity; it is not a way to relabel an arbitrary local executable.

App, daemon, Tailnet and update-helper are independent components. `packaging/release.json` identifies the release collection; each compiled image retains its own version, source SHA, input fingerprint and ABI. Rust components use their actual dependency closure and separate canonical build invocations, with shared intermediate caches but no GUI feature leakage into the daemon. Common Rust libraries remain statically linked; the embedded Tailnet library has an explicit versioned C ABI. A changed shared dependency rebuilds its consumers, not unrelated Go images.

Client installers and build descriptors live in `artifacts/native/`: `Passession-<version>-<platform>-<arch>.<AppImage|deb|zip|exe>`, Android APK/AAB and iOS IPA/unsigned-app packages. Component delivery uses `Paseo-Component-<id>-<version>-<target>.tar.gz` (ZIP on Windows), `component-build-<id>.json` descriptors and `components-<target>.json` inventories. Native daemon payloads retain `paseo-runtime.json`, `bundle-provenance.json`, `desktop-runtime.json`, skills, dependencies and notices. Copying only an executable is not a supported installation.

Bundle provenance records the executable digest **after relocation**, because runtime patching can change its bytes. `sourceExecutableSha256` separately identifies the input executable. Check an artifact against its final digest, not against the pre-relocation source hash.

The Visual C++ ARM64 redistributable directory can also contain x64-view DLLs for emulated applications. Windows ARM64 packaging keeps the matching native CRT images (including Arm64X) and omits an x64-view image only after checking direct and delay-loaded imports of every bundled executable, DLL and selected CRT. A reference to an omitted image or any other unexpected CRT architecture fails packaging. The client and daemon executable architecture checks remain strict.

## Installed identity and the desktop update feed

The product is named Passession (window titles, shortcuts, installer, Add/Remove Programs, notifications, `Passession.app`). Executable names, package names and IDs keep the previous client's values, so `paseo open`, `paseo://` handlers, saved state and in-place updates keep working: `paseo_sessions` (Linux bundle, `/opt/paseo-sessions`, `/usr/bin/paseo-sessions`, Debian package `paseo-sessions`), `paseo_sessions.exe`, bundle ID `sh.paseo.sessions.desktop`, Android package `sh.paseo.sessions`, and the Windows data folder `%APPDATA%\Paseo\Paseo Sessions`. New Windows installs go to `%LOCALAPPDATA%\Programs\Passession`; an upgrade stays in the folder recorded under `HKCU\Software\Paseo Sessions` and replaces the old `Paseo Sessions` shortcuts. A macOS update keeps the installed bundle's path. The cargo binary `paseo-sessions` is renamed when bundled.

Linux packages also cover libraries the client opens at runtime, which `ldd` and `dpkg-shlibdeps` cannot see: Makepad's `libEGL`, `libX11` (idle time and close-to-tray), and the optional `libXss` (X11 idle time), `libSM`/`libICE` (session logout without a GNOME SessionManager), and `libfontconfig` (system-font discovery). The AppImage bundles these X11/XSMP/fontconfig libraries with their closure (`libraries::LINUX_DLOPENED`; the packaging host must have them installed); the Debian package depends on `libegl1`, `libx11-6` and `xdg-utils` and recommends `libxss1`, `libsm6`, `libice6` and `libfontconfig1`. Add any new `dlopen` library to both lists.

Client package feeds are `app-<channel>-<platform>-<arch>.json`, with `runtime: "app"` and `appId: "sh.paseo.sessions.desktop"`. There are no `flutter-*` aliases or old-runtime fallbacks: clients that only understand the old feed require a manual installer update. The daemon package feed remains `daemon-<channel>-<platform>-<arch>.json`. Component-aware clients use `components-<channel>.json` and verify immutable artifact origins and per-component provenance before downloading or activating a selection.

The NSIS installer upgrades in place: at the registered install location it first deletes every file listed in the previous installation's `.paseo-sessions-owned-files` inventory and removes emptied directories, then installs the new payload. Files the user created there are never deleted.

Whole-application swaps (macOS `.app` bundle, AppImage) move the installed application into a sibling `.passession-update-<operationId>/previous` directory and then move the verified candidate into place. That copy exists only while the update can roll back: an interrupted or unhealthy update restores it. Once the relaunched application and daemon identities are confirmed and the commit is durably recorded, the helper deletes that sibling directory and its own extraction staging; the install location is app-owned. A deletion failure is reported as a warning and does not change the committed result, and re-running a committed operation retries the deletion. Failed and rolled-back operations keep their recovery material. Native installers (deb, rpm, NSIS) leave no updater swap directory.

Desktop component updates use a private per-install immutable store and a locked, coherent active/trial selection. The installed entry point re-enters the selected app; a corrupt established selection fails closed rather than silently running the base installation. The helper verifies archive and extracted-tree digests, component ABI requirements and actual process identities. Helper-only updates do not restart the GUI or daemon; daemon-only updates keep the GUI alive. App or Tailnet activation requires a GUI restart, unsaved-document consent, authenticated handoff and observed healthy component identities. Until commit, the transaction journal holds the original selection: failed, interrupted or rolled-back activation restores it with its objects intact. A commit keeps no previous set (`previous` stays in the schema as `null`) and then best-effort deletes every store object, archive and receipt that neither the active selection nor a still-live original process uses; a deletion failure is a warning in the committed status and is retried by the next transaction. Each transaction also clears a `previous` set left by an older helper and prunes unreferenced objects before staging. Interrupted staging payloads, base installation files, service bindings and user data are never deleted or replaced.

Windows update security checks do not start PowerShell. The daemon and GUI share `paseo_client::windows_security` for WinTrust signature verification and owner/DACL checks; the Go helper calls the same Windows APIs and reads the install registry directly. A valid catalog signature takes precedence. A rejected catalog may fall back only to a valid embedded signature; catalog lookup failures remain errors. An image is unsigned only when no catalog lists it, WinTrust reports no signature and its PE header declares no certificate table. Signed installations require the same verified leaf certificate Subject DER. The daemon and helper retain unsigned-to-signed upgrades; GUI component verification retains its unsigned-to-unsigned policy.

An already-installed updater still uses its old verification code. If that version fails while loading PowerShell security modules, install the new official full installer manually; a candidate's native verification cannot repair the old updater before activation. On native Windows, run the client `windows_security::tests` and the helper `TestNativeAuthenticode*` and registry tests with a PowerShell 7 `PSModulePath`. Cross-compilation and Wine do not prove Windows trust-chain, catalog or signed-update behavior.

macOS app/Tailnet delivery preserves an intact signed app bundle. A new signed container records a package identity without relabeling unchanged compiled components. Android and iOS still install complete OS packages; unchanged raw images can be reused during packaging, but mobile apps do not self-replace native libraries outside the OS installer.

Settings → host → Version reports the selected host's installed and available versions/components, update operation and reconnection state. App-owned desktop hosts use the owner/helper handoff; standalone hosts use the durable daemon updater. Running work may delay activation, and unsaved desktop documents may require local approval. A request is not reported committed until the owning runtime observes the activated identities.

## Signing and mobile build numbers

`xtask/src/common.rs` resolves the build number from `PASEO_BUILD_NUMBER`, then `ANDROID_VERSION_CODE`, then `packaging/android/version.properties`; empty variables count as unset. Android enforces a minimum of `13000`. Confirm the next build number against the actual store history; do not infer store acceptance from a local version string.

Android release signing uses `ANDROID_KEYSTORE_PATH`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, and `ANDROID_KEY_PASSWORD`, or `packaging/android/key.properties`. Without a release key, APK and AAB output is explicitly named **debug-signed**, not store-ready. An AAB is produced by Gradle's bundle task, not by renaming an APK; it is a store upload bundle, not a directly installable package. Credentials are not part of this fork. The Android application ID is `sh.paseo.sessions`; the minimum SDK stays 24 (see [Android](android.md) for the Makepad compatibility patches that keep it).

iOS signs with `PASEO_IOS_PROVISIONING_PROFILE` (a `.mobileprovision` path) and `PASEO_IOS_SIGNING_IDENTITY` (a keychain code-signing identity): the profile is embedded, its application identifier, team, `get-task-allow` and `aps-environment` override the app entitlements, and the bundle is zipped as an `.ipa`. Without them, the build emits an explicitly **unsigned device `.app` archive**, not an installable IPA or App Store submission. The Xcode `PASEO_IOS_EXPORT_OPTIONS_PLIST` path no longer exists. macOS and Windows signing use the native packaging signing configuration; neither platform has been claimed executed in the local migration evidence.

Optional Firebase configuration for Android must match `sh.paseo.sessions`: `GOOGLE_SERVICES_FILE_SESSIONS` is staged into the Gradle project and restored afterward. iOS push uses APNs directly (device token exchanged with Expo, environment from the embedded provisioning profile) and does not link Firebase, so `GOOGLE_SERVICE_INFO_PLIST_SESSIONS` is no longer used.

## Workflow definitions and publication controls

`.github/workflows/desktop-release.yml` is the manual-only **Native Release** workflow despite its retained filename. Its persisted component plan compares target-specific input fingerprints with authenticated published inventories. It compiles only affected images, generates only affected packages and carries forward unchanged immutable references with their original identities. A no-change plan succeeds without native builds or publication. Ordinary CI similarly selects affected Rust packages and Go checks; test-only changes can require CI without rebuilding shipped images.

The complete target inventory has **eight targets**: Linux x64/arm64, macOS x64/arm64, Windows x64/arm64, Android arm64 APK and iOS arm64. Android publication uses `android-arm64`; emulator x64, universal APKs and AABs are not release targets ([Android](android.md#build-and-launch)). Only affected targets enter the native build matrix. Rust compilation provisions pinned Zig 0.15.2 and Ghostty inputs; Go-only work skips them. Packaging-only work consumes validated prior raw components without recompiling. Android uses platform 37 and NDK 28.2.13676358; macOS uses architecture-matched runners. Without signing secrets, Android remains debug-signed and iOS remains unsigned. This workflow does not publish npm packages or build the website.

`windows-com-smoke` runs the real COM/native-helper checks when a Windows app compilation is planned. Publication requires the applicable check to pass; it may be skipped when no affected Windows app is built.

The Windows release job builds and records artifacts without compiling the daemon's separate test harness. Run `preserved_runtime_copy_matches_the_runtime_digest` and `standalone_launcher_finishes_with_inherited_stderr_and_preserves_errors` manually on Windows when changing activation or rollback. They cover runtime file preservation and launcher completion, not a complete live-daemon update. Linux tests and cross-compilation alone do not verify Windows flush-handle permissions or handle inheritance.

Caches contain inputs and intermediate state, not release authority. Newly compiled artifacts are recorded against the resolved source; unchanged artifacts are reused only through verified immutable origin references and preserve their original source/input identities:

- `Swatinem/rust-cache` keeps third-party dependency builds (it drops workspace and path crates). Native jobs use `native-release-<platform>-<arch>`; Native Desktop Packages reuses `native-release-linux-x64`, because it builds the same release-profile graph with the same toolchain and cache-relevant environment. `source` and `publish` share `release-xtask`, and the COM smoke job uses `release-windows-com-smoke`. Cargo compares the variables build scripts track with its own environment, so `xtask` removes `SOURCE_DATE_EPOCH` (tracked by `cc`) and the `CARGO_PKG_*`/`CARGO_MANIFEST_*` identity that `cargo xtask` exports (ring tracks the patch version) from every Cargo command it starts. Otherwise each release recompiles ring, SQLite, the tree-sitter grammars and everything depending on them. Packaging tools still receive `SOURCE_DATE_EPOCH`.
- `zig-0.15.2-<os>-<arch>-<job key>-<hash of libghostty-vt-sys/build.rs>` holds `ZIG_GLOBAL_CACHE_DIR` (Ghostty's Zig packages), Zig's content-addressed local cache for that target (`$GHOSTTY_CACHE_DIR/zig-cache-<GHOSTTY_COMMIT>`) and the finished build's installed outputs (`$GHOSTTY_CACHE_DIR/install-<GHOSTTY_COMMIT>-<inputs>`). A warm build copies the installed outputs into `OUT_DIR` without checking the clone or running Zig; release runs with only Zig's cache restored still spent about 95–105 seconds in that build script on Linux, Windows and Android. `<inputs>` covers `build.rs`, the Zig release, host and target triples, optimize mode, CPU model and the `ANDROID_NDK_HOME`/`GHOSTTY_ZIG_SYSTEM_DIR` values; a runner image update alone does not rebuild the library. `build.rs` pins the commit, so its hash covers it. Building there instead of in `OUT_DIR` changes only absolute paths in the intermediate archive's debug info, which release binaries strip ([VENDORED](../packages/terminal/vendor/VENDORED.md)).
- `ghostty-src-<os>-<hash of build.rs>` holds the Ghostty clone (`$GHOSTTY_CACHE_DIR/ghostty-src`), one entry per runner OS shared by every job and workflow. `build.rs` needs it only when no installed outputs match, and reuses it only when `HEAD` is the pinned `GHOSTTY_COMMIT` and `git status --ignored` is empty; otherwise it reclones. Jobs on the same OS racing to save it report that another job is creating the entry.
- `go-<os>-<arch>-native-release-<platform>-<arch>-<hash of Go sources>` holds the Go module and build caches of one Native Release target. `actions/setup-go`'s own cache is keyed on the runner OS and `go.sum` only, so Linux, Android and CI shared one entry that kept its first saver's build cache; the Linux and Android Tailscale builds then recompiled every package (79 and 44 seconds in a release run, against under 12 on macOS and Windows).
- `native-packaging-<os>-<platform>-<arch>-<inputs>` exists only for Linux desktop (pinned AppImage tools, re-verified by SHA-256 before use; keyed on `xtask/src/assets.rs`) and Android (the unpacked pinned Gradle distribution and `~/.gradle/caches`; keyed on the `GRADLE_VERSION`/`GRADLE_SHA256` pins and the Gradle build files). The downloaded Gradle archive is not cached.
- Keys contain no run IDs. The Zig, Go, Rust and packaging entries fall back to the previous entry through restore keys when their inputs change. Rust caches save only from the default branch.

`xtask` runs in the dev profile; root `Cargo.toml` optimizes only its SHA-256 and ZIP/gzip crates in that profile. The bytes it writes do not depend on how it was compiled, and the release profile is unchanged.

Workflow artifact uploads are temporary: both Native Release and Native Desktop Packages retain them for **7 days**. Recover within that window or rebuild the exact authorized source; expired uploads cannot support `publish-only`. This retention does not delete published GitHub Release assets.

Android `record` and `verify` require matching release-profile metadata, source SHA, architecture and format; the metadata and artifact are both covered by provenance hashes. Archive validation checks APK versus AAB layout, bundled notices and the exact Makepad native ABI inventory against ELF headers. These structural checks reject a renamed APK and mislabeled native libraries; they do not replace Android signing verification, bundletool validation, installation or store acceptance.

For a reduced Windows x64, Linux x64 and Android release, select `platform=linux-windows-x64-android`; `platform=linux-windows-x64-android-macos-arm64` adds Apple Silicon macOS. The collection inventory covers exactly the selected targets, while the native matrix contains only affected ones. Recovery verifies the persisted plan, newly produced outputs and authenticated unchanged references; it does not relabel reused artifacts with the collection source SHA.

Manual inputs include `tag`, `mode` (`build` or `publish-only`), `recovery_run_id`, `platform`, `checkout_ref`, `publish`, and `rollout_hours`. **`publish` defaults to true**; explicitly select false for an authorized non-publishing build. Tag pushes do not trigger the workflow. Do not dispatch a publishing workflow as a build test.

Dispatch recurring release builds from `main` after integration so they can reuse its native dependency caches. GitHub Actions caches are scoped to the workflow ref: an identical cache key on an unrelated feature branch is not available to the first `main` build, and `checkout_ref` does not change that scope. Compare timings for the same target/profile and distinguish cold-cache runs from restored-cache runs; do not weaken provenance checks or release optimization to improve a benchmark.

The source commit must carry the same `.github/workflows/` files as the default branch until publication finishes. Publication creates and updates the release with the workflow's `GITHUB_TOKEN`, and GitHub refuses both (`403 Resource not accessible by integration`, or 404) when the target commit adds or modifies workflow files relative to the default branch; that token cannot be granted workflow authorization ([releases API](https://docs.github.com/en/rest/releases/releases)). So land workflow changes on `main` before tagging, do not push workflow changes to `main` while a release is running, and if a workflow fix is needed after tagging an unpublished version, move that tag to the fixed `main` commit and rebuild rather than publishing artifacts from the older commit.

`publish-only` consumes an exact prior completed canonical workflow run and its persisted component plan. Recovery requires complete, unique, unexpired planned outputs plus authenticated unchanged references; an omitted target must not disappear from the collection. `cargo xtask release recovery`, `record`, `verify`, `manifests` and `publish` own integrity, feeds and publication. The publication repository is `emotionbug/Passession`; manual `gh` commands must name it explicitly. Desktop rollout defaults to 36 hours; rollout changes require publication authorization.

Publication retains the numeric release ID returned by draft creation and uses that ID for post-upload verification. Draft tag aliases can change during metadata updates; they are not a stable release identity. Metadata updates and final publication explicitly bind the canonical version tag.

`checkout_ref` may pin a packaging correction without moving an existing source tag if its collection version still matches the tag. Newly produced artifacts bind the exact resolved checkout and their component inputs. Unchanged images may retain older source identities only when the plan authenticates their original artifacts and provenance; arbitrary old outputs must not be relabeled or mixed into the publication. Published notes identify the collection build source when it differs from the tag.

`.github/workflows/docker.yml`, `desktop-packages.yml`, and `release-notes-sync.yml` are also manual-only, alongside relay deployment and rollout. Docker publication requires `publish=true` and an explicit matching source version, including when the selected ref is a tag. These workflows do not establish a completed release or remove the distribution requirements above.

Linux/Windows ARM64 client builds need only the Rust toolchain and Zig on the native ARM64 runner; the former reusable Flutter ARM64 SDK release (`flutter-sdk-*` tags) is not consumed. Leave published `flutter-sdk-*` prereleases untouched; they are not product releases.

## Authorization and verification

A release requires an explicit approved source revision, version/channel decision, publication target and authorization. Keep preparation separate from publication. Native release publishing commands, tag pushes and publishing workflow dispatches have external effects; they are not verification shortcuts.

Before a release decision, gather actual Rust checks, target-platform execution, packaging/signing/install/update evidence, and distribution review for the exact revision and binary inventory. Integrity verification is not a compliance audit, proof of signing, or permission to publish.

Actual execution evidence and missing platform checks are recorded in [testing](testing.md). The Android emulator and Linux X11 runs do not establish Windows UI behavior, full GNOME Wayland/XWayland input behavior, signed installation or in-place update success. Keep those boundaries distinct from successful release builds and artifact-integrity checks.

## Release branch discipline

While you finalize a release on `main`, use a temporary `next` branch for work intended for the following
release. This applies to both beta and stable releases.

- Create each new `next` from freshly fetched `origin/main`. Reuse it while active.
- "This goes to next" means create the PR against `next` or retarget an existing
  PR, and keep that destination through delivery.
- Keep `next` current by merging `origin/main` into it as release fixes land.
  Avoid rebasing this shared branch because agents and open PRs depend on its history.
- After the release ships, bring `next` up to date and open a `next` → `main` PR.
  Pass CI and merge without squashing away the individual PR commits needed for
  the changelog. Retarget remaining PRs based on `next` to `main` and delete the integrated
  `next`. Create it fresh when needed again.

PR tests run for every base branch; automatic push tests target only `main`.
Required-check protection and merge strategy are repository settings, not
guarantees made by these workflow files. Retargeted or non-`main` changes can
run the single CI workflow manually. Packaging and deployment remain
manual-only on every branch.

### Hotfix from a release tag

If `main` contains changes you do not want to release, branch from the affected
release tag and cherry-pick only the required fixes. Run CI on that branch, then
use the normal release flow with it as the explicit source, choosing a new patch
or beta version. Ensure the fixes and changelog also reach `main` and any active
`next`, preserving newer development and version changes there. This is a
short-lived hotfix branch, not another maintained release track.
