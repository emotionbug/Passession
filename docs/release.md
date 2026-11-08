# Release

The checkout version is **0.13.13**. Normal CI runs tests on one Ubuntu x64 runner. Building, packaging and publishing artifacts require an explicit manual workflow dispatch; commit and tag pushes do not start those operations.

## Distribution requirements

Native packages exclude speech recognition, voice conversation and TTS, including Sherpa, the speech ONNX runtime and espeak-ng. Generic effect-sound playback remains.

Rust tooling in `xtask/src/notices.rs` generates Cargo and bundled-asset attributions: the daemon closure in `runtime/THIRD-PARTY-NOTICES.txt`, and the client's `paseo-gui` closure for the packaged target plus every packaged font in the application's `THIRD-PARTY-NOTICES.txt` (desktop bundle, APK assets, iOS bundle). Both also attribute the natively built Ghostty library described below. Preserve those notices with the exact packaged artifacts and review the licenses of remaining bundled dependencies and platform runtimes. Generated attributions are not a compliance certification or a corresponding-source offer.

The revision-pinned objc2 supplements preserve the authors' complete upstream `LICENSE.md`, including its Apple SDK caveat. That file is a licensing declaration linking standard terms, not a fabricated replacement license. Frozen OpenCode ABI bytes and the V8 notice use explicit LF checkout attributes so Windows cannot silently change their validated content.

The makepad 1.0.0 crates are published without license files; `packaging/cargo-notice-supplements.json` carries the MIT text from the exact makepad commit each crate was published from. makepad's vendored zune-image forks cite the upstream zune-image commit of the same release (`upstreamRevision`). Supplements apply to registry sources only: a vendored, patched path crate such as `packages/gui-platform/vendor/makepad-platform` must contain its own license file. A Cargo dependency without license text fails packaging by name.

A crate published without `.cargo_vcs_info.json` (the arborium highlighting crates, `cesu8`, `fxhash`) cannot prove its source revision, so its supplement binds `checksum`, the registry SHA-256 `Cargo.lock` records for the downloaded `.crate`, and pins the license URL with `upstreamRevision`. Each arborium grammar crate carries arborium's own license plus the license of the upstream tree-sitter grammar at the commit its `arborium.yaml` names.

The daemon and the client statically link Ghostty's VT library, which `libghostty-vt-sys` compiles with Zig from a pinned Ghostty commit instead of shipping it in a Cargo package. `packaging/native-notices.json` lists the works inside that archive (Ghostty, the simdutf and Highway SIMD libraries, uucode and its Unicode data, Zig's compiler-rt and standard library) with license text pinned at upstream revisions, and binds the list to the crate version and the `GHOSTTY_COMMIT` its `build.rs` pins. Bumping either fails notice generation until the list is re-reviewed against Ghostty's `src/build/GhosttyZig.zig` and `SharedDeps.zig`. `libghostty-vt` 0.2.2 is published without a license file and uses a supplement; the vendored `libghostty-vt-sys` path crate carries its own `LICENSE`.

Fonts are third-party works with their own licenses. `packaging/resource-notices.json` binds each packaged font file's SHA-256 to license text pinned to an upstream revision or an immutable archive digest; an unknown or changed font fails packaging. Its `excluded` list removes reviewed files from every package: makepad-widgets 1.0.0 ships Liberation Mono 1.04 under Red Hat's GPL-2.0-with-font-exception agreement, so the client uses Jetendard (OFL-1.1; JetBrains Mono with Pretendard Hangul fitted to two cells) for code and the Liberation file is not redistributed. Jetendard's release asset is not immutable, so each shipped TTF is bound by its own digest, and its notices include the upstream JetBrains Mono, Nerd Fonts, Pretendard and Yeomil Mono licenses it asks redistributors to keep; Makepad's LXGW WenKai Chinese fallback (36 MB) is left out because the client supports English and Korean, and Han characters fall back to Noto Sans KR. The emoji font stays: Makepad has no system font fallback, so without it emoji would not render. An excluded file whose bytes change fails packaging, and so does any `crate://…/resources/…` reference in client sources that names a resource not staged: Makepad loads fonts lazily and a style using a missing font panics.

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

