# Compact native panels

The Rust shell owns compact presentation in `packages/gui/src/shell/{layout,chrome,commands}.rs`. `common::is_compact_layout` uses a width below 600 logical pixels; `Layout::compact_destination` retains Sidebar, Agent or Explorer independently of transient geometry. Connections and persisted state belong to `paseo-gui-core`.

The compact flow is sidebar → session → supporting file view. Back returns from the file explorer to the session, then to the sidebar. A detail tab (file, diff, subagent, log) replaces the header's sidebar button with a visible Back control that returns to the tab that opened it. Leaving the app does not stop remote sessions. Settings and focused modal workflows retain their own navigation behavior.

The header `⋯` leads with the selected session's actions (rename, fork, restart agent, archive, delete) before the workspace actions, so a phone reaches them without the tab switcher.

The Git Log is two screens on a phone. The log screen lists commits as two-line rows (lane graph, subject with its first ref pill, then author · age · short hash), with a Branch chip that opens a sheet of HEAD, local, remote and tag refs, a search toggle and refresh; the whole row is its tap target and a long press opens copy and diff actions. Tapping a commit opens the commit screen: message, author and date, copyable hash, parents and the changed files, each of which opens the commit diff as a detail tab. Back, Escape or the screen's arrow returns to the log with its scroll position and selection; the header's Back leaves the Git Log for the tab that opened it.

Keep navigation intent separate from transient geometry. Resizing or rotating must not change host identity, create duplicate sessions, lose drafts or mutate persisted collapse state. Compact sidebar navigation changes only `compact_destination`; the wide sidebar's saved visibility changes only from the wide layout. Repeated open/close actions and a cancelled gesture must settle to one coherent destination rather than leaving invisible hit regions.

The sidebar remains host → directory → session on compact screens. Every host is represented, empty directories remain visible, and equal paths on different hosts stay separate. Directory creation and remote browsing retain their explicit host/path target.

Keyboard and safe-area changes must keep the composer and primary actions reachable. Menus, pickers and supporting views must not intercept input intended for a different layer. A touch drag does not focus a different pane: focus changes on a valid release within the pane that received the press. Modal Back handling precedes compact navigation; see [floating-panel ownership](floating-panels.md#sizing-and-interaction). Provide visible controls as well as gestures.

Exercise the real Rust app with rapid navigation, rotation, keyboard, reconnect, persisted restart and Back. Linux compact-window evidence is not Android/iOS device evidence; see [mobile testing](mobile-testing.md) and [testing](testing.md).
