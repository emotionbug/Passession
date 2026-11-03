# Release

The checkout version is **0.13.4**. Normal CI runs tests on one Ubuntu x64 runner. Building, packaging and publishing artifacts require an explicit manual workflow dispatch; commit and tag pushes do not start those operations.

## Distribution requirements

Native packages exclude speech recognition, voice conversation and TTS, including Sherpa, the speech ONNX runtime and espeak-ng. Generic effect-sound playback remains.

Rust tooling in `xtask/src/notices.rs` generates Cargo and bundled-asset attributions. Preserve those notices with the exact packaged artifacts and review the licenses of remaining bundled dependencies and platform runtimes. Generated attributions are not a compliance certification or a corresponding-source offer.

The revision-pinned objc2 supplements preserve the authors' complete upstream `LICENSE.md`, including its Apple SDK caveat. That file is a licensing declaration linking standard terms, not a fabricated replacement license. Frozen OpenCode ABI bytes and the V8 notice use explicit LF checkout attributes so Windows cannot silently change their validated content.

## Release contents

| Target                        | Native artifact scope                                                                         |
| ----------------------------- | --------------------------------------------------------------------------------------------- |
| Linux, macOS, Windows desktop | Flutter application, Rust daemon/CLI, native desktop integration and runtime assets           |
| Android                       | Flutter remote client APK; no daemon                                                          |
| iOS                           | Flutter remote client; no daemon; unsigned app archive unless provisioning/export is supplied |
| Standalone daemon             | Rust executable with libraries, skills, notices and provenance                                |
| Container                     | Native daemon and runtime assets; no browser UI or Node executable in the default image       |
| Rust libraries                | Client, protocol, plugin and highlighting crates; source/package availability is separate from binary releases |

No Electron, Chromium, web client, embedded automation, or interactive terminal UI is shipped. Script/setup PTY support stays in the daemon and the client presents read-only logs. Backend plugins use native executables; no Node worker or JavaScript compiler is packaged.

## Local build versus distribution identity

Use Rust/Cargo, Flutter, and each target's native toolchain. Build desktop artifacts on the matching OS; build iOS on macOS with Xcode. Build and release commands run through `cargo xtask`, without Node or npm.

```sh
cargo fetch --locked
cargo xtask build-daemon                # debug binary and runtime assets
cargo xtask build-daemon --release      # optimized local build, not publication
cargo xtask build-flutter linux         # matching host only
cargo xtask package-desktop
cargo xtask build-flutter android --arch arm64
cargo xtask build-flutter ios
```

The default Flutter release build uses optimized Rust; a debug development build uses the debug profile. Optimization is independent of publication: `--release` alone must not turn a local binary into an updater-enabled official build.

Published provenance is explicitly selected by `PASEO_NATIVE_RELEASE=1` together with the optimized build and validated `PASEO_UPDATE_REPOSITORY`, `PASEO_RELEASE_CHANNEL`, and `PASEO_BUILD_SOURCE_SHA`. The source SHA must identify the checked-out source. Standalone `cargo xtask package-daemon` requires that release identity; it is not a way to relabel an arbitrary local executable.

Client artifacts and build descriptors live in `artifacts/flutter/`. Standalone daemon archives are `Paseo-Daemon-<version>-<platform>-<arch>.tar.gz` (ZIP on Windows), with a channel/platform/architecture manifest. Native runtime metadata includes `paseo-runtime.json`, `bundle-provenance.json`, and desktop-specific `desktop-runtime.json` when applicable. Preserve assets together; copying only the executable omits runtime libraries and skills.

Bundle provenance records the executable digest **after relocation**, because runtime patching can change its bytes. `sourceExecutableSha256` separately identifies the input executable. Check an artifact against its final digest, not against the pre-relocation source hash.

The Visual C++ ARM64 redistributable directory can also contain x64-view DLLs for emulated applications. Windows ARM64 packaging keeps the matching native CRT images (including Arm64X) and omits an x64-view image only after checking direct and delay-loaded imports of every bundled executable, DLL and selected CRT. A reference to an omitted image or any other unexpected CRT architecture fails packaging. The Flutter and daemon executable architecture checks remain strict.

## Signing and mobile build numbers

`xtask/src/common.rs` resolves the build number from `PASEO_BUILD_NUMBER`, then `ANDROID_VERSION_CODE`, then `13001`. Android enforces a minimum of `13000`. Confirm the next build number against the actual store history; do not infer store acceptance from a local version string.

