# Vendored makepad-widgets 1.0.0

Source: published crates.io archive `makepad-widgets-1.0.0.crate`, SHA-256
`a5860c55bd4c7f57ce6787f3b79800d57e23073fde4d62c33db4faabdae30c64`.
The root workspace patches this crate and excludes the vendor directory. The
archive's Cargo.lock and .cargo_vcs_info.json are omitted. The published archive
omits a license file; LICENSE retains Makepad's upstream MIT license, identical
to the existing makepad-platform vendor license. The manifest declares
MIT OR Apache-2.0. Upstream fonts and icons remain unchanged; keep their existing
packaging notice coverage when staging resources from this path dependency.

## Window input ownership

`src/window.rs` applies the platform's native window-input scope before dispatching
to navigation, overlays or content. A key sent to one OS window must not dismiss a
different window's sheet or move its keyboard focus. Pointer routing and lifecycle
broadcasts keep their existing behavior; the platform patch owns native source
identity and unambiguous headless admission.

The same filter drops `Event::Drag`/`Event::Drop` that the platform addressed to another
window (`Cx::drag_event_is_for_window`): their position is in that window's coordinates,
so a second window would otherwise hit-test a file drag meant for the first.

## Native window caption

`src/window.rs` no longer shows the drawn `caption_bar` and `windows_buttons` on
Windows. The platform patch gives Windows the standard overlapped frame, so the OS
draws the caption, its buttons and Snap; see makepad-platform's "Native window chrome".
The caption bar stays defined for consumers that enable it explicitly.

## Scrollbar visibility

`src/motion.rs` shares platform choices, timing, curves and springs with the
client. `src/scroll_bar.rs` hides the thumb after 300 ms on Android and iOS
and fades it over 250 ms. Desktop keeps its 600 ms hold and 300 ms fade.
`fade_when_idle` stays enabled by default; hover and drag hold the thumb
visible, and new interaction cancels a pending fade. Both scrollbar shaders
fade the painted thumb without changing its hit area or geometry.

The hold uses one timeout. Frames run only during the fade and stop when
it finishes. A non-overflowing bar cancels both the timeout and fade.

`src/portal_list.rs` reports pixel scrolling within an item to the scrollbar.
Its index-based scroll position can stay unchanged during such a scroll, so
position updates alone cannot reveal the thumb.

Paseo's runtime theme calls `set_desktop_scroll_bar_color` with its foreground
color on palette updates. Each scrollbar applies it on draw only when changed,
using 54% alpha normally, 72% on hover and 90% while dragging. Without this
bridge, the stock dark-theme translucent white thumb disappears on light
surfaces. The thread-local palette follows the application's UI-thread theme
ownership; mobile and consumers not setting a color retain upstream colors.

## Caret blink

`src/text_input.rs` uses the shared 500 ms blink interval unless `blink_speed`
overrides it. The caret fades on macOS and iOS and switches immediately on
other platforms. Typing and caret movement restore full opacity. Focus loss,
backgrounding and hidden caret areas stop the timer and any fade.

The caret is a solid bar on whole device pixels. Upstream filled it through
`Sdf2d::box`, whose antialiasing leaves a 2px bar at about a third of its colour on
1x displays (measured 31–39% on a 100%-scale Windows desktop), split unevenly over
the pixel columns it straddles. `draw_cursor` now snaps the caret's position and
size to the pass's device pixels (at least one pixel wide) and the shader returns
the premultiplied colour without the distance field.

## Android composition

