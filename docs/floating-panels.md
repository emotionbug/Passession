# Floating panels

Menus, selects, tooltips and dialogs use `packages/gui/src/common/overlay.rs` and are painted above content by `src/shell/overlays.rs`. Shell workflows use `src/shell/sheets`; feature-specific surfaces, including timeline tool/subagent sheets and composer suggestions, retain their feature owner. These are Makepad surfaces, not OS dialogs; file pickers and other OS presenters belong to `paseo-gui-platform`.

## Coordinates and ownership

Measure an anchor in its owning window's coordinate system. Requests carry both window and owner; the shell drains only its window's requests. Respect safe areas, viewport bounds and keyboard insets. A viewport or theme change must update geometry without changing the selected host/session or resetting a draft.

The trigger and overlay share one operation lifetime. Tab-owned answers return as `overlay.result` commands to that tab; standalone widgets use the shared widget-owner answer path. Retain the original host/entity in the request state. Cancellation, owner removal or navigation must invalidate delayed work rather than reopen a dismissed surface or apply its answer to another selection.

## Sizing and interaction

Keep content readable, bounded and scrollable. A searchable picker needs room for both its query and results; opening the keyboard must not cover the selected result or confirmation. Avoid first-frame flashes at an unmeasured position. Do not use offscreen placement as a substitute for an explicit opening/loading state.

Input follows the drawn layer order: standalone tool-call sheets, shell menus/dialogs/sheets, then the Command Center and retained feature surfaces. Snapshot the arrival-time owner before dispatch. Escape or Android Back belongs to that owner even if it closes or replaces itself; do not forward the same dismissal to its parent. Repeated Escape and an already-handled Back do not unwind another layer. Retained owners keep their registration order until they actually close and reopen. Nested subagent content remains reachable through its ancestor owner; a different widget ID alone is not grounds to block it. Unrelated modal siblings in the same tab remain blocked.

Outside dismissal is a release action. The press and valid release must resolve to the same backdrop; moving from content to the backdrop, scrolling, long press, touch cancellation and native text-handle dragging must not dismiss or activate a row. Reuse the existing `TouchScroll` for list motion, stop its inertia when ownership changes, and keep sheet resizing separate from field selection. Scroll starts in the list viewport, not the title, search field or scrim.

Covered fields may draw but must not autofocus. Closing an owner releases its editor and restores only a still-valid originating focus target; a newer focus request wins. Release the modal sweep lock when the final modal closes. Keep geometry within the keyboard-adjusted viewport even when it is smaller than the preferred popup minimum; clipped content needs a scrollable body rather than offscreen actions.

## Popovers over content

Dispatch order is not paint order. A feature's popover (the composer's suggestions, its context card) is painted in an overlay list above content that is dispatched before its owner: the message list, a parent's chrome, the shell's header. Makepad gives a press to the first area that contains it, and `hits_with_capture_overload` only adds a second capture, so without more both the popover and the control behind it act on one tap, a drag on the popover scrolls the list behind, and the content keeps the hover.

Declare such a surface once, when drawing it: `common::overlay::float(area)` with the hit area that covers it. The shell installs the drawn floating surfaces of its window as pointer shields (`Cx::set_pointer_shields`, vendored makepad-platform) for the dispatch of panels and chrome, after its own overlays have taken their input. Inside a shield a press, hover, sweep or scroll reaches only the surface's area and what is drawn in its draw list; every other `hits*` call reports nothing, so no per-widget "is a popup open" flag is needed. A surface stops shielding when it is redrawn without the area, detached with its layer, or dropped.

A full-window sheet declares its backdrop with `common::overlay::modal_float(owner, area)`, using its window-modal owner id. While that sheet is on top, only its backdrop and floating cards drawn inside it shield input; popovers behind it cannot interfere. The sweep lock alone does not protect against scrollers reading raw wheel positions. `WaModal` and timeline sheets declare their backdrops. A sheet without a registered backdrop keeps its own input rules.

Use `common::overlay::AnchoredPopover` for a button that opens a transient, non-modal panel such as the desktop task and subagent lists. The card is anchored and bounded by the current layout, without a scrim, sweep lock, or window-modal slot. Only the card shields pointer input: wheel and hover outside remain available to the normal target, and a wheel over a scrollable card scrolls the card without scrolling content underneath.

A click that starts and releases outside the card and its trigger closes the popover without consuming the click or taking focus from the clicked control. A drag, wheel, touch cancellation, or gesture crossing the card edge does not dismiss it. The trigger handles its own toggle, so one click closes it and an adjacent trigger can open its own panel in one click. Escape closes the top popover and returns keyboard focus to its still-drawn trigger; window deactivation closes without restoring focus. Drop the popover when its owner is hidden or retargeted. Resizing repositions the card; crossing the compact breakpoint closes the old presentation. On compact layouts the track buttons still open a modal bottom sheet with a dimmed, consuming backdrop.

For a pane-local loading/disconnected cover or a search card, wrap the painted surface in `common::widgets::PointerShield`. It owns a separate draw list and registers the visible area. Pointer shields follow every attachment up the draw-list tree: a cached cover stays active while replayed, but stops shielding as soon as a frame omits its branch. Wrap the card itself, not a transparent positioning container. Its controls keep receiving input while underlying siblings do not; non-input updates continue.

Rules that follow:

- Do not rely on capture overload, dispatch order or a parent's rectangle test to keep a popover's taps to itself. Register the popover.
- A handler that tests a raw `Event::Scroll`, `Event::MouseDown` or touch position against a rectangle bypasses hit testing. It must also ask `cx.pointer_shielded(own_area, abs)`.
- A tooltip or other surface that takes no input is not registered: the pointer passes through it.
- A modal surface still claims the window-modal slot or the shell's stack: that is what keeps keys, shortcuts and the shell's chrome away. The shield only adds the pointer and the wheel.

## Verification

Test opening, closing, cancellation, rapid repeated triggers, host changes, resize/rotation, keyboard motion, safe-area insets and long labels on the actual native platform. A Linux screenshot does not prove Android/iOS native presenter behavior. See [mobile testing](mobile-testing.md), [menus](menus.md), and [testing](testing.md).