Official Linux/Windows desktop builds compile `paseo-sessions` and `paseo` in one Cargo invocation. Their shared release identity and dependency feature graph avoid a second compilation pass for common dependencies. Both binary architecture checks, native runtime provenance, notices and packaging validation still run, with the existing optimized release profile. Local builds remain separate because the daemon must not inherit the client's update channel/repository; macOS remains separate because the client sets its own deployment target. `package-desktop` and `package-daemon` consume the staged build rather than compiling again; packaging still checks source identity and executable hashes. Shared compilation caches do not replace checkout-local staging or its provenance checks.

Client artifacts and build descriptors live in `artifacts/native/`: `Paseo-Sessions-<version>-<platform>-<arch>.<AppImage|deb|zip|exe>`, `Paseo-Sessions-<version>-android-<arch>[-debug-signed].<apk|aab>`, `Paseo-Sessions-<version>-ios-arm64.ipa` or `-unsigned.app.zip`. APK builds default to arm64; AAB builds default to universal. Standalone daemon archives are `Paseo-Daemon-<version>-<platform>-<arch>.tar.gz` (ZIP on Windows), with a channel/platform/architecture manifest. Native runtime metadata includes `paseo-runtime.json`, `bundle-provenance.json`, and desktop-specific `desktop-runtime.json` when applicable. Preserve assets together; copying only the executable omits runtime libraries, Makepad resources and skills.

Bundle provenance records the executable digest **after relocation**, because runtime patching can change its bytes. `sourceExecutableSha256` separately identifies the input executable. Check an artifact against its final digest, not against the pre-relocation source hash.

The Visual C++ ARM64 redistributable directory can also contain x64-view DLLs for emulated applications. Windows ARM64 packaging keeps the matching native CRT images (including Arm64X) and omits an x64-view image only after checking direct and delay-loaded imports of every bundled executable, DLL and selected CRT. A reference to an omitted image or any other unexpected CRT architecture fails packaging. The client and daemon executable architecture checks remain strict.

## Installed identity and the desktop update feed

Installed paths keep the previous client's names, so `paseo open`, shortcuts, `paseo://` handlers and in-place updates keep working: `paseo_sessions` (Linux bundle, `/opt/paseo-sessions`, `/usr/bin/paseo-sessions`, Debian package `paseo-sessions`), `paseo_sessions.exe` in `%LOCALAPPDATA%\Programs\Paseo Sessions`, and `Paseo Sessions.app` with bundle ID `sh.paseo.sessions.desktop`. The cargo binary `paseo-sessions` is renamed when bundled.

Linux packages also cover libraries the client opens at runtime, which `ldd` and `dpkg-shlibdeps` cannot see: Makepad's `libEGL`, `libX11` (idle time and close-to-tray), and the optional `libXss` (X11 idle time) and `libSM`/`libICE` (session logout without a GNOME SessionManager). The AppImage bundles all four X11/XSMP libraries with their closure (`libraries::LINUX_DLOPENED`; the packaging host must have them installed); the Debian package depends on `libegl1`, `libx11-6` and `xdg-utils` and recommends `libxss1`, `libsm6` and `libice6`. Add any new `dlopen` library to both lists.

The desktop client feed keeps its exact wire identity: asset `flutter-<channel>-<platform>-<arch>.json` with `runtime: "flutter"` and `appId: "sh.paseo.sessions.desktop"`. Installed clients — including every Flutter-era build — discover updates only by that identity; renaming it would strand them. Artifact names are read from the manifest, so they changed freely. The daemon feed `daemon-<channel>-<platform>-<arch>.json` is unchanged.

The NSIS installer upgrades in place: at the registered install location it first deletes every file listed in the previous installation's `.paseo-sessions-owned-files` inventory and removes emptied directories, then installs the new payload. Files the user created there are never deleted.

## Signing and mobile build numbers

`xtask/src/common.rs` resolves the build number from `PASEO_BUILD_NUMBER`, then `ANDROID_VERSION_CODE`, then `packaging/android/version.properties`; empty variables count as unset. Android enforces a minimum of `13000`. Confirm the next build number against the actual store history; do not infer store acceptance from a local version string.

