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

## Touch scrolling

`src/scroll_bars.rs` supplies `TouchScroll`: a pan starts only after the slop
threshold, then uses incremental deltas (including direction reversals).
Nested children handle input before the scroll container. Activating controls
uses the platform's sticky tap qualification, so a drag ending over its original
button is still a scroll rather than a click. Tests cover threshold, incremental
movement and reversal; actual Android sidebar, settings, logs and file surfaces
were exercised separately.

## Rust 1.93 diagnostics

`src/dock.rs` and `src/widget.rs` spell existing elided iterator lifetimes as
`'_`. Vendoring removes crates.io's dependency warning cap; these annotations
avoid exposing upstream lifetime-syntax warnings without changing the API.
