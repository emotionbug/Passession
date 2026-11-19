# Android

The Android application is the native Rust **Passession** client (Makepad), package `sh.paseo.sessions`. It connects to a Rust daemon on another machine; it does not run a local daemon, and its terminal tabs control shells on that host. The same client also targets iOS, Linux, macOS and Windows. There is no Expo/Metro, EAS, Flutter engine, embedded browser or web client in the native application.

## Build and launch

Use Rust/Cargo for `xtask`, Zig 0.15.2 (see [development](development.md#toolchains)), a JDK 17+ and the Android SDK with NDK **28.2.13676358** (`ANDROID_HOME`; `ANDROID_NDK_HOME` overrides the NDK location). Install the Rust targets once:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
```

From the repository root:

```sh
cargo fetch --locked
cargo xtask build-gui android                       # arm64 APK
cargo xtask build-gui android --arch universal      # APK with both 64-bit ABIs
cargo xtask build-gui android --arch x64            # x86_64 emulator APK
cargo xtask build-gui android --format aab          # universal Android App Bundle
cargo xtask build-gui android --profile dev-fast    # debug-signed arm64 APK
cargo xtask dev app android --device <device-id>
```

`build-gui` defaults to the release Rust profile. Use `--profile dev-fast|debug|release` to select one; `--debug` remains an alias for `--profile debug`. `dev app` defaults to `dev-fast`. Both non-release profiles use the Android debug variant and debug key, even when production credentials are present.

APK builds default to `--arch arm64`; AAB builds default to `--arch universal` (arm64-v8a and x86_64). Either format accepts `--arch arm64|x64|universal`. The client is 64-bit only: the Ghostty terminal library's Rust bindings assume a 64-bit layout, so `armeabi-v7a` is not built and `--arch arm` fails with that reason. An AAB contains the selected ABIs for store-generated device APKs; it is not directly installable with `adb`. Use an APK for local installation.

Published releases contain only the arm64 APK. Other ABI and AAB options above
remain explicit local tooling for emulator/development or store preparation;
the release workflow neither builds nor publishes them.

The build compiles `paseo-gui` as a `cdylib` per ABI with the NDK clang for API 24, stores it as `lib/<abi>/libmakepad.so` (the library name the vendored `MakepadActivity` loads), copies every Makepad resource directory to `assets/makepad/<crate>/resources`, and adds `THIRD-PARTY-NOTICES.txt` and `LICENSE` to the assets. It then runs a pinned Gradle 9.9.0 (SHA-256 checked, cached under `$XDG_CACHE_HOME/paseo-packaging`) on `packaging/android` with AGP **9.5.1** (its default DSL and built-in Kotlin support, backed by Kotlin Gradle plugin 2.4.21), compile SDK **37.0**, target SDK 36 and `--no-daemon`. APKs use the assemble task; AABs use the bundle task. Gradle state and staged resources stay in the checkout's `.dev/build/gui/android/<profile>/`; Cargo compilation uses the [shared target cache](development.md).

**Minimum SDK is 24**, as for the Flutter client. Published makepad-platform 1.0.0 cannot load below API 29: it links `libamidi` (API 29), `libaaudio` and `libnativewindow` (API 26), references `AChoreographer_postFrameCallback64` (API 29) directly, and its Java calls `WindowInsets.Type` (API 30). Paseo therefore patches makepad-platform through the vendored copy at `packages/gui-platform/vendor/makepad-platform` (newer libraries and symbols resolved at runtime; app audio uses AudioTrack on every version) and the Java in `packaging/android/makepad` (API guards, listed in `PATCHES.diff`). Linking against the API 24 NDK stubs turns a newer `-l` library into a link error, and xtask also rejects a library whose `DT_NEEDED` names a system library newer than API 24. A newer *symbol* in an API 24 library is not caught at link time; keep those behind runtime lookup.

The development run selects the single attached `adb` device or the `--device` serial, builds only that device's primary ABI (an x86_64 emulator gets an x86_64 library; a 32-bit-only device is rejected), installs with `adb install -r`, starts the launcher activity and follows `adb logcat` for the app process until interrupted. `--run` rejects an explicit `--arch` or AAB output.

Artifacts are copied into `artifacts/native/` as `Passession-<version>-android-<arch>[-debug-signed].apk` or `.aab`. Non-release profiles and release builds without a production key carry the `-debug-signed` suffix. `packages-android-<arch>[-aab].json` records the artifact filename, format, profile and source revision for release validation. The independent artifact copy keeps Gradle's incremental output intact.

The earlier ARM64 Rust APK was signature/alignment-checked and installed as an in-place update on Android 17; visual verification remained blocked by the device lock. That evidence does not cover the new profile/AAB paths, establish API 24 compatibility or prove store readiness; see [testing](testing.md).

## Project layout

- `packaging/android/` — Gradle project (`settings.gradle.kts`, `app/build.gradle.kts`), launcher and notification icons (`res/`), `app/debug.keystore`, `version.properties`.
- `packaging/android/makepad/` — Makepad's Android Java (`dev.makepad.android.*`) vendored from makepad commit `a1868e19d3bb955200957efb3c880791a43424eb`, the revision makepad-platform 1.0.0 was published from. Local changes include API 24–29 guards and the shared Editable-backed `MakepadInputConnection`: UTF-16 text/selection/composition snapshots, session/revision fencing, batch ownership, modifier-preserving composition and hardware insertion. The activity also keeps long-press coordinates within the current gesture and cancels long presses after scrolling starts. `PATCHES.diff` records the changes; `UPSTREAM.json` records upstream/current digests. The crate's JNI symbols require these exact classes; crates.io has no `cargo-makepad` for 1.0.0.
- `packages/gui-platform/android/` — `AndroidManifest.xml` (launcher `sh.paseo.sessions.MainActivity`, which extends `MakepadActivity`; the `paseo` VIEW filter on the `PaseoLinkActivity` trampoline; Firebase messaging service; `PaseoTailnetService`; FileProvider `sh.paseo.sessions.files`), the Java glue in `java/sh/paseo/sessions/`, `res/` (theme and FileProvider paths) and `platform.gradle.kts` (androidx.core and the Firebase BOM/messaging). The Gradle project compiles these directly; there is no second Gradle app or Makepad Java copy there.

Native code reaches the Java glue by class and method name over JNI, so the release build never minifies.

Android resizes the native surface for the keyboard. The shell uses a plain root
`View`, not a second keyboard-panning container, and treats navigation/keyboard
bottom occlusion as a union rather than adding overlapping insets. Composer and
modal geometry share that allocation.

When Android destroys the window Surface, rendering and video texture updates
wait for a replacement Surface and current EGL context. Background events may
still update the model; pending redraws remain queued. See the vendored
[Surface lifetime constraint](../packages/gui-platform/vendor/makepad-platform/VENDORED.md#android-surface-lifetime).

## Verify screen-reader support

Enable TalkBack in Android's Accessibility settings, then open Passession.
Swipe through the sidebar and a session: TalkBack should announce headings,
session names, selected items, and names for icon-only buttons. Double-tap a
session or menu item to activate it, focus the composer to open the keyboard,
and scroll a list past the initially visible rows. Open a dialog and verify
that TalkBack stays inside it until it closes.

With the app in the foreground, inspect the same virtual nodes through ADB:

```sh
adb shell uiautomator dump /sdcard/paseo-window.xml
adb pull /sdcard/paseo-window.xml
```

Check the nodes' `text` or `content-desc`, `class`, `bounds`, and action state
such as `clickable`, `selected`, and `checked`. Test TalkBack gestures on the
device too; the XML alone does not establish speech, traversal, or activation.

## One activity per process

The native app serves exactly one activity. `MainActivity` is `singleTop` with
the package's default task affinity, and every start of it uses the launcher's
own intent (MAIN + LAUNCHER, payloads in extras). The task's root intent
therefore always equals the launcher's, and each entry point reaches the
existing instance without a second window or task:

- The launcher brings the existing task forward unchanged. A document picker,
  share chooser, camera or permission dialog left open on top is still there,
  and its result still arrives. The Embedded Tailscale notification returns to
  the app the same way.
- `paseo://` links go through `PaseoLinkActivity`, an exported trampoline with
  no UI (`Theme.NoDisplay`, `noHistory`, `excludeFromRecents`, empty
  affinity). It accepts only a VIEW of scheme `paseo`, forwards the link in an
  extra with `NEW_TASK | SINGLE_TOP | CLEAR_TOP` and finishes. A link opened
  from a browser therefore never creates `MainActivity` inside the browser's
  task.
- Notification taps (local notifications and FCM messages presented by the app)
  use the same three flags and distinct request codes. FCM tray notifications
  use the launch intent plus `CLEAR_TOP`. The existing instance receives the tap
  through `onNewIntent`.

Links and notification taps navigate, so unlike the launcher they close a
picker, chooser or camera that is open above `MainActivity` and cancel its
result.

Only the most recently created activity drives the native loop; a stale
instance's lifecycle and surface callbacks are ignored. A configuration or
system destroy keeps the native app running for the next instance. When the
live activity finishes (for example, the task is removed from Recents), the app
shuts down and the process exits, so the next launch starts a fresh process
rather than a second native app in the old one. See the vendored
[activity lifecycle constraint](../packages/gui-platform/vendor/makepad-platform/VENDORED.md#android-activity-lifecycle-one-native-app-per-process).

## Embedded Tailscale in the background

While Embedded Tailscale is enabled, `PaseoTailnetService` runs as a
`specialUse` foreground service. It keeps the process out of the cached and
frozen states while the app is in the background; it holds no wake lock and
owns no networking. Its silent, ongoing notification on the low-importance
`tailnet` channel shows the connection state, opens the app when tapped and
offers Pause or Resume and Stop. Those actions reach the app as
`PlatformEvent::TailnetControl`. Pause disconnects until Resume, or until the
app restarts; Stop turns Embedded Tailscale off.

- Android 12+ refuses a foreground-service start from the background. The
  service then starts the next time the activity resumes, and the connection
  keeps running in the meantime.
- Without notification permission (Android 13+), the service still runs and
  Android hides its notification.
  `open_permission_settings(PermissionSettings::NotificationChannel("tailnet"))`
  opens that channel's settings.
- Battery optimization and background restriction can still stop the process.
  `PlatformHandle::background_restrictions` reports both
  (`isIgnoringBatteryOptimizations`, `isBackgroundRestricted`), and
  `PermissionSettings::BatteryOptimization` opens the exemption list. The app
  needs no `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` permission.
- The service is never restarted by itself. It stops when the app withdraws its
  presence, and when the native app shuts down.

## Runtime permissions

The app asks for runtime permissions only from in-context flows: camera when
the QR scanner opens, notifications when a feature needs them. Nothing is
requested at launch.

`PaseoBridge.permission()` reports `notDetermined` until the app has requested
a permission once. It stores that fact as `permissionAsked.<permission>` in the
`paseo_platform` shared preferences. After that, a refusal is `denied` while
`shouldShowRequestPermissionRationale` is true, because asking again still
prompts. Otherwise it is `blocked`, and only system settings can change it. A
request is still sent when the permission is blocked: Android answers at once,
and a reset in system settings prompts again. A request interrupted before the
user chose clears the flag.

`open_permission_settings` uses these screens:

- `ACTION_APP_NOTIFICATION_SETTINGS` for notifications.
- `ACTION_CHANNEL_NOTIFICATION_SETTINGS` for a channel that already exists.
- `ACTION_APPLICATION_DETAILS_SETTINGS` for camera and the app.
- `ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS` for battery optimization.

If a vendor build lacks the precise screen, the app details screen opens
instead.

## Text interaction and system font size

Long-press input and selected content use Android's native floating Paste/Copy/Cut/Select All menu, with actions limited by editor capabilities and clipboard availability. Selecting text does not copy it automatically. Tapping another control dismisses the old selection without swallowing that control's click. See [native text selection](native-text-selection.md).

Android's system text-size setting feeds the shared typography layer without changing display density, touch coordinates or saved application font preferences. The bridge caches Android SP conversion samples, including the nonlinear font curve on newer Android versions, and refreshes them on configuration/resume changes. Text, row allocation and hit geometry reflow together. The common TextInput layout cache includes font metrics, available width and alignment, so existing drafts resize without replacing their text, selection or edit history.

Shared touch scrolling estimates release velocity from recent movement rather than a zero-biased smoothing accumulator. Short flicks retain their velocity; stationary holds, reversal, bounds and new input cancel stale motion. Direct dragging remains one-to-one and long-press text selection keeps its gesture ownership.


## Signing and identity

The project uses the maintained Sessions debug keystore at `packaging/android/app/debug.keystore` for local debug compatibility; it is the same key the Flutter project used, so debug builds upgrade in place. This is not a production signing credential.

For release signing supply all four values together:

- `ANDROID_KEYSTORE_PATH`
- `ANDROID_KEYSTORE_PASSWORD`
- `ANDROID_KEY_ALIAS`
- `ANDROID_KEY_PASSWORD`

Alternatively use the corresponding `storeFile`, `storePassword`, `keyAlias`, and `keyPassword` properties in `packaging/android/key.properties` (ignored by Git). A partial configuration is an error. `PASEO_ANDROID_REQUIRE_RELEASE_SIGNING=1` rejects a release build without production credentials instead of creating debug-signed output.

The app ID stays `sh.paseo.sessions`, separate from the upstream app. Optional Firebase configuration is supplied with `GOOGLE_SERVICES_FILE_SESSIONS`; the wrapper checks that it belongs to this package, stages it as `packaging/android/app/google-services.json` for the Gradle Google Services plugin, and restores the previous state after success, failure or cancellation. Play Store credentials, submission and signing authorization are not included in this source fork.

## Version codes

`xtask/src/common.rs` resolves the build number from `PASEO_BUILD_NUMBER`, then `ANDROID_VERSION_CODE`, then `packaging/android/version.properties` (`versionCode=`); empty variables count as unset. Android clamps it to at least `13000`. `cargo xtask release version` rewrites `version.properties` (it replaced the pubspec `+build` suffix). Set the next code against actual installed/store history before a distribution. Do not reuse the former Expo version-code formula or EAS remote counters.

The repository's `fastlane/metadata/android` store text and changelog tooling are metadata, not evidence of an F-Droid/Play release. The old Expo source-only/F-Droid flags are not an Android build mode for this project. A source-distribution/store build needs its own completed dependency, signing and policy review; no such submission is claimed here.

## Connect to a development daemon

Start an isolated daemon on the development machine:

```sh
PASEO_LISTEN=0.0.0.0:6768 cargo xtask dev daemon
```

Add its host address in the client. A standard Android AVD can reach the machine at `10.0.2.2:6768`; `localhost` inside the emulator means the emulator itself. A physical phone needs a reachable LAN/VPN address or encrypted relay. Use a password for any network-reachable daemon and avoid exposing the port beyond the intended network.

An explicitly configured ADB reverse tunnel can instead forward the daemon port to the device's loopback. There is no Metro port to forward and no `EXPO_PUBLIC_*` endpoint baked into a JavaScript bundle. Select the host connection in the client.

## Navigation and retained features

The compact sidebar is host → directory → session. Back returns from an open file explorer to the session and from the session to the sidebar; leaving the app must not stop remote sessions. Directory collapse is persisted per host/path. Add Project browses folders on the selected host, not Android's local filesystem.

Markdown, source editing, large summary-first diffs, schedules, settings, generic effect-sound playback, interactive terminals on the host's shell and read-only script/setup logs are native client surfaces. Notification permissions and delivery require actual Android testing; desktop execution does not prove Android push delivery.

## Release boundary

`cargo xtask build-gui ios` is a separate macOS/Xcode path; without a provisioning profile and signing identity it produces an unsigned app archive. Native mobile targets do not inherit desktop daemon ownership. Review [release](release.md) for exact signing and artifact rules before publishing.
