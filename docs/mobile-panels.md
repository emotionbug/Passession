# Compact native panels

The Rust shell owns compact presentation in `packages/gui/src/shell/{layout,chrome,commands}.rs`. `common::is_compact_layout` uses a width below 600 logical pixels; `Layout::compact_destination` retains Sidebar, Agent or Explorer independently of transient geometry. Connections and persisted state belong to `paseo-gui-core`.

The compact flow is sidebar → session → supporting file view. Back returns from the file explorer to the session, then to the sidebar. Leaving the app does not stop remote sessions. Settings and focused modal workflows retain their own navigation behavior.

Keep navigation intent separate from transient geometry. Resizing or rotating must not change host identity, create duplicate sessions, lose drafts or mutate persisted collapse state. Repeated open/close actions and a cancelled gesture must settle to one coherent destination rather than leaving invisible hit regions.

The sidebar remains host → directory → session on compact screens. Every host is represented, empty directories remain visible, and equal paths on different hosts stay separate. Directory creation and remote browsing retain their explicit host/path target.

Keyboard and safe-area changes must keep the composer and primary actions reachable. Menus, pickers and supporting views must not intercept input intended for a different layer. Provide visible controls as well as gestures.

Exercise the real Rust app with rapid navigation, rotation, keyboard, reconnect, persisted restart and Back. Linux compact-window evidence is not Android/iOS device evidence; see [mobile testing](mobile-testing.md) and [testing](testing.md).
