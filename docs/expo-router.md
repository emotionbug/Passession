# Native navigation

Navigation belongs to the Rust shell in `packages/gui/src/shell`. This filename is retained for links; Expo Router and React Navigation are not runtime dependencies.

`layout.rs` owns each window's tabs, split tree, focused pane, Explorer and compact destination. `panels.rs` mounts retained feature roots; `commands.rs` handles navigation, close guards and platform targets. `app.rs` owns the process-wide core/platform runtime and desktop windows. Host connections are shared; selected targets are not.

## Identity and selection

The primary tree is **host → directory → session**. A path is only meaningful together with its host. Preserve independent host and directory collapse state, including empty directories. Search can reveal matching ancestors without overwriting the saved collapse choices.

Workspaces remain durable daemon entities and Command Center targets, but they are not sidebar grouping rows. Closing a chat tab is navigation, not archive. Reopening a session must select its actual host and current persisted identity instead of inventing a new session. Attention navigation also retains the `(host, session)` pair through command selection; session IDs alone can collide across hosts.

Feature dispatch receives the owning panel's `UiContext.route`, `target`, `tab_key`, activity and window ID. Never resolve a split pane's action through the focused pane of another window. Moving or resizing a panel changes presentation, not its remote identity. Persist window layout through the core storage boundary described in [data model](data-model.md#client-side-stores).

Creation completion replaces the originating New Session tab in its existing pane or Explorer slot. Do not open the result through the focused pane or a desktop activation route: receipt updates can arrive after focus moves elsewhere. Keep the form recoverable until its destination exists, including the terminal response after workspace creation.

## Compact behavior

Back returns from file explorer to the session, then from session to sidebar. Leaving the mobile client does not stop a remote daemon or its sessions. Settings and modal workflows retain their own navigation state. Width changes must not change host/session identity or duplicate provider connections.

## Startup and external targets

Wait for initialized client state before restoring a window's layout. Platform activation is queued until the window is ready; the shell retains pending routes while resolving their host. Handle stale or unavailable targets visibly, including a restored terminal tab whose daemon terminal has exited. Do not discard saved hosts or drafts because startup or reconnect failed. Restored browser and plugin targets are not live product surfaces.

Window close and application Quit go through file close guards and platform lifetime policy. A tray-hidden window remains logically open; the retained first window can become dormant while other windows remain. Reusing it must not stop/restart the shared daemon or steal another window's layout.

Verify navigation through actual Rust startup, reconnect, resize and restart. Pure state tests do not establish Android/iOS lifecycle behavior. See [mobile testing](mobile-testing.md) and [testing](testing.md).
