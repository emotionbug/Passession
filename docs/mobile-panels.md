# Compact native panels

The Rust shell owns compact presentation in `packages/gui/src/shell/{layout,chrome,commands}.rs`. `common::is_compact_layout` uses a width below 600 logical pixels; `Layout::compact_destination` retains Sidebar, Agent or Explorer independently of transient geometry. Connections and persisted state belong to `paseo-gui-core`.

The compact flow is sidebar → session → supporting file view. Back returns from the file explorer to the session, then to the sidebar. A detail tab (file, diff, subagent, log) replaces the header's sidebar button with a visible Back control that returns to the tab that opened it. Leaving the app does not stop remote sessions. Settings and focused modal workflows retain their own navigation behavior.

Sidebar/session and session/detail transitions use the shared navigation motion intents:
Android uses Material Shared Axis, iOS uses a right-edge push with parallax, and a
compact desktop window uses a shorter fade/offset. Desktop non-compact tab switches
and split navigation remain instant. The Explorer keeps its right-side slide and
backdrop. Header controls, Escape, Back and the left-edge back gesture use the same
destination model; modals and floating surfaces can consume Back first.

Both screens are retained while moving; the outgoing screen has no hit targets and
pointer events are ignored until the destination settles. Back and programmatic
navigation can change the destination mid-motion. Resize/rotation snaps to the target.
Motion styles are selected by platform but do not call native animation APIs.

The header `⋯` exposes the selected session's actions (rename, fork, restart agent, archive, delete). There is no workspace action group or nested session tab switcher. The rightmost header button exposes Files, Git Log and Commit.

The Git Log is two screens on a phone. The log screen lists commits as two-line rows (lane graph, subject with its first ref pill, then author · age · short hash), with a Branch chip that opens a sheet of HEAD, local, remote and tag refs, a search toggle and refresh; the whole row is its tap target and a long press opens copy and diff actions. Tapping a commit opens the commit screen: message, author and date, copyable hash, parents and the changed files, each of which opens the commit diff as a detail tab. Back, Escape or the screen's arrow returns to the log with its scroll position and selection; the header's Back leaves the Git Log for the tab that opened it.

Commit is IntelliJ's Commit tool window: a scrollable tree of Changes (tracked) and Unversioned Files, grouped by directory with single-child chains compacted (or a flat list), each root, directory and file carrying a tri-state checkbox, above the multiline message and Commit button. The untracked split relies on the daemon's optional `untracked` flag on diff entries; older daemons list every file under Changes. File exclusions survive refresh; new files are not silently selected. On phones the message editor and full-width action stay pinned at the bottom above the keyboard, and Enter inserts a newline without dismissing it; desktop drags the divider between tree and message. A checkout that is not a Git repository, a disconnected or outdated host, loading, a failed load and a clean tree each get their own state and hide the form. Failed commits retain the message and selection; refresh never submits another commit automatically. A successful commit clears the submitted message and refreshes the list.

Keep navigation intent separate from transient geometry. Resizing or rotating must not change host identity, create duplicate sessions, lose drafts or mutate persisted collapse state. Compact sidebar navigation changes only `compact_destination`; the wide sidebar's saved visibility changes only from the wide layout. Repeated open/close actions and a cancelled gesture must settle to one coherent destination rather than leaving invisible hit regions.

The sidebar remains host → directory → session on compact screens. Every host is represented, empty directories remain visible, and equal paths on different hosts stay separate. Directory creation and remote browsing retain their explicit host/path target.

Keyboard and safe-area changes must keep the composer and primary actions reachable. Menus, pickers and supporting views must not intercept input intended for a different layer. A touch drag does not focus a different pane: focus changes on a valid release within the pane that received the press. Modal Back handling precedes compact navigation; see [floating-panel ownership](floating-panels.md#sizing-and-interaction). Provide visible controls as well as gestures.

Full-screen and compact bottom sheets use the shared sheet motion style. Dragging can resize a draggable sheet directly; releasing it snaps via the same transition scheduler rather than resizing and arranging the list on each frame.

Exercise the real Rust app with rapid navigation, rotation, keyboard, reconnect, persisted restart and Back. Linux compact-window evidence is not Android/iOS device evidence; see [mobile testing](mobile-testing.md) and [testing](testing.md).