`src/text_input.rs` publishes the focused editor to the Android InputConnection
and applies `Hit::TextIme` snapshots through the existing edit history. One
composition remains one undo group; finishing only clears composition state,
without inserting the committed syllable twice. Desktop TextInput events retain
their existing behavior. The platform and packaged Java bridge must be shipped
with this widget change (see the platform's VENDORED.md).

Keyboard presentation is limited to focus/tap, and native composition state is
published without restarting the editor on every cursor/selection update.
Each TextInput's background, selection, text and caret share a widget-specific
draw-call group, preventing neighboring numeric/password fields from occluding
one another.

`TextInput::release_focus` and its ref helper share teardown with focus loss:
stop the blink timer, end composition/history state and release the owned native
editor even when its last draw area is invalid. Retained panels and discarded
settings fields use this lifecycle instead of waiting for an impossible hit on
an undrawn area. A newer pending editor keeps its focus and IME.

`TextInput.is_multiline` declares editor semantics independently of layout or
content and supplies Android's native `single_line` flag at every publication.
Multiline Enter replaces the selection with one newline through the existing
undo history and emits `Changed`, without hiding the keyboard. Single-line
Enter/Done (including Shift+Enter) emits `Returned` with its original modifiers
and dismisses the IME; it never inserts a newline. Multiline desktop composers
can opt into `return_key_submits`: plain Enter emits `Returned`, while Shift+Enter
and mobile Enter still insert a newline. GUI wrappers propagate their multiline
declarations instead of synthesizing Shift+Enter or replacing drafts on return.
`keyboard_edit_tests` cover mobile/desktop return actions, selection replacement,
cursor retention, undo and read-only behavior alongside the viewport regressions.

## Native text actions and typography

`TextInput` publishes capability-limited native actions for its current
selection or caret and applies them through existing edit/undo operations.
Touch release no longer auto-copies on Android. Outside dismissal and focus
teardown collapse selection and invalidate native action ownership. Painted
selection/caret areas provide anchors that the platform resolves after layout.

The retained text layout is keyed by the font catalog revision as well as font
family/size/spacing/scale, available width, wrapping and alignment. Replacing a
system face under the same family id invalidates retained metrics, including
navigation before repaint, without replacing text, selection or IME composition.
Color-only animation keeps the cached layout. Viewport widths are converted to
unscaled layout coordinates once. `TextInput::retained_text_layout` exposes the
current editor's cached rows to composer sizing and caret reveal; painting and
navigation reuse those rows rather than shaping the same text again.
`TextInput::text_ref` reads the editor text without cloning it; `text_layout`
remains available for other text such as the placeholder.

Cursor navigation and pointer placement rebuild an invalidated layout using the
last viewport before resolving rows. Input events may arrive between an edit
(including paste/undo/redo) and its paint; Arrow Down previously unwrapped the
missing layout, while Arrow Up discarded the movement.

`TextInputKeyCommand` resolves exact native modifiers once for TextInput and the
GUI's manual source editors. Word targets preserve Unicode graphemes; movement,
selection and deletion use the existing history and composition ownership.
See `docs/native-text-selection.md` for the OS key contract.

Wrapped layouts have zero horizontal scroll extent. The layouter intentionally
allows trailing breakable spaces beyond the wrap width; treating their advance as
horizontal overflow shifted all painted row starts under the left clip. Unwrapped
horizontal and multiline vertical scrolling retain their existing bounds.

A focused editor also reveals its existing caret when its viewport changes,
including keyboard-height changes without a width change. This updates scroll
only, not text, selection, composition or undo groups. An unchanged viewport
keeps manual scroll until an edit or explicit cursor move requests reveal.

Bounded multiline editors use the shared `TouchScroll` controller and wheel
events to reach earlier text without moving the caret, selection or composition.
They claim nested touch moves only after scrolling starts; a gesture starting
outward at an edge remains available to the surrounding form. Editing and
selection-handle capture cancel editor inertia.

Touch focus is acquired on a valid release inside the input, not on pointer
down. Leaving the field, panning, cancellation, focus/model replacement or
geometry changes invalidate the pending tap. Long press remains a distinct
selection action; mouse and explicit programmatic focus stay immediate.
`touch_focus_tests` cover the release boundary and invalidation transitions.

Long press opens the native toolbar at the hold threshold, without waiting for
finger-up. Identical-position move events do not remap a held selection when
IME presentation moves the input; actual movement still extends the range.

Word selection (long press, double tap) keeps the caret when the only segment
under it is a line break. At the end of a text that ends in a break, upstream's
boundary search selected that invisible break, so the toolbar had no drawn
anchor and the empty last line offered no Paste. `touch_focus_tests` pin both
the kept caret and ordinary word selection.

`draw_cursor` returns the caret rect it draws, and a focused editable input asks for
its input method with `Cx::show_text_ime_caret` and that rect relative to the
background. Upstream passed the caret offset inside the text rect against the
background area (off by the padding) and no height. Windows uses both to draw the
composition at the caret at the text's size.

## Shared touch selection handles

`src/text_selection_handles.rs` paints insertion/range handles and delegates
gesture ownership to the platform registry. `TextInput` uses the same helper as
Paseo's manual source, Markdown, chat and log owners. It preserves endpoint
identity through crossing, publishes IME selection through existing edit history,
and suppresses native actions during a handle drag. The Live shader requires an
explicit reachable import and `new_local`, not bare Rust-field initialization.

Knobs use an owner-aligned overlay turtle to escape nested text-content clips
without escaping draw-list/window clipping. A clipped caret suppresses its knob;
the overlay still follows the owner's final alignment.

`ScrollBars` and `PortalList` cancel touch inertia on handle capture.
`PortalListRef::scroll_selection` scrolls selected content without starting an
independent pan/fling; the owning timeline row remains responsible for source
hit testing after layout. Handle timing and edge speeds are not duplicated in
these consumers.

## Touch scrolling

`src/scroll_bars.rs` supplies the shared `TouchScroll` controller: slop-qualified
pan, complete initial displacement, incremental reversal, elapsed-time velocity
and exponential inertia, bounds and animation scheduling. Its first animation
frame establishes the render-clock epoch independently of Android touch uptime.
New touches, wheel input, long presses, pause and focus loss cancel motion.

`FlingSpec` supplies decay and stop speed at controller construction. Paseo
sets the per-OS spec and shared frame-request callback before constructing
widgets; Makepad's internal scroll controllers pick these up through
`TouchScroll::default()`. The standalone vendor defaults preserve its previous
physics and scheduling without depending on the GUI crate.

`ScrollBars`, `PortalList` and the app's manually drawn scroll surfaces consume
this controller. PortalList's duplicate drag/flick samples and per-frame decay
are removed. Its separate smooth scroll remains. At the top edge, Apple lists
resist outward movement and spring back; Android lists clamp. Other desktops
clamp unless the list explicitly sets `max_pull_down`, in which case it eases
back into place. Programmatic scrolling and selection still clamp on all
platforms. The rebound stops requesting frames at rest. Nested children handle
input first; selection and horizontal child drags retain their gesture ownership.
Sticky tap qualification prevents a pan ending over its original control from
becoming a click. Eighteen `touch_scroll_tests` cover slop, subdivision, reversal, bounds,
interruption, frame-rate/clock independence, release velocity and selection.

`TouchScroll::begin` rejects non-touch gestures even for direct callers such as
`PortalList`. Mouse drags remain text selection instead of panning/flinging the
list, and an idle list no longer overrides the child's selection cursor.

Release velocity uses a fixed-size 32-sample history over the last 100 ms,
including interpolation at the window boundary. Equal-timestamp moves retain
their displacement; a stationary hold cancels motion, while a late release
does not dilute a genuine recent flick with a synthetic zero-velocity sample.
Reversal and boundary termination discard velocity in the previous direction.

## Wheel under a pointer shield

`ScrollBar::handle_scroll_event` (`src/scroll_bar.rs`) and `TextInput`'s wheel
handler (`src/text_input.rs`) read `Event::Scroll` positions directly instead of
through `hits`, so the platform's pointer shields (see the adjacent
makepad-platform `VENDORED.md`) did not reach them: a wheel over a popover
scrolled the page or field underneath. Both now ask `Cx::pointer_shielded` for
their own area first. `PortalList` scrolls through `hits` and needed no change.

