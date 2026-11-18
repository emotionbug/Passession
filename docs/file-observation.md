# File observation

Filesystem observation is Rust daemon functionality. Native file version observation lives in `packages/daemon/src/files/observer.rs`; checkout/Git observation lives in `packages/daemon/src/git/observation.rs`. Consumers use those owners rather than constructing a separate Node watcher or watching a remote path on the client machine.

## Individual files

The file observer resolves the requested cwd/path within the daemon's scope, shares a target by resolved path, and publishes version/status snapshots. It watches the parent non-recursively so atomic file replacement remains visible. Relevant changes are debounced; when the native watcher is unavailable or fails, the observer uses bounded polling. The implementation currently uses a five-second degraded polling interval and a 50 ms event debounce.

Observation ends when its receivers close or the owner shuts down. Path deletion and replacement are ordinary observable states, not permission to reset the editor. A client must preserve its unsaved draft and reconcile the new remote revision rather than silently overwriting either side.

## Checkout/Git scope

Git observation is separate from a file editor's version subscription. Share checkout facts among consumers with the same host/cwd, keep Git-ignore and repository-metadata policy in the Git owner, and avoid one full-tree scan or Git process per visible file. Directory identity includes the host.

Observation follows demand. A checkout is observed only while a client presents it — a `checkout.observe.request` subscription from the native Git store (Git panels and the chat header's Git button) or a diff subscription — and for 30 seconds after the last one ends. The first demand refreshes status and starts one background fetch; while observed, the checkout is fetched every 180 seconds. Registered workspaces are not observed by themselves. A client that does not advertise the observation capability keeps the previous connection-lifetime observation of the workspaces it can see.

After acknowledging `checkout.observe.request`, the daemon sends that subscriber its current checkout status, even when the cache is already warm. Later updates are broadcast only when the observed snapshot changes.

The worktree uses one non-recursive watch per non-ignored directory, plus ancestors needed to see tracked files inside ignored directories. It enumerates the tree at startup, then adds or drops watches as directories come and go. An edit to `.gitignore` only reconciles that file's subtree; a change to `info/exclude` or the effective Git ignore configuration can reconcile the whole tree. There is no scheduled whole-tree inventory. Common and worktree-private Git directories are watched non-recursively, with a few additional metadata directories and the ancestor directories of refs currently needed by observed checkouts. The observer never watches `objects/`.

Events settle for one second, with a two-second maximum wait during sustained activity. Ordinary file batches check only the changed paths for ignores. A batch over 1,000 paths or a backend event loss refreshes all status; lost topology is reconciled. A failed watcher uses bounded polling and retries setup. Branch names for workspace descriptors and session placement come from mtime-cached reads of `HEAD`, `gitdir`, `commondir` and the repository config, without a Git process. Full status and forge fields outside an observed checkout are served from cache.

Status/diff consumers request the facts they need. The Changes surface is summary-first; obtaining a file tree must not wait for every patch body or require a second client-side filesystem scanner. Script/setup output remains its own PTY stream, not a filesystem observation substitute.

Checkout observation caches ref facts and working-tree status separately. Initial status reads branch headers from porcelain v2. Ordinary edits refresh only a bounded set of literal paths; index changes, overflow, or scopes that cannot preserve rename/ignore semantics use full status. Ref-only updates reuse working-tree status unless HEAD moved or the effective status configuration changed. Ref and config changes, not worktree edits, invalidate forge facts.

Diff subscriptions re-read only comparisons affected by those changes. Checkout status messages do not include line totals; workspace directory reads compute those totals lazily from the cached checkout, and legacy directory observers keep them current while subscribed.

## Verification

Exercise actual filesystem edits, atomic replacement, deletion, rename, inaccessible paths, watcher failure, last-subscriber release, and owner shutdown. Test against a disposable tree/home. Use Rust daemon tests and real native client read-back; the retired TypeScript server watcher benchmarks and Node/libuv implementation advice do not apply to this runtime.

A Linux run is not proof of Windows/macOS watcher behavior. See [testing](testing.md) for recorded platform evidence.
