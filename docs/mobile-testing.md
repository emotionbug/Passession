# Mobile testing

The mobile client is the native Rust client (Makepad) on Android and iOS. It connects to a separate Rust daemon; it never owns a local daemon process. Test the actual native application, not a web rendering or an old Expo/Metro dev client.

## Build and connect

Use `cargo xtask dev app android --device <adb-serial>` or, on macOS/Xcode, `cargo xtask dev app ios --device <device-id-or-simulator-udid>`. Android installs a debug build of the device's own ABI with `adb install -r`, starts the launcher activity and follows its `logcat`. iOS runs a booted simulator with an ad-hoc signed build (`simctl`); a physical device needs `PASEO_IOS_PROVISIONING_PROFILE` and `PASEO_IOS_SIGNING_IDENTITY` (`devicectl`). With exactly one attached candidate `--device` may be omitted. Artifact builds use `cargo xtask build-gui android`, `cargo xtask build-gui android --arch arm64`, or `cargo xtask build-gui ios`; signing limitations are documented in [Android](android.md) and [release](release.md).

Start a disposable Rust daemon with `cargo xtask dev daemon` or an explicitly isolated home/listen address. A standard Android AVD reaches the host through `10.0.2.2`, not its own loopback. Physical devices require reachable LAN/VPN/relay addresses and matching credentials. Do not expose a passwordless daemon for convenience.

## Behavioral matrix

Exercise compact and wide layouts, keyboard visible/hidden, rotation/resize, foreground/background, disconnect/reconnect, and persisted restart. Record the exact OS/device/build and whether it is a simulator/emulator or hardware.

- Host → directory → session navigation, empty directories, collapse persistence, filtering and same-path isolation between hosts.
- Select an idle session and return to the sidebar. The highlighted row must still show its chat outline. Switch to a running session and return: both rows must keep their outlines, with the running badge over the selected icon.
- With a connected, streaming session at the latest message, open the sidebar. Have a distinctive new row arrive while the sidebar is visible, then immediately reselect the same session. Capture the transition: the first readable conversation frame should include that row, without scrolling or waiting for another event. Test reconnect separately.
- On a connected host, open two saved conversations and return to the first. Each must show its own stored question and formatted answer without another tap or a new message. Repeat from a new-session draft opened through Android's image Sharesheet, without sending the draft. For a long saved reply, check its final paragraph and scroll back to earlier text.
- Repeat the return with a running turn less than an hour old and no incoming messages. Without touching the screen, watch its elapsed label and activity dots for at least six seconds: the seconds must advance without a multi-second pause and catch-up jump.
- Back from file explorer to session, session to sidebar, and leaving the app without stopping remote work.
- Add Project through the selected daemon's remote folder browser.
- Chat streaming, draft retention, Korean IME composition during streaming, permissions, session rename/import/history and closing a tab without archiving. Exercise composing-syllable replacement, backspace, selection, focus transfer and keyboard dismissal, not only pasted Korean text.
- Markdown selection/copy/links, file search/read-only source on mobile, Git Log and lazy historical patch navigation. Source editing/save-failure coverage belongs to desktop; Android and iOS intentionally disable source editing.
- Setup/service scripts and read-only log output. Check natural process exit, retained output after exit, initial tail positioning, touch-scroll pause and explicit Follow without new output.
- Interactive terminals on the remote host: create one from New session → Terminal, type through the soft keyboard and Korean IME, paste, run a full-screen program, rotate, background/resume and reconnect, then close the tab and confirm the daemon terminal is gone.
- Schedules, profiles, connected hosts, and server errors.
- Drag across actionable rows/buttons/links, return to the touch origin, and cancel a gesture. Scrolling must not open a session, follow a link, dismiss a permission, remove an attachment or run a script.
- Open nested pickers over forms; one Back/Escape must close only the top layer. Close a focused search, then open and activate a context menu. Closed or never-drawn inputs must not retain keyboard ownership.
- Type while a composer is covered by a sheet, then return to it: its draft must be unchanged. Keep a synthetic stream running behind another panel and verify its completion when returning.
- Commit selected files through the real daemon, verify unrelated staging and working files remain unchanged, and exercise failure → explicit refresh → retry with a retained multiline message. Verify the new commit in Git Log and its historical patch.
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

### Retained glyphs, overlay layers and touch-release focus

The follow-up API 35 run used rebuilt production widgets with an isolated mock
conversation containing 400 long ASCII code lines and 2,400 distinct Hangul
syllables. No paid provider requests were sent in this pass. Evidence remains
under `/home/emotionbug/.cache/paseo-android-qa-xkwu3602/`:

