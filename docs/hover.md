# Hover and input affordances

Rust widgets own hover and hit testing. The session tree is in `packages/gui/src/shell/sidebar.rs`; shared hit maps and controls are in `common/{paint,widgets}.rs`. Use the owning widget's current Makepad area and drawn geometry, not a retained draw primitive whose area can be replaced by a later draw.

A row and its nested actions must share a coherent hit region. Moving from the row to its visible action must not hide that action or activate the row behind it. Do not create invisible overlapping targets or let an overlay intercept clicks intended for a different host/session.

Anything revealed on hover needs a keyboard and touch route: visible menu controls, focus actions, or an appropriate long-press menu. Hover is not available on a phone and must not be inferred solely from operating-system name; desktop devices can also have touch input.

Popover triggers and content need stable focus/ownership across movement. Dismiss when the user leaves the interaction, not in the middle of travelling between trigger and surface. A delayed callback must not reopen a menu for a row that has been removed or changed identity.

Verify nested actions, scrolling under the pointer, overlay dismissal, focus, and compact touch behavior in the real Rust app. Include a click after the last overlay closes: a stale sweep lock can leave otherwise correct rows unreachable. Browser tests cannot establish native pointer behavior. See [menus](menus.md), [floating panels](floating-panels.md), and [testing](testing.md).
