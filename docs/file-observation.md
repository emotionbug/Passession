# File observation

Filesystem observation is Rust daemon functionality. Native file version observation lives in `packages/daemon/src/files/observer.rs`; checkout/Git observation lives in `packages/daemon/src/git/observation.rs`. Consumers use those owners rather than constructing a separate Node watcher or watching a remote path on the client machine.

## Individual files

The file observer resolves the requested cwd/path within the daemon's scope, shares a target by resolved path, and publishes version/status snapshots. It watches the parent non-recursively so atomic file replacement remains visible. Relevant changes are debounced; when the native watcher is unavailable or fails, the observer uses bounded polling. The implementation currently uses a five-second degraded polling interval and a 50 ms event debounce.

Observation ends when its receivers close or the owner shuts down. Path deletion and replacement are ordinary observable states, not permission to reset the editor. A client must preserve its unsaved draft and reconcile the new remote revision rather than silently overwriting either side.

## Checkout/Git scope

Git observation is separate from a file editor's version subscription. Share checkout facts among consumers with the same host/cwd, keep Git-ignore and repository-metadata policy in the Git owner, and avoid one full-tree scan or Git process per visible file. Directory identity includes the host.

Observation follows demand. A checkout is observed only while a client presents it — a `checkout.observe.request` subscription from the native Git store (Git panels and the chat header's Git button) or a diff subscription — and for 30 seconds after the last one ends. The first demand refreshes status and starts one background fetch; while observed, the checkout is fetched every 180 seconds. Registered workspaces are not observed by themselves. Worktree watches are non-recursive on each directory Git does not ignore (empty and tracked directories included), reconciled when directories or ignore rules change, with the existing polling fallback. Branch names for workspace descriptors and session placement come from mtime-cached reads of `HEAD`, `gitdir`, `commondir` and the repository config, without a Git process; full status and forge fields outside an observed checkout are served from cache. A client that does not advertise the observation capability keeps the previous connection-lifetime observation of the workspaces it can see.

Status/diff consumers request the facts they need. The Changes surface is summary-first; obtaining a file tree must not wait for every patch body or require a second client-side filesystem scanner. Script/setup output remains its own PTY stream, not a filesystem observation substitute.

## Verification

Exercise actual filesystem edits, atomic replacement, deletion, rename, inaccessible paths, watcher failure, last-subscriber release, and owner shutdown. Test against a disposable tree/home. Use Rust daemon tests and real native client read-back; the retired TypeScript server watcher benchmarks and Node/libuv implementation advice do not apply to this runtime.

A Linux run is not proof of Windows/macOS watcher behavior. See [testing](testing.md) for recorded platform evidence.
