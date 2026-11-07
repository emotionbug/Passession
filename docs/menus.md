# Native menus

In-app menus use `common::overlay` requests rendered by `packages/gui/src/shell/overlays.rs`. Use `MenuEntry`/`MenuItem` and the shared select surface instead of adding a feature-local popup engine. Desktop application and tray menus are separate Makepad/platform integration.

A trigger owns the entity, host, window and answer recipient it acts on. Tab menus and feature menus retain that target while open; an `overlay.result` is not permission to re-read a different globally selected session. Widgets without a tab use the widget-owner answer path. Keep right-click/long-press actions aligned with visible menu alternatives and keyboard access.

Distinguish selected value, keyboard/pointer focus, disabled state and pending operation. A pending asynchronous mutation must not be shown as successful merely because the menu closed. Destructive actions require explicit confirmation and must preserve the original target through that confirmation.

Choose presentation based on available space and input, not a blanket desktop/mobile OS split. Shared menus can use anchored popovers or compact sheets; selects share search, descriptions and selected/disabled states. Use the common overlay placement and dismissal helpers for feature-owned popovers too; keep feature data and answer routing with their existing owner. Follow [floating-panel ownership](floating-panels.md#sizing-and-interaction) for release validation, focus restoration and keyboard bounds. Do not transplant fixed delays from the former implementation without evidence of a native issue.

Submenus are useful when a current value matters more than the full option list. Avoid hiding essential primary actions several levels deep. Labels use sentence case and remain localized; long host/path/model labels must remain identifiable.

Test actual pointer, touch, focus, cancellation, failure and resize behavior. Desktop application/tray menus are separate native integration with their own platform verification requirement. See [testing](testing.md).

Desktop keyboard ownership is established by the native backend, not by the overlay stack
or the last clicked/editor-focused widget. macOS `NSEvent`/responder windows, Windows HWNDs
and X11 event windows retain their full generation-bearing `WindowId` through
`Cx::call_window_event_handler`. X11 asynchronous paste retains its requestor window.
The Makepad window wrapper filters raw key, text/IME and clipboard input **before** navigation
and overlay handlers; area hit-testing also rejects another window's input. Escape can
therefore reach only the source window's existing topmost eligible modal owner.

OS focus gain/loss saves and restores each window's editable focus independently of
Alt-Tab and pointer activity. Retained areas follow redraw remapping and are cleared when
their editor/window closes; recycled window slots cannot receive an older generation's
input. Focus transitions, actions, timers and other lifecycle events still broadcast.
Native macOS/Windows reentrant focus notifications are queued in source order, without
deferring synchronous clipboard or close-query responses.

Serialized `KeyEvent`, text input and existing stdin transport formats are unchanged.
Source-less legacy stdin input is accepted only with one live window; ambiguous multi-window
input is rejected, not assigned to the first/main window. Synthetic multi-window callers must
use the explicitly addressed Cx dispatch API. Mobile/web/direct single-window routing is
unchanged. These source changes and regression cases still require native macOS, Windows
and X11 runtime verification; static routing changes are not proof of exercised UI behavior.
