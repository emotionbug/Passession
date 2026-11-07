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

## Verification

Test opening, closing, cancellation, rapid repeated triggers, host changes, resize/rotation, keyboard motion, safe-area insets and long labels on the actual native platform. A Linux screenshot does not prove Android/iOS native presenter behavior. See [mobile testing](mobile-testing.md), [menus](menus.md), and [testing](testing.md).