- `screen-442` records widespread glyph corruption at 200% text size before the
  paged-atlas fix. This reproduced common atlas exhaustion, not the exact
  missing-selected-syllable sequence in the user's screenshot.
- `screen-446`–`448`, `454` and `458` show intact Korean/code text after the fix,
  including selected `나` with the adjacent `눠` still visible. Scrolling across
  the distinct-syllable fixture preserves text and the opaque latest arrow.
  First-time software-rendered rasterization of this stress fixture is slow;
  these screenshots are not a throughput or frame-rate benchmark.
- `screen-461`–`464` cover 100%/200% task/subagent pills, a separate latest
  button and large-text subagent rows. `screen-455` verifies latest navigation.
- `screen-465`–`472` cover the sidebar search border, Settings root/Appearance,
  theme dropdown, Git diff options and file explorer at 200%.
- `screen-478` verifies scaled/wrapped file source text and aligned line
  numbers after correcting its missing system-size adapter. Android source
  files intentionally remain read-only.
- `screen-452`–`453`, `460` and `481`–`482`, with the
  `release-focus-*.txt` input-method captures, cover composer, sidebar search
  and manual file Find. DOWN, CANCEL and outside release do not open the IME;
  a valid inside release does. `screen-483` verifies that held source dragging
  selects text instead of scrolling.

The scoped run passed three atlas allocator tests, three common TextInput
touch-focus tests and four GUI manual-field/source touch tests. The rebuilt
Linux smoke client also accepted keyboard input, selected Korean text with
the mouse, cleared that selection on the same click into sidebar search, and
scrolled to older content and back through the latest arrow. Desktop evidence
is in `desktop-shared-renderer-*.png`; it used private X11/software rendering,
not the user's Wayland session. No physical-device result is inferred.

### Long-press native toolbar follow-up

The same API 35 emulator exposed three remaining faults: inputs deferred the
toolbar until release; an identical-position MOVE after IME relayout collapsed
the selected word; and manual file views requested actions against a parent
area that had not yet been published for the current frame. The common input,
manual editor and parent-area paths now handle those cases. Closing the native
toolbar no longer doubles as an editor blur/IME dismissal.

- `screen-499`–`500` show Cut/Copy/Paste/Select All on the Korean composer,
  first while the finger is still down and then after release.
- `screen-501`–`505` exercise Select All, Copy, Cut, empty-composer long-press
  Paste, and restoration of exactly one `계속 있는데` draft with the IME open.
- `screen-508`–`509` show the read-only source Copy/Select All toolbar before
  and after release. `screen-510` verifies that an outside click still opens
  the sidebar.
- `screen-511`–`513` exercise empty-search Paste, copying `한글` from source
  into search, and the populated-search Cut/Copy/Paste/Select All toolbar while
  the finger remains down. These checks used 200% system text size.
- `screen-516`–`517` verify the manual file Find field's held native Paste
  menu and an actual pasted `한글` query resolving to one match.

The follow-up common input/manual-field scoped run passed seven tests and the
Android debug APK was rebuilt, installed and exercised. Native toolbar evidence
comes from the emulator's actual system UI, not a mocked menu or an automatic
copy-on-selection substitute.

### Shared cursor and selection handles

The API 35 emulator exercised the Rust-owned common handles with native
ActionMode and Magnifier; there is no EditText overlay.

- `screen-538`–`543` cover Korean source selection, both endpoint directions,
  crossing, stationary edge scrolling and same-click outside navigation.
- `screen-550`–`551` cover a virtualized conversation row and selection-owned
  PortalList edge scrolling instead of an independent parent pan.
- `screen-554`–`557` verify full composer insertion/range knobs, capture below
  the text-content clip, a dragged range spanning `계속 있는데`, native
  magnification and toolbar restoration after release.
- `screen-558`–`559` verify the shared sidebar-search knob at 100% and 200%
  system text size. The test restored 100% afterward.
- `screen-561`–`563` hold the finger stationary at the source viewport edge.
  The native lens changes from line 031 to 035 as selection scrolls, then
  disappears and restores Copy/Select All on release. Offscreen endpoints
  do not leave detached knobs.
- `screen-566`–`569` compose `계속` through the actual Korean IME in manual
  file Find, display insertion and native Select All range handles, then
  close Find and verify that handles, toolbar and IME are dismissed.

Runtime checks caught missing Live shader expansion and nested text clipping;
the common renderer now has an explicit reachable Live dependency and an
owner-aligned overlay. A single latest-state UI runnable prevents magnifier
update starvation; stationary source coordinates call `Magnifier.update()`
so scrolling refreshes the native sample.

`cargo test --locked -p paseo-gui -p makepad-platform -p makepad-widgets --lib`
passed 207 tests. The Android debug APK was rebuilt, installed and exercised.
This pass did not exercise physical-device handles, API 24–27 fallback, desktop
touchscreens, log-view gestures or the standalone Markdown preview.

