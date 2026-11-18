---
title: CLI reference
description: "Paseo CLI reference: manage projects, sessions, plugins, scripts, schedules, daemons, and permissions from your terminal."
nav: CLI reference
order: 35
category: Orchestration
---

# CLI reference

The Rust Paseo CLI manages agents and daemon operations from your terminal. These instructions target the unpublished 0.13.1 source checkout; build the native runtime as described in [Updates](updates.md), not an old npm CLI package. Keep the executable with its generated adjacent assets.

Bare `paseo` opens CLI chat. Use `paseo onboard` for explicit setup, `paseo daemon start` to start a detached daemon, and `paseo daemon status` to see its state. The default home remains `~/.paseo`.

> **Agent orchestration:** You can tell coding agents to use the Paseo CLI to spawn and manage other agents. Paseo recognizes the calling agent, so CLI-created workers get the same directory and parent defaults as MCP-created workers.

## Quick reference

```bash
paseo run "fix the tests"            # Start an agent
paseo ls                             # List running agents
paseo attach <id>                    # Stream agent output
paseo send <id> "also fix linting"   # Send follow-up task
paseo logs <id>                      # View agent timeline
paseo stop <id>                      # Stop an agent
paseo usage                          # Show account quotas and balances
```

## Account usage

Show account quotas and balances from the host's usage sources:

```bash
paseo usage
paseo usage ls --refresh
paseo usage ls --agent <agent-id>
paseo usage inspect <report-id>
paseo usage inspect <report-id> --refresh
```

`paseo usage` is equivalent to `paseo usage ls`. The summary groups windows and balances by account, with report IDs, source and account labels, status, and fetch timestamps. Missing values appear as `-`; unavailable and failed reports include their reasons.

Copy the full report ID from the list into `inspect` to see reset times, balances and units, plan details, supplied forecasts, and login failures with recovery instructions. Report IDs require exact matches.

`--agent` selects the account quota associated with an agent. It does not measure that agent's token consumption or cost. Use the full agent ID, and do not combine `--agent` with `inspect`.

Queries use the host's five-minute cache when available. Add `--refresh` to bypass it. Use `--host` or `--home` to [select a daemon](#select-one-daemon). Hosts without usage-source support return an update message.

### Usage in scripts

```bash
paseo usage ls --json
paseo usage inspect <report-id> --json
paseo usage ls --quiet
```

List JSON is always an array, including `[]` when no reports are returned. Inspect JSON is one report entry. Each entry includes `id`, `sourceId`, `sourceLabel`, `account`, `fetchedAt`, and a nested `report`. Read `report.status` before accessing its data:

| Status        | Fields                                                                 |
| ------------- | ---------------------------------------------------------------------- |
| `available`   | `windows`, with optional `planLabel`, `balances`, and `details`        |
| `unavailable` | `problem`, describing expired or rejected credentials or missing quota |
| `error`       | `error`, the failure message                                           |

Entries may also include `loginErrors`. Structured output preserves numeric values and optional fields. Standard `--format yaml`, `--no-headers`, and `--no-color` options are supported; `--quiet` prints full report IDs.

A completed query exits with status `0`, including when individual reports are unavailable or failed. Connection or request failures, unsupported hosts, unknown report IDs, and conflicting selectors exit with status `1` and write the error to stderr. Human output says **No usage reports returned** for an empty result; this does not establish whether account discovery succeeded for every source.

## Connected hosts

`--host` selects the daemon whose saved connections you manage. Registrations use their
`registrationId`, not their label.

```sh
paseo --host localhost:6767 host add 'ssh://deploy@build-box' --label build-box --meta team=infra
paseo --host localhost:6767 host ls --json
paseo --host localhost:6767 host inspect hst_123
paseo --host localhost:6767 host update hst_123 --meta team=tools --remove-meta obsolete
paseo --host localhost:6767 host update hst_123 --connection 'ssh://deploy@new-address'
paseo --host localhost:6767 host disable hst_123
paseo --host localhost:6767 host enable hst_123
paseo --host localhost:6767 host rm hst_123
```

