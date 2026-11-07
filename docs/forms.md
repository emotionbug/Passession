# Native forms

Rust feature owners retain form state: `packages/gui/src/features/settings`, `schedules` and `composer` (creation), with host forms in `src/shell/sheets`. Use shared `common::widgets` controls, including `PTextField`, and `common::overlay` confirmation/select/prompt surfaces rather than a second form kit.

## State and data

Keep user intent, derived values, loaded server data and operation status distinct. Late provider/model discovery must not overwrite a choice the user already made. Edit forms retain the identity of the entity being edited; a host switch must not silently redirect a pending save.

Creation and editing need explicit initial data and lifecycle. A disconnected or failed catalog is not an empty successful catalog. Preserve drafts on save failure and show an actionable error. Validate user input at the boundary while retaining daemon-side validation as the authority.

Provider options are opaque provider-owned data. A provider or model change must remove incompatible dependent selections rather than sending stale options. CLI-default profiles are host-owned, not a separate local form preference.

## Presentation

Use localized labels, accessible focus order and native controls. Compact forms must remain usable with the keyboard and text scaling. Navigation and choice rows follow the [shared measured-row contract](design.md#reuse-and-hierarchy); editable fields still size from their native content and padding. Loading, pending, disabled, error and success are different states; a dismissed dialog is not proof that a mutation succeeded.

Remote directory selection uses `features/files/picker.rs` (`RemoteFolderPicker`) against the chosen host. Local attachment and save dialogs use `paseo-gui-platform`. Do not populate a remote destination with a client-side OS directory picker.

## Verification

Test slow and failed loads, out-of-order results, validation errors, duplicate submission, cancellation, retry, changing host/provider/model, and saving/read-back through the real daemon. Widget tests cover local transitions; actual native and wire behavior require their own execution evidence. See [testing](testing.md).
