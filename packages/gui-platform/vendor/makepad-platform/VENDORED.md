# Vendored makepad-platform 1.0.0

Source: the published crate `makepad-platform 1.0.0` from crates.io
(`makepad-platform-1.0.0.crate`, SHA-256
`261157075e9bdd494925fe5dc1650828c8c047f087b413d00f0e3e8e650eb7b8`), extracted
unchanged except for the files listed below. `Cargo.lock` from the archive was
dropped; the workspace lockfile resolves dependencies. `LICENSE` is the MIT
license of the upstream repository (makepad/makepad, the crate is MIT OR
Apache-2.0).

The root `Cargo.toml` applies it with
`[patch.crates-io] makepad-platform = { path = "packages/gui-platform/vendor/makepad-platform" }`
and excludes this directory from the workspace members.

## Why

Passession supports Android 7.0 (API 24) and later, the floor of the client
it replaces. makepad-platform 1.0.0 links NDK libraries that the API 24 sysroot
does not provide, so the Android library could not link with `minSdk 24`:

| Library / symbol | First API | Change |
|---|---|---|
| `libaaudio` (all `AAudio*`) | 26 | resolved with `dlopen` at first use (`aaudio_sys.rs`) |
| `libamidi` (all `AMidi*`) | 29 | resolved with `dlopen` at first use (`amidi_sys.rs`) |
| `libnativewindow` `ANativeWindow_acquire/release` | 26 | linked from `libandroid` (exported there since API 9) (`acamera_sys.rs`) |
| `AChoreographer_postFrameCallback64` | 29 | resolved with `dlsym` like the existing API 33 path (`ndk_sys.rs`, `android_jni.rs`) |

`src/os/linux/android/ndk_dynamic.rs` generates wrappers with the original
signatures. When a library is missing the wrapper returns the library's own
"unavailable" error (`AAUDIO_ERROR_UNAVAILABLE`, `AMEDIA_ERROR_UNSUPPORTED`), so
callers follow their existing error paths. On API 24–28 the render loop uses
the existing paced loop instead of the Choreographer.

Paseo's own audio output on Android uses `android.media.AudioTrack` through
`paseo-gui-platform` on every Android version, so app audio does not depend on
AAudio being present. MIDI is unused by Paseo.

## Window-scoped desktop input

Desktop backends preserve the originating `WindowId` through
`Cx::call_window_event_handler`; a scoped native key, text or clipboard event
cannot become a broadcast to every retained window. OS keyboard focus is tracked
independently of the last pointer position or editable field. Window generations
remain part of identity so a delayed event cannot target a replacement window.

Each `CxWindow` retains its editable/previous focus references across native
focus changes. Area remapping updates inactive windows too; editor closure,
window closure and slot reuse retire the corresponding references. Area-level
keyboard hit testing checks the same source scope, so an old global text focus
cannot consume another window's native input.

`KeyEvent`, `TextInputEvent` and the existing headless input payloads retain their
serialized shapes. An unscoped headless keyboard event is admitted only when its
live destination is unambiguous; multi-window synthetic callers use the explicit
scoped entry point. Lifecycle and non-input notifications remain broadcasts.
The adjacent widgets patch applies this scope before navigation and overlays,
not merely before the window's content view.

## Windows clipboard ownership

`win32_window.rs` opens the clipboard with a real window owner and closes it on
every exit. Published UTF-16 uses `GMEM_MOVEABLE`; Windows takes ownership only
after `SetClipboardData` succeeds. Failed publication frees the allocation.
Reads stop at the first UTF-16 NUL, not at the global allocation's padded end.
Missing text formats and failed locks do not dereference null or unwind through
the window procedure. `GlobalUnlock` returning zero on the final successful
unlock is not a Rust error to unwrap.

## Windows input-method composition

Upstream ignored input-method compositions on Windows: the input method drew the
text being composed (a Hangul syllable before it is committed) in its own window at
the window's top-left corner, and the editor received it only as `WM_CHAR` once
committed. Windows now composes inline through the same full-editor snapshots
(`TextImeEvent`) that Android uses, so every editor that handles `Hit::TextIme`
shows the composition in place and groups it in its own undo history.