Capture screenshots/logs without unrelated personal data or credentials. Preserve failing output, use bounded waits for observable conditions, and clean up only test-owned app instances, homes and processes. Installation or device changes require the appropriate authorization.

### Session-only navigation and selected-file commits (2026-11-15)

The v0.13.26 debug x86_64 APK ran on an isolated Android 15/API 35 emulator at
720×1568 and 320 dpi, connected to a password-protected disposable native daemon.
Only the development mock provider was enabled. Evidence is retained at
`/tmp/passession-session-git-qk3fp78_/`:

- `04-directory-sessions.png` shows two existing sessions under one host/directory,
  without a workspace grouping layer. `05-session.png` has no nested tab counter.
- `07-supporting-panel.png` shows Files / Git Log / Commit and the real repository's
  uncommitted files. `09-multiline-commit-message.png` shows multiline Enter retaining
  the keyboard, with the editor and Commit button above it.
- A real rejecting Git hook left HEAD, unrelated staging, selection and the two-line
  message intact (`11-rejection-retains-draft.png`). Refresh did not resubmit.
  Explicit retry committed only `selected.txt`; unrelated staged content, different
  unstaged content and the untracked file remained exact (`commit-proof.json`).
- `15-commit-success.png` shows the cleared message and refreshed remaining files.
  Git Log displays the new commit (`16-git-log.png`); its historical patch displays
  the expected old/new content (`20-historical-patch-loaded.png`).
- The directory's plus action opens a local-only form without Isolation/Worktree
  controls (`23-local-session-form.png`). Submitting empty Chat creates an idle
  session directly, retaining both existing sessions (`empty-session-proof.json`).
- The live protocol rejects legacy managed-worktree creation as unsupported
  (`session-api-proof.json`). No existing worktree folder was deleted.

`runtime-proof.json` records the APK SHA-256 and bounded 200-line process-log check;
no fatal exception, fatal signal or Rust panic appeared in that capture. The test
daemon/emulator were stopped and their private credentials, repository and daemon
home removed. Production processes and physical Android devices were untouched.
This run does not establish physical-device, iOS or Windows GUI execution.

### Opaque Git entries and installed-font coverage (2026-11-15)

An isolated Android 15/API 35 x86_64 emulator at 720×1568 and 320 dpi ran the
fixed debug client against a disposable native daemon with only the mock
provider enabled. Evidence is at `/tmp/passession-git-cjk-proof-d6e5dzjp/`.

- A real linked Git worktree under `tmp/actdt-patch-check-fixture/` no longer
  prevents the parent checkout's Commit list from loading. The actual UI
  committed exactly `notes/new.txt` and `workflow.txt`; the nested checkout
  and its private untracked file remained untouched (`commit-ui-proof.json`,
  `android-commit-success.png`).
- Cold opening the multilingual code block discovers installed Android fonts
  asynchronously and refreshes the already displayed layout without reopening
  it (`android-code-fonts-fixed.png`). The historical Git diff also renders
  those scripts (`android-historical-diff-fonts-settled.png`).
- `font-discovery.log` records system faces for CJK, Armenian, Georgian, Thai,
  Devanagari, Arabic, Deseret and Cherokee. This exercises collection faces,
  variable fonts and supplementary-plane input, not a CJK-only fallback list.
- `verification.json` records the APK digest and scoped regression/lint results.
  RTL contextual glyphs no longer become forced squares, but full bidi visual
  ordering is not established by this test.

The test daemon/emulator and their private fixture data are removed after the
run. Production installations and physical Android devices are unchanged.
This does not establish Windows UI or Apple runtime behavior.

## Embedded Tailscale service and single activity (2026-11-16)

A disposable Android 15/API 35 x86_64 emulator (1080×2400, 420 dpi) ran the
`dev-fast` x86_64 APK (SHA-256
`4e1f5cc724daffe68c4a58a422ba1727bf4c5fd14a1fbb64cb990b5a22daaebd`). No daemon,
account login or physical device was involved.

- Launcher, explicit `FLAG_ACTIVITY_NEW_TASK`, a `paseo://` VIEW sent while
  Settings was in front, and a second launcher start all kept exactly one
  `MainActivity` in one task with the package affinity.
- Enabling Embedded Tailscale (the node reached `NeedsLogin`) started
  `PaseoTailnetService` in the foreground (`specialUse`, channel `tailnet`)
  with Pause and Stop actions. Pause showed Paused with Resume, and the app's
  Tailscale page showed the same state. Tapping the notification body returned
  to that same activity and process.
