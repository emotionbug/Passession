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

## macOS native tools

`build.rs` selects the Objective-C compiler and archiver through target-qualified
`CC`/`AR` variables (literal triple, then underscore triple), then `TARGET_CC`/
`TARGET_AR`, then `CC`/`AR`. Unset overrides preserve native `clang`/`ar`.
Cross builds supply executable wrappers with their target and SDK flags; changing
an override reruns the build script.

`src/os/apple/apple_sys.rs` uses the SDK's canonical `System` library and
`CoreMIDI` framework spelling. Native macOS's usual case-insensitive filesystem
hid the upstream `system`/`CoreMidi` names; a case-sensitive cross SDK does not.

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

Cocoa file drags and drops also carry their native destination window, like XDND
and OLE. They do not depend on or transfer keyboard focus. `performDragOperation:`
returns the actual handled BOOL. Destination exit/end/drop only clears drag
feedback; the separate source-end notification releases capture in the source
window, never through a synthetic release in window zero.

Desktop clipboard producers (`xlib_app.rs`, `macos_app.rs`, `win32_window.rs`)
ask the focused widget through the window-scoped `TextPasteShortcut` event
before requesting keyboard-shortcut paste. Terminals veto plain Ctrl+V at that
source; it remains PTY input even when X11 clipboard replies arrive after later
keys. Ordinary editors allow it by default. Menu, native text-action and IME
paste delivery does not use this veto; serialized `TextInputEvent` is unchanged.

### macOS Command key equivalents

Owned-window Command keys check the native application menu before text-input
dispatch. A handled menu key is not delivered again as a raw key (which could
execute New Window twice). If the original equivalent is unmatched and carries
non-ASCII input-source text, `key_equivalent.rs` retries the physical key's ASCII
equivalent, so Korean `ㅂ` at Q can invoke Command-Q. The original equivalent
retains priority; Latin layouts, ordinary composition and Control keys are not
rewritten. AppKit panels retain their own responder/menu routing.

### Horizontal mouse wheel

X11 button 4/5 and Windows `WM_MOUSEWHEEL` become horizontal scrolling while
Shift is held. Native horizontal input remains horizontal: X11 button 6/7 and
Windows `WM_MOUSEHWHEEL` preserve their own direction and modifier state.

## Windows clipboard ownership

`win32_window.rs` opens the clipboard with a real window owner and closes it on
every exit. Published UTF-16 uses `GMEM_MOVEABLE`; Windows takes ownership only
after `SetClipboardData` succeeds. Failed publication frees the allocation.
Reads stop at the first UTF-16 NUL, not at the global allocation's padded end.
Missing text formats and failed locks do not dereference null or unwind through
the window procedure. `GlobalUnlock` returning zero on the final successful
unlock is not a Rust error to unwrap.

## macOS input-method composition

`NSTextInputClient` previously retained `setMarkedText` without notifying the
editor, so Hangul appeared only after `insertText` committed it. Focused editors
now mirror into a window-owned `ImeComposer`; marked replacements deliver
`TextImeEvent` snapshots immediately, using Cocoa's UTF-16 replacement and
selection ranges. `markedRange` and `selectedRange` describe that same mirror.
`unmarkText` finishes the visible composition without inserting it again.

Focus changes discard the old native composition and fence snapshots to their
window and editor area. Programmatic discard suppresses re-entrant callbacks.
Keys that began or continued composition stay with the input method, including
the Return that commits a syllable, rather than reaching chat submission or
editor navigation afterward. `ime_composer` tests cover Hangul steps, commit,
unmark, cancellation, editor switches and supplementary Unicode ranges.

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

## Native renderer identity and animation pacing

X11 and Android request EGL swap interval 1 after each new window surface
becomes current. X11 dispatches `NextFrame` before each window repaint and
normally relies on swap backpressure. Software EGL (for example, llvmpipe on
Xvfb) or a rejected swap interval uses a display-rate deadline in the existing
X11/timer `select` wait. The fallback uses the XRandR screen rate when available,
otherwise 60 Hz. The optional XRandR library stays loaded until after
`XCloseDisplay`, which invokes registered extension callbacks. Android
also keeps its existing Choreographer (or timer) wake.

