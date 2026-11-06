# Mobile testing

The mobile client is the native Rust client (Makepad) on Android and iOS. It connects to a separate Rust daemon; it never owns a local daemon process. Test the actual native application, not a web rendering or an old Expo/Metro dev client.

## Build and connect

Use `cargo xtask dev app android --device <adb-serial>` or, on macOS/Xcode, `cargo xtask dev app ios --device <device-id-or-simulator-udid>`. Android installs a debug build of the device's own ABI with `adb install -r`, starts the launcher activity and follows its `logcat`. iOS runs a booted simulator with an ad-hoc signed build (`simctl`); a physical device needs `PASEO_IOS_PROVISIONING_PROFILE` and `PASEO_IOS_SIGNING_IDENTITY` (`devicectl`). With exactly one attached candidate `--device` may be omitted. Artifact builds use `cargo xtask build-gui android`, `cargo xtask build-gui android --arch arm64`, or `cargo xtask build-gui ios`; signing limitations are documented in [Android](android.md) and [release](release.md).

Start a disposable Rust daemon with `cargo xtask dev daemon` or an explicitly isolated home/listen address. A standard Android AVD reaches the host through `10.0.2.2`, not its own loopback. Physical devices require reachable LAN/VPN/relay addresses and matching credentials. Do not expose a passwordless daemon for convenience.

## Behavioral matrix

Exercise compact and wide layouts, keyboard visible/hidden, rotation/resize, foreground/background, disconnect/reconnect, and persisted restart. Record the exact OS/device/build and whether it is a simulator/emulator or hardware.

- Host → directory → session navigation, empty directories, collapse persistence, filtering and same-path isolation between hosts.
- Back from file explorer to session, session to sidebar, and leaving the app without stopping remote work.
- Add Project through the selected daemon's remote folder browser.
- Chat streaming, draft retention, Korean IME composition during streaming, permissions, session rename/import/history and closing a tab without archiving. Exercise composing-syllable replacement, backspace, selection, focus transfer and keyboard dismissal, not only pasted Korean text.
- Markdown selection/copy/links, file search/read-only source on mobile, and large summary-first diff navigation. Source editing/save-failure coverage belongs to desktop; Android and iOS intentionally disable source editing.
- Setup/service scripts and read-only log output; no interactive terminal emulator. Check natural process exit, retained output after exit, initial tail positioning, touch-scroll pause and explicit Follow without new output.
- Schedules, profiles, connected hosts, and server errors.
- Drag across actionable rows/buttons/links, return to the touch origin, and cancel a gesture. Scrolling must not open a session, follow a link, dismiss a permission, remove an attachment or run a script.
- Open nested pickers over forms; one Back/Escape must close only the top layer. Close a focused search, then open and activate a context menu. Closed or never-drawn inputs must not retain keyboard ownership.
- Type while a composer is covered by a sheet, then return to it: its draft must be unchanged. Keep a synthetic stream running behind another panel and verify its completion when returning.
- Refresh Git after changing file content without changing the added/deleted line counts; the patch must not remain stale.
- Effect-sound playback, audio interruptions and recovery, notification permissions/taps, and QR pairing on the actual mobile platform.

Use current native semantic controls and fresh screenshots/observations. Former React test IDs, Maestro flows, Agent Device scripts, Flutter integration tests and native-module patches from retired clients are not executable acceptance instructions. A source-only change cannot demonstrate an OS permission sheet, app lifecycle callback or device audio path.

## Evidence boundaries

Rust unit tests prove their in-process contract. They do not prove the JNI/Objective-C bridges, signed install/upgrade, push delivery or device audio playback.

The 2026-11-06 Android 15/API 35 x86_64 emulator run used the actual native APK, two disposable password-protected daemons, a synthetic Git workspace and the mock provider. OpenWnn's Korean Dubeolsik IME performed real composition. This is not paid-provider or physical-device verification.

| Surface | Observed result |
|---|---|
| Korean input | Composition/backspace/commit in session search, chat, profile and file search; new session created from `한글 검수`; draft retained after restart, rotation and background/resume |
| Touch gestures | Sidebar, timeline, file/Markdown, logs, settings and schedule scrolling; drags over permission Dismiss, attachment removal, links and script Run did not activate them; deliberate taps did |
| Hosts and sessions | Numeric port editing, direct authentication, two hosts with the same project path, independent session groups and restored selection |
| Chat and attachments | Synthetic streamed output, image sending, Android document picker upload with exact server-side bytes, permission dismissal without approval |
| Files and Git | File tree, Korean search, two-stage keyboard/search Back, Markdown local links, working diff and empty committed scope; no commit, push or staging |
| Scripts and logs | Finite 240-line script finished with exit code 0; completed output remained available; initial tail, touch pause and Follow-to-tail worked |
| Settings and schedules | Light/System theme and system-bar contrast, numeric fields, Korean profile save/cancel, future schedule list, action sheet, scrollable editor/cancel and empty run history |
| Layout and lifecycle | Portrait/wide rotation, visible active tab after overflow, attachment menu bounds, settings/explorer Back, cold restoration, Home/resume, disconnect indication and automatic daemon reconnection |