Android release signing uses `ANDROID_KEYSTORE_PATH`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, and `ANDROID_KEY_PASSWORD`, or `packaging/android/key.properties`. Without a release key, APK and AAB output is explicitly named **debug-signed**, not store-ready. An AAB is produced by Gradle's bundle task, not by renaming an APK; it is a store upload bundle, not a directly installable package. Credentials are not part of this fork. The Android application ID is `sh.paseo.sessions`; the minimum SDK stays 24 (see [Android](android.md) for the Makepad compatibility patches that keep it).

iOS signs with `PASEO_IOS_PROVISIONING_PROFILE` (a `.mobileprovision` path) and `PASEO_IOS_SIGNING_IDENTITY` (a keychain code-signing identity): the profile is embedded, its application identifier, team, `get-task-allow` and `aps-environment` override the app entitlements, and the bundle is zipped as an `.ipa`. Without them, the build emits an explicitly **unsigned device `.app` archive**, not an installable IPA or App Store submission. The Xcode `PASEO_IOS_EXPORT_OPTIONS_PLIST` path no longer exists. macOS and Windows signing use the native packaging signing configuration; neither platform has been claimed executed in the local migration evidence.

Optional Firebase configuration for Android must match `sh.paseo.sessions`: `GOOGLE_SERVICES_FILE_SESSIONS` is staged into the Gradle project and restored afterward. iOS push uses APNs directly (device token exchanged with Expo, environment from the embedded provisioning profile) and does not link Firebase, so `GOOGLE_SERVICE_INFO_PLIST_SESSIONS` is no longer used.

## Workflow definitions and publication controls

`.github/workflows/desktop-release.yml` is the manual-only **Native Release** workflow despite its retained filename. It builds the Rust client and daemon for every target, preserves complete per-target artifacts, validates recovery provenance, and publishes only through its publication path. Its matrix is for explicitly requested artifacts, not default CI or per-platform test repetition.