Add accepts TCP/host-and-port, `ws://`/`wss://`, local socket/pipe, SSH, and relay/pairing URLs.
SSH connects to an already running daemon. Add `?password=...` to an SSH URI when that daemon requires a password (`&password=...` after `daemonPort`). Unreachable targets remain saved and retry automatically.
`ls` includes disabled and failed hosts. `inspect` includes safe connection details and recovery guidance.
JSON output never includes connection credentials.

`update --meta` replaces supplied keys; `--remove-meta` removes keys. Replacing a connection retains
the pinned daemon identity. Add `--replace-identity` with `--connection` only after verifying a changed
destination. `rm` removes the saved connection; sessions on that host keep running.

## Provider diagnostics

Ask the daemon to inspect the provider environment it actually uses:

```bash
paseo provider diagnostic claude
paseo provider diagnostic codex --json
paseo --host devbox:6767 provider diagnostic opencode
```

The diagnostic includes the configured command, daemon `PATH` and shell, matching binaries, resolved path, version, model count, and provider status. Use the global `--host` option for a remote daemon. This is the same diagnostic shown under **Settings → your host → Providers → provider → Diagnostic**.

## Running agents

Use `paseo run` to start a new agent with a task:

```bash
paseo run "implement user authentication"
paseo run --provider codex "refactor the API layer"
paseo run --no-wait "run the focused test suite"
paseo run --output-schema schema.json "name this branch"
paseo run --cwd ~/dev/my-app "implement feature X"
paseo run --cwd ~/dev/my-app "review the current diff"
paseo run --output-schema schema.json "extract release notes"
paseo run --output-schema '{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}' "summarize release notes"
```

From a human shell, `paseo run` starts a session in the current directory. Use `--cwd <path>` to select another existing directory on the target host. Paseo does not create Git worktrees or check out branches for a run.

When an existing Paseo agent runs the command, Paseo recognizes it through `PASEO_AGENT_ID`. Without an explicit directory, the new agent becomes its subagent in the caller's directory. An explicit `--cwd` changes the working directory without changing parentage.

Use `--output-schema` to return only matching JSON output. You can pass a schema file path or an inline JSON schema object. This mode cannot be used with `--no-wait`.

By default, `paseo run` waits for completion. Use `--no-wait` to return immediately while the agent keeps running, the same flag `paseo send` takes.

The old `run --background` and `-d` aliases still mean `--no-wait`. They are deprecated, hidden from help, and print a warning to stderr. Use `--no-wait` in scripts.

## Projects

Register the current directory as a project, then list the projects known to the daemon:

```bash
cd ~/dev/my-app
paseo project create
paseo project ls
```

Use the project ID from `paseo project ls` to rename, reset, or delete a project:

```bash
paseo project rename <project-id> "My app"
paseo project rename <project-id> --reset
paseo project delete <project-id>
```

`--reset` restores the name derived from the project directory. Deleting a project removes its registration and archives its active directory-owner records. It does not delete the project directory or Git worktrees.

For a local daemon, `paseo project create [path]` defaults to the current directory and resolves relative paths on the CLI machine. When you use the global `--host` option or `PASEO_HOST`, provide a path that the target daemon can access:

```bash
paseo --host devbox:6767 project create /srv/repos/api
```

The remote daemon interprets that path on its own machine. See [Sessions and directories](/docs/workspaces) for the host → directory → session navigation model.

## Terminals

These commands control daemon-side PTYs on the host. The app opens interactive shells as **Terminal** tabs and shows script output as read-only logs.

Select a session with `--agent`, or a directory with `--cwd`:

```bash
paseo terminal create --agent <agent-id> --name Development
paseo terminal ls --cwd ~/dev/my-app --json
paseo terminal send-keys <terminal-id> -l "echo ready"
paseo terminal send-keys <terminal-id> Enter
paseo terminal capture <terminal-id>
paseo terminal kill <terminal-id>
```

`--agent <id>` selects the session's directory owner. Without it, creation uses `--cwd` or the current directory. `ls --all` lists every terminal on the host and cannot be combined with agent or directory filters.

Use `--json` for structured output and the global `--host` option to target another daemon. Results retain internal `workspaceId` ownership fields for protocol compatibility; select sessions and directories rather than managing those IDs.

## Scripts

List, start, and stop scripts configured in the selected directory's `paseo.json`:

```bash
paseo script ls
paseo script start web
paseo script stop web
```