- `update_text_input`, `update_terminal_text_input` and `clear_text_input` mirror
  the focused editor in `ime_composer.rs` (`ImeComposer`, Windows and tests only).
- While an editor is mirrored, `win32_window.rs` handles `WM_IME_COMPOSITION` and
  `WM_IME_ENDCOMPOSITION` itself (result, composition and its caret through
  `ImmGetCompositionStringW`), and does not pass them to `DefWindowProc`, so no
  `WM_IME_CHAR`/`WM_CHAR` repeats committed text. `WM_IME_SETCONTEXT` hides the input
  method's composition window; candidate lists still show.
- `windows.rs` turns each step into snapshots of the mirror, delivered only while
  the mirrored editor has key focus. A republished editor that differs from the
  mirror while composing (a click, other typing, filtered input, another editor)
  ends the composition where it stands: the input method's composition is discarded
  (`ImmNotifyIME` `CPS_CANCEL`; its re-entrant notifications are dropped) and a
  still-focused editor receives a snapshot without a composing range. Blurring the
  editor discards the composition the same way; the editor keeps the text.
- `ime_composer` unit tests cover Hangul syllables committed into the next one,
  clearing and cancelling, selection replacement in UTF-16 units, and interruptions.
  They are not evidence that a particular Windows input method works.

Candidate lists follow the caret: `CxOsOp::ShowTextIME` places the composition form
(`CFS_POINT`) at the caret's top-left, keeps candidates off the caret line
(`CFS_EXCLUDE`) and sizes the composition font to the caret, keeping the input
method's own face. The spot is re-applied on `WM_IME_STARTCOMPOSITION`; only the
newest IME op of a batch is applied, because ops pop newest first, and an unchanged
spot is not re-sent each frame. `Win32_UI_Input_Ime` and `Win32_Globalization` are
enabled for these calls.

`Cx::show_text_ime_caret(area, caret)` passes the caret's rect relative to `area`
and records its height (`Cx::ime_caret_height`); `show_text_ime(area, pos)` clears
it, so platforms fall back to a 16-pixel line.

## Windows swap-chain background colour

`os/windows/d3d11.rs` ignores the result of `IDXGISwapChain1::SetBackgroundColor`
instead of unwrapping it. The colour is cosmetic; Wine's DXGI returns `E_NOTIMPL`,
and the unwrap aborted window creation there, so the Windows build could not be
exercised under Wine.

## Android Surface lifetime

`os/linux/android/android.rs` gates drawing and `SurfaceTexture.updateTexImage`
on a live window Surface. `SurfaceDestroyed` unbinds EGL, but queued RenderLoop
messages and background model updates can still arrive. Those events must not
consume redraw/next-frame work, clear upload dirty bits or allocate GPU objects
until `update_surface` has made the retained EGL context current again.

The installed 0.13.10 APK's crash PC `0x27bebe8` is the indirect
`glDrawElementsInstanced` call. The device's Adreno stack then copies from null
in `memcpy`. An isolated host EGL run of the drawing handler and buffer uploader
reproduced the missing-context state corruption: the old path cached `Some(0)`
and cleared upload state; the gated path deferred work and uploaded after
restoration. This establishes the lifecycle defect, not the device's missing
pre-crash event sequence or an on-device confirmation of the fix.

## Android composing input

`event/keyboard.rs`, `event/event.rs`, `event/finger.rs`, `cx_api.rs`, and
`os/linux/android/{android,android_jni}.rs` bridge the packaged Java full-editor
InputConnection. Ship those Rust changes and `packaging/android/makepad` together:
the upstream key-character JNI alone cannot carry Hangul composing replacements.
JNI strings use UTF-16, including supplementary characters; widget offsets remain
UTF-8. Editor identities distinguish multiple fields sharing one drawn area.
Session changes fence stale connections; revision acknowledgements cannot replace
newer IME text. Focused custom editors must publish via `update_text_input` after
edits and during drawing, and handle `Hit::TextIme` through their own undo model.
The focused-area reference follows Makepad redraw area remapping.
`update_terminal_text_input` publishes the same editor as a terminal kind: the Java
`EditorInfo` drops `CAP_SENTENCES` and sets `NO_SUGGESTIONS`, so shell input is not
auto-capitalized or autocorrected while composing IMEs keep working. Changing the
kind restarts the input session like the password and numeric flags.

