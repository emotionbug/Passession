# Vendored makepad-widgets 1.0.0

Source: published crates.io archive `makepad-widgets-1.0.0.crate`, SHA-256
`a5860c55bd4c7f57ce6787f3b79800d57e23073fde4d62c33db4faabdae30c64`.
The root workspace patches this crate and excludes the vendor directory. The
archive's Cargo.lock and .cargo_vcs_info.json are omitted. The published archive
omits a license file; LICENSE retains Makepad's upstream MIT license, identical
to the existing makepad-platform vendor license. The manifest declares
MIT OR Apache-2.0. Upstream fonts and icons remain unchanged; keep their existing
packaging notice coverage when staging resources from this path dependency.

## Desktop scrollbar visibility

`src/scroll_bar.rs` adds desktop auto-hide, enabled by `fade_when_idle` by
default. Scrolling reveals the thumb, holds it for 600 ms, then fades it over
300 ms. Hover or drag holds it visible. A new interaction cancels the pending
fade. Both upstream scrollbar shaders multiply their premultiplied output by
opacity, preserving hit testing and geometry while invisible.

The hold uses one timeout, not a repeating timer or an animation. Frames run
only during the fade and stop at zero opacity. A non-overflowing bar cancels
its timeout and fade. Android and iOS bypass this behavior entirely.

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

`src/text_input.rs` retains the upstream cursor fade on macOS and iOS. On other
platforms, blink transitions cut the existing animator track and repaint once
instead of scheduling opacity-animation frames. The 500 ms blink interval,
focus handling, selection and editing behavior remain unchanged. This central
change applies to every TextInput, including feature-owned fields.

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

## Native text actions and typography

`TextInput` publishes capability-limited native actions for its current
selection or caret and applies them through existing edit/undo operations.
Touch release no longer auto-copies on Android. Outside dismissal and focus
teardown collapse selection and invalidate native action ownership. Painted
selection/caret areas provide anchors that the platform resolves after layout.

The retained text layout is keyed by font family/size/spacing/scale, available
width, wrapping and alignment. Live system-font changes and width reflow do not
reuse stale glyph metrics or replace the editor's text, selection or history;
color-only animation keeps the cached layout.

Touch focus is acquired on a valid release inside the input, not on pointer
down. Leaving the field, panning, cancellation, focus/model replacement or
geometry changes invalidate the pending tap. Long press remains a distinct
selection action; mouse and explicit programmatic focus stay immediate.
`touch_focus_tests` cover the release boundary and invalidation transitions.

Long press opens the native toolbar at the hold threshold, without waiting for
finger-up. Identical-position move events do not remap a held selection when
IME presentation moves the input; actual movement still extends the range.

## Touch scrolling

`src/scroll_bars.rs` supplies the shared `TouchScroll` controller: slop-qualified
pan, complete initial displacement, incremental reversal, elapsed-time velocity
and exponential inertia, bounds and animation scheduling. Its first animation
frame establishes the render-clock epoch independently of Android touch uptime.
New touches, wheel input, long presses, pause and focus loss cancel motion.

`ScrollBars`, `PortalList` and the app's manually drawn scroll surfaces consume
this controller. PortalList's duplicate drag/flick samples and per-frame decay
are removed; its separate smooth-scroll and pull-down behavior remains, with
pull-down decay expressed in elapsed time. Nested children handle input first;
selection and horizontal child drags retain their gesture ownership. Sticky tap
qualification prevents a pan ending over its original control from becoming a
click. Eighteen `touch_scroll_tests` cover slop, subdivision, reversal, bounds,
interruption, frame-rate/clock independence, release velocity and selection.

Release velocity uses a fixed-size 32-sample history over the last 100 ms,
including interpolation at the window boundary. Equal-timestamp moves retain
their displacement; a stationary hold cancels motion, while a late release
does not dilute a genuine recent flick with a synthetic zero-velocity sample.
Reversal and boundary termination discard velocity in the previous direction.

## History prepend anchors

`src/portal_list.rs` adds `measured_scroll_anchor`,
`PortalListRef::scroll_anchor(min_id)` and `not_filling_viewport()`. The anchor
uses measured content rows and the current pending offset, excluding the
history sentinel and end padding. Replacing rows preserves drag/fling state.
Two `scroll_anchor_tests` cover sentinel exclusion and unmeasured movement;
the app's pagination latch is evaluated after layout.


## Rust 1.93 diagnostics

`src/dock.rs` and `src/widget.rs` spell existing elided iterator lifetimes as
`'_`. Vendoring removes crates.io's dependency warning cap; these annotations
avoid exposing upstream lifetime-syntax warnings without changing the API.
