# Native menus

In-app menus use `common::overlay` requests rendered by `packages/gui/src/shell/overlays.rs`. Use `MenuEntry`/`MenuItem` and the shared select surface instead of adding a feature-local popup engine. Desktop application and tray menus are separate Makepad/platform integration.

A trigger owns the entity, host, window and answer recipient it acts on. Tab menus and feature menus retain that target while open; an `overlay.result` is not permission to re-read a different globally selected session. Widgets without a tab use the widget-owner answer path. Keep right-click/long-press actions aligned with visible menu alternatives and keyboard access.

Distinguish selected value, keyboard/pointer focus, disabled state and pending operation. A pending asynchronous mutation must not be shown as successful merely because the menu closed. Destructive actions require explicit confirmation and must preserve the original target through that confirmation.

Choose presentation based on available space and input, not a blanket desktop/mobile OS split. Shared menus can use anchored popovers or compact sheets; selects share search, descriptions and selected/disabled states. Use the common overlay placement and dismissal helpers for feature-owned popovers too; keep feature data and answer routing with their existing owner. Follow [floating-panel ownership](floating-panels.md#sizing-and-interaction) for release validation, focus restoration and keyboard bounds. Do not transplant fixed delays from the former implementation without evidence of a native issue.

Submenus are useful when a current value matters more than the full option list. Avoid hiding essential primary actions several levels deep. Labels use sentence case and remain localized; long host/path/model labels must remain identifiable.

Test actual pointer, touch, focus, cancellation, failure and resize behavior. Desktop application/tray menus are separate native integration with their own platform verification requirement. See [testing](testing.md).

Window-local overlay state does not supply OS-window identity for raw keyboard events.
The current Makepad window wrapper filters pointer events by window, but forwards raw
`KeyDown` events to every window; those events carry no window identifier. The shared
dismissal helpers prevent repeated/nested dismissal within an owner, not cross-window
keyboard broadcast. Multi-window Escape/shortcut routing remains a separate native input
limitation and must not be claimed fixed by the overlay consolidation.
