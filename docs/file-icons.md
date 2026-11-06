# File and project icons

File icons belong to `packages/gui/src/features/files/{icons_data,images}.rs`; the files feature exports the shared drawing API used by Git. The SVG table is retained product data, decoded off-thread. Shell project-row presentation belongs to `src/shell/sidebar.rs`; reuse it rather than adding an independent icon/identity registry.

Project icon discovery is a host-side operation in `packages/daemon/src/files/icons.rs`. It searches bounded candidate locations, applies file-size/type/dimension checks and returns optional icon data. A missing or unreadable decoration is not a failed project; keep a generic native fallback and the text label.

An icon is decoration, not identity or authority. Host/path/session keys must not depend on it, and fetched project content must not execute scripts or create an embedded browser. Preserve accessible labels and enough contrast in both themes; do not let icon color compete with status/error indicators.

Desktop installer icons are maintained under `packaging/desktop`; mobile icon assets belong to `packaging/android/res` and `packaging/ios/Assets.xcassets`. Adding an asset requires attribution and packaging ownership, not a reference into the retained Flutter source tree.

Verify long filenames, unknown extensions, unavailable project icons, theme changes and compact rows in the actual Rust app. Include multiple Explorer instances: texture/draw state must not leak between panels. Native packaging and platform execution are separate checks; see [testing](testing.md).