- With the app in the background, the process stayed `fg-service`
  (oom adj 200), and Resume from the notification was applied there. Stop
  removed the service and notification and disabled Embedded Tailscale.
- Removing the task ended the process together with the service. The
  following launch cold-started cleanly and restored the service from the
  saved setting.

That run used the earlier `singleTask` build. The service behaviour above is
unchanged by the `singleTop` launch-mode change below.

### Launch modes and permission flows (2026-11-16)

The same emulator setup ran the `singleTop` + launcher-shaped-intent build
(SHA-256 `d08d568d1b732f0aff824d16ff7e021c4157b14ebf8f50c6531511820dca51f4`)
from a cleared app state.

- No permission prompt appeared at launch.
- Settings › Notifications on the phone: Allow → system prompt. With the system
  prompt open, Home and then the launcher icon returned to that same prompt,
  with one `MainActivity`.
- Refusing once showed "Notifications are off" with Allow. Refusing again
  showed "Open settings", which opened the app's notification settings.
  Allowing there and returning showed Granted without restarting the app.
- A test notification tap and the Embedded Tailscale notification tap returned
  to the one `MainActivity` in its task. A `paseo://` VIEW sent while Settings
  was in front went through `PaseoLinkActivity` (nothing left behind), reached
  the same instance and opened the pairing sheet with the link.
- Turning on Embedded Tailscale showed the in-app explanation before the
  system prompt. Cancelling left the service running with its notification
  hidden, and the Tailscale page showed "Notifications are off" and "Battery
  limits". Allowing from that row made the service notification appear at
  once; Open settings opened the battery screen.
- Scan QR showed the camera explanation, then the prompt. A first refusal
  offered Allow camera again, a second offered Open settings. Allowing in the
  app details and returning started the camera preview.

Not covered: a logged-in tailnet and real host traffic, Android 12–14 and
API 24–28, refused background starts, and a picker opened by the app itself.
macOS, iOS and the Windows `gui-platform` crate were type-checked on Linux;
nothing ran on those systems.

## Photo picker and photo library saves (2026-11-22)

An API 35 x86_64 AOSP emulator ran the x64 dev-fast APK against a disposable
password-protected daemon reached through `10.0.2.2`.

- New session › + › Add image opened the system photo picker
  (`PhotoPickerActivity`), not DocumentsUI. The selected photo arrived as an
  attachment in the composer. Upload file still opened DocumentsUI.
- Files › long-press `diagram-qa.png` › Download showed "Saved to Photos" with
  no location prompt. MediaStore listed it under `Pictures/Passession/` as
  `image/png`, and the pulled copy was byte-identical to the host file.
- Opening Command Center from the compact sidebar, typing `diagram`, and pressing
  Android Back twice dismissed the IME first and the palette second. The app
  stayed in the foreground and a session could be opened from the sidebar.
  The image-picker intent was restricted to `image/*`; a gallery save left
  `is_pending=0` and retained the original PNG bytes.

- Mermaid › Save PNG on API 35 published `diagram.png` in `Pictures/Passession/`.
  The stored PNG decoded to a 132×164 diagram with both nodes and their edge.
- On API 35, Files › Download for `report.txt` opened `CREATE_DOCUMENT` with
  `text/plain`. Cancel left Downloads empty; Save created a byte-identical
  file. Share file opened the Android Sharesheet instead.
- The chat image viewer saved a workspace PNG (1,730 bytes) and a picked,
  sent JPEG (1,858 bytes). The gallery copies matched the source SHA-256 and
  MIME. Sharing the workspace image opened the system Sharesheet.
- A private test provider shared two extensionless URIs, a PNG and a HEIC,
  through the API 35 Sharesheet. Passession asked for a destination. Cancel
  removed the staged files; choosing an existing chat retained both original
  byte streams and left every test-host timeline unchanged. A cold single-image
  share to New session showed an unsent thumbnail without creating a session.
  An existing chat also accepted an unsent attachment while its host was offline.
- An Android 9/API 28 Google APIs emulator opened `GET_CONTENT` with `image/*`
  for Add image. DocumentsUI handled it on that image; selecting the photo
  added it to the unsent draft. Saving a photo opened `CREATE_DOCUMENT` with
  `image/png` and its original name. The copy in Downloads matched the source.

The iOS photo picker and Photos save were type-checked for `aarch64-apple-ios`
on Linux. Add-only permission denial and cancellation still need an iPhone
with a signed build; neither was exercised on hardware.

On a signed iPhone build, start with an existing draft and cancel Add image
both with Cancel and by dismissing the picker. The draft should be unchanged.
With Photos add-only access not yet decided, tap Save on a known PNG and
choose not to allow access. Check for an error, no “Saved” acknowledgement,
and no new asset. These are hardware checks, not results from the cross-build.