The complete release matrix has **eight targets**: Linux x64/arm64, macOS x64/arm64, Windows x64/arm64, Android arm64 APK and iOS arm64. Android publication uses only `android-arm64`; x86_64 and universal APKs and AABs are not release targets, and 32-bit Android is not built at all ([Android](android.md#build-and-launch)). macOS uses `macos-15` (arm64) and `macos-15-intel` (x64); Android uses the runner image's Android 37.0 platform (`compileSdk`), installs NDK 28.2.13676358 (r28c) from Google's `android-ndk-r28c-linux.zip`, pinned by SHA-256, only when the image lacks it (not through `sdkmanager`, which the image's cmdline-tools report as deprecated), and adds only `aarch64-linux-android`; iOS adds `aarch64-apple-ios`. Every native job installs the official Zig 0.15.2 archive for its runner, checked against the SHA-256 digests from ziglang.org, and restores Zig's global cache with Ghostty's fetched packages. Native Release inlines that step because it builds older tags; CI and Native Desktop Packages use the equivalent `.github/actions/setup-zig`, and `docker/base/Dockerfile` installs the official Zig tarball checked against its pinned SHA-256. Without signing secrets, Android remains debug-signed and iOS remains unsigned. No npm package publication or website build is part of this workflow.

The `windows-com-smoke` job runs `cargo test --release -p paseo-gui-platform --lib share_item_resolves_across_apartments` on each planned Windows runner in parallel with the native builds. That test resolves its own feature graph, so running it after the shipped build only lengthened the Windows job; publication still requires it to pass (or to be skipped because no Windows target is planned).

Caches never carry release outputs; every artifact is rebuilt, recorded and verified from the exact source SHA. They hold only inputs and intermediate state:

- `Swatinem/rust-cache` keeps third-party dependency builds (it drops workspace and path crates). Native jobs use `native-release-<platform>-<arch>`; Native Desktop Packages reuses `native-release-linux-x64`, because it builds the same release-profile graph with the same toolchain and cache-relevant environment. `source` and `publish` share `release-xtask`, and the COM smoke job uses `release-windows-com-smoke`.
- `zig-0.15.2-<os>-<arch>-<job key>-<hash of libghostty-vt-sys/build.rs>` holds `ZIG_GLOBAL_CACHE_DIR` (Ghostty's Zig packages) and Zig's content-addressed local cache for that target (`$GHOSTTY_CACHE_DIR/zig-cache-<GHOSTTY_COMMIT>`), so a warm build skips the Zig compile. `build.rs` pins the commit, so its hash covers it. Building there instead of in `OUT_DIR` changes only absolute paths in the intermediate archive's debug info, which release binaries strip ([VENDORED](../packages/terminal/vendor/VENDORED.md)).
- `ghostty-src-<os>-<hash of build.rs>` holds the Ghostty clone (`$GHOSTTY_CACHE_DIR/ghostty-src`), one entry per runner OS shared by every job and workflow. `build.rs` reuses it only when `HEAD` is the pinned `GHOSTTY_COMMIT` and `git status --ignored` is empty; otherwise it reclones. Jobs on the same OS racing to save it report that another job is creating the entry.
- `native-packaging-<os>-<platform>-<arch>-<inputs>` exists only for Linux desktop (pinned AppImage tools, re-verified by SHA-256 before use; keyed on `xtask/src/assets.rs`) and Android (the unpacked pinned Gradle distribution and `~/.gradle/caches`; keyed on the `GRADLE_VERSION`/`GRADLE_SHA256` pins and the Gradle build files). The downloaded Gradle archive is not cached.
- Keys contain no run IDs. The Zig, Rust and packaging entries fall back to the previous entry through restore keys when their inputs change. Rust caches save only from the default branch.

`xtask` runs in the dev profile; root `Cargo.toml` optimizes only its SHA-256 and ZIP/gzip crates in that profile. The bytes it writes do not depend on how it was compiled, and the release profile is unchanged.

Workflow artifact uploads are temporary: both Native Release and Native Desktop Packages retain them for **7 days**. Recover within that window or rebuild the exact authorized source; expired uploads cannot support `publish-only`. This retention does not delete published GitHub Release assets.

Android `record` and `verify` require matching release-profile metadata, source SHA, architecture and format; the metadata and artifact are both covered by provenance hashes. Archive validation checks APK versus AAB layout, bundled notices and the exact Makepad native ABI inventory against ELF headers. These structural checks reject a renamed APK and mislabeled native libraries; they do not replace Android signing verification, bundletool validation, installation or store acceptance.

For a reduced Windows x64, Linux x64 and Android release, select `platform=linux-windows-x64-android`. This preset supports both building and `publish-only` recovery without requiring macOS, desktop ARM64 or iOS artifacts. Recovery and publication require exactly three targets (`linux-x64`, `windows-x64`, `android-arm64`) from the same source SHA; desktop feed verification and later rollout changes use the declared desktop target inventory rather than assuming the full matrix.

Manual inputs include `tag`, `mode` (`build` or `publish-only`), `recovery_run_id`, `platform`, `checkout_ref`, `publish`, and `rollout_hours`. **`publish` defaults to true**; explicitly select false for an authorized non-publishing build. Tag pushes do not trigger the workflow. Do not dispatch a publishing workflow as a build test.

Dispatch recurring release builds from `main` after integration so they can reuse its native dependency caches. GitHub Actions caches are scoped to the workflow ref: an identical cache key on an unrelated feature branch is not available to the first `main` build, and `checkout_ref` does not change that scope. Compare timings for the same target/profile and distinguish cold-cache runs from restored-cache runs; do not weaken provenance checks or release optimization to improve a benchmark.

`publish-only` consumes preserved artifacts from an exact prior completed canonical workflow run. Recovery requires a complete, unique, unexpired target inventory; one-architecture macOS recovery must not silently replace the other architecture. `cargo xtask release recovery`, `record`, `verify`, `manifests` and `publish` own recovery, artifact hashes, integrity, update feeds and publication. The publication repository is `emotionbug/Passession`; any manual `gh` command must name it explicitly with `--repo emotionbug/Passession`. Desktop rollout defaults to 36 hours; a rollout change is a publication-side operation requiring authorization, not a local check.

Publication retains the numeric release ID returned by draft creation and uses that ID for post-upload verification. Draft tag aliases can change during metadata updates; they are not a stable release identity. Metadata updates and final publication explicitly bind the canonical version tag.

`checkout_ref` may pin a native-packaging correction without moving an existing source tag, provided its workspace version still matches that tag. Artifact provenance records the exact resolved checkout SHA; identify that build-source commit in the published notes when it differs from the tag. All targets in one publication must pass verification against that same SHA. Successful artifacts from an older source SHA cannot be mixed into the corrected publication; rebuild those targets rather than relabeling their provenance.

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
