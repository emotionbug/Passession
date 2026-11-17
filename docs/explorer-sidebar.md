# Explorer and supporting panes

The Rust shell presents Files, Git Log and Commit alongside sessions. `packages/gui/src/shell/{layout,panels,commands}.rs` owns placement and retained roots; `src/features/files` owns remote file sessions and `src/features/git` owns Git views.

## Identity and retention

Every target carries its host identity. File and Git facts are directory-backed: equal cwd strings on different hosts remain distinct. Existing internal directory-owner IDs still retain drafts and layout; they are not a user-facing workspace or nested session group. A view must not change project membership merely because it is moved or closed.

Preserve unsaved editor text when changing width, theme, tabs or supporting-view presentation. The file store binds an editing session to its owner/tab; shell close guards ask that owner before closing or retargeting. Closing a chat tab is not archive; closing a script log view is not stopping a script.

Serialize writes for an editor. A save acknowledgement belongs to the submitted text, not whatever the user has typed or undone since submission. Releasing a file/picker owner retires its pending requests; late replies must not populate a newly opened owner with the same ID.

## Explorer sidebar

The main navigation tree remains host → directory → session, including existing sessions, empty directories, status, search and collapse state. The file Explorer is a supporting surface, not another session grouping layer. Add Project uses `features/files/picker.rs` (`RemoteFolderPicker`) to list/select a directory on the chosen host; an OS picker on the client cannot select a remote checkout.

Files are read/written through the daemon. Markdown and source editing share the same remote identity and revision. Git Log opens historical commit details and lazy patch bodies. The working-tree Changes view and session added/deleted-line badge are removed.

Plain file-link clicks in conversations and Markdown previews stay inside the app.
Ctrl+click on Windows/Linux or Command+click on macOS reveals a local file in the OS
file manager (opens a directory directly). Carry this intent through asynchronous
path lookup. Native reveal requires the target host's server ID to match the
machine's local daemon; lifecycle ownership and a loopback connection alone cannot
prove locality (loopback may be an SSH tunnel).
Remote links retain their internal target and placement. External URLs and touch
activation keep their existing behavior.

Commit lists uncommitted files with checkboxes and a multiline message editor. A checked file commits its complete current working-tree contents, not only its staged hunks. The daemon uses a temporary index and preserves unrelated staging and working files; normal Git hooks and signing still apply. Refresh preserves exclusions and does not automatically select newly appearing files. Failure retains the message and selection; an explicit refresh is required before retrying, and an unknown outcome is never automatically replayed. Older hosts without `gitSelectedCommit` expose an unavailable state rather than falling back to committing every file.

Git action retry resumes the unfinished steps only. Refresh repository state after failure; when the daemon reports an unknown mutation outcome, require inspection of that state rather than automatically resubmitting an operation that may already have succeeded.

## Compact views and sheets

On compact screens Back returns from the file explorer to the session, then to the sidebar. Modal tasks and menus have their own focus/dismissal lifetime. Keep host/session selection and drafts independent of transient sheet/pane geometry. A width change must not discard unsaved work or start a second subscription for the same owner.

## Side panes

Wide layouts can show supporting content beside the session. The shell dispatches each retained panel with its own target and activity rather than one global selection. Persisted layout is client state under the platform application-support directory; browser and executable React plugin targets are not restored as live surfaces.

Source editing uses the Rust buffer/editor on desktop; Android/iOS retain the read-only file branch. There is no bundled Neovim or embedded browser. Interactive terminals open as their own tabs; script/setup PTYs stay on the daemon with read-only native logs ([terminal activity](terminal-activity.md)). File save/conflict/close behavior and platform window/tray behavior each need execution evidence; see [testing](testing.md).