Android release signing uses `ANDROID_KEYSTORE_PATH`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, and `ANDROID_KEY_PASSWORD`, or the checked-in project's local signing properties. Without a release key, output is explicitly named **debug-signed**, not store-ready. Credentials are not part of this fork. The Android application ID is `sh.paseo.sessions`.

iOS uses `PASEO_IOS_EXPORT_OPTIONS_PLIST` for an exported IPA. Without it, the build emits an explicitly **unsigned device `.app` archive**, not an installable IPA or App Store submission. macOS and Windows signing use the native packaging signing configuration; neither platform has been claimed executed in the local migration evidence.

Optional Firebase configuration must match `sh.paseo.sessions`: `GOOGLE_SERVICES_FILE_SESSIONS` for Android and `GOOGLE_SERVICE_INFO_PLIST_SESSIONS` for iOS. The wrapper stages these files for the build and restores previous state afterward.

## Workflow definitions and publication controls

`.github/workflows/desktop-release.yml` is the manual-only **Native Release** workflow despite its retained filename. It builds Flutter/Rust targets, preserves complete per-target artifacts, validates recovery provenance, and publishes only through its publication path. Its matrix is for explicitly requested artifacts, not default CI or per-platform test repetition.

The complete release matrix has **eight targets**: Linux x64/arm64, macOS x64/arm64, Windows x64/arm64, Android universal and iOS arm64. Each target's artifacts are retained for **30 days**. macOS uses `macos-15` (arm64) and `macos-15-intel` (x64); Android explicitly provisions platform-tools. Without signing secrets, Android remains debug-signed and iOS remains unsigned. No npm package publication or website build is part of this workflow.

For a reduced Windows x64, Linux x64 and Android release, select `platform=linux-windows-x64-android`. This preset supports both building and `publish-only` recovery without requiring macOS, desktop ARM64 or iOS artifacts. Recovery and publication require all three selected targets from the same source SHA; desktop feed verification and later rollout changes use the declared desktop target inventory rather than assuming the full matrix.

Manual inputs include `tag`, `mode` (`build` or `publish-only`), `recovery_run_id`, `platform`, `checkout_ref`, `publish`, and `rollout_hours`. **`publish` defaults to true**; explicitly select false for an authorized non-publishing build. Tag pushes do not trigger the workflow. Do not dispatch a publishing workflow as a build test.

`publish-only` consumes preserved artifacts from an exact prior completed canonical workflow run. Recovery requires a complete, unique, unexpired target inventory; one-architecture macOS recovery must not silently replace the other architecture. `cargo xtask release recovery`, `record`, `verify`, `manifests` and `publish` own recovery, artifact hashes, integrity, update feeds and publication. The publication repository is `emotionbug/paseo`; any manual `gh` command must name it explicitly with `--repo emotionbug/paseo`. Desktop rollout defaults to 36 hours; a rollout change is a publication-side operation requiring authorization, not a local check.

`checkout_ref` may pin a native-packaging correction without moving an existing source tag, provided its workspace version still matches that tag. Artifact provenance records the exact resolved checkout SHA; identify that build-source commit in the published notes when it differs from the tag. All targets in one publication must pass verification against that same SHA. Successful artifacts from an older source SHA cannot be mixed into the corrected publication; rebuild those targets rather than relabeling their provenance.

`.github/workflows/docker.yml`, `desktop-packages.yml`, and `release-notes-sync.yml` are also manual-only, alongside SDK assembly, relay deployment and rollout. Docker publication requires `publish=true` and an explicit matching source version, including when the selected ref is a tag. These workflows do not establish a completed release or remove the distribution requirements above.

### Reusable Linux and Windows ARM64 Flutter SDKs

Before explicitly requested Linux/Windows ARM64 app builds can succeed, publish the SDK pinned by `.github/flutter-sdk.json` through the manually dispatched **Flutter ARM64 SDK** workflow (`.github/workflows/flutter-sdk.yml`). Normal Ubuntu x64 tests do not require it. Dispatch only after the SDK tooling and pin are present at the approved workflow revision. It assembles both SDKs on native `ubuntu-24.04-arm` and `windows-11-arm` runners using `cargo xtask flutter-sdk build`, then publishes the complete pair with `cargo xtask flutter-sdk publish`. This bootstraps pinned official Flutter source and downloads official native Dart/engine artifacts; it is **not an engine build or a separately maintained Flutter repository**.