## Pointer shapes

`TextInput` requests `MouseCursor::Text` on hover-in. `RadioButton` no longer
resets the cursor on hover-out, which could overwrite the newly hovered
control's request. Shared shape resolution lives in makepad-platform.

## Rows that fit the viewport

`PortalList` computes `fits_viewport` only when every row in its range was
laid out and their total size fits within half a pixel. A partially drawn long
list during pull-down does not count as fitting. Fitting rows rest at their
configured top or bottom edge, report the end reached, draw no scrollbar and
leave wheel and touch movement to an enclosing scroller. They neither start
nor continue pull-down or fling motion. Previously a bottom-aligned short
list's positive offset became an 80px drag jump and a bounce that never settled.
The `scroll_anchor_tests` cover both alignment edges and incomplete layouts;
`touch_scroll_tests` cover drag and release when content is shorter than its
viewport.

## Final-position list painting

`PortalList::begin_measurement` opts into an exact extent-only pass, followed by
`next_final_item` for painting at resolved row positions. Ordinary consumers
retain their single-pass behavior. The measurement pass must not replace live
hit areas or emit primitives; the prepared immutable snapshot supplies both its
height and the later visible content.

Upward rows and sticky-bottom groups previously moved after glyph emission.
Their preliminary origins cannot safely decide which glyphs are offscreen.
The final pass supplies the resolved viewport while keeping measured neighbors,
full source geometry, scroll anchors and selection available to the owner.

