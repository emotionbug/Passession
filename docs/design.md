# Native design

Paseo is minimal, spacious and quiet. The agent workflow is primary; supporting tools help users understand, direct and review work. The native client uses Makepad Rust widgets in `packages/gui`. `src/theme.rs` owns palettes and typography; `src/theme_makepad.rs` supplies the stock-widget theme and font chains; `src/common` owns shared controls and painting. Feature presentation stays under `src/features`.

## Reuse and hierarchy

Read the existing native widget before adding another primitive. Reuse semantic colors, typography, spacing and interaction states. Paseo primitives read the runtime palette; stock DSL widgets need the theme's change-driven application path rather than fixed light/dark literals. See [native styling](unistyles.md). Hierarchy should be legible through weight, contrast and grouping, not a different arbitrary font size for every row.

Custom-painted lists share `common::widgets::RowMetrics` and `RowLayout` for leading, title, subtitle and trailing slots. Measure scaled font styles before laying out a list; use the resulting extent for painting, hit regions, scroll content and row advancement. Keep virtualized lists uniform within a draw rather than shaping off-screen labels, and reserve badge/icon height as well as text height. Wrapped bodies and editable fields keep content-driven `Fit` layout and their existing text/selection owners.

Adjacent labels use `InlineMetrics` to align their measured baselines and reserve the union above and below them. Giving Latin and Hangul labels the same top, or centering each independently, misaligns fallback fonts. Measure the actual truncated layouts and reuse them for ordinary and animated painting; the combined height also owns the row hit box and attached-detail offset.

Trailing actions reserve their space before title truncation. Empty text slots paint nothing; a long label must not cover a neighboring action. Row geometry does not own input: keep the surface's existing touch-up activation, scroll cancellation, long press and selection routing.

Use sentence case and concrete action names. A destructive consequence needs an explicit confirmation at the operation boundary; red styling alone is not consent. Errors should say what failed and offer a relevant retry without replacing the user's work.

## Navigation and identity

The sidebar is **host → directory → session**. Keep a visible expandable host row even for one host, preserve empty directories, and scope equal paths to their host. Search reveals matching ancestors temporarily. Sessions are direct Command Center targets; do not reintroduce workspace screens, grouping controls or nested session tabs. Existing internal owner IDs remain a persistence detail.

Host, directory and session rows have distinct actions. A directory's create action must use that exact host/path. Closing a session tab does not archive it. Add Project uses a remote folder picker, not a local OS path dialog for remote data.

## Responsive interaction

Design the compact case explicitly. The sidebar, session and supporting file view must remain reachable with Back and visible controls. Keyboard insets, safe areas, text scaling and rotation must not hide primary actions or change session identity. Wider layouts may show more context without creating a second state model.

In the wide layout, side columns shrink toward their minimum widths (200px for the sidebar, 180px for the Explorer) before squeezing the main area. The main area reserves at least 320px, or enough for the visible split tree when it needs more. When all three areas cannot fit, the sidebar steps aside while the Explorer is open; its toggle brings it back by closing the Explorer. Split leaves aim for 240px wide and 160px tall; when space runs out, each branch gets a share based on its subtree's minimum. These constraints never change saved split ratios. A sidebar shorter than 600px folds its navigation rows into one row; one narrower than 260px indents sessions one step less and moves their age into the subtitle.

A picker opened from a field (`WaSelectRef::open_at`) or the chat composer's model controls drops down from that trigger on wide layouts: below it, or above when the room below is short and there is more above, lined up with the trigger's nearer edge and kept inside the window. The trigger names the first page, so only picker subpages need a title and Back control. One Escape or an outside click closes it, and so does a window resize, since the trigger moves. Compact layouts present the same picker as a titled bottom sheet. Pickers without a field (a choice inside another sheet) stay dialogs.

Mobile multiline editors insert a newline on Enter and keep the keyboard open; single-line fields complete their action and dismiss it. Touch scrolling preserves the current editor, draft and keyboard. Only a completed outside tap changes focus. Bound long compact composers to their available viewport so the current line and action row remain reachable without forcing ordinary scrolling back to the caret.

Hover is a convenience, never the only access to an action. Provide pointer, keyboard and touch paths with clear focus and semantic labels. Align visible content while keeping sufficiently large hit targets. Long paths and provider/model labels must remain distinguishable without expanding every row uncontrollably.

Hit maps resolve the last registered target first. A row or card registers its own hit before its controls, so it never swallows its switch, menu or remove button, and a nested button never leaves a dead strip beside it. Painting and hits come from one fitted layout: when a width cannot hold a control group, reflow, shrink or move the rest behind an overflow control (the tab strip shows `+N`) rather than dropping controls or letting one overlap its neighbor's hit. Pane focus follows any press inside the pane, including content that captures the press itself; touch focus waits for a tap released in the same pane.

## Content surfaces

Markdown and source editing are native. Historical commit diffs are summary-first and load patch bodies lazily. Supporting tabs are Files, Git Log and Commit; there is no working-tree Changes view or session line-count badge. Commit provides file selection, a multiline message and explicit submission while preserving unrelated staging. Preserve drafts and selection through transient errors. Interactive terminals are native terminal tabs; display script output as read-only logs, with real lifecycle state and exit errors. The app does not contain a browser.

Commit lists files owned by the current checkout. Git's opaque untracked nested
repositories are not traversed; they must not block unrelated files. Directory
and gitlink selections are rejected explicitly rather than implying that a
nested checkout's changes were committed. Use Git directly for those entries.

The Git Log tab (`git_log`, IntelliJ's Git Log) follows the same tiers. From 900px a pane shows the ref tree (HEAD, local branches with ahead/behind, remotes grouped by remote, tags), the commit table (lane graph, ref pills, author, date, hash) and the selected commit's changed files above its message, author, hash and parents; the details move under the table below 1100px. Between 600px and 900px the ref tree folds into the Branch filter and the author column drops. Phones get the stacked flow in [compact panels](mobile-panels.md). The graph is laid out client-side from `parents` in `--date-order`, so appending a page never changes earlier rows; with text or author filters only commit dots are drawn, because a filtered list has gaps in its parent chains. A changed file opens the existing commit diff tab. History comes from the daemon's `checkout.log`, `checkout.refs` and `checkout.commit.get` requests behind the `gitLog` feature; an older daemon shows an update notice instead of an empty log.

Loading, empty, unavailable, failed, pending and successful states are separate. A disconnected host must not look like an empty host. A failed save must not look like a completed save. A permission request must not disappear merely because a different pane is selected.

## Verification

Review actual native screenshots and interaction, not mockups or a browser replica. Check light/dark theme, compact/wide sizes, keyboard, long content, loading/error states and slow networks. Measure core event processing, Makepad layout/draw work and daemon work separately. [Testing](testing.md) distinguishes current Rust evidence from historical client runs; unexecuted platforms remain unverified.