Windows queues sync-interval-1 presentation with a maximum frame latency of
one where supported. The next animation tick inherits the swap's backpressure;
without a window submission, it waits for the compositor instead. Minimized or
hidden windows skip rendering; `DXGI_STATUS_OCCLUDED` suspends submissions for
that swap chain until a periodic `DXGI_PRESENT_TEST` succeeds. Device setup tries
hardware adapters first, then the WARP software driver, and reports a startup
error if neither succeeds.

On macOS, a CoreVideo display link targets the key window's screen (otherwise
the first window's screen). It coalesces ticks onto the AppKit event queue
while `NextFrame` work or an IOSurface reuse wait is pending. Input and resize
paints do not advance animations themselves. If CoreVideo cannot start, the
existing timer supplies animation ticks capped at 60 Hz.

`gpu_info.rs` stores and logs the actual native API/renderer once during
initialization. EGL also reports GL vendor/version, shading-language version and
EGL vendor. Windows reports DXGI adapter description, D3D feature level and WARP
selection; Metal reports the selected device name. The same info reaches the
existing client diagnostics report and flags known CPU renderers.

## Opt-in macOS IOSurface presentation

`PASEO_METAL_SURFACE_PRESENT=1` enables an experimental, unified-memory macOS
window presenter. `os/apple/macos/metal_surface.rs` wraps three row-aligned
BGRA IOSurfaces as shared `BGRA8Unorm` Metal render targets. No drawable copy is
used for display. `CAMetalLayer` continues to be the default and handles live
resize, incompatible viewports, and unsupported or failed presenters. iOS
continues to use its own `MTKView` path.

The existing drawable layer leaves `colorspace` nil (no color matching).
The plain CALayer's IOSurfaces carry `kIOSurfaceColorSpace` set to the serialized
device RGB color space, preserving the same unmanaged premultiplied SDR values.
Labeling those values with the calibrated window/screen profile caused visible
color changes on a P3 Mac display; leaving the IOSurface untagged did too. The
native probe matched the nil-colorspace drawable with explicit device RGB.
Windows spanning different displays still need native testing. `contentsScale`
is the native backing scale; the unflipped hosted view uses top-left gravity and unflipped
texture contents. Size, scale, display/profile changes, fullscreen and wake
discard the ring; a new Metal device also gets new textures and clear state.

One window frame can be in flight. The Metal completion handler releases GPU
reads and posts a coalesced main-thread wake; scheduling alone does not guarantee
that Core Animation can read the pixels. The main thread assigns distinct
surface contents in a disabled-actions `CATransaction`. The newest submitted
front is reserved; older fronts are quarantined until a successor reports use
and a fresh `IOSurfaceIsInUse` check on the old surface is clear. If no buffer
is free, repaint work coalesces and reuse is retried at display cadence, not
in a spin loop. If a visible window cannot reclaim a buffer for 250 ms, the
presenter abandons its ring without reusing any uncertain buffer.