## History prepend anchors

`src/portal_list.rs` adds `measured_scroll_anchor`,
`PortalListRef::scroll_anchor(min_id)` and `not_filling_viewport()`. The anchor
uses measured content rows and the current pending offset, excluding the
history sentinel and end padding. Replacing rows preserves drag/fling state.
Two `scroll_anchor_tests` cover sentinel exclusion and unmeasured movement;
the app's pagination latch is evaluated after layout.

## Distance to the list end

`src/portal_list.rs` adds `end_overflow` and `PortalListRef::end_overflow()`:
how far the range's last row extends below the viewport at its final position,
`None` while that row is not laid out. `is_at_end()` is exact, so one pixel
short of the end read as "not at the end"; Paseo's jump-to-latest button hides
within the original app's 32px of the newest edge instead. One
`scroll_anchor_tests` case covers the distance, end padding and the unknown case.

## Rust 1.93 diagnostics

`src/dock.rs` and `src/widget.rs` spell existing elided iterator lifetimes as
`'_`. Vendoring removes crates.io's dependency warning cap; these annotations
avoid exposing upstream lifetime-syntax warnings without changing the API.

## Image decoding

`Cargo.toml` and `Cargo.toml.orig` replace Makepad's `makepad-zune-jpeg` and
`makepad-zune-png` forks with `image` 0.25 (`png` and `jpeg` only, no default
features), the decoder the GUI already uses. `src/lib.rs` no longer reexports
`makepad_zune_jpeg` or `makepad_zune_png`, and `ImageError::PngDecode`/`JpgDecode`
carry `image::ImageError`; the `From<PngDecodeErrors>` conversion is gone.

`src/image_cache.rs` keeps `ImageBuffer`'s 0xAARRGGBB layout. Eight-bit gray,
gray-alpha, RGB and RGBA pass to `ImageBuffer::new` unchanged; other bit depths
are converted to RGBA8 first. Animated PNG frames come from the decoder already
composited (blend and dispose ops applied) and are packed left to right, top to
bottom, into one texture as before; the frame count is the number of decoded
frames. `image_size_by_data` returns a decode error for a corrupt JPEG header
instead of panicking. `image_cache::tests` cover alpha/ARGB packing, APNG
composition and disposal, and JPEG decoding with a corrupt header.

## Markdown parser

`pulldown-cmark` moves from 0.12.2 to 0.13 (still `simd`, no default features),
the version the GUI uses, so the lockfile carries one parser.
