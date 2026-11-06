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

### Common input, scrolling and Settings Back

The next isolated API 35 x86_64 pass used the clean internal v37 debug APK, not
a new published release. Evidence remains under
`/home/emotionbug/.cache/paseo-android-qa-xkwu3602/`:

- Shift-assisted OpenWnn composition produces `계속 있는데` in the composer,
  session search and rename prompt (`screen-360`, `367`, `373`). Hardware
  `xyz` and a visible Latin soft `q` preserve the existing Hangul prefix
  (`screen-374`, `387`–`388`). Rename was cancelled without changing the session.
- The full Latin keyboard, composer and modal fit without duplicate keyboard
  avoidance or status-bar overlap (`screen-368`, `387`–`388`). Sidebar search
  retains its complete border at the measured Hangul field height (`screen-360`).
- Sidebar motion continues after release without selecting a session
  (`screen-363`–`364`); reversal returns toward the beginning (`screen-372`).
  Settings scrolling moves the list without opening a row (`screen-383`).
- Stationary long-press followed by drag expands text selection while the
  timeline stays still and does not falsely detach from latest (`screen-356`).
- Android Back moves General → Settings root → sidebar (`screen-357`–`359`),
  and also closes the scrolled root Settings page (`screen-383`–`384`).
- A 351-row mock history crosses the initial 200-row tail boundary without
  repeatedly fetching/climbing. The content viewport in `screen-380` and
  `381`, captured two seconds apart after prepend, is pixel-identical.
  Jump-to-latest returns to messages 346–350 (`screen-382`).

Six SDK-only instrumentation cases against the installed production Java classes
passed (`shared-ime-six-regressions.txt`): Shift composition, native navigation,
delayed open-batch acknowledgements, no-op batch corrections, replacement/UTF-16
backspace and hardware insertion after a composing prefix. The last case first
failed with expected `한a`, actual `a` (`ime-hardware-prefix-before.txt`).
Host regressions passed: ten shared motion tests, two measured-anchor tests,
six history-hold tests and two inset tests. The six history tests were rerun
after the selection-ownership change.

The final APK is 223,030,137 bytes, SHA-256
`90f0bea4ba802094bcf43d5a7723499451fbd40857e0f793169802a3894459c4`.
All nine Java vendor digests match and the regenerated patch applies to the
verified upstream baseline. The bounded final 100-line process capture
(`shared-input-final-logcat.json`) contains no fatal exception, fatal signal,
panic or temporary IME probe marker. The original emulator IME was restored,
the QA instrumentation uninstalled and owned Android runtimes/scaffolds removed.

OpenWnn runs in hardware-keyboard mode on this emulator; this is actual IME
composition, not proof for Samsung/Gboard's full soft Hangul layout. Latin
soft-key input and keyboard geometry were exercised separately. No physical
device, hardware frame-rate or battery improvement is claimed.

### Native text actions and system text size

The next Android pass used the production Java/Rust classes on the API 35
`emulator-5580`, with an isolated daemon and SDK-only instrumentation driver.
Evidence is retained under
`/home/emotionbug/.cache/paseo-android-qa-xkwu3602/`:

- `screen-402`–`403`: real Android floating Cut/Copy/Paste/Select All, followed
  by Paste-only on an empty input. Copy preserves the draft, Cut removes the
  selected text, and Paste restores Korean and a UTF-16 emoji exactly once.
- Production `InputConnection` edits retain composition across Shift for
  `계속 있는데`. These operations exercise the actual editor bridge, not the
  full Samsung/Gboard soft keyboard.
- `screen-406` and `417`: read-only text exposes Copy/Select All only; selecting
  all copies all 80 paragraphs, including offscreen content. A held drag selects
  paragraphs without scrolling. Releasing selection does not automatically copy.
- `screen-418`: one hamburger tap clears selection and the active native menu
  and opens the sidebar. Android Back hides the IME first when it is open;
  another Back dismisses the text-action menu.
- `screen-412`–`414`: live 150% → 200% → 100% system text-size changes resize
  an existing Korean draft without changing density, text or saved app sizes.
  The common TextInput layout cache now includes font/width/wrap metrics.
- `screen-415`–`416`: a 650-pixel/170-ms flick reaches substantially older
  paragraphs; a slower 550-pixel/1600-ms reverse drag tracks directly without
  activating a row. This is behavioral evidence, not a hardware benchmark.
- `screen-420`–`421`: multiline English/Korean/emoji subagent titles remain
  single-line and separate from their model/status subtitles at 100% and 200%.
- `screen-423` exposed missing scaling in the shared Settings form toolkit.
  After correction, `screen-427`–`430` show scaled root/General/Appearance
  controls, complete dropdown rows, wrapped hints and editable numeric fields
  at 200%, with measured row/field heights rather than fixed clipping boxes.

The final scoped run passed 40 Rust tests: 11 GUI regressions, 10 platform
input/ownership regressions, 18 common touch-motion regressions and one Android
font-curve regression. The rebuilt debug APK was installed and these actual
surfaces exercised. No full-suite, physical refresh-rate, native Wayland or
Windows UI result is inferred from this pass.

Capture screenshots/logs without unrelated personal data or credentials. Preserve failing output, use bounded waits for observable conditions, and clean up only test-owned app instances, homes and processes. Installation or device changes require the appropriate authorization.