The pin is the single source of SDK artifact identity: Flutter version/channel, official source repository and exact source/engine revisions, destination repository, and reserved release tag. The initial tag is `flutter-sdk-3.47.5-r1` in `emotionbug/paseo`. Each platform produces `paseo-flutter-sdk-<version>-<linux|windows>-arm64.tar.gz` and a matching JSON identity/checksum manifest in `artifacts/flutter-sdk/`. Intermediate workflow artifacts expire after **7 days**; the reserved GitHub release assets are the durable consumer source.

For Flutter 3.47.5, the pin also records one Windows bootstrap correction: upstream `bin/internal/shared.bat` invokes literal `$git` when reading the revision. The producer changes that command to `git`, verifying both original and corrected SHA-256 values with the upstream-required CRLF checkout bytes. Published SDK validation permits only that exact declared change; other tracked-source changes still fail. This does not modify the engine. A future version with the upstream fix can set `windowsBootstrapPatch` to `null`.

SDK archives encode sparse filesystem inputs as ordinary file data. Consumers continue accepting only regular files, directories and contained symlinks; GNU sparse and other special entry types are not permitted. The expanded-size limit therefore covers the full logical file data.

Before making package roots relative, SDK assembly resolves filesystem aliases to canonical paths. In particular, Windows `RUNNER~1` temporary paths must not survive as references outside the archive. Validation rejects package URLs that leave the SDK even when a build-machine alias happens to resolve back into it; extracted SDKs must not depend on that machine's directory layout.

Only the publish job has `contents: write`; builds and app consumers use read-only permissions. Publication validates both platform inventories and their builder revision, keeps the release draft until complete, and publishes a **non-latest prerelease**. The reserved `flutter-sdk-*` tag is not a Paseo product release and must never replace the product's latest release or update feeds. Published SDK assets are immutable: do not delete, clobber, or repurpose an existing SDK tag.

Publication refuses any existing reserved release or tag, including a partial draft; it neither resumes nor overwrites that identity. If publication fails after creating a draft, leave it untouched and select a new `rN` release tag in the pin before another authorized producer run. Consumers reject drafts and require the complete four-asset inventory with matching tag/builder revision.

The shared setup action downloads the exact reserved SDK assets with `cargo xtask flutter-sdk install --version <version> --channel <channel>`. It checks the requested version/channel against the pin, verifies archive identity/size/hash before safe extraction, and checks Flutter identity and native Dart ABI before exporting the SDK. Its optional cache stores downloaded archives/manifests, not an unvalidated executable SDK; restored bytes are checked again. A missing SDK release is an actionable prerequisite failure: run the dedicated SDK workflow, rather than bootstrapping Flutter inside an app job or substituting x64 Dart. Non-ARM64 Linux/Windows and Apple hosts continue using the existing archived SDK setup.

For an upgrade, update `.github/flutter-sdk.json` with the reviewed official version/channel/source/engine identity and a **new reserved release tag**; increment the `rN` suffix for a same-version SDK assembly change. Update the requested Flutter version/channel in `ci.yml`, `desktop-packages.yml`, and `desktop-release.yml` together when those change. After approval, dispatch the SDK producer at that revision and retain its run evidence before starting dependent ARM64 builds. Do not reuse old assets under a new identity. SDK assembly/ABI validation alone is not proof of application execution on either platform.

## Authorization and verification

A release requires an explicit approved source revision, version/channel decision, publication target and authorization. Keep preparation separate from publication. Native release publishing commands, tag pushes and publishing workflow dispatches have external effects; they are not verification shortcuts.

Before a release decision, gather actual Rust/Flutter/SDK checks, target-platform execution, packaging/signing/install/update evidence, and distribution review for the exact revision and binary inventory. Integrity verification is not a compliance audit, proof of signing, or permission to publish.

Current actual Linux evidence and explicit missing platform checks are in [testing](testing.md). Android ARM64 release-mode compilation passed with debug signing, not production credentials; no Android device execution or Windows/macOS/iOS execution is claimed. Consult the testing record for desktop tray/editor follow-up results rather than treating a build as runtime proof. No release was published, installed, committed, pushed, or offered by the documentation update.

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