Pass `--agent <agent-id>` to select a session's directory owner, or `--cwd <path>` to select another directory. Without either, commands use the current directory. Use the global `--host` option to target another daemon. Standard output options such as `--json` are supported.

The output includes each script's lifecycle and supervised terminal ID. Services also include their assigned port, proxy URL, and health. See [Project scripts and services](/docs/worktrees#scripts-and-services) for `paseo.json` configuration.

## Plugins

> **Trust every plugin you add.** Native plugin executables, validation and declared preparation commands run unsandboxed with the daemon user's file, process, network and credential access. Dependencies and future updates are part of the trust decision. There is no JavaScript worker or Node fallback; the native client does not execute plugin UI. Global `--host` targets management at the remote daemon; directory paths must exist there and are not uploaded. `plugin init` creates a project locally.

The 0.13.1 checkout provides a Rust scaffold, not a claim that the SDK has been published. Use Rust 1.93+ and the local SDK path until your chosen native SDK revision is actually available remotely:

```sh
paseo plugin init ./plugin --id my-plugin --sdk-path /absolute/path/to/paseo/packages/plugin
cargo build --manifest-path ./plugin/Cargo.toml --release --target-dir ./plugin/target
./plugin/target/release/paseo-plugin-server --validate ./plugin 0.13.1
paseo plugin install ./plugin
paseo plugin ls my-plugin
paseo plugin update my-plugin --check
paseo plugin update my-plugin --yes
paseo plugin update --all --check
paseo plugin update --all --yes
paseo plugin reload my-plugin
paseo plugin logs my-plugin
paseo plugin disable my-plugin
paseo plugin enable my-plugin
paseo plugin remove my-plugin
```

On Windows, the scaffold selects `.exe`; invoke `.\plugin\target\release\paseo-plugin-server.exe`. The manifest requires `runtime: "native"` and `server: {"executable":"contained/relative/binary","args":[]}`. Rebuild before reloading a directory installation; reload does not compile Rust. Both root `pluginsEnabled` and per-installation enablement must permit execution.

Source syntax below is illustrative; substitute a real trusted repository/package/artifact:

```text
paseo plugin add owner/slug
paseo plugin install github:owner/repository --ref v1.2.0
paseo plugin install git:https://git.example/group/repository.git --ref v1.2.0
paseo plugin install github:owner/monorepo --path plugins/review
paseo plugin install cargo:paseo-review@1.2.0
paseo plugin install release:<HTTPS-archive-URL> --version <semver> --sha256 <digest> --format <tar.gz|zip>
```

Bare `owner/slug` is a registry listing, not a directory or GitHub shorthand. Prefix local relative paths with `./` or `../`. Git also accepts a `:<directory>` suffix. Cargo install versions use `@exact-semver`; omitting it selects the latest stable non-yanked release. Release archives require a lowercase 64-character SHA-256 over archive bytes and an explicit version; recognized URL suffixes can supply `--format`. `--path` selects a contained source subdirectory. A registry must provide native records; this checkout does not prove public listings have migrated. npm/legacy JavaScript sources are rejected, not executed.

Ordinary managed updates require `--yes`, and `--check` never applies. Explicit `--version <semver>` selects and authorizes one Cargo target; `--ref <ref>` does so for Git. Release updates use `--url <HTTPS-archive-URL> --version <semver> --sha256 <digest> [--format tar.gz|zip]`, retaining the installed URL origin; there is no implicit latest-release discovery. Explicit selections cannot be combined with `--all`. Proposals bind immutable artifacts; changed proposals fail instead of substituting content. Directory sources are edited/rebuilt/reloaded, not remotely updated.

`paseo plugin reload <id>` waits for the daemon's stop/start operation and queues behind other plugin work. Canceling the CLI or losing its connection does not cancel the daemon's operation; another reload queues a new stop/start rather than merely observing the previous one.

`paseo plugin ls [id]` reads installed metadata without querying the source. `paseo plugin logs <id>` returns a recent daemon-side snapshot, not a live follower. Native stdout is reserved for IPC; authors log via stderr or `Context::log`. Add `--json` for structured log entries, or use `paseo --host <target> plugin logs <id>` for another daemon.

