# Floating panels

Menus, selects, tooltips and dialogs use `packages/gui/src/common/overlay.rs` and are painted above content by `src/shell/overlays.rs`. Shell workflows use `src/shell/sheets`; feature-specific surfaces, including timeline tool/subagent sheets and composer suggestions, retain their feature owner. These are Makepad surfaces, not OS dialogs; file pickers and other OS presenters belong to `paseo-gui-platform`.

## Coordinates and ownership

Measure an anchor in its owning window's coordinate system. Requests carry both window and owner; the shell drains only its window's requests. Respect safe areas, viewport bounds and keyboard insets. A viewport or theme change must update geometry without changing the selected host/session or resetting a draft.

The trigger and overlay share one operation lifetime. Tab-owned answers return as `overlay.result` commands to that tab; standalone widgets use the shared widget-owner answer path. Retain the original host/entity in the request state. Cancellation, owner removal or navigation must invalidate delayed work rather than reopen a dismissed surface or apply its answer to another selection.

## Sizing and interaction

Keep content readable, bounded and scrollable. A searchable picker needs room for both its query and results; opening the keyboard must not cover the selected result or confirmation. Avoid first-frame flashes at an unmeasured position. Do not use offscreen placement as a substitute for an explicit opening/loading state.

Pointer travel between anchor and content must not dismiss the surface prematurely. The shell's modal sweep lock prevents input reaching covered content; release it when the final modal closes so later input is not trapped. Touch users need a deliberate trigger and dismissal path; keyboard users need predictable focus and Escape/Back behavior. Nested overlays should dismiss in order and restore focus without submitting the underlying composer.

## Verification

Test opening, closing, cancellation, rapid repeated triggers, host changes, resize/rotation, keyboard motion, safe-area insets and long labels on the actual native platform. A Linux screenshot does not prove Android/iOS native presenter behavior. See [mobile testing](mobile-testing.md), [menus](menus.md), and [testing](testing.md).