The `android_ime_tests` unit tests cover replacement boundaries and offset
conversion. They are not evidence that a particular Android keyboard works.

Editor selection updates do not restart the keyboard. Focus transfer publishes the
next editor before deferred blur hides the IME; disposing a focused field clears
its native editor. API 33+ invalidates changed text, while API 24–32 uses the
guarded restart path. Showing the keyboard is a focus/tap action, not a caret-blink
side effect. Android Back is delivered to the owning modal/editor before shell
navigation.

`clear_key_focus_if` releases an editor only when it still owns the pending
focus request. A closing/hidden field cannot overwrite a newer field's focus.
`Area::Empty` never owns keyboard focus. Android also rejects invalid editor
areas before publishing native state. Undrawn fields must not register phantom
editors when the real field blurs, restart the IME or cancel its pending hide.
The focus regression covers initial state, pending ownership, activation and blur.

## Native text-action ownership

`event/keyboard.rs`, `event/event.rs`, `event/finger.rs`, `cx_api.rs` and
`os/cx_shared.rs` share selection ownership, action capabilities and request
tokens. Copy/Cut/Paste/Select All are delivered only to the current owner.
Outside pointer-down clears the previous selection before normal dispatch,
without consuming the new target's click. Focus loss reaches retained editors
even when their old area is no longer drawable.

The Android JNI bridge presents the packaged Java floating ActionMode and
uses UTF-16 for clipboard strings. Anchor areas follow redraw remapping and
resolve from visible painted instances after layout/alignment/clipping, not
from provisional draw-time rectangles. Hidden anchors dismiss the native menu;
stale tokens cannot edit a different owner. `TouchUpdateEvent::cancelled`
preserves Android ACTION_CANCEL through common gesture dispatch.

Native toolbar dismissal only retires its action request. It does not blur the
editor or hide its keyboard when Android cannot offer an action (for example,
an empty input with an empty clipboard). Outside-pointer and lifecycle focus
teardown remain separate selection-invalidating operations.

## Shared text-handle routing

`event/text_handle.rs`, the keyboard ownership registry and `os/cx_shared.rs`
route `Event::TextHandle` before ordinary touch updates. Registration follows
redraw area remapping; the captured touch and endpoint remain stable across
selection crossing. One controller supplies slop, bounded elapsed-time edge
scrolling, lifecycle cancellation and magnifier requests for every text owner.
The crate root reexports the event/action/endpoint/phase/target types.

Android JNI connects these requests to the packaged Java API-28 Magnifier.
Selection and editing remain Rust-owned; this does not add a native EditText.
Native toolbar presentation is suspended during capture and resumes through
the owning widget after a successful release.
Magnifier updates use one latest-state UI runnable, refresh stationary sampling
during edge scrolling, and constrain the sample to the visible owner viewport.

## Touch activation and draw ordering

`event/finger.rs` retains movement cancellation once a touch reaches the tap
slop, even if it returns to its origin. `was_tap` excludes cancelled and long-press
gestures. Android `ACTION_CANCEL` releases captures without synthesizing clicks.
Scroll consumers keep incremental movement after crossing their pan threshold.
Sweep locks reject `Area::Empty`: opening a modal before its first draw must
not leave an ownerless lock that blocks later menus. The touch regression
checks empty acquisition, real ownership and release.

`draw_vars.rs` and `draw_list.rs` support explicit draw-call groups so distinct
text fields and overlay layers cannot merge primitives across each other's
backgrounds. Draw-call/list uniform slices count floats, not bytes; their tests
bound the exposed storage. The GLES uniform upload uses each mapping's actual
slot count rather than padded uniform-buffer storage.

`texture.rs` exposes `Texture::is_unique` for the adjacent draw crate's paged
font atlas. Retained draw-call/DrawVars texture clones prevent recycling a page
whose glyph UVs are still in use; this does not claim immediate GPU deallocation.