Removal deletes the installation's settings; do not use remove/reinstall as a data-preserving migration. Duplicate-ID installation is rejected, and same-ID managed-source replacement is not implemented. The [migration guide](plugins/migration.md) documents an explicit directory-runtime port preserving the existing ID/settings and old receipts. See the [plugin reference](plugins/reference.md) for source, lifecycle, trust and IPC details, and [quickstart](plugins/index.md) for the runnable Rust example.

## Listing agents

```bash
paseo ls                    # Non-archived sessions with active directory ownership
paseo ls -a                 # Also include archived sessions
paseo ls -g                 # Non-archived sessions, including inactive directory ownership
paseo ls -a -g --json       # All sessions, including archived, as JSON
paseo ls --background       # Also include retained background sessions
```

## Streaming output

Use `paseo attach` to stream an agent's output in real-time:

```bash
paseo attach abc123   # Attach to agent (Ctrl+C to detach)
```

Agent IDs can be shortened, `abc` works if it's unambiguous.

## Sending messages

Send follow-up tasks to a running or idle agent:

Use the recipient's agent ID from `paseo ls`, or [copy it from the agent's tab](/docs/orchestration-workflows#send-a-prompt-to-another-agent).

```bash
paseo send <id> "now run the tests"
paseo send <id> --image screenshot.png "what's wrong here?"
paseo send <id> --no-wait "run this task"
paseo send <id> --behavior interrupt "stop and do this instead"
```

Agent-originated sends (`PASEO_AGENT_ID` set) default to steering the running turn. Other sends default to interrupting it. Use `--behavior steer` or `--behavior interrupt` to override the default. Steering uses the same fallback as the client: if steering is unavailable, the running turn is interrupted and replaced.

## Viewing logs

```bash
paseo logs <id>                  # Full timeline
paseo logs <id> -f               # Follow (streaming)
paseo logs <id> --tail 10        # Last 10 entries
paseo logs <id> --filter tools   # Only tool calls
```

## Waiting for agents

Block until an agent finishes its current task:

```bash
paseo wait <id>
paseo wait <id> --timeout 60   # 60 second timeout
```

Useful in scripts or when one agent needs to wait for another.

## Schedules

Run an agent on a cron schedule. The CLI also accepts simple cadence presets and compiles them to cron. See [Schedules from the CLI](/docs/schedules-cli) for the full reference.

```bash
paseo schedule create --every 30m --cwd ~/dev/my-app "Continue the refactor and leave a note."
paseo schedule ls
paseo schedule pause <id>
```

## Permissions

Agents may request permission for certain actions. Manage these from the CLI:

```bash
paseo permit ls                # List pending requests
paseo permit allow <id>        # Allow all pending for agent
paseo permit deny <id> --all   # Deny all pending
```

## Agent modes

Change an agent's operational mode (provider-specific):

```bash
paseo agent mode <id> --list   # Show available modes
paseo agent mode <id> bypass   # Set bypass mode
paseo agent mode <id> plan     # Set plan mode
paseo agent detach <id>        # Make a subagent top-level
```

Detaching is an explicit lifecycle action, not a creation flag. The agent keeps running; only its relationship to its parent changes.

## Daemon management

Define an instance once, then start its saved configuration:

```bash
paseo daemon config set daemon.listen 127.0.0.1:6799 --home ~/paseo-test
paseo daemon config set daemon.relay.enabled false --home ~/paseo-test
paseo daemon start --home ~/paseo-test
paseo project ls --home ~/paseo-test
paseo daemon restart --home ~/paseo-test
paseo daemon stop --home ~/paseo-test
```

`start` runs in the background and reports the actual listening address and supervisor PID. It accepts only home selection and `--timeout <seconds>` (default 600). If waiting times out, the supervisor remains running: use the printed status, log, and stop instructions. A worker that exits before becoming ready makes startup fail.

`paseo daemon ensure [--home <directory>] [--runtime <bundled-runtime-directory>] --json`
installs or starts an independent daemon. Use the complete bundled runtime directory
(including adjacent assets) for the first install; after that `--runtime` is unnecessary.
If an independent daemon already runs for the home, `ensure` leaves it alone. Simultaneous
requests for the same home share one installation. Success reports `state` (`installed`,
`started`, or `running`), `owner`, `pid`, and `listen`, after the daemon publishes its listen
address and local credential. Failure exits nonzero with `{"state":"failed","error":"…"}`.

On Linux, a working systemd user manager starts the daemon through `paseo.service`
(or a home-specific unit name if that name is taken). Paseo enables a new unit for
subsequent logins. A recognized existing unit for the same home is reused through an
ExecStart-only drop-in; its original unit file is not replaced. Linux without a
working user manager, macOS, and Windows start a detached daemon without login
autostart. On every platform the daemon keeps running when Passession exits.

The daemon shows a tray icon with its version and listen address in a supported
graphical session. The icon remains after you close Passession and disappears
when the daemon stops. Its menu can reopen Passession or stop the daemon. Stopping
requires a second click to confirm because it can interrupt running agents.
On Linux the icon needs a session D-Bus StatusNotifierWatcher (provided by many
desktop shells or a shell extension); it appears when the watcher becomes
available. Headless, SSH-only, and container sessions do not show an icon.

The tray is enabled by default. Run `paseo daemon config set daemon.tray false`
and restart the daemon to turn it off for that home.

A daemon started by an older Passession is replaced only after its running work
has drained and new work is paused. During active work `ensure` returns
`owner: "desktop-legacy"` and `migration: "pending"` and leaves that daemon running.
Run `ensure` again after work finishes. Migration keeps the same daemon home,
server identity, database, and local credential.

`restart` requests a replacement worker from the existing supervisor. It rereads the file and retains the supervisor's original environment and arguments. Success confirms a different ready worker, following a changed address for a home target. It never starts a stopped daemon or refreshes the supervisor binary. A timeout reports whether the request was acknowledged; it does not prove why reconnection failed.

`stop --home` waits for that local supervisor to exit. On POSIX it signals the supervisor without contacting a TCP endpoint. On Windows it uses the ready daemon's shutdown RPC; an unbound instance requires explicit `--force`. `--force` permits forced process-tree cleanup after the graceful timeout (default 15 seconds). `stop --host` only reports **shutdown requested**; remote process exit is not verified. A service manager may start another instance after the captured supervisor exits.

`status` separates local supervisor state, its published endpoint, the configured address, and RPC reachability. A stopped home is never probed at its configured address. An unbound live supervisor is **not ready**. If an authenticated local connection remains open but status details time out, the result stays `reachable` with a note explaining the unavailable details. Worker and provider fields are omitted. An explicit `--host` query still fails when its status request fails.

`reload` validates the file, applies runtime-safe changes, and reports `appliedPaths`, `restartRequiredPaths`, and `overrideControlledPaths`. It never implicitly restarts. Use `--json` or `--format yaml` for structured results. An older host lacking the capability reports that it needs an update.

The root aliases `start`, `status`, `restart`, `reload`, and `pair` use the same commands as `daemon`. Root `run` and `stop` remain agent operations.

### Durable updates

```bash
paseo daemon update --home ~/paseo-test --from /path/to/complete-runtime \
  --resume-agent <agent-id> --continuation-file continuation.txt
paseo daemon update status <operation-id> --home ~/paseo-test --json
paseo daemon update wait <operation-id> --home ~/paseo-test
```

`update` returns an `operationId` once the operation is durably admitted and handed to an executor outside the daemon; the replacement continues if this command, its connection, or the calling agent ends. Pass `--operation-id <uuid>` to fix the identity before submitting: repeating the same ID and input returns the existing operation, while the same ID with a different target or continuation is rejected. Without `--from` it installs the latest official release. `--from` takes a complete prepared runtime from this computer (not a bare executable); a local build may keep the same version number with a different digest and is never labeled an official release.

Official updates reuse release verification from the daemon's private cache for the selected home when the published inventory is unchanged. Stable releases normally use GitHub's direct download endpoint; beta releases still require the releases API for discovery.

`--resume-agent` and `--continuation-file` name one existing agent and the message it receives after the update commits. The file is read before submission, so later changes to it have no effect. `--idle-timeout <duration>` (default `5m`) bounds the wait for other running turns; when it expires the current daemon stays and the operation ends busy. `--force-running` interrupts those turns instead. While the update drains the daemon, new messages and agent creation are refused; stopping agents, closing items, and answering permissions still work. Connected apps that support it show a maintenance notice for that period.

`status` reads the operation record (`native-update-operation.json` in the home) and works while the daemon is down. A record left by the previous updater (`native-update.json`) is ignored. `wait` only observes; interrupting it never cancels the update. The `phase` `committed`, `rolled_back` or `failed` is the update result and `wait` exits 0 only for `committed`. `continuation.state` (`pending`, `accepted`, `not_delivered`, `unknown`, `failed`) is reported separately: an unconfirmed resume does not undo a committed update, and a later `status` can show an `unknown` resume as `accepted` once its message receipt proves delivery, without sending it again.

`--from` and `--continuation-file` require a local `--home`. With `--host`, `update` installs only an official release through a daemon that advertises `durableDaemonUpdates`, and `status`/`wait` query it remotely; a lost acknowledgement is retried with the same operation ID.

Windows daemons through 0.13.30 can fail before their first managed update commits.
Older versions can report `Cannot determine free space for the installation store`
or access denied while preserving the old runtime. Version 0.13.30 fixes those
failures but can still time out during startup or rollback when a descendant retains
the launcher's stderr pipe. Use a complete runtime containing the subsequent
launcher-completion fix; 0.13.30 is not sufficient for this recovery.
A phone request still uses the running daemon's old update code. Unpack the complete
fixed Windows daemon runtime and run **that runtime's CLI** from an independently
opened PowerShell window, under the same Windows account and with the existing
daemon home. Replace the example paths:

```powershell
& 'C:\path\to\fixed-runtime\paseo.exe' daemon update `
  --home 'C:\path\to\existing-home' --from 'C:\path\to\fixed-runtime'
```

An agent-launched shell can report `Cannot launch the update executor outside the
daemon job`: its Windows Job deliberately prevents child processes from escaping.
Use a separately opened shell instead of weakening that process boundary. Do not
overwrite the running executable or delete the operation journal. Wait for the
operation to report `committed`; later phone updates then use the fixed daemon.

To prepare an existing local build without claiming official release provenance:

```bash
cargo xtask build-daemon --release
cargo xtask stage-runtime /absolute/path/to/runtime --profile release --kind standalone-local
```

The runtime contains the executable, its libraries, skills, notices and matching build
provenance. Admission pins the input bytes; changing a directory or archive after admission
requires a new operation ID. Symlinked inputs, incomplete packages and insufficient space
are rejected before the running daemon is stopped.

Managed replacement supports a verified systemd user service or a detached daemon.
Installations created by `ensure` are compatible with daemon updates, even if the
initial runtime came from a desktop bundle. Foreground and unrecognized launch
owners are refused. An existing systemd service first adopted by the updater is
switched to the fixed launcher through an ExecStart-only drop-in, preserving its
original configuration. Later updates select immutable runtimes without rewriting the
service path. Updating a standalone runtime that predates drain support requires
explicit `--force-running`.

The fixed launcher, `bin/paseo` in the installation store, is also the stable `paseo`
command: every user command it receives runs the active runtime's CLI, so put it (or a
symlink to it) on `PATH` and the command follows updates. A link to one runtime directory
breaks once a later update removes that runtime.

The installation store lives outside `PASEO_HOME`, located by `native-installation.json`.
Active/trial runtime selection is separate from the operation journal. If a trial cannot
become ready, the launch owner restores the known-good runtime; a later launcher invocation
also recovers an interrupted, uncommitted activation. Readiness checks the actual supervisor
and worker images, their process identities, the authenticated server identity and the launch
owner—not merely an open port. The replaced runtime is kept only until the update commits;
no backup runtime remains afterwards. Runtime cleanup runs after a commit and when the
launcher starts with no update in progress. It removes runtimes of committed updates, including
a backup an earlier version kept, and best-effort failures are reported without undoing the
commit. It retains the active and trial runtimes, the candidate and known-good runtimes of
uncommitted, failed or rolled-back operations, and anything a running process uses. It does
not copy or restore the user's chat/receipt database.

### Foreground deployments and migration

Use environment overrides with the foreground deployment command:

```bash
PASEO_LISTEN=127.0.0.1:6799 PASEO_RELAY_ENABLED=false paseo daemon run --home ~/paseo-test
```

It stays attached until the supervisor exits or you cancel, without a readiness timeout. Worker restart retains these launch inputs. Stop and relaunch the deployment to change them. If the home already has a live supervisor, `run` returns `already_running` without owning or launching a foreground process.

Add `--no-tray` to `daemon run` to disable the icon for that launch without
changing the persisted `daemon.tray` setting.

Managed `start` ignores inherited daemon-setting overrides, including `PORT`, `PASEO_LISTEN`, and relay settings. It preserves provider credentials and executable/runtime controls. `start --foreground` is removed; use `daemon run`. Former start/restart configuration flags such as `--port` and `--no-relay` fail before side effects; use the corresponding persisted setting. `--web-ui` is obsolete and has no native replacement. See [configuration edits](/docs/configuration#apply-changes).

### Select one daemon

Every daemon-connected CLI command accepts global `--home` or `--host`, before or after the command. A home selects a local supervisor's published endpoint; a host selects an explicit endpoint. There is no configured-address or default-port fallback.

| Selectors                                  | Result                                                 |
| ------------------------------------------ | ------------------------------------------------------ |
| `--home`                                   | That local home, overriding both environment selectors |
| `--host`                                   | That endpoint, overriding both environment selectors   |
| Both flags, or conflicting duplicate flags | `TARGET_AMBIGUOUS`                                     |
| Only `PASEO_HOME` or only `PASEO_HOST`     | The corresponding target                               |
| Both environment selectors, without a flag | `TARGET_AMBIGUOUS`                                     |
| Neither                                    | Default local home, `~/.paseo`                         |

Local-only `start`, `daemon run`, `config`, `onboard`, and `set-password` reject explicit `--host` and ignore `PASEO_HOST`. Endpoint operations retain TCP, Unix socket, Windows pipe, SSH, and pairing-offer transports. A host-side CLI controlling a container needs `--host` or `PASEO_HOST`.

## Hub

```bash
paseo hub login [url]          # Approve and store organization-scoped CLI access
paseo hub init                 # Create and optionally deploy a starter trigger here
paseo hub connect [url]        # Enroll this daemon using CLI access
paseo hub projects             # List legacy projects in the authenticated organization
paseo hub status               # Show the current Hub relationship
paseo hub permissions list     # Show what this Hub may do on this daemon
paseo hub permissions grant hub.execute    # Let Hub automations run agents here
paseo hub permissions revoke hub.execute   # Take it back
paseo hub disconnect           # End it
paseo hub deploy               # Validate and install .paseo/triggers/*.yml
paseo hub deploy --dry-run     # Validate without installing
paseo hub deploy -p <project>   # Deploy an existing legacy project bundle
paseo hub logout               # Remove the active stored CLI login
```

Run deploy from the repository root. By default it reads every direct `.paseo/triggers/*.yml` file in deterministic path order. It validates all triggers before installing them one at a time. If an installation fails after earlier ones succeeded, the error lists the installed files. `--dry-run` only validates; it does not create or activate revisions.

Pass `-p, --project <slug>` for an existing legacy bundle: `.paseo/hub.yml`, direct `.paseo/workflows/*.yml` files, and referenced workflow partials. See [Deploy from the CLI](/docs/hub/configuration#deploy-from-the-cli).

`login` opens the Hub approval page and stores a durable organization-scoped CLI credential under `PASEO_HOME`. In an interactive terminal it offers to connect this daemon, then separately asks whether to allow Hub automations to run agents. Connection defaults to yes; execution permission defaults to no. It then links to Hub's **Triggers** page and prints `paseo hub init` for setup as code. `--json` and non-TTY login remain login-only and never prompt. The stored login is separate from the daemon relationship created by `connect`.

`init` requires a TTY. It signs in and connects the daemon as needed, then lists the organization's app connections that can back a starter trigger. One usable connection is selected automatically; with several, you choose a **Trigger connection**. If none is ready, setup sends you to **Hub → Apps** and stops before selecting an agent or writing files.

Setup asks which agent provider, model, and mode to run. Providers must be enabled and expose both a selectable model and an execution mode. Suggested model and mode entries are the daemon's defaults; a mode is still selected explicitly when there is no default. Hub validates the choice against the daemon before deploying, and accepts only Claude, Codex, and OpenCode for its unattended runs. `deploy` applies the same checks, so it needs the named daemon connected. Setup then asks for the identity allowed to trigger the bot: a GitHub username, Slack member ID, or Discord user ID. It validates the trigger, writes `.paseo/triggers/<provider>-help.yml`, and asks whether to deploy. Replacing that file requires confirmation; existing legacy bundles and other trigger files are preserved. See the [generated starter trigger](/docs/hub/configuration#generated-starter-trigger).

Interactive logout checks the same-origin daemon relationship and asks whether to disconnect before deleting the login. Declining removes only the login. JSON and noninteractive logout never prompt or disconnect implicitly; `--disconnect-daemon` is the explicit automation path, and `--force` applies to that daemon disconnection. If a requested disconnection fails, the login is preserved.

Every command resolves and normalizes its destination before Hub or daemon work. Origin precedence is an explicit command origin or `--hub`, then `PASEO_HUB_URL`, then the active stored login origin, then the hosted default `https://hub.paseo.sh`. The hosted default never overrides an active login. Credential precedence is `--api-key <secret>`, then `PASEO_HUB_API_KEY`, then a stored login for the exact resolved origin. A stored credential is never sent to a different origin. API keys passed through flags or the environment are not stored.

Human output reports the resolved destination before each action. JSON output keeps stdout machine-readable and includes the normalized Hub origin. Bundle diagnostics identify paths without printing configuration contents or credentials.

See [Daemons in Hub](/docs/hub/daemons), [Hub configuration](/docs/hub/configuration), and the [Hub public API](/docs/hub/api).

## Connecting to a remote daemon

The global `--host` option accepts either a local target (`host:port`, a unix socket, or a Windows pipe) or a pairing offer URL, the same `https://app.paseo.sh/#offer=...` link the mobile app uses for QR pairing. With an offer URL the CLI connects through the Paseo relay with end-to-end encryption, so you can drive a daemon on another machine without exposing it to the network.

Get an offer URL from the daemon you want to control:

```bash
paseo daemon pair          # prints the QR and link when relay is enabled
paseo daemon pair --relay  # enables relay without prompting
paseo daemon pair --json   # structured output; never prompts
```

Relay is off for new installations. A disabled relay returns a `RELAY_DISABLED` error; pass `--relay` to provide explicit consent. For a stopped home, pairing is labelled offline; `--relay` saves relay enablement and the offer includes a start instruction. A live but unreachable home never falls back to an offline identity. Relay pairing is end-to-end encrypted. See [Security](/docs/security).

Use it from anywhere:

```bash
paseo --host 'https://app.paseo.sh/#offer=eyJ2IjoyLC...' ls
paseo --host "$OFFER_URL" run "fix the failing tests"
```

You can also set it once via `PASEO_HOST` instead of passing `--host` on every command. An explicit flag overrides the environment variable.

## Multi-agent workflows

The CLI is designed to be used by agents themselves. You can instruct an agent to spawn sub-agents for parallel work:

```bash
# Agent A spawns Agent B and waits for it
agent_id=$(paseo run --no-wait --quiet --title api-agent "implement the API")
paseo wait "$agent_id"
paseo logs "$agent_id" --tail 5
```

Because Agent A's ID is present in the environment, Agent B is created as its subagent. It inherits the caller's directory unless `--cwd` is specified.

Simple implement + verify loop:

```bash
# Requires jq
while true; do
  paseo run --provider codex "make the tests pass" >/dev/null

  verdict=$(paseo run --provider claude --output-schema '{"type":"object","properties":{"criteria_met":{"type":"boolean"}},"required":["criteria_met"],"additionalProperties":false}' "ensure tests all pass")
  if echo "$verdict" | jq -e '.criteria_met == true' >/dev/null; then
    echo "criteria met"
    break
  fi
done
```

This pattern enables hierarchical task decomposition, a lead agent can break down work, delegate to specialists, and synthesize results.

## Output formats

Most commands support multiple output formats for scripting:

```bash
paseo ls --json                # JSON output
paseo ls --format yaml         # YAML output
paseo ls -q                    # IDs only (quiet)
```

## Global options

- `--host <target>`, connect to a different daemon (`host:port`, unix socket, or `https://app.paseo.sh/#offer=...` for relay). See [Connecting to a remote daemon](#connecting-to-a-remote-daemon).
- `--json`, JSON output
- `-q, --quiet`, minimal output
- `--no-color`, disable colors
