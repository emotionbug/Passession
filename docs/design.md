# Native design

Paseo is minimal, spacious and quiet. The agent workflow is primary; supporting tools help users understand, direct and review work. The native client uses Makepad Rust widgets in `packages/gui`. `src/theme.rs` owns palettes and typography; `src/theme_makepad.rs` supplies the stock-widget theme and font chains; `src/common` owns shared controls and painting. Feature presentation stays under `src/features`.

## Reuse and hierarchy

Read the existing native widget before adding another primitive. Reuse semantic colors, typography, spacing and interaction states. Paseo primitives read the runtime palette; stock DSL widgets need the theme's change-driven application path rather than fixed light/dark literals. See [native styling](unistyles.md). Hierarchy should be legible through weight, contrast and grouping, not a different arbitrary font size for every row.

Custom-painted lists share `common::widgets::RowMetrics` and `RowLayout` for leading, title, subtitle and trailing slots. Measure scaled font styles before laying out a list; use the resulting extent for painting, hit regions, scroll content and row advancement. Keep virtualized lists uniform within a draw rather than shaping off-screen labels, and reserve badge/icon height as well as text height. Wrapped bodies and editable fields keep content-driven `Fit` layout and their existing text/selection owners.

Trailing actions reserve their space before title truncation. Empty text slots paint nothing; a long label must not cover a neighboring action. Row geometry does not own input: keep the surface's existing touch-up activation, scroll cancellation, long press and selection routing.

Use sentence case and concrete action names. A destructive consequence needs an explicit confirmation at the operation boundary; red styling alone is not consent. Errors should say what failed and offer a relevant retry without replacing the user's work.

## Navigation and identity

The sidebar is **host → directory → session**. Keep a visible expandable host row even for one host, preserve empty directories, and scope equal paths to their host. Search reveals matching ancestors temporarily. Do not reintroduce workspace grouping controls; workspaces remain available for orchestration and Command Center.

Host, directory and session rows have distinct actions. A directory's create action must use that exact host/path. Closing a session tab does not archive it. Add Project uses a remote folder picker, not a local OS path dialog for remote data.

## Responsive interaction

Design the compact case explicitly. The sidebar, session and supporting file view must remain reachable with Back and visible controls. Keyboard insets, safe areas, text scaling and rotation must not hide primary actions or change session identity. Wider layouts may show more context without creating a second state model.

Hover is a convenience, never the only access to an action. Provide pointer, keyboard and touch paths with clear focus and semantic labels. Align visible content while keeping sufficiently large hit targets. Long paths and provider/model labels must remain distinguishable without expanding every row uncontrollably.

## Content surfaces

Markdown and source editing are native. Diffs are summary-first and load bodies lazily; do not block opening the entire Changes view on every patch. Preserve drafts and selection through transient errors. Display script/setup output as read-only logs, with real lifecycle state and exit errors. The app does not contain an interactive terminal emulator or browser.

Loading, empty, unavailable, failed, pending and successful states are separate. A disconnected host must not look like an empty host. A failed save must not look like a completed save. A permission request must not disappear merely because a different pane is selected.

## Verification

Review actual native screenshots and interaction, not mockups or a browser replica. Check light/dark theme, compact/wide sizes, keyboard, long content, loading/error states and slow networks. Measure core event processing, Makepad layout/draw work and daemon work separately. [Testing](testing.md) distinguishes current Rust evidence from historical client runs; unexecuted platforms remain unverified.