The backend-neutral `damage.rs` history is shared with EGL. Only a successfully
published frame advances the logical scene and history. A partial repair loads
and stores color and depth, scissor-clears both, and replays intersecting draw
items without changing their order or depth bias. The pass-owned depth buffer
must still represent the previous scene outside the repair; a changed depth
resource forces full repair. The optional Metal pixel oracle uses independent
scratch attachments, the same encoder inputs, and fenced readback; it rejects
the candidate and switches to the full drawable path on mismatch. See
[development](../../../../docs/development.md#macos-iosurface-verification)
for switches, Mac runtime scenarios, and timing/trace fields.

## Android Surface lifetime

`os/linux/android/android.rs` gates drawing and `SurfaceTexture.updateTexImage`
on a live window Surface. `SurfaceDestroyed` unbinds EGL, but queued RenderLoop
messages and background model updates can still arrive. Those events must not
consume redraw/next-frame work, clear upload dirty bits or allocate GPU objects
until a native window surface has made the EGL context current again.

The installed 0.13.10 APK's crash PC `0x27bebe8` is the indirect
`glDrawElementsInstanced` call. The device's Adreno stack then copies from null
in `memcpy`. An isolated host EGL run of the drawing handler and buffer uploader
reproduced the missing-context state corruption: the old path cached `Some(0)`
and cleared upload state; the gated path deferred work and uploaded after
restoration. This establishes the lifecycle defect, not the device's missing
pre-crash event sequence or an on-device confirmation of the fix.

Android chooses a window config with RGB8 and 24-bit depth, preferring no alpha
or stencil; Quest's OpenXR window requires alpha and pbuffer support instead.
Both paths reject MSAA for the window surface. A bad surface is re-created with
the retained context when the native window is still valid. On context loss the
GL backend forgets old names without deleting them in the new context, preserves
CPU data, and re-creates textures, geometry, instance/uniform buffers, VAOs,
programs and offscreen targets on the next draw. It repaints capture passes so
they repopulate the new render targets. Producer-owned Android video decoders
bound to the lost context are stopped; their external textures cannot be carried
into the new context.

## Android activity lifecycle (one native app per process)

Upstream quit the native loop on any activity `onDestroy`, including a stale
instance destroyed after its replacement attached through `SwitchedActivity`,
and a later activity in the same process then started a second `Cx`, which
process-wide state (the platform handle, JNI registrations) cannot accept.
The packaged Java (`packaging/android/makepad`) forwards lifecycle and surface
callbacks only from the most recently created activity and sends `Destroy` only
when that activity is finishing. Here, `android_jni.rs` starts the frame source
once per process (a reattaching activity would otherwise add a second vsync
chain), and `android.rs` drops the `Cx` and ends the process with exit status 0
once the loop has quit, so the next launch starts in a fresh process.

## Android composing input

`event/keyboard.rs`, `event/event.rs`, `event/finger.rs`, `cx_api.rs`, and
`os/linux/android/{android,android_jni}.rs` bridge the packaged Java full-editor
InputConnection. Ship those Rust changes and `packaging/android/makepad` together:
the upstream key-character JNI alone cannot carry Hangul composing replacements.
JNI strings use UTF-16, including supplementary characters; widget offsets remain
UTF-8. Editor identities distinguish multiple fields sharing one drawn area.
Session changes fence stale connections; revision acknowledgements cannot replace
newer IME text. Focused custom editors must publish via `update_text_input` after
edits and during drawing, declare whether they are single-line, and handle
`Hit::TextIme` through their own undo model. Single-line editors request Android's
Done action; it flushes composition before a session/revision-fenced native Enter.
Committed text, including multiline paste, is never interpreted as an action.
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
Outside mouse-down clears the previous selection before normal dispatch, without
consuming the new target's click. Outside touch defers that dismissal until a
completed single-finger tap, using the shared tap slop and timeout. Pans (even
returning to their origin), scroll events, cancellation, long press and multi-touch
retain the editor and IME. The pending tap follows redraw remapping but is retired
on focus/selection transfer or selection-handle capture, so it cannot blur a newer
editor. Android lifecycle blur remains immediate. Focus loss reaches retained
editors even when their old area is no longer drawable. The
`outside_text_selection_tests` regression filter covers these dispatch/ownership
rules; the Android lifecycle case is Android-only.

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

## Pointer shape and contrast

`src/pointer.rs`, called by `os/cx_shared.rs`, resolves a move to the default
arrow when the newly hovered area asks for no shape. The previous area keeps
its shape only while it remains hovered. Release, wheel and keyboard input
re-test the last pointer position after the next drawn frame and frames started
within 300 ms, unless a button is held or the pointer is outside the window.
`Cx::refresh_hover()` requests the same for changes outside input dispatch.

`src/cursor_tone.rs` inverts neutral body pixels when the system pointer's fill
matches the app theme. Coloured parts, shadows and already contrasting images
remain unchanged. X11 uses the user's Xcursor images and size; Windows copies
the stock cursor with `GetIconInfo`/`GetDIBits`/`CreateIconIndirect`.
X11 caches the current shape and tone and restores resize-edge cursors on
re-entry. Windows keeps the requested shape across `WM_MOUSELEAVE`, uses a null
cursor for `Hidden` instead of unbalanced `ShowCursor` calls, and defers shape
changes while the pointer is outside the app.

## Native geometry during callbacks

`os/window_state.rs` keeps nested focus and geometry notifications until the native
callback returns. Windows and macOS share this queue; repeated geometry retains
the original old state and the latest size, position and DPI for each window.
Focus order is preserved. Drag, clipboard and window-query callbacks still require
synchronous answers and do not enter this queue.

Windows `MoveWindow`, `ShowWindow` and DPI changes synchronously send `WM_SIZE`;
AppKit `setFrame` can likewise notify during platform operations. Dropping those
notifications leaves the app drawing at its previous size. The Windows posted-size
workaround in `restore` and `maximize` is removed. `d3d11.rs` allocates and compares
rounded physical pixel sizes including DPI. Metal already compares scaled sizes;
X11 delivers resize notifications later through `ConfigureNotify`.

## Pointer shields

Makepad hit-tests in dispatch order, not paint order: the first area that
contains a press takes it, and `capture_overload` only adds a second capture
(`CxFingers::capture_digit` pushes), so the area underneath still receives the
release and acts as well. A popover whose owner is dispatched after the content
it covers therefore could not own its own taps, hover or wheel.

`event/finger.rs` adds `Cx::set_pointer_shields(Vec<Area>)` (lowest first; an
empty list ends the scope) and `Cx::pointer_shielded(area, abs)`. While shields
are declared, a touch or mouse press, hover, sweep take-over or scroll inside the
topmost valid shield under the pointer is reported only to that area and to areas
whose draw list is the shield's or nests in it (`codeflow_parent_id`); every
other `hits*` call gets `Hit::Nothing`, capture overload included. Captures taken
outside a shield keep their moves and release; with no shield declared nothing
changes. Shields follow redraw area remapping. Handlers that read raw event
positions call `pointer_shielded` themselves.

Passession declares them in one place: features register popovers with
`common::overlay::float` and full-window sheets with `modal_float`, and the shell
installs the drawn ones of its window around the dispatch of panels and chrome
(`docs/floating-panels.md`). The
`paseo_touch_tests` shield cases cover press, capture overload, release, a
nested list, mouse, wheel, hover hand-over, a gesture begun outside, stale
shields and stacked shields.

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

X11 `WM_DELETE_WINDOW` now dispatches `Event::WindowCloseRequested` and
only destroys the window if the app accepts it. Previously every
`WM_PROTOCOLS` client message bypassed the shell's close guard and destroyed
the target. The Openbox smoke sends `_NET_CLOSE_WINDOW` and checks that a
refusal leaves the window visible, then that closing the last window exits.

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
  readable, so `select` returned at once. Stdin is no longer selected.

The remaining signal/action poll timer fired every 8 ms even with no work
(125 main-thread wakeups/s; measured 0.73% CPU in a debug idle window). It is
removed. `thread.rs` posts Linux/X11 UI/action signals to a process-lifetime,
nonblocking, close-on-exec eventfd (`os/linux/x11/ui_wake.rs`), selected alongside
the X connection. A read coalesces pending posts before dispatch checks the signal
flags and drains the action channel; posts racing with dispatch remain pending.
Media notifications retain their existing signal path. Disk live edits, explicit
DSL reloads and received Linux WebSocket messages also wake the UI, so live reload
does not depend on a periodic UI poll.

Application timers still supply the select deadline, and animation/next-frame
work keeps polling until its paints finish. Timer callbacks now paint their work
before sleeping again. `ui_wake` tests cover coalescing/rearming, cross-thread
signals/actions waking select without X input, and timer delivery without signals.
There is no native Wayland backend in this vendored crate; compositor sessions
use this X11 path through XWayland. The separate `linux_direct` backend's 8 ms
poll timer is unchanged, as are Android, OpenHarmony and stdin-hosted loops.

To measure the change, leave a smoke-harness window idle without animation or
application timers and compare `voluntary_ctxt_switches` in
`/proc/<pid>/task/<pid>/status` before and after 30 seconds. The removed timer
contributed about 3,750 switches in that interval; unrelated app/media work can
still wake threads. Post a `SignalToUI` signal and `Cx::post_action` from a worker
while idle and check prompt UI delivery, then exercise an animation and timer.
No post-change runtime measurement is recorded here.

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

## Desktop file drops (X11, Windows, macOS)

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

**macOS** (`os/apple/macos/file_drag.rs`, shared by the source and destination
callbacks). NSURL's decoded filesystem path is used instead of stripping
`file://` from `absoluteString`, which left Korean names percent-encoded and
made attachment reads fail. Native file URLs also encode outgoing paths;
internal IDs use the URL fragment separately from literal `#` in filenames.
Native Foundation tests read real files with composed/decomposed Hangul,
spaces, emoji, literal percent sequences and fragment-like names, and cover
internal-ID and empty-path round trips.

**Window routing**: a drag has one target window and its position is in that window's
coordinates, so `Drag`/`Drop` are dispatched through `call_window_event_handler` and
`Cx::drag_event_is_for_window` (`os/cx_shared.rs`) lets the adjacent widgets patch keep
other windows from hit-testing them. X11 now cycles `drag_hits`
state after each drag event like the other desktops.

## Scoped timer delivery

`Cx` records the destination of timers created inside an app-defined owner
scope, including timers created by stock widgets during a root's draw or
event handling. A one-shot is removed on lookup; stopping a timer removes its
registration too. This does not change the platform event payload or scheduling.

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

## windows 0.61

`Cargo.toml` and `Cargo.toml.orig` move `windows` and `windows-core` from 0.56 to
the 0.61 the GUI already uses, drop the `implement` feature and the direct
`windows-targets` 0.48 dependency, and add `windows-future` 0.2. The lockfile no
longer carries a second `windows` generation.

The `os/windows/*.rs` changes follow the 0.61 API: COM `*_Impl` traits are
implemented on the generated `*_Impl` types, interface parameters arrive as
`Ref<'_, T>`, optional handles are `Option` (`Some(hwnd)`, `None` for a null
window), `BOOL` comes from `windows::core`, `PROPERTYKEY` from
`Win32::Foundation`, DXGI flags are typed, and `HANDLE`/`HDROP` wrap pointers.
Three changes are not mechanical:

- `wasapi.rs` reads the endpoint's friendly name with `PropVariantToBSTR` and
  clears the `PROPVARIANT` afterwards, instead of formatting it.
- `WasapiBaseRef` is `Send`: its event handle is a process-wide kernel object
  that the worker threads already signal and wait on.
- `win32_window.rs` maps a failed `CreateWindowExW`, now a `Result`, to the
  null `HWND` 0.56 returned on failure.

`winrt_midi.rs` stores the WinRT event token as the `i64` 0.61 returns.

## CPU-backed texture uploads

Upstream Makepad [disabled GL partial uploads globally in `f10203851d27`](https://github.com/makepad/makepad/commit/f10203851d27a6b10f37bc79db32e631e2ead582),
following a simulator-only workaround; the commit did not diagnose the failure.
The disabled path relied on GLES3 unpack row/skip enums even in GLES2, assumed
rows were as wide as the texture, and used bytes per pixel as unpack alignment
(16 is not a legal alignment). It also left the unpack state changed.

`texture.rs` now computes a validated source range and row pitch for a dirty
rectangle, or the entire texture for a new allocation. GL allocates with
`glTexImage2D`, then uses `glTexSubImage2D` at unchanged dimensions and format.
It aligns byte textures to 1, restores unpack state, and uses row length only
on profiles that support it. GLES2 packs strided rows into reusable scratch.
The OpenHarmony simulator converts BGRA to RGBA in scratch instead of
mutating the retained texture pixels.

D3D11 creates a default-usage texture and view once per allocation and uses
`UpdateSubresource` with a destination box. Metal calls `replaceRegion` for the
dirty region. Both point to the first dirty pixel with the original source
row pitch. Frame tracing reports submitted upload count and texel bytes on
these three backends.

## Retained window damage

`damage.rs` compares the ordered render tree with the last successful present,
including old/new bounds, draw-list transform/clip/shift, rebuild and retained
instance/uniform revisions, texture/geometry content, and broad invalidations.
Resource uploads cannot accidentally acknowledge visual damage. Clip-bounded
vertex paths opt in by source, and unsupported transforms, changed unknown
lists, time-varying or external pixels, and unclassified repaint requests fall
back to rendering the full window. An unchanged draw with unknown bounds is
replayed inside the repair scissor.

X11 and Android keep a bounded EGLSurface-owned damage history. The acquired
buffer's age selects the repair region, which limits the color/depth clear
and draw submission. `EGL_KHR_partial_update`, when available, receives the
repair region; `eglSwapBuffersWithDamageKHR/EXT` instead receives the delta
from the immediately preceding presentation. Age zero, an insufficient
history, resize, expose/recreation, failed binding or presentation never
use assumed backbuffer contents. Culling preserves depth-bias order.

Windows uses flip-sequential `Present1` dirty rectangles with D3D11.1
`ClearView` and a scissor. DXGI reconciles old backbuffer contents outside
the rect; no additional window-sized copy is submitted. Without `ClearView`
support (or if flip-sequential creation fails), the regular flip-discard
path is retained. Metal still renders directly to a fresh drawable: a
persistent BGRA8 target would allocate another 3.06 MB at 850×900 and a
full drawable copy would read and write 6.12 MB per frame (367 MB/s at
60 Hz), even if the only change is a small indicator. There is no Metal
runtime measurement supporting that cost.

See the development guide for the Linux same-frame pixel oracle.