The x86_64 Android path uses the libc thread-ID API instead of an ARM-specific
syscall number. OpenXR string pointers use the target's `c_char` signedness
without copying their backing strings.

The GLES shader diagnostic reader uses initialized storage and the driver's
written length, including zero-length and non-UTF-8 logs. A rejected cached
program with no log previously crashed in `strlen`; the API 35 emulator
reproduced this on startup. Source recompilation now proceeds without clearing
the cache manually. Failed and retired linked programs use `glDeleteProgram`,
not the shader-object deletion API.

## Adjacent shader compiler patch

The root also patches `makepad-shader-compiler 1.0.0` from the published archive,
SHA-256 `8bae5f220762bc75344b4050cbc12428e39a04faa9b2ea80f90edccca7985bca`.
Its archive metadata identifies upstream revision
`a1868e19d3bb955200957efb3c880791a43424eb`, dirty source, path
`platform/shader_compiler`. The adjacent vendor directory retains the upstream
MIT license. `src/generate.rs` emits GLSL floating-point literals without the
unsupported ES 1.00 `f` suffix. Android shader generation inlines constants
instead of exhausting the GLES uniform limit. These changes were exercised by
the API 35 emulator's real native renderer, not merely shader-source checks.
Methods returning borrowed checker/analyser structs (`analyse.rs`, `ty_check.rs`,
`swizzle.rs`) spell their elided lifetime (`TyChecker<'_>`), as Rust's
`mismatched_lifetime_syntaxes` lint asks; behavior is unchanged.

## Packaged builds do not poll live files (GuiShell)

`src/live_cx.rs` `start_disk_live_file_watcher` returns immediately when
`MAKEPAD_PACKAGE_DIR` is set at compile time. xtask sets it for every staged
desktop build (`build-gui`, `dev app`, `--smoke`); upstream then spawned a
thread that re-read every live source file every 100 ms although a bundle ships
no sources. Plain `cargo run` keeps live reloading. Input, redraws and studio
live edits are unchanged.

## Hiding and showing one window (GuiShell)

Upstream `CxOsOp::HideWindow` and `CxOsOp::Deminiaturize` were `todo!()` on X11
and Windows. The desktop shell keeps the first window alive but hidden after
the user closes it while other windows stay open (the original kept that
engine dormant and reused it for the next New Window), so both ops are
implemented:

| Op | X11 (`xlib_window.rs`, `linux_x11.rs`) | Windows (`win32_window.rs`, `windows.rs`) | macOS (`macos_window.rs`) |
|---|---|---|---|
| `HideWindow` | `XWithdrawWindow` | `ShowWindow(SW_HIDE)` | unchanged (`orderOut:`) |
| `Deminiaturize` | `XMapRaised` + EWMH `_NET_ACTIVE_WINDOW` | `SW_RESTORE` when iconic, else `SW_SHOW`, then `SetForegroundWindow` | `deminiaturize:` then `makeKeyAndOrderFront:` |

`x11_sys.rs` gains the `XWithdrawWindow` and `XMapRaised` bindings.

### macOS zoom and native fullscreen (PlatformNative)

`MaximizeWindow` / `RestoreWindow` now use `isZoomed` and guarded `zoom:`,
not `toggleFullScreen:`. Native fullscreen remains a separate operation:
`FullscreenWindow` / `NormalizeWindow` enter/exit its Space without changing
the meaning of maximized.

The macOS-only `WindowGeom.is_maximized` reports AppKit zoom.
`is_fullscreen` still reports native fullscreen; `is_fullscreen_transitioning`
covers entry/exit animations, including failure callbacks, so their temporary
bounds cannot replace saved normal geometry. Shell geometry persistence skips
those fullscreen states and saves outer bounds to match `setFrame:` on restore.
The macOS operation queue is LIFO: the shell enqueues zoom before position/size
so AppKit captures the restored normal frame as its unzoom destination.


## X11 clipboard on a fresh X server (PlatformNative)

`XlibAtoms::new` in `os/linux/x11/xlib_app.rs` interned `UTF8_STRING` with
`only_if_exists`. On an X server where no client had interned it yet (a fresh
Xvfb, a minimal session) the atom was `None`, which broke both directions:

- Paste: Ctrl+V sent `XConvertSelection` with target `None`, a `BadAtom`, and
  Xlib's default error handler exits the process.
- Copy: the `SelectionRequest` handler advertised `None` in `TARGETS` and
  refused every `UTF8_STRING` request, so other applications never received
  the copied text.

The atom is now created like every other atom. Separately, the
`SelectionNotify` handler read the reply property unconditionally; the
property is `None` when the owner refuses the conversion (no clipboard owner,
or an owner without a text target, such as an image-only clipboard), and
reading it was another `BadAtom`. That reply is now ignored, as a paste with
nothing to insert.

No other atom in `xlib_app.rs` or `xlib_window.rs` uses `only_if_exists`.

## X11 event loop blocks when idle (GuiShell)

Upstream, an idle window kept the GUI thread at ~90–97% CPU. There were two causes:

- `XlibEvent::Paint` in `os/linux/x11/linux_x11.rs` always returned `EventFlow::Poll`, so
  the loop painted again straight away. It now returns `EventFlow::Wait` when no next frame,
  redraw, platform op or dirty pass is pending.
- `SelectTimers::select` in `os/linux/select_timer.rs` also selected fd 0. Under
  `xvfb-run`, desktop launchers and systemd, stdin is at EOF or `/dev/null` and always
  readable, so `select` returned at once. Only the X connection fd is selected now.

Timers, signals and input wake the loop as before. Idle CPU measured on Xvfb: 1–2%.

## Linux timer cancellation (GuiShell)

`SelectTimers` in `os/linux/select_timer.rs` keeps pending timers as a delta list: each
entry waits relative to the one before it. Upstream `stop_timer` assumed only the head is
ever stopped and dropped the entry, so stopping any other pending timer moved every later
timer earlier by its delay. A toast replaced within its lifetime, a restarted debounce or a
cancelled hover timer could therefore fire almost at once. A stopped timer now hands its
delay to its successor; a fired head is popped without that hand-off because its delay is
already consumed. A unit test in the same file pins the deadlines after stops.

## Native window chrome (GuiShell)

Desktop windows use each OS's own title bar instead of Makepad's drawn chrome.

- Windows (`os/windows/win32_window.rs`, `win32_app.rs`): windows use
  `WS_OVERLAPPEDWINDOW`. The `WM_NCCALCSIZE`/`WM_NCHITTEST` overrides that hid the
  frame and hit-tested a drawn caption are gone, so the OS draws the caption, buttons,
  resize borders and Snap. `WM_SETCURSOR` over the client area re-applies the app's
  cursor after a border showed a resize cursor; only the shape changes, not the show
  count. Creation sizes the client area (`set_inner_size`), and `WindowGeom.inner_size`
  is the client size even when maximized. `WM_DPICHANGED` applies Windows' suggested
  rectangle. `set_position` takes physical pixels like `get_position` and creation;
  upstream scaled it again, so restored windows drifted above 100% scaling.
- Windows window class: the icon is the executable's icon resource 1 (large and small),
  falling back to the stock logo. Upstream always used `IDI_WINLOGO`, so the caption,
  taskbar and Alt+Tab showed the Windows logo.
- macOS (`os/apple/macos/macos_window.rs`): a standard titled window. Upstream used a
  full-size content view under a transparent, untitled bar, so the traffic lights sat
  on top of the app's header.
- `Cx::set_window_dark_appearance(dark)` makes the chrome follow the app's own theme:
  `DWMWA_USE_IMMERSIVE_DARK_MODE` on every Windows caption (new windows at creation;
  existing ones redraw their frame), `NSApp.appearance` (Aqua/DarkAqua) on macOS, and
  `_GTK_THEME_VARIANT` on X11 windows for GTK-based window managers. X11 already kept
  server-side decorations (`custom_window_chrome: false`). Before the first call each
  OS keeps its default.

## X11 window geometry