Local screenshots are under `/home/emotionbug/.cache/paseo-android-qa-xkwu3602/`; the corresponding tool transcript records individual REA evidence IDs. Directory labels alone are not assertions: use the rendered screenshot and accompanying observation. The initial uninstrumented v22 pass ends at `screen-239`, including `screen-233` (visible active tab), `screen-235` (foreground restoration), `screen-238` (scroll without session activation) and `screen-239` (real Korean input). `final-runtime-logcat.txt` and `software-render-timing.txt` retain that initial pass's health/performance observations.

The SwiftShader debug run sometimes spent roughly two seconds rendering a frame. The emulator process also exited during the run (exit 134, later 139); one test build exhausted host disk space. Only test-owned x86_64 incremental caches were removed, the existing AVD was recovered, and final tab/lifecycle verification used the supported `swangle` backend. These environment failures are retained, not counted as passing runs or evidence of hardware performance.

Unverified: older Android versions, other keyboard implementations, physical-device behavior/performance of these fixes, iOS, camera/QR pairing, notification delivery/taps, audio interruptions, paid live providers, session import and destructive/external Git actions. The physical phone was restored to its original Flutter APK; it was not used for this emulator acceptance pass. See [testing](testing.md) for test and earlier build boundaries.

### Input ownership follow-up

The same isolated API 35 emulator exposed additional failures through actual interaction:

- One Escape closed both a schedule Host picker and its parent form (`screen-247`–`248`). Key-down dismissal now belongs to the top modal; the parent remains open (`screen-295`–`296`).
- Closing command search left a keyboard and blocked later context-menu interaction. Empty sweep locks are rejected; `Area::Empty` cannot own focus, and Android accepts only valid editor areas. Temporary native tracing observed never-drawn fields registering empty editors after blur; that instrumentation was removed. Real Hangul search → Escape now hides the keyboard (`screen-288`–`289`), and the following project menu opens and activates (`screen-290`–`291`).
- A covered composer no longer accepts characters behind its model sheet. Model-picker Back returns to Controls before closing it; the Korean draft remains unchanged (`screen-302`–`305`).
- The project settings form exceeded a compact viewport and clipped its left edge (`screen-277`–`278`). It now sizes from the current allocation (`screen-291`). Dragging its script row scrolls without opening the editor; a deliberate tap opens it (`screen-292`–`293`).
- The Schedules sidebar button changed an underlying destination without leaving the full-screen page. It now closes the compact page and reveals the sidebar (`screen-299`–`300`).
- A synthetic 59-second stream completed while the timeline was hidden behind Files; returning showed its completion and persisted output (`screen-306`–`312`). File loading and touch scrolling remained functional.
- Git Refresh retained an old patch when an external edit kept the same `+1/-1` summary (`screen-313`–`315`). Successful checkout actions now invalidate working-patch caches and in-flight identities while keeping offscreen patches lazy. The final v32 APK displayed the new text with unchanged counts (`screen-317`–`318`) and retained the next refreshed value across hide/show (`screen-319`). The existing summary-only server subscription carries no content revision; same-count external edits still require explicit Refresh rather than guaranteed automatic publication.

The routing changes separate interactive input from focus cleanup and background responses. Retained hidden timelines defer presentation work without dropping responses; composer requests share an in-flight provider lookup per host/directory; unrelated session timers do not trigger composer processing. Git/file panels redraw on their actual dirty state rather than every timer/signal. Unit tests cover provider-request ownership/reconnection and stale focus clears; these are not measured CPU, network-request-count or battery improvements. Desktop multi-window pointer locking and desktop shortcut-capture teardown were not exercised by this Android run.

The final follow-up capture ends at `screen-319`. `empty-focus-probe-logcat.json` retains the temporary diagnosis; `event-ownership-final-logcat.json` retains the final APK process's bounded 100-line log. No fatal exception, panic, fatal signal or probe marker appeared in that capture. It does retain software-renderer configuration/SurfaceSyncGroup warnings and missing Firebase default-options configuration; push delivery remains unverified.

Capture screenshots/logs without unrelated personal data or credentials. Preserve failing output, use bounded waits for observable conditions, and clean up only test-owned app instances, homes and processes. Installation or device changes require the appropriate authorization.