Upstream `XlibWindow::set_inner_size` and `set_position` were empty, so the
platform operations used for saved geometry and compact-window transitions did
nothing. They now call `XResizeWindow` and `XMoveWindow`, followed by `XFlush`.
Resize uses logical size × DPI, matching window creation and `get_inner_size`;
position remains in X11 pixels, matching creation and `get_position`.

## Desktop file drops (X11, Windows)

Dragging files from the desktop onto a window did not work on Linux at all and only
partly on Windows.

**X11** (`os/linux/x11/xdnd.rs`, replacing the stub `Dnd` in `xlib_window.rs`; `xlib_app.rs`
dispatch; `x11_sys.rs` gains `XTranslateCoordinates`). Upstream announced `XdndAware`,
answered every `XdndPosition` with "accepted, `XdndActionPrivate`" and requested the
selection on `XdndDrop`, but never read it, never sent `XdndFinished` and never produced a
`Drag`/`Drop` event. The module is now a complete XDND version 5 target:

- the file list (`text/uri-list`) is requested on the first position, so widgets know the
  files while the pointer moves; until it arrives the drag is accepted provisionally;
- every position is an `Event::Drag` and the widgets' `DragResponse` is the `XdndStatus`
  (`XdndActionCopy`/`Move`/`Link`, or refused: the source shows "no drop" and leaves);
- `XdndDrop` is an `Event::Drop`; `XdndFinished` reports whether a widget handled it;
- `XdndLeave` and the end of a drop are `Event::DragEnd`;
- root positions are translated to the target window with `XTranslateCoordinates` and
  scaled like its mouse events;
- all three inline types of `XdndEnter` are read (upstream skipped the third);
- `file_paths_from_uri_list` parses CRLF/LF lists, comments, `file:///`, `file://host/`
  and `file:/` forms and percent-decodes UTF-8 names (unit-tested).

A list too large for one property (`INCR`) is not read.

**Windows** (`os/windows/droptarget.rs`, `dropfiles.rs`, `win32_window.rs`,
`windows.rs`). Upstream discarded a drag of more than one file in `DragEnter` (nothing
was ever delivered), decoded names one UTF-16 unit at a time, unwrapped the data
object's `EnumFormatEtc`/`GetData` results inside the COM callback, never wrote the drop
effect back (the source kept showing "allowed" everywhere and received its own allowed
effects, possibly *move*, as the result), and never sent `Event::DragEnd`. Now:

- `CF_HDROP` is requested directly and read with `DragQueryFileW`: every file, both
  `DROPFILES` encodings, full UTF-16;
- `WM_DROPTARGET`'s result is the answer to the drag source: the effect the widgets chose
  for Enter/Over (`DragResponse::None` until a widget accepts; a copy falls back to a
  link, never to a move, when the source offers no copy), and whether a widget handled
  the Drop;
- `DragLeave` of any drag, and the end of a drop, produce `Event::DragEnd`.

**Both**: a drag has one target window and its position is in that window's
coordinates, so `Drag`/`Drop` are dispatched through `call_window_event_handler` and
`Cx::drag_event_is_for_window` (`os/cx_shared.rs`) lets the adjacent widgets patch keep
other windows from hit-testing them. macOS still broadcasts. X11 now cycles `drag_hits`
state after each drag event like the other desktops.

## Build-warning fixes

These patches only remove compiler warnings; they change no behavior.

- `component_map.rs` (`entry`) and `event/network.rs` (`split_url`) spell their
  elided return lifetimes (`Entry<'_, K, V>`, `SplitUrl<'_>`).
- The Android-only text-action request path (`TextActionsAnchor::Rect`,
  `CxKeyboard::show_text_actions` and its token counter in `event/keyboard.rs`)
  compiles only for Android and the crate's unit tests.
- `Cx::call_single_window_event_handler` (`os/cx_shared.rs`) serves only the
  desktop stdin transports and tests, so mobile targets omit it.
- `os/linux/openxr.rs` dropped an unused duplicate `AnchorAdvertisement` struct
  (and the serde import it alone needed). Upstream's unfinished Quest
  colocation payload in `os/linux/openxr_anchor.rs` is kept as published under a
  reasoned `#[allow(dead_code)]`, since only its `_start_colocation_*`
  placeholders reference it.
